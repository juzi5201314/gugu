# 调度器

本章规定 Gugu runtime 当前的 M:N 调度器、可复制协程栈、工作窃取、park/ready、抢占、I/O/计时器和 GC/foreign-call 交接。语言可观察的公平、等待、退出和动态并行度规则见[并发与调度](../spec/concurrency.md)与[运行时](../spec/runtime.md)；队列结构和时间片是内部规范，不能被用户程序观察或依赖。

## 权威边界

[表达式](../spec/expressions.md)、[并发与调度](../spec/concurrency.md)、[运行时](../spec/runtime.md)和[平台 ABI](../spec/platform-abi.md)唯一规定 async/select求值、同步、公平、动态控制、进程寿命及 FFI结果。本章只固定官方 runtime如何实现已经给定的调度事件和状态转换；队列位置、worker身份、随机序列、时间片和park协议不能成为程序语义。

除平台 rt0、context switch、signal/exception stub和必须的 machine intrinsic外，官方 scheduler/runtime主体使用 Gugu实现；不得维护一份 Rust语义等价runtime作为正常执行路径。

runtime源码需要在锁所有权或 root publication的常数临界区暂缓 safepoint时，只能使用 compiler内部的 [`NoSafepointRegion`](gir-lir.md#nosafepointregion)；它不是用户 attribute，也不允许建立另一套不受 poll预算约束的 runtime路径。

## 内存返回接缝

stack、control、wait、resource、range 和 Mosaic GC message 的跨 owner 生命周期归还统一遵守[内存所有权与消息通道](memory-messaging.md)。`CoroutineHot`、ready/wake 和 join 的现有布局与线性化语义不改变；raw return 只作用于地址稳定的 slab/span 记录，`MarkTicket`、`EdgeDelta`、`RegionTransfer` 和 `HandleForward` 只作用于 stable managed descriptor/handle。

跨协程完成路径可先用 `ReturnSlabCache` 按 source slab 聚合地址稳定的 stack/control/wait slot，再发送 owner return；`MarkMailbox` 按 target owner、cycle 和 generation 聚合 GC work，二者都不参与 runnable、wake 或 join 的线性化。

ready/wake 消息与 memory return/GC 消息必须在类型和 verifier 上分离。memory/GC message 可以批量延迟，但不能成为 coroutine 可运行性、channel 完成或 join 完成的判据；producer gate、topology epoch、cycle credit 和 queue-page grace 继续覆盖消息存储稳定性。

## 三层实体

调度器采用 Go 风格的三层模型，但使用完整名称而不是单字母缩写：

- `Coroutine`：用户协程，拥有可复制 stack、寄存器 context、coroutine-local 和等待状态；
- `LogicalProcessor`：执行 Gugu 用户代码所需的 runtime capability，数量等于当前 `parallelism` 目标；
- `WorkerThread`：操作系统线程，绑定一个 processor 时执行用户代码，也能在无 processor 时执行普通 `ForeignBridge`、`ForeignBridge[DirtyCpu]`、poller 和 runtime 系统工作。

一个 `Coroutine` 同时最多由一个 worker 执行；一个 `LogicalProcessor` 同时最多绑定一个 worker；一个 worker同时最多绑定一个 processor。普通 `ForeignBridge` 先经过 `BlockingBridge` admission并取得 `BridgeCredit`，进入 native时可以让原 worker短暂保留 processor lease，runtime retake后才释放；`ForeignBridge[DirtyCpu]`立即释放 processor，dirty work不取得 processor。

ID 表示固定为 `CoroutineId(u64)`、`LogicalProcessorId(u64)` 和 `WorkerThreadId(u64)`，进程内单调分配且不复用。计数溢出说明 runtime 内部不变量已破坏，进入 `RuntimeInvariant` fatal，不能回绕。

`LogicalProcessorId` 是稳定身份，不兼作数组下标。每次topology epoch都发布一份仅含`{ ptr, id, numa_domain }`的稠密active processor `Vec`；窃取、广播和NUMA分组只在该快照内按连续slot迭代，每次解引用前验证control block当前ID与state仍匹配，退役后重建快照。公开`parallelism`不增加固定硬上限，scheduler元数据必须保持`O(active_processors)`，禁止使用`P × P` mailbox或稀疏ID `HashMap`。

scheduler 维护进程级 `dirty_cpu_target`、`dirty_cpu_limit`、`dirty_cpu_active` 和 intrusive `dirty_wait_queue`。当 `parallelism = 1` 时 target 为 1；否则 target 为 `parallelism - 1`，给 managed scheduler 保留至少一个 CPU 执行槽。`dirty_cpu_limit` 在降低并行度时不能小于已经 active 的数量；超出新 target 的 work 只允许自然排空，期间不接纳新 dirty call。绑定 processor 的 managed worker 上限为 `max(1, parallelism - min(dirty_cpu_active, parallelism - 1))`，因此 `parallelism > 1` 时 dirty worker与 managed worker共享 CPU预算；`parallelism = 1` 时允许一个 managed worker和一个 dirty worker由 OS 时间片复用。没有可用额度时，调用方在已发布 bridge roots且不持有 processor 的 `DirtyWaiting` 状态排队，不创建无界 OS thread，也不在等待额度时执行 native code。

## Coroutine

### 控制块

`Coroutine` 的逻辑控制块由两个地址稳定的 runtime slab record组成。队列和汇编使用 `CoroutineHot*` 作为 `Coroutine*`；typed visitor、debugger和 runtime layout query共同消费同一 schema，禁止维护平行影子控制块：

```text
CoroutineSlot {                         // non-moving，128 byte
    hot: CoroutineHot,                  // 64 byte / 64-byte aligned
    stack: StackDescriptor,             // 64 byte / 64-byte aligned
}

CoroutineHot {
    state: AtomicU64,
    run_link_next: UnsafeCell<CoroutineHot*>,
    current_processor: AtomicPtr<LogicalProcessor>,
    preferred_processor: AtomicPtr<LogicalProcessor>,
    preferred_processor_id: AtomicU64,
    wait_word: AtomicU64,
    cold_index: u64,
    run_batch_len: UnsafeCell<u8>,
    reserved: [u8; 7],
}

CoroutineCold {
    id: CoroutineId,
    context: CoroutineContext,
    morestack_scratch: MorestackScratch,
    wait_record: WaitRecord,
    foreign_bridge: ForeignBridgeState,
    join_state,
    coroutine_locals,
    panic_state,
    select_rng: [u64; 4],
    select_scratch: SelectScratchCache,
    gc_scan_epoch: AtomicU64,
}
```

`CoroutineSlot` 与 `CoroutineCold` 都来自分段 non-moving slab；扩容只能增加新页，不能移动旧 record。`cold_index` 在 segmented cold table 中稠密解析，不形成 managed pointer。`preferred_processor` 只是 last-owner locality hint，只有pointer当前ID与`preferred_processor_id`一致且state仍为active时才有效；这对值防止processor control block复用形成hint ABA，指向`Retiring`时必须忽略。`run_link_next` 和 `run_batch_len` 只由当前 queue owner普通读写：producer在 Release 发布 batch前独占，consumer通过 Acquire摘取后独占；其它访问是 runtime memory-safety错误。`r14` 指向 `CoroutineHot`，`StackDescriptor` 位于同一 `CoroutineSlot` 的固定下一条 cache line。

`select_scratch` 只保存 readiness bitmap，不保存 managed pointer；source、send payload 和 result slot仍位于用户 frame并由 stack map追踪。需要登记多个 case时，`wait_record` 的 tagged `Select` variant持有 `SelectTxn` 与一个 non-moving `SelectWaitBlock` handle；block按 case数量在取得任何 source锁前从 runtime wait-node size class取得，node只保存 Coroutine handle、generation、case index和 stack-high-relative payload/result offset，由 typed visitor按 descriptor扫描，禁止保存会被 stack copy悬空的裸 pointer。cleanup完成后 block立即归还；scratch由同一 `SelectTxn` 的 owner 负责释放或转入有界 cache，typed visitor跳过其 raw bytes。

`ForeignBridgeState` 位于 cold record，只在 lifecycle 为 `Foreign` 或 `DirtyWaiting` 时有效，固定保存 `{ mode, call_stub, frame_offset, frame_size, lease_word, dirty_link, bridge_credit, error_state }`。compiler 在 coroutine stack 上物化 ABI bridge frame；`frame_offset` 是从逻辑 `stack_high` 到 frame起点的 checked深度，record不保存会因 stack copy失效的裸 stack pointer。`call_stub` 是 non-moving code pointer；`lease_word` 保存本次 bridge generation和进入 native后期望的完整 lifecycle word，不是 pointer；`dirty_link` 只供 `dirty_wait_queue` 使用，不能复用 runnable `run_link_next`；`bridge_credit` 是 `BlockingBridge` admission发放的非指针额度，attached 与 detached bridge 共用且最多归还一次。ABI frame里的 managed/raw pointer按调用点 stack map追踪，交给 native 的 managed地址还必须在进入 bridge前 pin或复制。

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

bit 4固定为 `ENQUEUED`，bit 5固定为 `STACK_SCAN_LOCKED`，bit 6固定为 `FOREIGN_DETACHED`，bit 7固定为 `BATCH_PUBLISHING`，bits 8..63是56-bit `foreign_generation`。`BATCH_PUBLISHING` 只能与 `Runnable|ENQUEUED` 同时出现，表示本次ready从非runnable owner转交给producer staging时已经由batch publish协议认领；已经在local/inbox/carry中的`ENQUEUED`节点做queue-to-queue batch transfer时不设置该bit，避免overflow/redistribution逐节点RMW。节点尚未公开还是已经位于inbox/carry/local queue，只能由登记的`ProducerStaging`与queue ownership判断，不能只看该bit。`FOREIGN_DETACHED` 只能与 `Foreign` lifecycle同时出现：为0表示普通 bridge仍保留原 processor lease，为1表示 processor已经被 retake或该调用从一开始就是 dirty bridge。每次从 `Running` 进入普通/active dirty `Foreign` 或 `DirtyWaiting` 时 generation加1；`DirtyWaiting -> Foreign` 保持同一 generation。generation溢出进入 `RuntimeInvariant` fatal，不能回绕。抢占与 GC stop通知刻意不占 lifecycle bit：它们只写 active `LogicalProcessor.poll_flags`，并投毒该 processor当时 `Running` coroutine的 `stack_check`；Runnable/Waiting context已经稳定，不需要逐 coroutine通知。fast path禁止读取 lifecycle或全局 GC epoch。所有 lifecycle转换比较完整 `u64`并携带必要的 acquire/release ordering，不能只比较低位或以互斥锁外普通读写替代；generation使延迟 retaker不能把旧调用误认成新的 `Foreign`，消除 lifecycle ABA。

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

`Dead` 是终态。`ENQUEUED` 在节点进入 `run_next`、local deque、producer staging、remote inbox、injection或detached carry前与 `Runnable` 同一原子转换设置，取为 `Running` 时清除；任何时刻同一 coroutine只能有一个 runnable ownership位置。从Waiting/Foreign等非runnable owner经batch ready时在同一次CAS额外设置`BATCH_PUBLISHING`，producer发布后不逐节点执行原子清位，普通`Runnable -> Running`与`ENQUEUED`一起清除它；已经`ENQUEUED`的range在local、remote和injection之间只转移ownership，不修改state。`ProducerStaging` 是 unpublished batch 的唯一权威登记。`STACK_SCAN_LOCKED` 是 scanner/foreign retaker对完整 state word的临时所有权，释放时必须保留 generation；离开 `Foreign` 时清除 `FOREIGN_DETACHED`，其它 lifecycle携带该 bit都是 `RuntimeInvariant`。

### context

x86_64 `CoroutineContext` 保存 `rsp`、resume `rip`、`rbx`、`rbp`、`r12` 和 `r13`。`r14` 固定重建为当前 `Coroutine*`，`r15` 在绑定后重建为当前 `LogicalProcessor*`。普通 suspend/call safepoint 已按[栈图](stack-maps.md)spill 用户 pointer，XMM 和 caller-saved register 不属于持久 context。

抢占 poll 若要扫描寄存器，slow path 先保存栈图编号中的全部通用寄存器；恢复前再装载。context 的发布使用 release，接手 worker acquire 后才能读取 stack bounds、resume PC 和保存寄存器。

## LogicalProcessor

每个 processor 包含：

- 独占一条 cache line的 `PollControl { poll_flags, requested_gc_epoch, ack_gc_epoch }`；只有 processor owner写 ack，collector以 acquire读取；
- 独占一条 cache line的 `ProcessorOwnership { state, owner, current_coroutine, run_started_ns }`，其中 state只取 `Idle`、`Bound` 或 `Retiring`；`Bound` 同时覆盖 managed execution和 attached普通 bridge，`run_started_ns` 只在 current lifecycle为 `Running` 时解释；
- 容量固定为256的 `LocalDeque` 与一个 owner-only `run_next`；
- `REMOTE_INBOX_SHARDS = 8` 个 `RemoteBatchHead`、对应 detached carry和 round-robin cursor；
- 一个 owner-only injection carry；
- TLAB cursor/limit、write-barrier buffer和 per-processor mark work；
- 七个固定小栈 class的本地 cache head与总字节计数；
- 一个 owner-local `TimerWheel` 与 overflow heap；
- 调度随机状态和 service tick。

active processor表、remote inbox和注入域的内存均为 `O(P)`；每个 processor固定8个 shard用于分散 all-to-one producer对单一 head line的竞争。修改8这个值属于 scheduler性能策略变更，必须通过本章的 shard sweep门禁，不能按 `P` 动态扩成平方级 mailbox。

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
    current_coroutine: AtomicPtr<CoroutineHot>,
    run_started_ns: AtomicU64,
    padding: [u8; 32],
}
```

`LogicalProcessor` 按 `poll`、`ownership`、local deque的 thief-visible head、owner tail、remote heads、stack cache、TLAB/barrier和其它冷字段的顺序布局。构建时必须断言两个控制块的 `size_of == align_of == 64`，且 `offset_of!(LogicalProcessor, poll)` 与 `offset_of!(LogicalProcessor, ownership)` 都是64的倍数并相差至少64；backend使用同一 runtime layout query取得 `[r15 + poll_flags_offset]`，禁止手写重复 offset。`PollControl` 只与同一次 preempt/GC handshake相关的低频写共享 line；scheduler state、owner、current coroutine、run timestamp、queue、stack cache、TLAB和 monitor字段不能落入该 line。

### batch inbox 与 injection

`QUEUE_PAD_BYTES = 128`。每个 `RemoteBatchHead` 是独占128-byte区域的 `AtomicPtr<CoroutineHot>`；相邻 shard、local head、local tail和 idle event counter都不能共享该区域。128-byte padding只用于数量为 `O(P)` 的高争用元数据，不用于每个 coroutine。Booting 时对 size、alignment和 offset做静态断言。

所有能从非 owner线程 ready coroutine的 runtime执行上下文都必须登记稳定的 `ProducerHandle`。worker、poller、foreign/callback线程各自拥有一个 handle；handle保存 `{ shard_seed, publish_active, topology_epoch_seen, slab_epoch_seen, pending_node, staging_first, staging_last, staging_count, staging_target, staging_shard }`。一次 staging只允许一个 target/shard，`BATCH_MAX = 128`：目标改变、达到上限、当前 runtime调用的有限事件批处理结束或 producer准备 deregister时立即 flush。普通单次 wake形成单节点 batch并立即发布，禁止等待未来事件凑批而增加唤醒延迟。`pending_node` 与 staging chain都是 typed runtime root；producer被 OS抢占或在 CAS retry loop进入 safepoint时，节点仍可枚举。

开始一个publish batch前，producer先Release设置自己独占cache line中的`publish_active = true`，再Acquire读取read-mostly queue control epoch；topology变化时必须刷新active snapshot后才能选择target，slab reclaim gate开启时不得开始新batch。完成head CAS/flush后发布seen epoch，再Release清`publish_active`。retirement或page grace在发布新epoch后只等待当时active的旧epochproducer；它观察到inactive后开始的新producer必然先读取新control word。该协议每batch只有producer-local stores与一次共享只读load，不执行全局RMW或SeqCst fence。
从Waiting/Foreign等非runnable owner认领单个节点时，producer先把仍由原owner保活的节点写入`pending_node`，再用完整state CAS设置`Runnable|ENQUEUED|BATCH_PUBLISHING`；CAS失败立即清pending并服从winner，成功才接入staging chain并清空`pending_node`，不得存在state已经认领而handle与旧owner都不可见的窗口。local overflow、carry redistribution等已经`ENQUEUED`的range先原子认领queue ownership，再直接接入staging，不改state。batch内按newest-to-oldest链接，第一节点的`run_batch_len`保存1..128，其余节点保存0。发布采用Linux `llist_add_batch`形态：

```text
old = head.load(Relaxed)
last.run_link_next = old
head.compare_exchange_weak(old, first, Release, Relaxed)
```

CAS失败只用返回的实际 head重写 `last.run_link_next`并重试。成功后 batch整体可见；producer清空 staging，但不逐节点清 `BATCH_PUBLISHING`。旧 head只作为不透明 pointer值写入 producer独占的 last node，producer不得解引用它。control slab page地址稳定且 consumer只用 atomic exchange摘取整链，因此普通 publish不进入 generic epoch、hazard-pointer或全局 refcount协议。

target processor owner以 `head.swap(null, Acquire)`取得某 shard当时的完整链，并把它保存到该 shard的 typed detached carry；remote head不能混用逐节点 pop或基于旧 head的 consumer CAS。owner每次 service最多转移128项，先从batch首节点读取并验证`run_batch_len`，遍历时先保存`next`；节点真正写入local deque时才把`run_link_next`与`run_batch_len`普通清零，仍需carry或重新发布的batch必须保留原边界。detached carry未清空前不再次摘该 shard的新 head；8个 shard按 round-robin服务，因此持续生产的单一 shard不能永久排除其它 shard。newest-to-oldest遍历依次 push local tail，owner再从 tail pop，batch内实际执行恢复 oldest-to-newest；不额外反转链表。

Booting 时按宿主 NUMA topology建立至少一个 `InjectionDomain`；无法可靠探测时只建立一个。每个 domain同样含8个128-byte padded batch head，但允许多个 worker竞争 `swap(null)`，成功者把链登记为自己的 typed injection carry。无合法 preferred processor、目标正在 `Retiring`、local overflow和未归属的外部事件进入 source-local domain；worker先检查当前 NUMA domain，再按随机起点检查其它 domain。一次 winner只在本轮消费一个 publish batch或128项；carry中还有其它 batch且存在 idle demand时，按 `run_batch_len`边界把后续 batch重新发布到其它 injection shard，不能让单 worker私占无界 burst。processor retire、worker stop和producer deregister前必须发布全部 staging/carry。

remote/injection publish只有在 CAS前观察到 `old == null` 时才把进程级 `work_seq`递增一次；普通非空队列追加不写共享 pending word。preferred target为 `Bound`且系统存在 idle processor时，还设置该 target的 `PREEMPT`并投毒当前 `stack_check`，促使 owner尽快 drain/overflow；target为 `Idle`时唤醒一个 worker去绑定它。hint只省略通知，park正确性始终由 queue recheck与 `work_seq`封闭。
### LocalDeque

本地队列容量有严格256上界，key是稠密递增 ticket，访问模式是 owner尾部 push/pop与 thief头部 steal，因此使用内联 `[UnsafeCell<MaybeUninit<CoroutineHot*>>; 256]`，不使用通用 deque、per-slot allocation或 `P × P`结构。slot普通读写只发生在算法已经证明唯一 ownership的区间；push的 Release tail/head发布与 consumer的 Acquire/CAS取得构成可见性边。head、tail分别占用128-byte区域，debug构建持续检查逻辑距离不超过256。

官方实现必须在 model/bench构建中提供两个同语义变体，并由 target性能档案在编译 runtime时选择一个；release镜像只包含被选变体，不在 dispatch热路径分支。档案没有通过本章全部正确性与性能门禁前，使用 `Classic64` correctness baseline：

- `Classic64` 使用不回绕的 `AtomicU64 head/tail`。owner push先Acquire读head、Relaxed读tail，写slot后Release发布tail；owner pop沿Lê等人修正的 Chase–Lev顺序执行 SeqCst fence，最后一项以AcqRel CAS与thief竞争。thief/overflow以AcqRel CAS一次认领连续头部范围。任一ticket将溢出时进入 `RuntimeInvariant`，不能回绕解释旧slot；
- `Packed55` 使用一个 `AtomicU64` 编码55-bit steal ticket与9-bit `real_head - steal_head`，合法距离0..256，511保留为 `RESETTING`。owner pop用AcqRel CAS推进real head；一次thief先CAS预留至多128项、复制后再CAS把steal head追到当时real head，期间其它thief退出但owner可继续pop。接近55-bit上界时owner先把队列全部发布到injection，在空队列且无in-flight steal时CAS为 `RESETTING`，Release把tail归零，再Release发布packed head为零；thief遇到sentinel退出，持旧expected的CAS必然失败。禁止使用会在进程寿命内静默回绕的两个裸`u32` lane；
- 两种变体的owner都只能在唯一processor capability下修改tail；thief只能认领head范围。每个unsafe slot访问必须带局部 `SAFETY` 证明，说明ticket范围、发布边和唯一读写者。

`run_next` 用于刚 ready、与当前工作具有局部性的一个 coroutine。放入新值时若已有旧值，旧值先进入普通 local deque；同一 coroutine连续从 `run_next`获得优先的次数最多为1，随后必须经过普通队列以维持公平。

local deque满时，owner一次认领最旧128项，按 newest-to-oldest串成publish batch并送往 idle processor的 remote inbox；没有 idle target时送往source NUMA injection domain，再放入新项。overflow不访问runnable全局mutex，也不逐项分配或逐项通知。

### Select scratch

`SelectScratchCache` 是 coroutine cold record 中的固定描述符，不是可任意增长的 `Vec`。每次 `select` 先以内联的8个 `u64` readiness word处理不超过8个 case；超过8个 case时按 `1, 2, 4, ... 1024` 个 word的 size class 从 owner-local raw cache取得，超过1024 word则从 owner slab/extent取得精确或向上取整的临时 allocation。`SelectTxn` 记录 `{ owner, class, capacity_words, used_words, generation }`，scratch只含 bitmap和规范化 source index，不含 managed pointer。

规范化 source 顺序、case-to-word mapping和去重结果组成 immutable `SelectPlanKey`；同一编译期固定 plan 可在 coroutine cold cache中复用 mapping，但 readiness bits和 winner state每次清零。动态 case集合不得伪装成固定 plan，仍按 `WaitSourceId`排序去重并逐 source登记。

select cleanup在 winner提交、取消注销和 payload reservation完成后立即归还 scratch。每个 processor的 retained cache按 size class维护，累计容量不得超过 `select_scratch_cache_bytes` profile上限；超过上限的 allocation直接归还 owner slab/extent，不得由 coroutine长期保留。cache miss只发生在 begin slow path，cache hit不分配；pressure、processor交接、ForeignBridge、GC stop和 owner retire 都必须先清空该 processor cache的可转移 lease。cache与 wait-node 分开计入 runtime pressure，不能以复用掩盖实际 committed bytes。

大于8 case的路径可以 O(k) 构造并登记，但不得在 source 锁内完成排序、分配或跨 source 等待；`SelectTxn` 的每个 source 注册、winner CAS、注销和 scratch class都必须可计数。release profile必须记录 case 数为1/2/8/64/256时的 registration、lock、winner-conflict和 retained-bytes计数；未通过对应 profile门禁不能把大 case路径宣称为固定成本。

### TimerWheel 与 blocking bridge

per-processor timer使用四层、每层256 bucket的 hierarchical timing wheel，并保留一个按 exact deadline、timer sequence排序的 overflow min-heap。wheel的tick和 near horizon来自 runtime tuning profile；deadline按不早于目标时间的 tick 放入 bucket，poll 到 bucket 后仍比较 exact deadline，因此轮盘量化不能提前唤醒。horizon内的 timer 进入 wheel，远期 timer进入 heap；每次 service 按有限 `timer_drain_budget` 把已进入 horizon的 heap项转入 wheel，禁止一次搬运无界数量。

TimerEntry 位于 non-moving owner slab，含 `{ timer_id, generation, deadline, bucket_link, cancelled, owner }`。cancel 只在线性化点标记 generation/cancelled并唤醒等待者，不要求远程调用者取得 timer owner 锁；owner 在 cursor经过 bucket、inbox drain、pressure或 cancel/live 比例达到 profile阈值时批量摘除无效项并归还 entry。bucket 的取消节点不能无限滞留，compact 每次受 `timer_compact_budget` 限制并在后续 service 继续；active timer、cancelled bytes和 overflow heap capacity都计入 runtime pressure。

### Bounded blocking bridge

普通 `ForeignBridge`、`std.fs` 的文件操作、`process wait` 与其它不能使用 poller的阻塞系统调用统一经过 `BlockingBridge` admission；`ForeignLeaf` 不得走该路径，`ForeignBridge[DirtyCpu]` 仍使用独立 dirty admission。`BlockingBridge` 维护独立于 runnable、idle、timer和injection锁的 FIFO gate，并为每次 admitted call保留一个 `BridgeCredit`：attached bridge 进入 native前就占用该 credit，若被 retake，replacement worker只能消费同一个 credit；返回或取消后 credit归还。因此 retake不能通过每个调用无界创建 OS worker。

`max_blocking_workers`、`blocking_worker_stack_bytes`、`blocking_waiter_bytes`和`blocking_service_budget`属于 runtime tuning profile；worker 数量不能超过 `max_blocking_workers`，并且所有 worker system stack、bridge frame、wait node和排队 payload计入 runtime committed/pressure账本。没有 credit时调用方在 `Waiting` 中排队并释放 processor，不执行 native code；取得 credit后才转为 `Foreign`。等待队列有 FIFO service和有限 batch，队列背压表现为协程挂起而非 OS thread oversubscription；waiter metadata达到 profile memory cap时按统一 runtime resource-exhaustion错误结束 admission，不绕过上限创建 worker。

CPU quota/container探测先决定 managed `parallelism`，再按 profile计算 blocking worker cap；探测失败使用 target descriptor的固定 profile，不读取宿主无限制的 CPU 数。active、waiting、credit、worker stack committed和队列 bytes必须进入 `RuntimeStats`，长阻塞调用与短调用的计数分开；任何普通 bridge实现不能以“预计很快”跳过 admission。

## WorkerThread

worker 状态固定为 `Booting`、`Running`、`Spinning`、`Parked`、`Foreign` 和 `Stopping`。每个 worker 使用宿主创建的 non-moving OS stack 运行 rt0、scheduler、GC slow path、signal/exception handler、普通 `ForeignBridge` 和 `ForeignBridge[DirtyCpu]` C call；`ForeignLeaf` C call 与 Gugu 用户代码一样运行在 coroutine stack。

worker TLS 保存 `WorkerThread*`、当前 processor、当前 coroutine、system-stack bounds、barrier buffer 和 foreign callback depth。TLS 不承载 coroutine-local 用户值。

worker park使用独立的 `IdleRegistry`，它只保存 idle-worker LIFO与generation token，不保存runnable task；其mutex不能与dirty admission、timer、injection或任何runnable queue共用。`work_seq: AtomicU64`与`idle_count: AtomicU64`分别位于独立128-byte区域。二者都使用checked单调值，generation或`work_seq`即将溢出时进入`RuntimeInvariant`，不能回绕匹配旧park。Linux token用futex/eventfd，Windows用`WaitOnAddress`/semaphore；旧generation的wake不能唤醒下一次park。

park协议固定为：worker先Acquire快照 `work_seq`，检查自身 processor flags、所有本地/remote carry与head、当前/远端 injection、poller和processor demand；仍为空时取得 idle mutex，在锁内再次检查 `work_seq`与同一工作集合，只有序号未变且仍无工作才登记 `{ worker, generation }`并递增 `idle_count`。publisher在 batch head发生 empty-to-nonempty后先Release递增 `work_seq`；Acquire观察到 `idle_count != 0` 时才取得 idle mutex摘一个匹配 generation的 worker并唤醒。这个最终 recheck封闭“检查为空—登记 park—producer发布”的丢 wake窗口；`idle_count`只是省锁 hint，不承担正确性。

## runnable 选择

`SERVICE_INTERVAL = 61`，`SERVICE_BATCH = 128`。一个绑定 processor的 worker每次 schedule先处理 `GC_STOP/PREEMPT`，再按固定顺序选择：

1. service tick是61的倍数且remote/injection存在工作时，先执行一次 external service；
2. 取 `run_next`；
3. 从 local deque tail取一个；
4. local为空时立即执行一次remote service，再执行当前 NUMA domain的 injection service；
5. 非阻塞检查 network poller和已到期 timer；它们只发布 batch，发布后回到步骤2；
6. 从同一 NUMA domain的其它 processor窃取；
7. 检查其它 NUMA domain的 injection，再从其 processor窃取；
8. 无工作时进入 bounded spinning，随后释放 processor并按 IdleRegistry协议 park。

external service先处理所选 shard已登记的 carry，carry清空后才对该 shard执行一次Acquire exchange；remote cursor每次从上次已服务 shard的下一个位置开始，最多检查8个 shard。一次 service最多把128项写入 local deque；空间不足而公平服务必须前进时，先把 local最旧128项作为一个 batch发布到 injection。每个持续非空 remote shard在至多8次remote service内被选中；同一 carry是有限快照，新发布的 head不能越过它。injection同样每次最多消费一个发布 batch或128项，并在存在 idle demand时重新分发carry中的后续 batch。

active processor少于2时不进入窃取。否则使用 worker-local xorshift64*：非零 state依次执行 `x ^= x >> 12; x ^= x << 25; x ^= x >> 27`，保存 x并输出 `x * 2685821657736338717`（`u64` 环绕）。先对当前 NUMA domain的稠密 active slot生成随机起点与互质step，再以同一方式遍历其它 domain；种子来自 OS entropy，失败时由 BLAKE3-256(`gugu-steal-rng-v1`、WorkerThreadId、单调启动计数)低64 bit派生，全零改为1。随机性只影响合法调度选择，不进入语言随机 API或可复现构建。

thief按照已选择的 LocalDeque变体从 victim头部认领当前数量的一半，向上取整，最多128；第一项运行，其余进入 thief local deque。窃取前验证每项 `Runnable|ENQUEUED`，成功取得后只转移 queue ownership，不清 `ENQUEUED`；真正切到 `Running` 时才清 state bits。remote inbox仍由其 target processor owner消费；empty-to-nonempty发布在存在 idle demand时向 target发 `PREEMPT`，owner会在同步 safepoint后 drain并把过量工作发布给 idle processor/injection。

waker在等待源的线性化点写result slot，递增/匹配generation，然后：

- 观察到`Waiting`时调用 `ready_publish`；如果调用者正持有目标processor的唯一owner capability，helper以完整CAS设置`Runnable|ENQUEUED`并进入该processor的`run_next/local`，否则通过`ProducerHandle.pending_node`认领并设置`Runnable|ENQUEUED|BATCH_PUBLISHING`，发布到目标remote inbox或injection；
- 观察到`Parking`时Release设置wait word的notified bit并再次Acquire读取lifecycle；若已经成为`Waiting`，继续调用 `ready_publish`，若仍是`Parking`则由scheduler的第二次检查接手；
- 观察到已过期generation或`Dead`时不做任何调度。

目标选择先使用仍为active的`preferred_processor`；调用者拥有该processor时走local，否则走该target的remote shard。hint为空、指向`Retiring`或来自无target的外部注入时，进入producer所在NUMA domain的injection。`preferred_processor`不建立语义亲和性，scheduler可在overflow、steal或processor退役时改变owner。所有channel、join、timer、poller、foreign return和callback ready都必须复用这个helper，禁止另建global runnable路径。

result写入Release，恢复coroutine以Acquire取得后读取。一个wait generation最多成功ready一次；重复wake是空操作。`select`的多个wait node共享一个原子winner，从`UNSET` CAS到case index，只有winner写payload并ready coroutine；loser只注销。

显式`yield`把Running转为`Runnable|ENQUEUED`并放到owner local queue tail，不用`run_next`或BatchInbox。这保证当前已有runnable至少有一次被选择机会。

## `select` 提交

每个 channel/Join 控制块创建时取得单调 `WaitSourceId(u64)`；GC 地址移动不改变该 ID。一次 `select` 先按源码顺序完成 HIR/GIR规定的一次求值，把 source、send payload、result slot和 barrier需求物化为稳定 case record。任何 scratch增长、随机采样、source排序和可能触发 barrier refill的 reservation都发生在取得 source锁前；持锁机器区间必须由 `NoSafepointRegion`标记并接受同一 `POLL_BUDGET`验证。

HIR把无 case且无 default的形式标为 `SelectPlan::Never`。scheduler对此不生成 source锁或随机取模，只通过专用 never wait记录把 coroutine置 Waiting；该记录没有普通 waker，何时因 runtime终止而不再执行只消费 `TerminationPlan`。只有至少一个非 default case时才进入下述算法。

coroutine 的 `select_rng` 使用 xoshiro256++。一次 next固定为 `result = rotl(s0 + s3, 23) + s0`，再执行 `t = s1 << 17; s2 ^= s0; s3 ^= s1; s1 ^= s2; s0 ^= s3; s2 ^= t; s3 = rotl(s3, 45)`，全部 `u64` 环绕。初始32字节状态为 BLAKE3-256(`gugu-select-rng-v1`、OS entropy、CoroutineId、进程启动 nonce)；全零时把 `s0`置1。该状态不与 `std.random`共享。`uniform(n)` 固定使用 threshold rejection：`threshold = 0u64.wrapping_sub(n) % n`，重复取值直到 `x >= threshold`，返回 `x % n`；所有 rejection与 permutation生成都在锁外执行。

取得任何 source锁前，runtime按 `case_count` 准备 readiness scratch：case数不超过8时使用 `SelectScratchCache` 的内联8个 `u64`；更大时按 size class或临时 extent取得 `ceil(case_count / 64)` 个 word，并把 capacity、owner和generation记录到 `SelectTxn`。scratch不保存 managed pointer，cleanup后立即归还或进入不超过 `select_scratch_cache_bytes` 的 owner-local cache。case record还必须预留其临界写可能产生的 barrier entry；若当前 buffer不足，refill在锁外完成。

### 1–8 case 的展开路径

`case_count <= INLINE_SELECT_CASES = 8` 且 legalized临界成本不超过 `POLL_BUDGET` 时，`LowerConcurrency` 在锁外生成无偏 Fisher–Yates case permutation，并把按 `WaitSourceId`排序、去重后的至多8次 `try_lock`、readiness检查、提交/登记和 unlock完全展开。成功取得第一个 source锁后进入单一 `NoSafepointRegion`；任一 `try_lock`失败就按逆序释放已经取得的锁，退出 region并进入一般路径，禁止持有前一个锁等待后一个锁。最终 LIR没有 region内 backedge、RNG循环或可能阻塞的 lock slow edge。

全部锁取得后，按预生成 permutation选择第一个 ready case，因此稳定 ready集合中的每个 case概率相同；在同一 region内重新验证并提交，再释放全部锁。没有 ready case且存在 default时，在持锁快照下选择 default。没有 default时，在同一快照下登记共享 `SelectTxn` 的 wait node，释放全部锁后执行下面的常数 arm协议。类型布局、屏障或 payload操作使 region成本超限时，即使 case较少也必须使用一般路径，不能扩大 budget或省略 poll。

### 任意 case 数的一般路径

一般路径一次只持有一个 source锁。外层 readiness扫描在每次迭代之间保留正常 budget poll；每个迭代的 lock成功边进入一个无 backedge的 `NoSafepointRegion`，记录一个 case的 readiness后立即 unlock。若 bitmap非空，在锁外用 `uniform(ready_count)`选择 case，只取得该 source锁并重新验证；成功时以该 source操作为线性化点，失败则重新扫描，不能凭过期 bitmap提交。

没有 ready case时，建立 `SelectTxn { generation, phase: Building, winner: UNSET }`，以锁外生成的随机完整排列逐 case登记 wait node；登记循环可以 safepoint，每次仍只取得一个 source锁。Building期间 waker可以 CAS winner并写 result/notified，但不能 ready仍在执行登记的 coroutine。全部 node登记后：存在 default时由 `winner: UNSET -> DEFAULT` 的 CAS与所有 source waker竞争，该 CAS成功就是 default线性化点；没有 default时，以一个无 backedge region把 lifecycle `Running -> Parking`、txn phase `Building -> Armed`并再次检查 winner，随后复用两阶段 park。winner已经产生时撤销 park或直接继续。winner确定后，loser node也按一次一个 source锁注销，清理循环继续接受 budget poll。

source内部锁的 acquire slow edge在尚未持有任何其它 source锁时才可 park；成功 edge的第一项必须是 `NoSafepointBegin`。常见固定尺寸 payload在预留 barrier permit后于同一 region提交。若 payload copy或 barrier工作无法在 budget内完成，第一次 region只发布带 source/generation的 transfer reservation并锁定 buffer slot或 rendezvous waiter，随后在锁外完成可 safepoint的 copy，第二次只取得同一 source锁发布结果；reservation本身进入 typed GC root，close与其它 send/recv必须按其线性化顺序等待或越过，不能观察半初始化 payload。

该协议保持“一次 select只提交一个 case”和“ready时 default不能获选”：较少 case的无竞争机器路径没有新增 poll或 marker指令，任意规模路径则不再跨 case持锁。channel/Join wait queue仍保持 FIFO；持续 ready case在重复 select中具有非零选择概率，满足语言的随机弱公平，具体随机序列不是语言保证。

## 计时器与 I/O

每个 processor 的 `TimerWheel` 与 overflow heap 归该 processor owner 修改；跨 processor 新 timer 通过目标 processor 的 timer inbox 发布并唤醒 poller。全局维护所有 wheel/heap 最早 exact deadline 的原子近似值；过早值只导致多一次 wake，过晚值不允许。wheel 的 bucket、overflow transfer、cancel compact 和 timer service 都受 `timer_drain_budget` / `timer_compact_budget` 限制。
Linux 使用一个进程级 epoll fd、eventfd wakeup 和 timerfd；Windows 使用一个进程级 IO completion port 与 waitable timer。poller 线程/无工作的 worker 把完成事件转换为对应 wait generation 的 ready。regular file、无法异步化的系统调用和普通 `ForeignBridge` 不占 poller，走有界 `BlockingBridge` admission；`ForeignBridge[DirtyCpu]` 走 dirty CPU admission；`ForeignLeaf` 留在当前 worker 上直接执行。

poller每批返回事件时按合法 preferred processor和shard分组，每组至多128项并通过 `ready_publish` 发布；无合法target的组进入poller所在NUMA domain的injection，不存在global ready queue。batch内部以反向link保持OS提供的批内顺序在target local deque中恢复，多batch之间不得额外构造强于[并发规范](../spec/concurrency.md)的可观察先后。timer deadline相同则按timer ID递增构造同一publish batch，只用于确定实现输出。

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

正常`stack_check == stack_low`；抢占或GC请求时固定为`POLL_SENTINEL = isize::MAX as usize`。官方两目标的coroutine stack reservation都位于低半canonical user address，因而真实`stack_low`、`rsp`与不下溢的candidate都可表示为非负`isize`，严格小于sentinel。`flags` bit 0为`COLD_COMPACTED`，其余位必须为0。`StackDescriptor`的`size_of == align_of == 64`、`CoroutineHot`的`size_of == align_of == 64`以及`offset_of!(CoroutineSlot, stack) == 64`必须由构建时断言；前面的lifecycle state、run link和其它调度字段不能与descriptor共享该cache line。

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
2. 不释放 `LogicalProcessor`，不唤醒替代worker，也不发布runnable；
3. 不提供 C 回调 Gugu 的入口。普通 stack map、raw pointer、pin、内存别名、write barrier 和展开边界规则仍然有效；C 调用链不得越过该预算。

leaf 调用必须保持声明者承诺的短时、无不可界定等待和无 runtime 交互，并且 C 调用链不得超过声明的 stack budget。scheduler 不检查 C 函数体；违反承诺时至少会让当前 processor 长时间不可用，错误的 stack budget 还可能破坏 coroutine stack。

### `ForeignBridge`

普通 bridge把 processor关联视为可被 runtime打破的短期 lease；`ForeignBridge[DirtyCpu]` 从进入调用起就是 detached。两种模式共同的进入顺序为：

1. spill所有 managed/stack pointer，在 coroutine stack上物化 ABI bridge frame，写入 `ForeignBridgeState` 的 call stub、相对 offset和下一 generation，并建立 `ForeignBridge` map；
2. bridge保存 user context并切到当前 worker OS stack，此时 lifecycle仍为 `Running`；
3. 以 release发布 processor当前 TLAB top/limit和 write-barrier buffer cursor，处理已经发布的 GC stop。普通 bridge只发布 cursor，不清空 buffer、不放弃剩余 TLAB，也不强制下次分配 refill；
4. 普通bridge令`g = old_generation + 1`，把完整期望字`Foreign(g)`写入`foreign_bridge.lease_word`，再以Release CAS从`Running(old_generation)`转入不带`FOREIGN_DETACHED`的`Foreign(g)`。processor仍为`Bound`，`ownership.owner/current_coroutine`仍指向原worker/coroutine；worker TLS中的processor pointer在native期间只是一项待验证lease，未赢得返回CAS前不得据此访问processor。进入动作不唤醒替代worker，也不发布runnable；
5. DirtyCpu bridge在独立dirty admission mutex下尝试取得额度：成功时以新generation转入`Foreign|FOREIGN_DETACHED`并递增active；失败时转入`DirtyWaiting`、用`foreign_bridge.dirty_link`发布到`dirty_wait_queue`。两条dirty路径都立即释放processor并按需唤醒managed worker；active dirty work由dirty worker从`(CoroutineHot*, stack_high-relative frame_offset)`重建ABI frame并在system stack执行，`DirtyWaiting`的原worker立即返回scheduler。dirty mutex不保护任何runnable、remote inbox、injection或idle registry状态。

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
3. **batch enqueue慢路径**：没有idle processor时，native worker以自己的`ProducerHandle.pending_node`登记节点，Release CAS为`Runnable(g)|ENQUEUED|BATCH_PUBLISHING`，再按合法preferred target发布到remote inbox；target无效时发布到当前NUMA injection。只有该路径必须经普通dequeue的`Runnable -> Running`。

dirty worker完成调用后在dirty admission mutex下递减active；若target允许则从queue取一个record、把同一generation的`DirtyWaiting`转为`Foreign|FOREIGN_DETACHED`并直接转交额度，否则唤醒managed scheduler。完成的dirty调用不使用原processor lease，按detached processor/batch enqueue路径恢复。native work不提供runtime强制取消。

DirtyCpu admission不可在持有 `LogicalProcessor` 时等待。并行度降低只更新 target；`dirty_cpu_limit` 保持 `max(target, active)` 直至多余 work排空，增加目标则按 FIFO唤醒排队调用。

### 外部代码回调

只有普通 `ForeignBridge` 允许外部代码回调 Gugu。回调首先按 lease-retake协议打破 outer coroutine的 attached lease；若赢得原 processor即可用它建立 callback coroutine frame，否则按普通规则取得其它 processor。callback结束后 processor交还 scheduler，不把 lease重新附着到仍在 C中的 outer coroutine；outer native返回时走 detached路径。bridge查找/建立当前 OS thread的 worker登记，嵌套 callback以 worker-local depth区分；返回 C前必须完成 callback panic边界和临时 root清理。从 `ForeignLeaf`、`ForeignBridge[DirtyCpu]` 或 `DirtyWaiting` 回调 Gugu是违反对应 unsafe契约，不进入隐式桥接路径。

## GC 协作

Mosaic GC 将 scheduler 与 collector 的交接分成三类：

1. `TurnRegion` 的 owner-local reset/publish，只要求当前 coroutine 在 region descriptor
   上到达 typed checkpoint；
2. `LocalHeap` 的 root snapshot、minor stop、remark 和 direct evacuation，继续使用
   processor-local poll control；
3. `SharedHeap` 的 mark mailbox、handle access guard 和 credit termination，不能等待
   全局扫描所有 incoming field，只能等待对应 owner、guard、pin 和 message lease。

major/minor stop 的 coordinator 对全局 `gc_stop_epoch` 只执行一次递增；该值只分配
generation，不被 managed fast path读取。coordinator随后按 processor ID遍历 active
processors，对每个 processor执行固定发布协议：

1. release写入 `requested_gc_epoch = gc_stop_epoch`；
2. acquire读取 `current_coroutine`，若完整 lifecycle为 `Running`，release把其 `stack_check`
   写为 `POLL_SENTINEL`；
3. 以 `poll_flags.fetch_or(GC_STOP, Release)` 发布请求并唤醒 worker/poller；
4. 再次 acquire验证 ownership/current；发生切换时，新 owner在 `Runnable -> Running` 前
   必须先观察 `GC_STOP`，coordinator只需投毒仍绕过绑定边执行的 current coroutine。

因此一次 stop请求写入 `O(active_processors)` 个彼此独占的 `PollControl` line，而不是
触碰 `O(live_coroutines)` 个 lifecycle word；global epoch每个 cycle只有一次写入，也不
构成 worker共享读取热点。loop/显式 poll通过一次 processor-local load观察 bit，函数 entry
通过一次 coroutine-local load观察 sentinel；两条路径都在 slow edge才读取 requested epoch。
禁止给 lifecycle增加 `GC_STOP`/`PREEMPT` bit或在 fast path比较 global epoch。

- Running coroutine在下一个同步 safepoint保存 context、发布自己的 root slice 和
  `TurnRegion` registry，并确认；compiler保证 Running不包含不可分析的 opaque native frame；
- Runnable/Waiting coroutine已有稳定 context，取得 `STACK_SCAN_LOCKED` 后可直接扫描；
- Parking coroutine的 context可能尚未发布，所属 worker必须先完成 park双检成为 Waiting/Runnable，
  或撤销 park恢复 Running并在 safepoint停下；processor在此之前不能确认 stop；
- attached `Foreign(g)` 的 Gugu stack与 bridge frame已经稳定，collector不等待 native返回，
  而是立即竞争同一 generation的 lease、发布 processor-local状态，转为
  `Foreign|FOREIGN_DETACHED`并确认该 processor；detached Foreign/DirtyWaiting直接按保存 PC
  的 `ForeignBridge` map扫描 ABI frame。任何 native OS stack都不扫描，dirty/detached foreign
  worker不参与 stop确认；
- processor只在 write-barrier buffer、EdgeDelta staging、TLAB边界和当前 owner credit 状态
  发布完成，且 ownership不再被旧 worker使用后确认 stop；
- `SharedHeap` access guard、pin、未消费 `MarkTicket`、`RegionTransfer` 和 owner credit
  必须登记在相应 root/lease record 中，不能依赖 scheduler 误把 message empty 当成 GC 完成。

`MosaicBaseline` 在所有 active processor 确认后进入现有 stop 阶段；扫描/复制某 coroutine
时持有 stack scan lock，ready 可以设置 Runnable 意图，但在 lock 释放前不能让 worker执行它。
GC 完成 LocalHeap relocation、SharedHeap handle publication 和 metadata 发布后，以 release
增加 resume epoch，worker acquire 后恢复。

`MosaicConcurrent` 可以在 SharedHeap 只使用 per-owner root slice、handle forwarding 和
credit termination，跳过与 shared heap 大小相关的全局 pointer-update stop；仍必须在
必要的局部 safepoint、handshake、pin、guard、owner retire、queue grace 或 emergency
pressure 边界同步。它不改变 `safepoint_poll()`、foreign、pin 或 unsafe 语义。

空闲 processor/worker优先协助 owner-local mark、MarkMailbox drain、EdgeDelta 合并和
SharedHeap forwarding；scheduler 保留至少一个 worker 处理 poller、timer 和 runnable，不能
因 GC work 无限延迟用户调度。

## 动态并行度

公开facade按[运行时](../spec/runtime.md#gc栈与运行时控制-api)验证并线性化请求后，向scheduler发布`ApplyParallelism { old, new, epoch }`；scheduler不再次决定零值错误、setter返回值或公开状态。增加时按runnable demand从processor pool取得控制块、分配新的稳定ID并重建稠密active快照，按宿主topology归入NUMA domain；只在需要时创建/唤醒worker，不按new一次性预建线程。同时提高dirty target并按FIFO admission等待项，必需分配失败上报runtime fatal入口。

降低时把ID最大的多余processor以AcqRel标为`Retiring`，从新active快照移除并发布新的topology epoch；dirty target降为`new == 1 ? 1 : new - 1`，active dirty work不强杀，实际limit保持不低于active直到排空。epoch发布时仍为active且持旧epoch的`ProducerHandle`必须完成当前head CAS、flush指向旧target的staging，Acquire新快照并发布`topology_epoch_seen`后清active；当时inactive的producer下次开始batch必先读取新queue control word，新登记producer直接从当前epoch开始。retirement等待这组旧epoch active producer越过checkpoint后，才最终摘取8个remote head与carry，从而封闭“检查active后、publish前”发生的late enqueue。

Retiring processor若正被attached普通bridge保留，立即按精确generation retake而不等待native返回。完成当前managed coroutine或取得lease并完成producer grace后，按`run_next`、local deque、8个remote head/carry、injection carry、timer inbox、barrier buffer、mark work与pending poll flag的固定顺序转移：runnable按batch边界进入同NUMA injection，timer重新选择active target，其它processor-local状态按所属子系统handoff。确认所有queue ownership为空后才进入Idle并归还pool。旧`preferred_processor`只作为失效hint，后续ready自动走injection。processor ID不复用，控制块重新激活时取得新ID；managed worker绑定仍遵守第一个章节定义的共享CPU预算。
processor retire 同时必须交接 owner inbox、raw local/range cache、return staging 和 forwarding chain；这些状态不能遗留在已移除的 processor。具体 generation、handoff 和压力排空顺序见[内存所有权与消息通道](memory-messaging.md#owner-retire)。

## 终止

runtime状态机先根据[进程寿命](../spec/runtime.md#进程寿命)生成`TerminationPlan { mode, admit_user_coroutines, wait_foreign, report_epoch }`；scheduler只执行该plan，不决定`process.exit`、fatal、defer或报告语义。停止接纳后先阻止新producer登记，要求全部producer flush `pending_node/staging`并越过最新topology/slab epoch，再唤醒parked worker、关闭新poller注册并等runtime critical section到达安全边界；`wait_foreign`同时覆盖普通foreign、`DirtyWaiting`和正在执行的dirty work。

worker无runtime/foreign责任后转Stopping。主线程按poller、processor、GC、stack arena、coroutine cold slab、`CoroutineSlot` slab顺序关闭内部设施，再把plan结果交给宿主退出。Dead coroutine的stack已经在完成路径归还；最后一个Join/handle与runtime root释放后，hot/cold slot可以在仍映射的slab page内按新generation复用，整页解除映射必须满足GC内部规范定义的queue-page grace period。

## 不变量与验证

调度器调试构建持续检查：

- `pending_node`要么仍由原wait/foreign owner保活且尚未claim，要么已经是唯一`Runnable|ENQUEUED` ownership；claim失败必须清空。其余`ENQUEUED`与producer staging、remote/injection head、detached carry、`run_next`或local deque中的唯一runnable ownership一致；`BATCH_PUBLISHING`只与`Runnable|ENQUEUED`同时出现，所有chain无环且首节点`run_batch_len`为1..128；
- Running coroutine具有唯一worker和processor；attached Foreign具有唯一foreign worker和保留processor，detached Foreign具有唯一foreign/dirty worker但没有processor，DirtyWaiting二者都没有；
- `ForeignBridgeState.lease_word`的generation与coroutine完整state一致；`FOREIGN_DETACHED`只出现在Foreign，attached Foreign必须与`ProcessorOwnership.current_coroutine`双向一致，旧worker未赢得返回CAS时不能访问processor；
- `ForeignBridge[DirtyCpu]`的active数量不超过`dirty_cpu_limit`，每个DirtyWaiting bridge恰有一个独立`dirty_link`等待位置和合法high-relative ABI frame；
- local deque逻辑距离不超过256，Classic64 ticket不回绕；Packed55的distance只取0..256或`RESETTING`，reset只能发生在空队列且没有in-flight steal时；
- `CoroutineHot`与`StackDescriptor`各为64 byte，`CoroutineSlot`为128 byte；`PollControl`、`ProcessorOwnership`为64 byte，RemoteBatchHead、local head/tail和idle counter分别占128-byte区域，所有runtime/backend offset来自同一layout query；
- wait generation单调且winner/ready最多一次；`preferred_processor`指向Retiring时不能成为新publish target；每个producer停止或越过topology epoch前staging为空；
- processor retire/foreign retake不丢pending、staging、head、carry、run_next、local runnable、timer、TLAB、barrier、mark work或poll flag；runnable common path不取得idle/dirty/control mutex；
- stack bounds、`POLL_SENTINEL`、context PC和stack map匹配；每个live/cached stack slot只属于一个span与一个cache/global位置，arena内部没有逐栈protection run；
- GC scan lock下不能执行/复制同一coroutine；Running processor的poll-free机器路径cost不超过`POLL_BUDGET`，所有cyclic路径有同步poll/checked entry；每个`NoSafepointRegion`无machine backedge、blocking/slow edge且legalized cost不超过同一budget。

queue模型测试必须在容量2/4与缩窄counter下穷举：producer CAS失败后重写batch tail、consumer在publish前后exchange、`pending_node -> state CAS -> staging`每个中断点、同一stable slot多generation快速重入、detached遍历先保存next、8-shard round-robin、multi-consumer injection只有一个exchange winner、carry重新分发、empty-to-nonempty与park最终recheck、processor retire等待late producer、slab page grace。LocalDeque两个变体都覆盖空/满、owner与1/N thief的最后一项竞争、连续范围认领、overflow128、Packed55 reservation提交、`RESETTING`期间旧expected失败和强制高位reset；model不得以“counter现实中难以回绕”跳过缩窄验证。

其余确定性runtime测试使用可控VM backend、poller/clock、monitor generation note和调度gate，覆盖park/wake竞争、select loser、timer cancel、stack class选择、cache refill/flush上界、span改换class、空页decommit、arena映射次数不随slot增长、四窗收缩迟滞、Waiting冷压缩与唤醒增长、内存压力收缩、hot/cold slab与五类固定布局、poisoned`StackCheck`同时处理增长与抢占、monitor无deadline深睡且无周期wake、发布更早deadline不丢wake、无竞争长运行不触发周期抢占、竞争出现后的10 ms资格判断、counted-loop outer chunk、uncounted-loop countdown、普通bridge无压力原processor快返回、20 µs前后runnable retake、return/retake完整state CAS竞争、generation ABA拒绝、idle processor直接恢复、batch enqueue慢路径、GC/retirement/callback立即打破lease、DirtyCpu额度耗尽与释放、ForeignLeaf保留processor、未知间接调用回退bridge、opaque asm进入dirty、动态parallelism、GC epoch发布/确认与ready竞争。默认测试不能真实分配百万stack、依赖真实10 ms/20 µs延迟、真实OS timer resolution或随机victim恰好出现。

poll/select确定性测试还必须证明：stop请求不遍历或CAS非Running coroutine state，global epoch与lifecycle不出现在poll/prologue fast path；普通prologue只有一次`stack_check` load与一个共享taken branch，loop poll只有一次`poll_flags` load；GC与增长同时pending时`MorestackEntry`先停机再按最新bounds增长。select分别覆盖1–8 case展开路径、try-lock失败释放、超过8 case逐source路径、Building期winner、default CAS竞争、大payload两阶段reservation、loser逐source注销，以及verifier拒绝region内backedge/blocking/barrier refill。

完成路径测试还必须证明：`finish_coroutine`切到system stack后不再访问旧`rsp`，result publication先于stack摘根与`Dead`，保留Join handle不会保留stack slot，GC与完成竞争时旧slot只发布一次；hot/cold slot在page保持映射时可按新generation复用，整页解除映射必须等全部producer/consumer越过slab epoch。

queue性能门禁属于bench/手工profiling，不进入默认nextest。LocalDeque必须逐项比较Classic64与Packed55的owner push/pop、owner+1/N thief、1/8/32/128项steal和overflow；BatchInbox必须覆盖1->1 singleton、all->1、all->all、poller completion burst、local overflow以及shard数1/4/8/16。端到端覆盖empty spawn/join、yield、channel ping-pong、cross-worker wake、injection burst和park/unpark。统一记录cycles/task、instructions/task、CAS attempt/failure、locked/SeqCst指令、L1/LLC miss、`perf c2c` line bouncing、wake p50/p99、service rounds、processor scaling、slab bytes/live coroutine和RSS。target档案只有在真实workload不回归、模型全部通过且结果可复现时才能从Classic64切到Packed55或改变8 shards/128-byte padding；一次实验只改变一个变量。

poll policy性能门禁至少比较`POLL_BUDGET` 1024/4096/16384在空整数counted loop、可向量化整数内存扫描、uncounted cyclic CFG、无frame调用链、allocation fast path、递归SCC和1/8/64-case select上的instructions/iteration、poll-word loads、branch misses与吞吐；机器码检查必须证明普通prologue只有一次`[r14 + stack_check]` load、budget poll只有一次`[r15 + poll_flags]` load、两者都不读取global epoch/lifecycle，counted inner loop没有独立poll countdown或poll-word load，且`NoSafepointBegin/End`编码为零字节。另在可控GC请求下记录request到processor ack的p50/p99/max cost units与wall-clock。修改默认4096、inline select门槛、vector/unroll cost model或opcode weight必须同时证明hot-path回归与stop-latency收益。

foreign bridge性能门禁以真实C ABI空stub、约100 ns/1 µs/10 µs CPU工作、受控阻塞、同步callback和GC重叠为workload，在单processor空闲、单processor有runnable压力和多processor饱和场景测量ns/call、原processor fast-return率、retake率、idle direct-resume率、batch enqueue、dirty admission mutex、BatchInbox CAS、OS thread wakeup、TLAB refill、cache miss与runnable/GC p99延迟。必须逐项比较立即handoff基线、20 µs grace和候选grace；不能只优化空stub而让阻塞调用、callback或GC stop失去进度保证。

stack allocator与冷压缩性能门禁以浅entry、递归增长、channel/Join长期等待、频繁park/wake和“深四轮、浅四轮”负载分别测10万与100万coroutine的完整bytes/live-coroutine、`stack_live/reserved/committed`、control slab、RSS/commit charge、Linux VMA或Windows reservation数量、本地cache命中、global class lock竞争、page fault/decommit、stack copy bytes和create/park/wake p50/p99。必须与8 KiB逐栈mapping基线比较；默认2 KiB、64 KiB processor cache、四窗迟滞或512 B冷class的调整必须同时证明内存收益与create/wake/深浅振荡吞吐。

monitor性能门禁必须在完全空闲、单个无竞争CPU coroutine、runnable饱和、timer密集、短bridge压力和GC stop场景记录10分钟内的monitor wake、CPU time、context switch、抢占/retake/GC p99以及Windows timer-resolution lease持有时间。空闲和无竞争CPU场景除显式maintenance deadline外必须保持零周期wake；短deadline收益不能以常驻1 ms timer resolution、busy-spin或更差的runnable/GC进度换取。

## 参考实现资料

- [Go runtime scheduler](https://go.dev/src/runtime/proc.go)
- [Linux lockless `llist`](https://github.com/torvalds/linux/blob/master/include/linux/llist.h)
- [Tokio fixed local queue](https://github.com/tokio-rs/tokio/blob/master/tokio/src/runtime/scheduler/multi_thread/queue.rs)
- [Tokio injection queue](https://github.com/tokio-rs/tokio/blob/master/tokio/src/runtime/scheduler/inject/shared.rs)
- [Chase–Lev dynamic circular work-stealing deque](https://doi.org/10.1145/1073970.1073974)
- [Lê et al.：弱内存模型下正确且高效的 work stealing](https://research.manchester.ac.uk/en/publications/correct-and-efficient-work-stealing-for-weak-memory-models)
- [Crossbeam deque](https://github.com/crossbeam-rs/crossbeam/blob/master/crossbeam-deque/src/deque.rs)
- [Tokio #5041：短 packed counter 的 ABA](https://github.com/tokio-rs/tokio/issues/5041)
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
