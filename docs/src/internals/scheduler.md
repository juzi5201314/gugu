# 调度器

本章规定 Gugu runtime 当前的 M:N 调度器、可复制协程栈、工作窃取、park/ready、抢占、I/O/计时器和 GC/foreign-call 交接。语言可观察的公平、等待、退出和动态并行度规则见[并发与调度](../spec/concurrency.md)与[运行时](../spec/runtime.md)；队列结构和时间片是内部规范，不能被用户程序观察或依赖。

## 权威边界

[表达式](../spec/expressions.md)、[并发与调度](../spec/concurrency.md)、[运行时](../spec/runtime.md)和[平台 ABI](../spec/platform-abi.md)唯一规定 async/select求值、同步、公平、动态控制、进程寿命及 FFI结果。本章只固定官方 runtime如何实现已经给定的调度事件和状态转换；队列位置、worker身份、随机序列、时间片和park协议不能成为程序语义。

除平台 rt0、context switch、signal/exception stub和必须的 machine intrinsic外，官方 scheduler/runtime主体使用 Gugu实现；不得维护一份 Rust语义等价runtime作为正常执行路径。

runtime源码需要在锁所有权或 root publication的常数临界区暂缓 safepoint时，只能使用 compiler内部的 [`NoSafepointRegion`](gir-lir.md#nosafepointregion)；它不是用户 attribute，也不允许建立另一套不受 poll预算约束的 runtime路径。

## 三层实体

调度器采用 Go 风格的三层模型，但使用完整名称而不是单字母缩写：

- `Coroutine`：用户协程，拥有可复制 stack、寄存器 context、coroutine-local 和等待状态；
- `LogicalProcessor`：执行 Gugu 用户代码所需的 runtime capability，数量等于当前 `parallelism` 目标；
- `WorkerThread`：操作系统线程，绑定一个 processor 时执行用户代码，也能在无 processor 时执行普通 `ForeignBridge`、`ForeignBridge[DirtyCpu]`、poller 和 runtime 系统工作。

一个 `Coroutine` 同时最多由一个 worker 执行；一个 `LogicalProcessor` 同时最多绑定一个 worker；一个 worker 同时最多绑定一个 processor。普通 `ForeignBridge` 进入 native 时可以让原 worker短暂保留 processor lease，runtime retake后才释放；`ForeignBridge[DirtyCpu]` 立即释放 processor，dirty work不取得 processor。

ID 表示固定为 `CoroutineId(u64)`、`LogicalProcessorId(u64)` 和 `WorkerThreadId(u64)`，进程内单调分配且不复用。计数溢出说明 runtime 内部不变量已破坏，进入 `RuntimeInvariant` fatal，不能回绕。

scheduler 维护进程级 `dirty_cpu_target`、`dirty_cpu_limit`、`dirty_cpu_active` 和 intrusive `dirty_wait_queue`。当 `parallelism = 1` 时 target 为 1；否则 target 为 `parallelism - 1`，给 managed scheduler 保留至少一个 CPU 执行槽。`dirty_cpu_limit` 在降低并行度时不能小于已经 active 的数量；超出新 target 的 work 只允许自然排空，期间不接纳新 dirty call。绑定 processor 的 managed worker 上限为 `max(1, parallelism - min(dirty_cpu_active, parallelism - 1))`，因此 `parallelism > 1` 时 dirty worker与 managed worker共享 CPU预算；`parallelism = 1` 时允许一个 managed worker和一个 dirty worker由 OS 时间片复用。没有可用额度时，调用方在已发布 bridge roots且不持有 processor 的 `DirtyWaiting` 状态排队，不创建无界 OS thread，也不在等待额度时执行 native code。

## Coroutine

### 控制块

`Coroutine` 控制块的字段与所有权按下列逻辑顺序固定；它不直接序列化进镜像，但 runtime、GC typed visitor 和 debugger 必须使用同一声明，禁止拆出平行的影子控制块：

```text
Coroutine {
    id,
    state: AtomicU64,
    stack: StackDescriptor,
    context: CoroutineContext,
    morestack_scratch: MorestackScratch,
    current_processor: AtomicPtr<LogicalProcessor>,
    wait_record: WaitRecord,
    foreign_bridge: ForeignBridgeState,
    run_link: intrusive queue link,
    join_state,
    coroutine_locals,
    panic_state,
    select_rng: [u64; 4],
    select_scratch: Vec<u64>,
    gc_scan_epoch: AtomicU64,
}
```

`select_scratch` 只保存 readiness bitmap，不保存 managed pointer；source、send payload 和 result slot仍位于用户 frame并由 stack map追踪。需要登记多个 case时，`wait_record` 的 tagged `Select` variant持有 `SelectTxn` 与一个 non-moving `SelectWaitBlock` handle；block按 case数量在取得任何 source锁前从 runtime wait-node size class取得，node只保存 Coroutine handle、generation、case index和 stack-high-relative payload/result offset，由 typed visitor按 descriptor扫描，禁止保存会被 stack copy悬空的裸 pointer。cleanup完成后 block立即归还，scratch bytes仍由 typed visitor跳过。

`ForeignBridgeState` 只在 lifecycle 为 `Foreign` 或 `DirtyWaiting` 时有效，固定保存 `{ mode, call_stub, frame_offset, frame_size, lease_word, dirty_link, error_state }`。compiler 在 coroutine stack 上物化 ABI bridge frame；`frame_offset` 是从逻辑 `stack_high` 到 frame起点的 checked深度，record不保存会因 stack copy失效的裸 stack pointer。`call_stub` 是 non-moving code pointer；`lease_word` 保存本次 bridge generation和进入 native后期望的完整 lifecycle word，不是 pointer；`dirty_link` 只供 `dirty_wait_queue` 使用，不能复用 runnable `run_link`。ABI frame里的 managed/raw pointer按调用点 stack map追踪，交给 native 的 managed地址还必须在进入 bridge前 pin或复制。

`morestack_scratch` 保存 return PC、九个 GPR参数和八个 XMM参数；只有 lifecycle Running且当前 PC为 `MorestackEntry` 时有效，并由该 entry map精确扫描，不属于常驻 runtime root。

`state` 低 4 bit 是 lifecycle：

| 值 | 状态 | 含义 |
|----|------|------|
| 0 | `New` | context 已构造，尚未入队 |
| 1 | `Runnable` | 可执行，位于 run queue 或即将发布 |
| 2 | `Running` | 正由一个绑定 processor 的 worker 执行 |
| 3 | `Parking` | 正在提交等待条件，尚未确定是否睡眠 |
| 4 | `Waiting` | 已挂在 channel/lock/timer/I/O/join 等等待源 |
| 5 | `Foreign` | 已取得 worker，在 OS stack 上执行普通或 dirty 外部调用 |
| 6 | `DirtyWaiting` | bridge roots 已发布，等待 dirty CPU额度且不绑定 worker/processor |
| 7 | `Dead` | 用户 body、panic 清理和 Join 发布已经结束 |

bit 4固定为 `ENQUEUED`，bit 5固定为 `STACK_SCAN_LOCKED`，bit 6固定为 `FOREIGN_DETACHED`，bit 7保留且必须为0，bits 8..63是56-bit `foreign_generation`。`FOREIGN_DETACHED` 只能与 `Foreign` lifecycle同时出现：为0表示普通 bridge仍保留原 processor lease，为1表示 processor已经被 retake或该调用从一开始就是 dirty bridge。每次从 `Running` 进入普通/active dirty `Foreign` 或 `DirtyWaiting` 时 generation加1；`DirtyWaiting -> Foreign` 保持同一 generation。generation溢出进入 `RuntimeInvariant` fatal，不能回绕。抢占与 GC stop通知刻意不占 lifecycle bit：它们只写 active `LogicalProcessor.poll_flags`，并投毒该 processor当时 `Running` coroutine的 `stack_check`；Runnable/Waiting context已经稳定，不需要逐 coroutine通知。fast path禁止读取 lifecycle或全局 GC epoch。所有 lifecycle转换比较完整 `u64`并携带必要的 acquire/release ordering，不能只比较低位或以互斥锁外普通读写替代；generation使延迟 retaker不能把旧调用误认成新的 `Foreign`，消除 ABA。

合法主转换为：

```text
New -> Runnable -> Running
Running -> Runnable
Running -> Parking -> Waiting -> Runnable
Running -> Parking -> Running
Running -> Foreign(attached) -> Running
Running -> Foreign(attached) -> Foreign(detached) -> Running|Runnable
Running -> DirtyWaiting -> Foreign(detached) -> Running|Runnable
Running -> Dead
```

`Dead` 是终态。`ENQUEUED` 在 queue slot/global link发布前与 `Runnable` 同一原子转换设置，取出后清除；任何时刻同一 coroutine不能出现在两个 runnable位置。`STACK_SCAN_LOCKED` 是 scanner/foreign retaker对完整 state word的临时所有权，释放时必须保留 generation；离开 `Foreign` 时清除 `FOREIGN_DETACHED`，其它 lifecycle携带该 bit都是 `RuntimeInvariant`。

### context

x86_64 `CoroutineContext` 保存 `rsp`、resume `rip`、`rbx`、`rbp`、`r12` 和 `r13`。`r14` 固定重建为当前 `Coroutine*`，`r15` 在绑定后重建为当前 `LogicalProcessor*`。普通 suspend/call safepoint 已按[栈图](stack-maps.md)spill 用户 pointer，XMM 和 caller-saved register 不属于持久 context。

抢占 poll 若要扫描寄存器，slow path 先保存栈图编号中的全部通用寄存器；恢复前再装载。context 的发布使用 release，接手 worker acquire 后才能读取 stack bounds、resume PC 和保存寄存器。

## LogicalProcessor

每个 processor 包含：

- 独占一条 cache line的 `PollControl { poll_flags, requested_gc_epoch, ack_gc_epoch }`；只有 processor owner写 ack，collector以 acquire读取；
- 独占一条 cache line的 `ProcessorOwnership { state, owner, current_coroutine, run_started_ns }`，其中 state只取 `Idle`、`Bound` 或 `Retiring`；`Bound` 同时覆盖 managed execution和 attached普通 bridge，`run_started_ns` 只在 current lifecycle为 `Running` 时解释；
- 一个容量固定为 256 的本地 runnable ring；
- 一个 `run_next` 单槽；
- TLAB cursor/limit、write-barrier buffer 和 per-processor mark work；
- 七个固定小栈 class 的本地 cache head与总字节计数；
- 按 deadline 排序的 timer binary heap；
- 调度随机状态。

本地队列容量有严格 256 上界、访问模式是 owner 尾部 push/pop 与 thief 头部 steal，因此使用内联 `[AtomicPtr<Coroutine>; 256]`、`AtomicU32 head` 和 `AtomicU32 tail`，不使用通用 deque。实现必须注释该上界，并以 `debug_assert!(tail.wrapping_sub(head) <= 256)` 检查不变量。

`poll_flags` 是普通内存中的固定小型 atomic word，不存在 protected “poll page”或 fault-based trap：bit 0为 `PREEMPT`，bit 1为 `GC_STOP`，bits 2..31必须为0；debug构建以 `debug_assert!(flags & !0b11 == 0)` 检查。请求方先 release发布关联的 processor-local epoch/state，再 `fetch_or(Release)`设置 bit；budgeted/显式 poll只以一次 acquire load读取该 word，值为0时继续。函数 prologue不再读它，而以一次 acquire读取 current coroutine的 `stack_check`；两条 fast path都不读全局 `gc_stop_epoch`或 lifecycle state。slow path重新 acquire读取 flags与所需 epoch，只有完成对应动作的一方才能清 bit。processor没有 Running coroutine时，scheduler在绑定新 coroutine前处理 flag或直接确认 epoch。

x86_64两项目标的 cache line常量固定为 `CACHE_LINE_BYTES = 64`。processor数量由 `parallelism` 严格有界，poll fast path是高频只读、ownership是跨线程调度读写、queue/TLAB是 owner局部写，因此表示固定为两个相邻但不共享的 cache line，而不是通用可变容器：

```text
#[repr(C, align(64))]
PollControl {
    poll_flags: AtomicU32,
    reserved: u32,
    requested_gc_epoch: AtomicU64,
    ack_gc_epoch: AtomicU64,
    padding: [u8; 40],
}

#[repr(C, align(64))]
ProcessorOwnership {
    state: AtomicU32,
    reserved: u32,
    owner: AtomicPtr<WorkerThread>,
    current_coroutine: AtomicPtr<Coroutine>,
    run_started_ns: AtomicU64,
    padding: [u8; 32],
}
```

`LogicalProcessor` 按 `poll`、`ownership`、local queue、stack cache、TLAB/barrier和其它冷字段的顺序布局。构建时必须断言两个控制块的 `size_of == align_of == 64`，且 `offset_of!(LogicalProcessor, poll)` 与 `offset_of!(LogicalProcessor, ownership)` 都是64的倍数并相差至少64；backend使用同一 runtime layout query取得 `[r15 + poll_flags_offset]`，禁止手写重复 offset。`PollControl` 只与同一次 preempt/GC handshake相关的低频写共享 line；scheduler state、owner、current coroutine、run timestamp、queue、stack cache、TLAB和 monitor字段不能落入该 line。

owner 在 tail 端放入/取出，thief 只以 CAS 推进 head。slot 写入以 release 发布，读取以 acquire 取得。`u32` counter 自然回绕，距离只在不超过 256 的窗口中按 wrapping arithmetic 解释。

owner push 先 acquire 读取 head、relaxed 读取 tail；`tail.wrapping_sub(head) < 256` 时写 `slot[tail & 255]`，再 release 发布 `tail + 1`。owner pop 先把 tail 减 1 并执行 SeqCst fence，再 acquire 读取 head，令 `distance = new_tail.wrapping_sub(head)`：`distance > 255` 表示原队列为空，恢复 tail；`distance == 0` 表示最后一项，必须以 AcqRel CAS 把 head 推到 `head + 1`，CAS 失败说明 thief 已取得它，随后把 tail 规范回新 head；`1..=255` 直接取得 slot。thief/overflow 先 acquire 快照 head/tail，再以 AcqRel CAS 一次认领连续头部范围；CAS 成功后才把已认领 slot 发布到目标队列。不能对回绕 counter 作普通大小比较。

`run_next` 用于刚 ready、与当前工作具有局部性的一个 coroutine。放入新值时若已有旧值，旧值先进入普通 local queue；同一 coroutine 连续从 `run_next` 获得优先的次数最多为 1，随后必须经过普通队列以维持公平。

local queue 满时，owner 把最旧的 128 个 runnable 按原 FIFO 顺序转入 global queue，再放入新项。global queue 无固定上界，使用 coroutine 控制块内的 intrusive link，在一个 scheduler mutex 下维护 FIFO head/tail；没有每次入队分配。

per-processor timer 使用以 deadline、timer sequence 为键的连续 binary min-heap。timer 数量无固定上界且主要操作为 peek/push/pop，因而使用 `Vec<TimerEntry*>`；取消通过 wait generation 标记失效，pop 时惰性丢弃，避免从 heap 中线性删除。

## WorkerThread

worker 状态固定为 `Booting`、`Running`、`Spinning`、`Parked`、`Foreign` 和 `Stopping`。每个 worker 使用宿主创建的 non-moving OS stack 运行 rt0、scheduler、GC slow path、signal/exception handler、普通 `ForeignBridge` 和 `ForeignBridge[DirtyCpu]` C call；`ForeignLeaf` C call 与 Gugu 用户代码一样运行在 coroutine stack。

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

每个 channel/Join 控制块创建时取得单调 `WaitSourceId(u64)`；GC 地址移动不改变该 ID。一次 `select` 先按源码顺序完成 HIR/GIR规定的一次求值，把 source、send payload、result slot和 barrier需求物化为稳定 case record。任何 scratch增长、随机采样、source排序和可能触发 barrier refill的 reservation都发生在取得 source锁前；持锁机器区间必须由 `NoSafepointRegion`标记并接受同一 `POLL_BUDGET`验证。

HIR把无 case且无 default的形式标为 `SelectPlan::Never`。scheduler对此不生成 source锁或随机取模，只通过专用 never wait记录把 coroutine置 Waiting；该记录没有普通 waker，何时因 runtime终止而不再执行只消费 `TerminationPlan`。只有至少一个非 default case时才进入下述算法。

coroutine 的 `select_rng` 使用 xoshiro256++。一次 next固定为 `result = rotl(s0 + s3, 23) + s0`，再执行 `t = s1 << 17; s2 ^= s0; s3 ^= s1; s1 ^= s2; s0 ^= s3; s2 ^= t; s3 = rotl(s3, 45)`，全部 `u64` 环绕。初始32字节状态为 BLAKE3-256(`gugu-select-rng-v1`、OS entropy、CoroutineId、进程启动 nonce)；全零时把 `s0`置1。该状态不与 `std.random`共享。`uniform(n)` 固定使用 threshold rejection：`threshold = 0u64.wrapping_sub(n) % n`，重复取值直到 `x >= threshold`，返回 `x % n`；所有 rejection与 permutation生成都在锁外执行。

取得任何 source锁前，runtime按 `case_count`准备 readiness scratch：case数不超过64时使用调用 frame的一个 `u64` bitmap；更大时把 coroutine-owned `select_scratch`增长到 `ceil(case_count / 64)` 个 `u64`。该 vector无固定上界、按 coroutine复用，且不保存 managed pointer。case record还必须预留其临界写可能产生的 barrier entry；若当前 buffer不足，refill在锁外完成。

### 1–8 case 的展开路径

`case_count <= INLINE_SELECT_CASES = 8` 且 legalized临界成本不超过 `POLL_BUDGET` 时，`LowerConcurrency` 在锁外生成无偏 Fisher–Yates case permutation，并把按 `WaitSourceId`排序、去重后的至多8次 `try_lock`、readiness检查、提交/登记和 unlock完全展开。成功取得第一个 source锁后进入单一 `NoSafepointRegion`；任一 `try_lock`失败就按逆序释放已经取得的锁，退出 region并进入一般路径，禁止持有前一个锁等待后一个锁。最终 LIR没有 region内 backedge、RNG循环或可能阻塞的 lock slow edge。

全部锁取得后，按预生成 permutation选择第一个 ready case，因此稳定 ready集合中的每个 case概率相同；在同一 region内重新验证并提交，再释放全部锁。没有 ready case且存在 default时，在持锁快照下选择 default。没有 default时，在同一快照下登记共享 `SelectTxn` 的 wait node，释放全部锁后执行下面的常数 arm协议。类型布局、屏障或 payload操作使 region成本超限时，即使 case较少也必须使用一般路径，不能扩大 budget或省略 poll。

### 任意 case 数的一般路径

一般路径一次只持有一个 source锁。外层 readiness扫描在每次迭代之间保留正常 budget poll；每个迭代的 lock成功边进入一个无 backedge的 `NoSafepointRegion`，记录一个 case的 readiness后立即 unlock。若 bitmap非空，在锁外用 `uniform(ready_count)`选择 case，只取得该 source锁并重新验证；成功时以该 source操作为线性化点，失败则重新扫描，不能凭过期 bitmap提交。

没有 ready case时，建立 `SelectTxn { generation, phase: Building, winner: UNSET }`，以锁外生成的随机完整排列逐 case登记 wait node；登记循环可以 safepoint，每次仍只取得一个 source锁。Building期间 waker可以 CAS winner并写 result/notified，但不能 ready仍在执行登记的 coroutine。全部 node登记后：存在 default时由 `winner: UNSET -> DEFAULT` 的 CAS与所有 source waker竞争，该 CAS成功就是 default线性化点；没有 default时，以一个无 backedge region把 lifecycle `Running -> Parking`、txn phase `Building -> Armed`并再次检查 winner，随后复用两阶段 park。winner已经产生时撤销 park或直接继续。winner确定后，loser node也按一次一个 source锁注销，清理循环继续接受 budget poll。

source内部锁的 acquire slow edge在尚未持有任何其它 source锁时才可 park；成功 edge的第一项必须是 `NoSafepointBegin`。常见固定尺寸 payload在预留 barrier permit后于同一 region提交。若 payload copy或 barrier工作无法在 budget内完成，第一次 region只发布带 source/generation的 transfer reservation并锁定 buffer slot或 rendezvous waiter，随后在锁外完成可 safepoint的 copy，第二次只取得同一 source锁发布结果；reservation本身进入 typed GC root，close与其它 send/recv必须按其线性化顺序等待或越过，不能观察半初始化 payload。

该协议保持“一次 select只提交一个 case”和“ready时 default不能获选”：较少 case的无竞争机器路径没有新增 poll或 marker指令，任意规模路径则不再跨 case持锁。channel/Join wait queue仍保持 FIFO；持续 ready case在重复 select中具有非零选择概率，满足语言的随机弱公平，具体随机序列不是语言保证。

## 计时器与 I/O

每个 processor 的 timer heap 归该 processor owner 修改；跨 processor 新 timer 通过目标 processor 的 timer inbox 发布并唤醒 poller。全局维护所有 heap 最早 deadline 的原子近似值；过早值只导致多一次 wake，过晚值不允许。

Linux 使用一个进程级 epoll fd、eventfd wakeup 和 timerfd；Windows 使用一个进程级 IO completion port 与 waitable timer。poller 线程/无工作的 worker 把完成事件转换为对应 wait generation 的 ready。regular file、无法异步化的系统调用和普通 `ForeignBridge` 不占 poller，走 blocking/foreign 交接；`ForeignBridge[DirtyCpu]` 走 dirty CPU admission；`ForeignLeaf` 留在当前 worker 上直接执行。

poller每批返回事件时保留 OS 提供的批内顺序并写 global ready queue；多事件之间不得额外构造强于[并发规范](../spec/concurrency.md)的可观察先后。timer deadline相同则按 timer ID递增入队，只用于确定实现输出。

## 可复制协程栈

### arena、size class 与 cache

协程栈向低地址增长。runtime 不为每个 coroutine 建立独立 mapping，也不在 arena 内交替设置 `RW`/`PROT_NONE` 条带：Linux 的不同 protection run 仍会拆成独立 VMA，不能解决高并发下的映射数量上界。官方实现使用 `STACK_ARENA_BYTES = 256 MiB` 的地址空间 reservation，payload 划分为 2 MiB span；每个 arena 只有首尾各一页不可访问的诊断 guard，内部不设置逐栈 guard。Linux 因而每个 arena至多形成三个 protection run，slot/span 的提交与回收不得调用会拆分 protection的 `mprotect`；Windows 使用一个 reservation并按页 `MEM_COMMIT`/`MEM_DECOMMIT`，不得逐栈 `VirtualProtect`。

arena 内部固定使用 `512 B, 1, 2, 4, 8, 16, 32 KiB, ... 2 MiB` 的二次幂 size class。一个 2 MiB span只服务一个 class，slot占用由 span内联 bitmap表示；arena只有128个 span，metadata按稠密下标存放，不使用 `HashMap`。大于2 MiB的 stack从同一 arena的连续 span buddy extent分配；超过半个 arena的请求才使用独立 reservation。空 span可以改换 class；Linux 对完全空闲的整页使用不改变 VMA protection的 `madvise` 回收物理页，Windows执行 `MEM_DECOMMIT`。runtime最多保留一个完全空的 arena，额外空 arena释放地址 reservation。

每个 processor只缓存 `512 B..32 KiB` 七个固定 class，总容量不得超过64 KiB；refill一次转移不超过16 KiB，free使总量超过64 KiB时向 global span归还，直至不超过32 KiB。该有界 cache让 create/finish热路径只做 intrusive pop/push；大栈和 cache miss才取得对应 class lock。processor退役、内存压力和完整 GC后的 trim epoch会 flush本地 cache。coroutine完成全部 defer后，`finish_coroutine` trampoline在旧 stack上把 result或 panic payload用 GC barrier移入控制块，再单向切到 worker system stack而不保存可恢复PC；owner取得 `STACK_SCAN_LOCKED`，从 typed root集合摘除旧 stack、清空 descriptor、把 slot发布给 cache，最后以 release发布 `Dead`并唤醒 Join。Join/handle只延长控制块与结果寿命，不能延长 stack寿命。release构建不清零整段旧字节，debug构建可以 poison，重新提交的匿名页仍由 OS提供零页语义。

arena slot分配只消耗虚拟容量；第一次切入该 stack前才提交覆盖 slot的宿主页。多个亚页 slot共享一个宿主页，只有页内没有 live或本地 cached slot时才可 decommit。`stack_committed_bytes` 包含 cache仍占用的已提交页；reservation本身只计入 `stack_reserved_bytes`。

arena首尾 guard只诊断越过整个 arena的失控访问，不隔离相邻 coroutine，也不属于语言内存安全契约。合法 managed code依靠 compiler prologue边界检查；`ForeignLeaf` 超过声明 `stack = N` 本来就是 unsafe契约破坏。这样避免以每个 stack一个 VMA换取不能覆盖任意 raw-pointer破坏的局部诊断。

### 分配与检查

新用户 coroutine的初始容量为：

```text
class_ceil(max(2 KiB, entry_required_frame + 512))
```

`entry_required_frame` 由 compiler随 coroutine entry descriptor发布，覆盖初始 frame、ABI entry record和直接 `ForeignLeaf` reserve；任何未检查的 entry stub都必须在 worker system stack上完成。主 coroutine使用相同 arena stack。新 coroutine从2 KiB或更高 class开始，`512 B`与`1 KiB`只供稳定等待 stack冷压缩；容量不再向宿主页取整。

每个 coroutine的 stack元数据集中在一条独占 cache line：

```text
#[repr(C, align(64))]
StackDescriptor {
    stack_check: AtomicUsize,
    stack_low: usize,
    stack_high: usize,
    capacity: usize,
    recent_high_water: usize,
    last_grow_gc_epoch: u32,
    low_use_gc_cycles: u8,
    flags: u8,
    reserved: u16,
    padding: [u8; 16],
}
```

正常 `stack_check == stack_low`；抢占或 GC请求时固定为 `POLL_SENTINEL = isize::MAX as usize`。官方两目标的 coroutine stack reservation都位于低半 canonical user address，因而真实 `stack_low`、`rsp`与不下溢的 candidate都可表示为非负 `isize`，严格小于 sentinel。`flags` bit 0为 `COLD_COMPACTED`，其余位必须为0。`StackDescriptor` 的 `size_of == align_of == 64` 以及 `offset_of!(Coroutine, stack) % 64 == 0` 必须由构建时断言；前面的 lifecycle state和后面的 context/wait/queue写不能与 prologue热读的 `stack_check` 共享 line。

除 `PollFreeLeaf` 外，每个 Gugu function prologue都在修改 `rsp` 前按机器字计算 `candidate = rsp.wrapping_sub(required_frame)`，再以一次 acquire load和有符号比较检查 `candidate >= stack_check`；真实容量不足但地址算术下溢时 candidate的 `isize`解释为负值，poison时任何真实 candidate都小于 `POLL_SENTINEL`，两种情况共享唯一 taken branch。`required_frame = frame_size + max_leaf_reserve`，其中 `max_leaf_reserve` 是本函数全部 direct `ForeignLeaf` call声明 `stack = N` 的最高值，checked计算并按目标 stack alignment取整。taken edge进入统一 `morestack_or_poll(required_frame)` stub。stub先通过 `r14`把尚未建立 frame的参数/return PC保存到 coroutine固定 scratch，再切 system stack；它必须先处理 processor pending poll/GC并允许当前 coroutine被调度出去，重新取得执行权后再读取最新 `stack_low`决定是否增长。完成全部请求和必要增长后恢复 scratch并重新执行原 prologue。`PollFreeLeaf` 没有 call，reserve固定为0。

增长容量为下式对应的 size class；任何增长都至少回到2 KiB：

```text
class_ceil(max(old_capacity * 2, used_bytes + required_frame + 512, 2 KiB))
```

请求超过 `GUGU_RUNTIME_STACK_MAX` 或容量 checked arithmetic 失败进入 `StackOverflow` fatal；请求仍在逻辑上限内但 arena/页面提交失败进入 `OutOfMemory` fatal。增长只在 safepoint完成，更新 `last_grow_gc_epoch`、清零 `low_use_gc_cycles`与 `COLD_COMPACTED`，复制和 `StackInterior` 修正遵循[栈图](stack-maps.md#协程栈复制)。

stack收缩采用四个完整 GC观察窗的迟滞，不再按一次 `used * 4 < capacity`立即复制。park、preempt和 stack growth slow path以 owner写更新 `recent_high_water`；每个完整 GC在 scan lock下取 `max(recent_high_water, used_bytes)`。只有该值加512 bytes不超过当前容量四分之一、最近四个完整 GC都未发生增长且 stack连续四次满足低占用时才收缩，任一条件失败即清零计数；完成本次采样后以当前 `used_bytes`开始下一观察窗。

满足迟滞后，`Runnable` stack收缩到能容纳 `max(used_bytes + 512, 2 KiB)` 的 class；`Waiting` stack可以冷压缩到能容纳 `max(used_bytes + 256, 512 B)` 的 class并设置 `COLD_COMPACTED`。Running、Parking、Foreign、DirtyWaiting、持有 stack scan lock或本周期已经增长的 stack不收缩。冷 stack唤醒后可以直接恢复；下一次容量不足由普通 prologue一次增长到不低于2 KiB。内存软上限施压时，完整 GC可以把“连续四次”缩短为一次，但仍禁止收缩从上一个完整 GC以来发生过增长的 stack。

该方案保持唯一的 stackful coroutine表示；冷压缩复用现有 stack copy与精确重定位，不增加第二套 stackless task状态机。周期性深浅负载会因 growth epoch和四窗计数保留容量，长期等待的浅 stack才下降到亚2 KiB class。

容量预算必须按完整 runtime实例测量，不能把虚拟容量当作 RSS：100,000个2 KiB stack的 live capacity约195.3 MiB，1,000,000个约1.91 GiB；全部稳定压到512 B时分别约48.8 MiB与488.3 MiB。后者仍不包含每 coroutine控制块、Join/等待节点和实际高于 floor的 frame。按2 KiB密集装入时，一百万 stack需要8个256 MiB payload arena，Linux protection run上界约24，而不是一百万个逐栈 VMA；实际 committed/RSS由触碰页、cache和宿主页共享决定。

### system stack 交接

`system_stack(call)` stub保存 coroutine context、把 `r14`/TLS current coroutine保持为 root、切换到 worker OS stack并调用 runtime函数或 `ForeignBridge` C call。runtime不能保存指向用户 stack的裸地址；需要回写的位置以 `(Coroutine*, stack_high-relative slot offset)` 或 GC handle表示。返回前确认 stack未被另一个 worker接管，再恢复最新 context。`ForeignLeaf` 不经过该 stub；其 stack budget已由 caller prologue的 `max_leaf_reserve`保证。`ForeignBridge[DirtyCpu]`使用同一 stub建立 bridge state，但 native body只能在取得 dirty CPU额度后运行。

## 抢占

### 事件与 deadline 驱动的 monitor

processor owner每次绑定 `Running` coroutine时把单调 `run_started_ns`写入 ownership line，再 release发布 `current_coroutine`。system monitor不执行固定1 ms轮询；它由递增 `monitor_sequence`、合并 flags、已发布的 `armed_deadline_ns`和按 processor稠密存放的 foreign-retake观察记录组成。Linux以 sequence futex word等待；Windows以 sequence防丢唤醒，并用一个 runtime monitor event参与等待。下列事件才递增 sequence并唤醒 monitor：

- runnable work出现且没有 idle processor；
- active processor从0变为非0；
- `retake_requested` 从无变有；
- 更早的 runtime maintenance deadline发布或 runtime终止。

monitor每次醒来 acquire快照 sequence与相关状态，处理已经到期的动作，再计算所有尚需观察动作中最早的绝对单调 deadline。它先发布 armed deadline，然后重新检查 sequence、flags与 active processor计数；任一项改变就重算，否则等待到该 deadline。过早的旧 deadline只造成一次额外 wake，过晚 deadline禁止发布。没有 runnable压力、foreign-retake压力或 maintenance deadline时无限期 park，不保留10 ms、1 ms或20 µs周期；这里以事件深睡取代周期轮询的指数退避。用户 timer仍由独立 poller的 timerfd/waitable timer负责，monitor不复制 timer heap。

调度时间片只在存在竞争时需要：有 runnable work且没有 idle processor时，monitor读取各 `Running` processor的 `run_started_ns`，把最早资格 deadline设为 `run_started_ns + 10 ms`；deadline已经过去就立即请求抢占。没有其它 runnable work时，即使一个 coroutine持续计算也不为它周期唤醒 monitor；新 work发布时会立即发现它已超过10 ms并发出请求。请求方对目标 processor的 `poll_flags` 设置 `PREEMPT`，并把 current coroutine的 `stack_check` release写为 `POLL_SENTINEL`。

Linux monitor使用 `FUTEX_WAIT_BITSET` 的绝对单调 deadline，不改变系统 tick频率。Windows无deadline时等待 monitor event；有deadline时用 `WaitForMultipleObjects` 同时等待该 event与 high-resolution waitable timer，sequence复查封闭“发布状态—arm timer—进入等待”之间的丢唤醒窗口。只有目标不支持 high-resolution timer、确有小于60 ms的已发布 deadline且动作会推进 runnable/GC时，才取得进程内引用计数的 `timeBeginPeriod(1)` lease，并在 deadline取消或进入深睡前匹配 `timeEndPeriod(1)`。poller与 monitor共享该 lease，禁止常驻提升 timer resolution。Windows 11对不可见进程不保证提升后的精度，因此20 µs/10 ms都是内部资格阈值而不是 wall-clock完成保证；正确性依赖 generation wake与同步 poll，不依赖定时器精确命中。

poll slow path：

1. `PollResume` slow edge按 map spill跨 poll roots；`MorestackEntry` 已经在 coroutine固定 scratch保存 entry参数，两者都在接触 runtime lock前切到 worker system stack，再 acquire读取 `poll_flags`；
2. 若 `GC_STOP` 的 requested epoch未确认，flush TLAB/barrier buffer、保存 context，以 release store发布 `ack_gc_epoch = requested_gc_epoch`并等待 resume；collector只以 acquire load判定确认完成；
3. 若有 `PREEMPT` 且没有尚未结束的 `NoSafepointRegion` 或其它 runtime critical section，把 Running转 Runnable、清该 bit并放入 local tail；
4. 清除已经完成的 flags；全部清空时把 current coroutine的 `stack_check`恢复为此时最新的 `stack_low`；
5. 调度另一 coroutine，恢复或绑定任何 coroutine前重新检查 processor flag与 GC epoch。

compiler的不变量是：持有 `LogicalProcessor` 且状态为 `Running` 的 coroutine，其PC属于带完整 metadata的 managed code或有限 inline asm；每条无限 managed路径无限次经过 poisoned `StackCheck`或 budgeted poll，任意 poll-free路径cost不超过 `POLL_BUDGET`。signal/APC只设置 processor flag、投毒 stack check并唤醒 worker；它不在任意机器PC复制 stack、运行用户 defer或扫描未知寄存器。请求保持 pending直至同步点处理。

## foreign call 与回调

每个导入 C 调用或 native definition 在 lowering 时先确定普通 `ForeignBridge`、`ForeignBridge[DirtyCpu]` 或 `ForeignLeaf`。未标注导入、effect 未知的间接调用和 `#[ffi(bridge)]` 调用点选择普通 `ForeignBridge`；`#[ffi(dirty_cpu)]` 导入/native definition/调用点和 managed `#[naked]` 选择 `ForeignBridge[DirtyCpu]`；`global_asm` 符号按显式 extern 声明选择；只有满足 `#[ffi(leaf)]` 声明且 effect 仍被静态保留的直接调用才选择 `ForeignLeaf`。

### `ForeignLeaf`

`ForeignLeaf` 是当前 worker、processor 和 coroutine stack 上的普通 C ABI call；其声明的 leaf stack budget 为 `N`（省略参数时为 0）：

1. 不建立 `ForeignBridge` map，不切换到 worker system stack，不执行 `Running -> Foreign`；
2. 不释放 `LogicalProcessor`，不唤醒替代 worker，也不把当前 coroutine 放入 global runnable queue；
3. 不提供 C 回调 Gugu 的入口。普通 stack map、raw pointer、pin、内存别名、write barrier 和展开边界规则仍然有效；C 调用链不得越过该预算。

leaf 调用必须保持声明者承诺的短时、无不可界定等待和无 runtime 交互，并且 C 调用链不得超过声明的 stack budget。scheduler 不检查 C 函数体；违反承诺时至少会让当前 processor 长时间不可用，错误的 stack budget 还可能破坏 coroutine stack。

### `ForeignBridge`

普通 bridge把 processor关联视为可被 runtime打破的短期 lease；`ForeignBridge[DirtyCpu]` 从进入调用起就是 detached。两种模式共同的进入顺序为：

1. spill所有 managed/stack pointer，在 coroutine stack上物化 ABI bridge frame，写入 `ForeignBridgeState` 的 call stub、相对 offset和下一 generation，并建立 `ForeignBridge` map；
2. bridge保存 user context并切到当前 worker OS stack，此时 lifecycle仍为 `Running`；
3. 以 release发布 processor当前 TLAB top/limit和 write-barrier buffer cursor，处理已经发布的 GC stop。普通 bridge只发布 cursor，不清空 buffer、不放弃剩余 TLAB，也不强制下次分配 refill；
4. 普通 bridge令 `g = old_generation + 1`，把完整期望字 `Foreign(g)` 写入 `foreign_bridge.lease_word`，再以 release CAS从 `Running(old_generation)` 转入不带 `FOREIGN_DETACHED` 的 `Foreign(g)`。processor仍为 `Bound`，`ownership.owner/current_coroutine`仍指向原 worker/coroutine；worker TLS中的 processor pointer在 native期间只是一项待验证 lease，未赢得返回 CAS前不得据此访问 processor。进入动作不唤醒替代 worker，也不访问 global runnable queue；
5. DirtyCpu bridge在 scheduler lock下尝试取得额度：成功时以新 generation转入 `Foreign|FOREIGN_DETACHED`并递增 active；失败时转入 `DirtyWaiting`、用 `foreign_bridge.dirty_link` 发布到 `dirty_wait_queue`。两条 dirty路径都立即释放 processor并按需唤醒 managed worker；active dirty work由 dirty worker从 `(Coroutine*, stack_high-relative frame_offset)` 重建 ABI frame并在 system stack执行，`DirtyWaiting` 的原 worker立即返回 scheduler。

普通 bridge由原 worker执行目标 C ABI call。native代码执行期间不能读取或修改保留 processor的 queue/TLAB/barrier/poll状态；所有 managed roots只来自已发布 bridge frame和显式 pin。

#### lease retake

retaker先 acquire读取 `ProcessorOwnership.current_coroutine` 及其完整 state word，只能对仍等于 `foreign_bridge.lease_word == Foreign(g)` 的 attached调用竞争：

1. CAS `Foreign(g) -> Foreign(g)|STACK_SCAN_LOCKED`；失败表示 native已经返回、另一 retaker已取得所有权或 generation已改变，当前尝试立即结束；
2. 赢得者再次验证 processor owner/current仍匹配，发布或转移 processor-local TLAB/barrier状态，从原 worker解除 processor关联；普通调度 handoff保留该 processor剩余 TLAB和完整 barrier buffer，GC retake则先按 stop协议 drain/publish；
3. release写入 `Foreign(g)|FOREIGN_DETACHED`并清除 scan lock，然后把 processor交给 GC、callback、Retiring流程或 managed scheduler。原 native worker仍继续执行 C，但不再拥有 processor。

GC stop、processor retirement和从该 native调用发生的 Gugu callback立即尝试 retake，不等待 grace。普通 runnable压力只有在“没有 idle processor且存在 runnable work”时设置合并的 `retake_requested`并唤醒 system monitor；monitor第一次观察某个 attached generation时在稠密 watch记录中保存 `generation` 与 `eligible_at = now + 20 µs`。压力持续时把全部 watch中最早 `eligible_at`并入 monitor绝对 deadline，到期后第二次仍观察到同一 generation才可 retake；scheduler后续压力路径也可以在 deadline到期后执行同一竞争，不为等待阈值 busy-spin。压力消失时取消 armed deadline但可以保留同 generation观察，native返回或 generation改变才清除记录。该 grace是 runtime内部策略与 benchmark参数，不是语言 wall-clock保证，也不能通过每次 bridge读取单调时钟实现；系统不存在固定20 µs或1 ms扫描周期。

generation与完整 state CAS线性化 native return/retake：两者只能有一个观察到 attached lease并成功，旧 monitor观察不能命中新 bridge invocation。

#### 返回路径

native返回后，执行 worker先把结果写回 bridge frame并捕获 `errno`/last-error。普通 bridge按以下顺序恢复：

1. **原 processor快路径**：以 acquire-release CAS把精确 `Foreign(g)` 转为 `Running(g)`。成功说明 lease从未被打破；worker重新取得 processor访问权，先处理 pending poll/GC epoch，再直接切回 coroutine stack并转换返回值。该路径不 flush/abandon TLAB、不唤醒线程、不进入 runnable queue，也不执行普通 dequeue；
2. **detached processor路径**：若 state含 `STACK_SCAN_LOCKED`，等待 retaker发布最终状态；随后尝试取得 idle processor。绑定成功后把精确 `Foreign(g)|FOREIGN_DETACHED` 转为 `Running(g)`并直接恢复；
3. **enqueue慢路径**：没有 idle processor时，release CAS为 `Runnable(g)|ENQUEUED`，放入 global runnable queue并唤醒 scheduler；只有该路径必须经普通 dequeue的 `Runnable -> Running`。

dirty worker完成调用后在 scheduler lock下递减 active；若 target允许则从 queue取一个 record、把同一 generation的 `DirtyWaiting` 转为 `Foreign|FOREIGN_DETACHED`并直接转交额度，否则唤醒 managed scheduler。完成的 dirty调用不使用原 processor lease，按 detached processor/enqueue路径恢复。native work不提供 runtime强制取消。

DirtyCpu admission不可在持有 `LogicalProcessor` 时等待。并行度降低只更新 target；`dirty_cpu_limit` 保持 `max(target, active)` 直至多余 work排空，增加目标则按 FIFO唤醒排队调用。

### 外部代码回调

只有普通 `ForeignBridge` 允许外部代码回调 Gugu。回调首先按 lease-retake协议打破 outer coroutine的 attached lease；若赢得原 processor即可用它建立 callback coroutine frame，否则按普通规则取得其它 processor。callback结束后 processor交还 scheduler，不把 lease重新附着到仍在 C中的 outer coroutine；outer native返回时走 detached路径。bridge查找/建立当前 OS thread的 worker登记，嵌套 callback以 worker-local depth区分；返回 C前必须完成 callback panic边界和临时 root清理。从 `ForeignLeaf`、`ForeignBridge[DirtyCpu]` 或 `DirtyWaiting` 回调 Gugu是违反对应 unsafe契约，不进入隐式桥接路径。

## GC 协作

major/minor stop的 coordinator只对全局 `gc_stop_epoch`执行一次递增；该值只分配 generation，不被 managed fast path读取。coordinator随后按 processor ID遍历 active processors，对每个 processor执行固定发布协议：

1. release写入 `requested_gc_epoch = gc_stop_epoch`；
2. acquire读取 `current_coroutine`，若完整 lifecycle为 `Running`，release把其 `stack_check`写为 `POLL_SENTINEL`；
3. 以 `poll_flags.fetch_or(GC_STOP, Release)` 发布请求并唤醒 worker/poller；
4. 再次 acquire验证 ownership/current；发生切换时，新 owner在 `Runnable -> Running` 前必须先观察 `GC_STOP`，coordinator只需投毒仍绕过绑定边执行的 current coroutine。

因此一次 stop请求写入 `O(active_processors)` 个彼此独占的 `PollControl` line，而不是触碰 `O(live_coroutines)` 个 lifecycle word；global epoch每个 cycle只有一次写入，也不构成 worker共享读取热点。loop/显式 poll通过一次 processor-local load观察 bit，函数 entry通过一次 coroutine-local load观察 sentinel；两条路径都在 slow edge才读取 requested epoch。禁止给 lifecycle增加 `GC_STOP`/`PREEMPT` bit或在 fast path比较 global epoch。

- Running coroutine在下一个同步 safepoint保存 context并确认；compiler保证 Running不包含不可分析的 opaque native frame；
- Runnable/Waiting coroutine已有稳定 context，取得 `STACK_SCAN_LOCKED` 后可直接扫描；
- Parking coroutine的 context可能尚未发布，所属 worker必须先完成 park双检成为 Waiting/Runnable，或撤销 park恢复 Running并在 safepoint停下；processor在此之前不能确认 stop；
- attached `Foreign(g)` 的 Gugu stack与 bridge frame已经稳定，collector不等待 native返回，而是立即竞争同一 generation的 lease、发布 processor-local状态、转为 `Foreign|FOREIGN_DETACHED`并确认该 processor；detached Foreign/DirtyWaiting直接按保存 PC的 `ForeignBridge` map扫描 ABI frame。任何 native OS stack都不扫描，dirty/detached foreign worker不参与 stop确认；
- processor只在 write-barrier buffer完成所需 drain、TLAB边界发布且 ownership不再被旧 worker使用后确认 stop。

所有 active processor 确认后进入 stop 阶段。扫描/复制某 coroutine 时持有 stack scan lock；ready 可以设置 Runnable 意图，但在 lock 释放前不能让 worker执行它。GC 完成 relocation 和 metadata 发布后以 release 增加 resume epoch，worker acquire 后恢复。

空闲 processor/worker优先协助 concurrent mark；scheduler 保留至少一个 worker 处理 poller、timer 和 runnable，不能因 GC work 无限延迟用户调度。

## 动态并行度

公开 facade按[运行时](../spec/runtime.md#gc栈与运行时控制-api)验证并线性化请求后，向 scheduler发布 `ApplyParallelism { old, new, epoch }`；scheduler不再次决定零值错误、setter返回值或公开状态。增加时按 runnable demand创建/复用 processor并唤醒 worker，不按 new一次性预建线程；同时提高 dirty target并按 FIFO admission等待项。必需分配失败上报 runtime fatal入口。

降低时把 ID最大的多余 processor标为 `Retiring`，并把 dirty target降为 `new == 1 ? 1 : new - 1`；active dirty work不强杀，实际 limit保持不低于 active直到排空。Retiring processor不接受新的远程 runnable；若它正被 attached普通 bridge保留，retirement立即按精确 generation retake而不等待 native返回。完成当前 managed coroutine或取得 lease后，按 local queue、timer inbox、barrier buffer、mark work的固定顺序转移状态，再进入 Idle。processor ID不复用，控制块从 pool重新激活时取得新 ID。managed worker绑定还必须遵守第一个章节定义的共享 CPU预算。

## 终止

runtime状态机先根据[进程寿命](../spec/runtime.md#进程寿命)生成 `TerminationPlan { mode, admit_user_coroutines, wait_foreign, report_epoch }`；scheduler只执行该 plan，不决定 `process.exit`、fatal、defer或报告语义。停止接纳后唤醒 parked worker、关闭新 poller注册并等 runtime critical section 到达安全边界；`wait_foreign` 同时覆盖普通 foreign、`DirtyWaiting` 和正在执行的 dirty work。

worker无 runtime/foreign责任后转 Stopping。主线程按 poller、processor、GC、stack arena顺序关闭内部设施，再把 plan结果交给宿主退出。Dead coroutine的 stack已经在完成路径归还；其控制块只在最后一个 Join/handle与 runtime root释放后回收。

## 不变量与验证

调度器调试构建持续检查：

- `ENQUEUED` 与实际 runnable 位置一一对应；
- Running coroutine具有唯一 worker和 processor；attached Foreign具有唯一 foreign worker和保留 processor，detached Foreign具有唯一 foreign/dirty worker但没有 processor，DirtyWaiting二者都没有；
- `ForeignBridgeState.lease_word` 的 generation与 coroutine完整 state一致；`FOREIGN_DETACHED`只出现在 Foreign，attached Foreign必须与 `ProcessorOwnership.current_coroutine`双向一致，旧 worker未赢得返回 CAS时不能访问 processor；
- `ForeignBridge[DirtyCpu]` 的 active数量不超过 `dirty_cpu_limit`，每个 DirtyWaiting bridge恰有一个独立 `dirty_link`等待位置和合法 high-relative ABI frame；
- local queue 距离不超过 256，global intrusive link 不成环；
- wait generation 单调且 winner/ready 最多一次；
- `StackDescriptor`、`PollControl`、`ProcessorOwnership` 的64-byte size/alignment/offset断言成立，stack bounds、`POLL_SENTINEL`、context PC和 stack map匹配；每个 live/cached stack slot只属于一个 span与一个 cache/global位置，arena内部没有逐栈 protection run；
- processor retire/foreign retake不丢 runnable、timer、TLAB、barrier、mark work或 pending poll flag；GC stop只写 active processor handshake与当时 Running stack guard，不修改全部 coroutine lifecycle；
- GC scan lock下不能执行/复制同一 coroutine；Running processor的 poll-free机器路径cost不超过 `POLL_BUDGET`，所有 cyclic路径有同步 poll/checked entry；每个 `NoSafepointRegion`无 machine backedge、无 blocking/slow edge且 legalized cost不超过同一 budget。

确定性 runtime测试必须使用可控 VM backend、poller/clock、monitor generation note和调度 gate，覆盖 local overflow、半队列窃取、park/wake竞争、select loser、timer cancel、stack class选择、cache refill/flush上界、span改换class、空页decommit、arena映射次数不随slot增长、四窗收缩迟滞、Waiting冷压缩与唤醒增长、内存压力收缩、三类64-byte控制块布局、poisoned `StackCheck` 同时处理增长与抢占、monitor无deadline深睡且无周期wake、发布更早deadline不丢wake、无竞争长运行不触发周期抢占、竞争出现后的10 ms资格判断、counted-loop outer chunk、uncounted-loop countdown、普通 bridge无压力原 processor快返回、20 µs前后 runnable retake、return/retake完整 state CAS竞争、generation ABA拒绝、idle processor直接恢复、global enqueue慢路径、GC/retirement/callback立即打破 lease、DirtyCpu额度耗尽与释放、ForeignLeaf保留 processor、未知间接调用回退 bridge、opaque asm进入 dirty、动态 parallelism、GC epoch发布/确认与 ready竞争。默认测试不能真实分配百万 stack、依赖真实10 ms/20 µs延迟、真实 OS timer resolution或随机 victim恰好出现。

poll/select确定性测试还必须证明：stop请求不遍历或 CAS非 Running coroutine state，global epoch与 lifecycle不出现在 poll/prologue fast path；普通 prologue只有一次 `stack_check` load与一个共享 taken branch，loop poll只有一次 `poll_flags` load；GC与增长同时 pending时 `MorestackEntry`先停机再按最新 bounds增长。select分别覆盖1–8 case展开路径、try-lock失败释放、超过8 case逐 source路径、Building期 winner、default CAS竞争、大 payload两阶段 reservation、loser逐 source注销，以及 verifier拒绝 region内 backedge/blocking/barrier refill。

完成路径测试还必须证明：`finish_coroutine` 切到 system stack后不再访问旧 `rsp`，result publication先于 stack摘根与 `Dead`，保留 Join handle不会保留 stack slot，GC与完成竞争时旧 slot只发布一次。

poll policy的性能门禁属于 bench/手工 profiling，不进入默认 nextest：至少比较 `POLL_BUDGET` 1024/4096/16384在空整数 counted loop、可向量化整数内存扫描、uncounted cyclic CFG、无 frame调用链、allocation fast path、递归 SCC和1/8/64-case select上的 instructions/iteration、poll-word loads、branch misses与吞吐；机器码检查必须证明普通 prologue只有一次 `[r14 + stack_check]` load、budget poll只有一次 `[r15 + poll_flags]` load、两者都不读取 global epoch/lifecycle，counted inner loop没有独立 poll countdown或 poll-word load，且 `NoSafepointBegin/End`编码为零字节。select基准必须分别比较展开路径、锁竞争后逐 source路径和大 payload reservation。另在可控 GC请求下记录 request到 processor ack的 p50/p99/max cost units和 wall-clock。修改默认4096、inline select门槛、vector/unroll cost model或 opcode weight必须同时证明 hot-path回归与 stop-latency收益，不能只优化单一 microbenchmark。

foreign bridge性能门禁属于 bench/手工 profiling：以真实 C ABI空 stub、约100 ns/1 µs/10 µs CPU工作、受控阻塞、同步 callback和 GC重叠为 workload，分别在单 processor空闲、单 processor有 runnable压力和多 processor饱和场景测量 ns/call、原 processor fast-return率、retake率、idle direct-resume率、global enqueue、scheduler mutex contention、OS thread wakeup、TLAB refill、cache miss与 runnable/GC p99延迟。必须逐项比较立即 handoff基线、20 µs grace和候选 grace；不能只优化空 stub而让阻塞调用、callback或 GC stop失去进度保证。

stack allocator与冷压缩性能门禁属于 bench/手工 profiling：以浅 entry、递归增长、channel/Join长期等待、频繁 park/wake和“深四轮、浅四轮”负载分别测10万与100万 coroutine的完整 bytes/live-coroutine、`stack_live/reserved/committed`、RSS/commit charge、Linux VMA或Windows reservation数量、本地 cache命中、global class lock竞争、page fault/decommit、stack copy bytes和 create/park/wake p50/p99。必须与8 KiB逐栈 mapping基线比较；默认2 KiB、64 KiB processor cache、四窗迟滞或512 B冷 class的调整必须同时证明内存收益与 create/wake/深浅振荡吞吐，不得只比较虚拟地址数字。

monitor性能门禁必须在完全空闲、单个无竞争CPU coroutine、runnable饱和、timer密集、短 bridge压力和GC stop场景记录10分钟内的 monitor wake、CPU time、context switch、抢占/retake/GC p99以及Windows timer-resolution lease持有时间。空闲和无竞争CPU场景除显式 maintenance deadline外必须保持零周期wake；短deadline收益不能以常驻1 ms timer resolution、busy-spin或更差的 runnable/GC进度换取。

## 参考实现资料

- [Go runtime scheduler](https://go.dev/src/runtime/proc.go)
- [Go runtime移除每次 cgo `_Psyscall` 状态记账](https://github.com/golang/go/commit/7244e9221ff25b0c93a13ad8f1aa8917ca50f6973)
- [Go runtime stack](https://go.dev/src/runtime/stack.go)
- [Go runtime network poller](https://go.dev/src/runtime/netpoll.go)
- [Rust 标准库线程 park/unpark](https://doc.rust-lang.org/std/thread/fn.park.html)
- [Go runtime asynchronous preemption](https://go.dev/src/runtime/preempt.go)
- [Linux `mprotect(2)` protection拆分与映射数量错误](https://man7.org/linux/man-pages/man2/mprotect.2.html)
- [Windows `timeBeginPeriod` 精度、作用域与功耗影响](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod)
- [Erlang NIF dirty scheduler](https://www.erlang.org/doc/apps/erts/erl_nif.html)
- [GHC FFI safety](https://ghc.gitlab.haskell.org/ghc/doc/users_guide/exts/ffi.html)
- [Wasmtime interrupting execution](https://docs.wasmtime.dev/examples-interrupting-wasm.html)
