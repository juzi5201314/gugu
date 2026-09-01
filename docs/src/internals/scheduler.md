# 调度器

本章规定 Gugu runtime 当前的 M:N 调度器、可复制协程栈、工作窃取、park/ready、抢占、I/O/计时器和 GC/foreign-call 交接。语言可观察的公平、等待、退出和动态并行度规则见[并发与调度](../spec/concurrency.md)与[运行时](../spec/runtime.md)；队列结构和时间片是内部规范，不能被用户程序观察或依赖。

## 权威边界

[表达式](../spec/expressions.md)、[并发与调度](../spec/concurrency.md)、[运行时](../spec/runtime.md)和[平台 ABI](../spec/platform-abi.md)唯一规定 async/select求值、同步、公平、动态控制、进程寿命及 FFI结果。本章只固定官方 runtime如何实现已经给定的调度事件和状态转换；队列位置、worker身份、随机序列、时间片和park协议不能成为程序语义。

除平台 rt0、context switch、signal/exception stub和必须的 machine intrinsic外，官方 scheduler/runtime主体使用 Gugu实现；不得维护一份 Rust语义等价runtime作为正常执行路径。

## 三层实体

调度器采用 Go 风格的三层模型，但使用完整名称而不是单字母缩写：

- `Coroutine`：用户协程，拥有可复制 stack、寄存器 context、coroutine-local 和等待状态；
- `LogicalProcessor`：执行 Gugu 用户代码所需的 runtime capability，数量等于当前 `parallelism` 目标；
- `WorkerThread`：操作系统线程，绑定一个 processor 时执行用户代码，也能在无 processor 时执行 foreign call、poller 和 runtime 系统工作。

一个 `Coroutine` 同时最多由一个 worker 执行；一个 `LogicalProcessor` 同时最多绑定一个 worker；一个 worker 同时最多绑定一个 processor。阻塞 foreign/system call 可以保留 worker 而释放 processor。

ID 表示固定为 `CoroutineId(u64)`、`LogicalProcessorId(u64)` 和 `WorkerThreadId(u64)`，进程内单调分配且不复用。计数溢出说明 runtime 内部不变量已破坏，进入 `RuntimeInvariant` fatal，不能回绕。

## Coroutine

### 控制块

`Coroutine` 控制块的字段与所有权按下列逻辑顺序固定；它不直接序列化进镜像，但 runtime、GC typed visitor 和 debugger 必须使用同一声明，禁止拆出平行的影子控制块：

```text
Coroutine {
    id,
    state: AtomicU32,
    stack: StackDescriptor,
    context: CoroutineContext,
    morestack_scratch: MorestackScratch,
    current_processor: AtomicPtr<LogicalProcessor>,
    wait_record: WaitRecord,
    run_link: intrusive queue link,
    join_state,
    coroutine_locals,
    panic_state,
    select_rng: [u64; 4],
    select_scratch: Vec<u64>,
    gc_scan_epoch: AtomicU64,
}
```

`select_scratch` 只保存 readiness bitmap，不保存 managed pointer；source、send payload 和 result slot仍位于用户 frame并由 stack map追踪。runtime typed visitor跳过 scratch bytes。

`morestack_scratch` 保存 return PC、九个 GPR参数和八个 XMM参数；只有 lifecycle Running且当前 PC为 `MorestackEntry` 时有效，并由该 entry map精确扫描，不属于常驻 runtime root。

`state` 低 4 bit 是 lifecycle：

| 值 | 状态 | 含义 |
|----|------|------|
| 0 | `New` | context 已构造，尚未入队 |
| 1 | `Runnable` | 可执行，位于 run queue 或即将发布 |
| 2 | `Running` | 正由一个绑定 processor 的 worker 执行 |
| 3 | `Parking` | 正在提交等待条件，尚未确定是否睡眠 |
| 4 | `Waiting` | 已挂在 channel/lock/timer/I/O/join 等等待源 |
| 5 | `Foreign` | 在 worker 的 OS stack 上执行外部调用 |
| 6 | `Dead` | 用户 body、panic 清理和 Join 发布已经结束 |

bits 4..6 固定为 `ENQUEUED`、`PREEMPT_REQUESTED` 和 `STACK_SCAN_LOCKED`；bits 7..31 必须为 0。所有 lifecycle 转换用 compare-exchange 并携带必要的 acquire/release ordering，不能以互斥锁外的普通读写替代。

合法主转换为：

```text
New -> Runnable -> Running
Running -> Runnable
Running -> Parking -> Waiting -> Runnable
Running -> Parking -> Running
Running -> Foreign -> Runnable
Running -> Dead
```

`Dead` 是终态。`ENQUEUED` 在 queue slot/global link 发布前与 `Runnable` 同一原子转换设置，取出后清除；任何时刻同一 coroutine 不能出现在两个 runnable 位置。

### context

x86_64 `CoroutineContext` 保存 `rsp`、resume `rip`、`rbx`、`rbp`、`r12` 和 `r13`。`r14` 固定重建为当前 `Coroutine*`，`r15` 在绑定后重建为当前 `LogicalProcessor*`。普通 suspend/call safepoint 已按[栈图](stack-maps.md)spill 用户 pointer，XMM 和 caller-saved register 不属于持久 context。

抢占 poll 若要扫描寄存器，slow path 先保存栈图编号中的全部通用寄存器；恢复前再装载。context 的发布使用 release，接手 worker acquire 后才能读取 stack bounds、resume PC 和保存寄存器。

## LogicalProcessor

每个 processor 包含：

- state：`Idle`、`Bound` 或 `Retiring`；
- 一个容量固定为 256 的本地 runnable ring；
- 一个 `run_next` 单槽；
- 当前绑定 worker/coroutine；
- TLAB cursor/limit、write-barrier buffer 和 per-processor mark work；
- 按 deadline 排序的 timer binary heap；
- scheduler tick、GC safepoint epoch 和随机窃取状态。

本地队列容量有严格 256 上界、访问模式是 owner 尾部 push/pop 与 thief 头部 steal，因此使用内联 `[AtomicPtr<Coroutine>; 256]`、`AtomicU32 head` 和 `AtomicU32 tail`，不使用通用 deque。实现必须注释该上界，并以 `debug_assert!(tail.wrapping_sub(head) <= 256)` 检查不变量。

owner 在 tail 端放入/取出，thief 只以 CAS 推进 head。slot 写入以 release 发布，读取以 acquire 取得。`u32` counter 自然回绕，距离只在不超过 256 的窗口中按 wrapping arithmetic 解释。

owner push 先 acquire 读取 head、relaxed 读取 tail；`tail.wrapping_sub(head) < 256` 时写 `slot[tail & 255]`，再 release 发布 `tail + 1`。owner pop 先把 tail 减 1 并执行 SeqCst fence，再 acquire 读取 head，令 `distance = new_tail.wrapping_sub(head)`：`distance > 255` 表示原队列为空，恢复 tail；`distance == 0` 表示最后一项，必须以 AcqRel CAS 把 head 推到 `head + 1`，CAS 失败说明 thief 已取得它，随后把 tail 规范回新 head；`1..=255` 直接取得 slot。thief/overflow 先 acquire 快照 head/tail，再以 AcqRel CAS 一次认领连续头部范围；CAS 成功后才把已认领 slot 发布到目标队列。不能对回绕 counter 作普通大小比较。

`run_next` 用于刚 ready、与当前工作具有局部性的一个 coroutine。放入新值时若已有旧值，旧值先进入普通 local queue；同一 coroutine 连续从 `run_next` 获得优先的次数最多为 1，随后必须经过普通队列以维持公平。

local queue 满时，owner 把最旧的 128 个 runnable 按原 FIFO 顺序转入 global queue，再放入新项。global queue 无固定上界，使用 coroutine 控制块内的 intrusive link，在一个 scheduler mutex 下维护 FIFO head/tail；没有每次入队分配。

per-processor timer 使用以 deadline、timer sequence 为键的连续 binary min-heap。timer 数量无固定上界且主要操作为 peek/push/pop，因而使用 `Vec<TimerEntry*>`；取消通过 wait generation 标记失效，pop 时惰性丢弃，避免从 heap 中线性删除。

## WorkerThread

worker 状态固定为 `Booting`、`Running`、`Spinning`、`Parked`、`Foreign` 和 `Stopping`。每个 worker 使用宿主创建的 non-moving OS stack 运行 rt0、scheduler、GC slow path、signal/exception handler 和 C foreign call；Gugu 用户代码运行在 coroutine stack。

worker TLS 保存 `WorkerThread*`、当前 processor、当前 coroutine、system-stack bounds、barrier buffer 和 foreign callback depth。TLS 不承载 coroutine-local 用户值。

worker park 使用递增 generation 的 semaphore/futex/WaitOnAddress token：park 前在 scheduler mutex 下把自身加入 intrusive idle-worker LIFO，释放锁后再次检查 global work、poller 和 processor demand；发现工作时以同一锁撤销 park。unpark 在锁下摘除一个 worker，再只消费匹配 generation，避免旧 wakeup 唤醒下一次 park。idle 栈和 global queue 共用 scheduler mutex，不使用存在 tag 回绕 ABA 的 lock-free 影子结构。

## runnable 选择

一个绑定 processor 的 worker 每次 schedule 按固定顺序：

1. 若 scheduler tick 是 61 的倍数，先从 global queue 取一批，防止 local work 长期饿死 global work；
2. 取 `run_next`；
3. 从 local queue tail 取一个；
4. 从 global queue 取批次；
5. 非阻塞检查 network poller 和已到期 timer；
6. 从其他 processor 窃取；
7. 无工作时进入 spinning 或释放 processor 并 park。

从 global queue 取得的批次大小为 `min(global_len / active_processors + 1, 128)`；第一项立即运行，其余按 FIFO 放入 local queue。

active processor少于 2 时不进入窃取。否则使用 worker-local xorshift64*：非零 state依次执行 `x ^= x >> 12; x ^= x << 25; x ^= x >> 27`，保存 x并输出 `x * 2685821657736338717`（`u64` 环绕）。输出生成 processor 起点，再重复生成 `1..active_processors` 的 step直到 Euclid gcd为 1，以该步长遍历；种子来自 OS entropy，失败时由 BLAKE3-256(`gugu-steal-rng-v1`、WorkerThreadId、单调启动计数)低 64 bit派生，全零改为 1。随机性只影响合法调度选择，不进入语言随机 API或可复现构建。

thief 对 victim 的 local queue 头部 CAS 窃取当前数量的一半，向上取整，最多 128；第一项运行，其余进入 thief local queue。窃取前验证每项 `Runnable|ENQUEUED`，成功取得后清除旧 queue 所有权并在新 queue 发布。

同时 spinning worker 数不超过 `ceil(active_processors / 2)`。每个 spinner 最多做 `4 * active_processors` 次 victim/poller 尝试；之后释放 processor 并 park。创建/唤醒 worker 时优先复用 parked worker，只在有 idle processor 且没有可唤醒 worker 时创建 OS thread。

## park 与 ready

所有 channel、锁、join、timer、I/O 和 `select` 等待共用两阶段 park 协议：

1. Running coroutine 填写 `WaitRecord { source, generation, payload, result_slot, notified }`；
2. 在等待源锁内登记 wait node，并把 lifecycle CAS 为 `Parking`；
3. commit 在同一锁域重新验证条件；已经满足或 notified 时撤销 wait node，CAS `Parking -> Running` 并继续；
4. 仍需等待时释放 source 锁，park stub 在 lifecycle 仍为 `Parking` 时保存完整 context并切到 worker system stack；
5. scheduler 在 context release 发布后 acquire 检查 notified：为 true 时 CAS `Parking -> Runnable|ENQUEUED` 并入队；为 false 时 CAS `Parking -> Waiting`，随后再次 acquire 检查 notified，若已为 true则立即 CAS `Waiting -> Runnable|ENQUEUED` 并入队；
6. 只有 lifecycle 已离开 `Parking` 后，原 user stack 才可由另一个 worker 恢复。

waker 在等待源的线性化点写 result slot，递增/匹配 generation，然后：

- `Waiting -> Runnable`：设置 `ENQUEUED` 并发布到当前/相邻 processor；

- 观察到 `Parking`：release 设置 wait record 的 notified bit并再次 acquire 读取 lifecycle；若已经成为 `Waiting`，继续执行 `Waiting -> Runnable`，若仍是 `Parking` 则由 scheduler 的第二次检查接手；
- 观察到已过期 generation 或 `Dead`：不做任何调度。

result 写入 release，恢复 coroutine acquire 后读取。一个 wait generation 最多成功 ready 一次；重复 wake 是空操作。`select` 的多个 wait node 共享一个原子 winner，从 `UNSET` CAS 到 case index，只有 winner 写 payload 并 ready coroutine；loser 只注销。

显式 `yield` 把 Running 转为 Runnable 并放到 local queue tail，不用 `run_next`。这保证当前已有 runnable 至少有一次被选择机会。

## `select` 提交

每个 channel/Join 控制块创建时取得单调 `WaitSourceId(u64)`；GC 地址移动不改变该 ID。一次 `select` 先按源码顺序求值完成的 HIR/GIR 契约，再按 `WaitSourceId` 排序并只锁一次每个唯一等待源；持锁区禁止 safepoint。重复引用同一 source 的 case 仍各有独立 case index。
HIR把无 case且无 default的形式标为 `SelectPlan::Never`。scheduler对此不生成 source锁或随机取模，只通过专用 never wait记录把 coroutine置 Waiting；该记录没有普通 waker，何时因 runtime终止而不再执行只消费 `TerminationPlan`。只有至少一个非 default case时才进入下述 readiness算法。

coroutine 的 `select_rng` 使用 xoshiro256++。一次 next 固定为 `result = rotl(s0 + s3, 23) + s0`，再执行 `t = s1 << 17; s2 ^= s0; s3 ^= s1; s1 ^= s2; s0 ^= s3; s2 ^= t; s3 = rotl(s3, 45)`，全部 `u64` 环绕。初始 32 字节状态为 BLAKE3-256(`gugu-select-rng-v1`、OS entropy、CoroutineId、进程启动 nonce)；全零时把 `s0` 置 1。该状态不与 `std.random` 共享。

取得任何 source 锁前，runtime 按 case_count 准备 readiness scratch：case 数不超过 64 时使用调用 frame 的一个 `u64` bitmap；更大时把 coroutine-owned `select_scratch` 增长到 `ceil(case_count / 64)` 个 `u64`。该 vector 无固定上界、按 coroutine 复用，增长可以 safepoint但必须发生在持锁前；不让罕见的大 select 永久扩大每个 frame。随后取得全部 source 锁并检查即时 readiness。若有 `n` 个 ready case，令 `threshold = 0u64.wrapping_sub(n) % n`，重复取 xoshiro 值直到 `x >= threshold`，再在源码序 ready 列表中选择 `x % n`；仍持锁原子提交该 case并释放全部锁。接受区长度能被 n 整除，每个 ready case 获得完全相同概率。

没有 ready case且存在 default 时，释放锁并选 default。没有 default时，先以相同无偏采样取得 `start in 0..case_count`，再重复采样 `step in 1..case_count` 直到 `gcd(step, case_count) == 1`，按 `(start + k * step) % case_count` 的完整排列登记共享 winner 的 wait node；`case_count == 1` 时 start 0、step 1。登记完成后通过两阶段 park原子提交等待，waker 的 winner CAS决定唯一 case。channel/Join 自身 wait queue 保持 FIFO，重复执行的持续 ready case具有非零且相等的选择概率，满足语言的随机弱公平；具体随机序列仍不是语言保证。

## 计时器与 I/O

每个 processor 的 timer heap 归该 processor owner 修改；跨 processor 新 timer 通过目标 processor 的 timer inbox 发布并唤醒 poller。全局维护所有 heap 最早 deadline 的原子近似值；过早值只导致多一次 wake，过晚值不允许。

Linux 使用一个进程级 epoll fd、eventfd wakeup 和 timerfd；Windows 使用一个进程级 IO completion port 与 waitable timer。poller 线程/无工作的 worker 把完成事件转换为对应 wait generation 的 ready。regular file、无法异步化的系统调用和所有 `extern "C"` 不占 poller，走 blocking/foreign 交接。

poller每批返回事件时保留 OS 提供的批内顺序并写 global ready queue；多事件之间不得额外构造强于[并发规范](../spec/concurrency.md)的可观察先后。timer deadline相同则按 timer ID递增入队，只用于确定实现输出。

## 可复制协程栈

### 分配与检查

新用户 coroutine 获得 8 KiB usable stack，加一页不可访问 lower guard；usable size 向宿主页大小取整。stack 向低地址增长，`stack_high` 固定，`stack_low` 和 `stack_guard` 位于低端。主协程也使用同一 heap-managed stack，不直接把初始 OS stack 当用户 stack。

每个 `frame_size != 0` 的 Gugu function prologue 都在修改 `rsp` 前检查 `rsp - required_frame >= stack_guard`，包括有 stack local 的 leaf；只有零 frame、无调用、无 safepoint的真正 leaf 可以省略。失败跳到 `morestack(required_frame)`；该 stub 先保存尚未建立 frame 的参数/return PC，切换到 worker OS stack，再执行增长。

新容量把下列表达式向上取整为宿主页整数：

```text
max(old_capacity * 2, used_bytes + required_frame + 1024, 8192)
```

请求超过 `GUGU_RUNTIME_STACK_MAX` 或容量 checked arithmetic 失败进入 `StackOverflow` fatal；请求仍在逻辑上限内但页面分配失败进入 `OutOfMemory` fatal。增长只在 safepoint 完成，复制与 `StackInterior` 修正遵循[栈图](stack-maps.md#协程栈复制)。

GC safepoint 可以收缩等待/暂停 stack：容量大于 32 KiB 且 `used_bytes * 4 < capacity` 时，新容量取能容纳 `max(used_bytes * 2 + 1024, 8192)` 的页整数。Running、Foreign、持有 stack scan lock 或本周期已经增长的 stack 不收缩。

### system stack 交接

`system_stack(call)` stub 保存 coroutine context、把 `r14`/TLS current coroutine 保持为 root、切换到 worker OS stack并调用 runtime 函数。runtime 不能保存指向用户 stack 的裸地址；需要回写的位置以 `(Coroutine*, stack slot offset)` 或 GC handle 表示。返回前确认 stack 未被另一个 worker 接管，再恢复最新 context。

## 抢占

processor scheduler tick 记录连续运行起点。system monitor 每 1 ms 检查；同一 coroutine 连续运行达到 10 ms 后设置 `PREEMPT_REQUESTED` 并令其 poll page/flag 进入 slow 状态。函数 prologue、循环 backedge、分配、调用、channel/select 和显式 safepoint 检查该状态。

poll slow path：

1. 保存栈图要求的寄存器；
2. 若 GC stop epoch 未完成，确认 stop 并等待；
3. 若只需调度抢占，把 Running 转 Runnable、清除 preempt flag并放入 local tail；
4. 调度另一 coroutine；
5. 恢复时重新检查 GC epoch。

Linux signal 或 Windows APC 只用于中断长时间阻塞的可中断系统调用、唤醒 worker 或促使代码尽快到达 poll；不在任意机器指令处复制 stack、运行用户 defer 或扫描未知寄存器。

## foreign call 与回调

每个 `extern "C"` 调用在 call bridge 前：

1. spill 所有 managed/stack pointer并建立 `ForeignBridge` map；
2. bridge 保存 user context并切到当前 worker OS stack，此时 lifecycle 暂仍为 Running且 processor 尚未释放；
3. 在 system stack flush TLAB/barrier buffer并处理已经发布的 GC stop epoch；
4. release CAS `Running -> Foreign`，随后释放 logical processor并按需唤醒替代 worker；
5. worker 进入 `Foreign` 状态并按目标 C ABI 调用。

返回后先保存 `errno`/last-error，再 release CAS `Foreign -> Runnable|ENQUEUED`、放入 global runnable queue并唤醒 scheduler；该 worker可以竞争取得 processor，但必须走普通 dequeue 的 `Runnable -> Running` 后才切回 coroutine stack和转换返回值。不能从 Foreign 直接执行用户代码。

外部代码回调 Gugu 时，bridge 查找/建立当前 OS thread 的 worker 登记，取得 processor，创建一个 callback coroutine frame并进入 Gugu。嵌套 callback 以 worker-local depth 区分；返回 C 前必须完成该 callback 的 panic 边界和临时 root 清理。外部线程退出或最外层回调返回后，临时 worker 可以注销。

## GC 协作

major/minor stop 请求递增全局 `gc_stop_epoch`，把所有 active processor 标为需要确认并唤醒 worker/poller：

- Running coroutine 在下一个 safepoint保存 context并确认；
- Runnable/Waiting coroutine 已有稳定 context，取得 `STACK_SCAN_LOCKED` 后可直接扫描；
- Parking coroutine 的 context 可能尚未发布，所属 worker必须先完成 park 双检成为 Waiting/Runnable，或撤销 park恢复 Running并在 safepoint停下；processor 在此之前不能确认 stop；
- Foreign coroutine 的用户 stack 已稳定，外部边界 root 位于 bridge handle；
- processor 在 write-barrier buffer flush、TLAB 边界发布后确认 stop。

所有 active processor 确认后进入 stop 阶段。扫描/复制某 coroutine 时持有 stack scan lock；ready 可以设置 Runnable 意图，但在 lock 释放前不能让 worker执行它。GC 完成 relocation 和 metadata 发布后以 release 增加 resume epoch，worker acquire 后恢复。

空闲 processor/worker优先协助 concurrent mark；scheduler 保留至少一个 worker 处理 poller、timer 和 runnable，不能因 GC work 无限延迟用户调度。

## 动态并行度

公开 facade按[运行时](../spec/runtime.md#gc栈与运行时控制-api)验证并线性化请求后，向 scheduler发布 `ApplyParallelism { old, new, epoch }`；scheduler不再次决定零值错误、setter返回值或公开状态。增加时按 runnable demand创建/复用 processor并唤醒 worker，不按 new一次性预建线程；必需分配失败上报 runtime fatal入口。

降低时把 ID最大的多余 processor标为 `Retiring`。它们不接受新的远程 runnable，完成当前 coroutine后按 local queue、timer inbox、barrier buffer、mark work的固定顺序转移状态，再进入 Idle。processor ID不复用，控制块从 pool重新激活时取得新 ID。

## 终止

runtime状态机先根据[进程寿命](../spec/runtime.md#进程寿命)生成 `TerminationPlan { mode, admit_user_coroutines, wait_foreign, report_epoch }`；scheduler只执行该 plan，不决定 `process.exit`、fatal、defer或报告语义。停止接纳后唤醒 parked worker、关闭新 poller注册并等 runtime critical section到达安全边界；是否等待 foreign由 plan给出。

worker无 runtime/foreign责任后转 Stopping。主线程按 poller、processor、GC、stack pool顺序关闭内部设施，再把 plan结果交给宿主退出。Dead coroutine 的 stack/控制块只有在 Join/handle与 runtime root都释放后回收。

## 不变量与验证

调度器调试构建持续检查：

- `ENQUEUED` 与实际 runnable 位置一一对应；
- Running/Foreign coroutine 具有唯一 worker，Running 具有唯一 processor；
- local queue 距离不超过 256，global intrusive link 不成环；
- wait generation 单调且 winner/ready 最多一次；
- stack bounds、guard、context PC 和 stack map 匹配；
- processor retire 不丢 runnable、timer、barrier 或 mark work；
- GC scan lock 下不能执行/复制同一 coroutine。

确定性 runtime 测试必须使用可控 poller/clock 和调度 gate，覆盖 local overflow、半队列窃取、park/wake 竞争、select loser、timer cancel、stack 增长/收缩、foreign 释放 processor、动态 parallelism、GC stop 与 ready 竞争。默认测试不能依赖真实 10 ms 时间片或随机 victim 恰好出现。

## 参考实现资料

- [Go runtime scheduler](https://go.dev/src/runtime/proc.go)
- [Go runtime stack](https://go.dev/src/runtime/stack.go)
- [Go runtime network poller](https://go.dev/src/runtime/netpoll.go)
- [Rust 标准库线程 park/unpark](https://doc.rust-lang.org/std/thread/fn.park.html)
