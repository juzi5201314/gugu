# 内存所有权与消息通道

本章规定 Gugu runtime 如何吸收 snmalloc 的本地性、owner-directed remote return、批量消息、slab、size class、范围管理和安全加固思想。本章是 compiler/runtime 的内部实现契约，不改变语言层的值传递、对象移动、resource lease 或用户可观察的回收语义。

本章中的 `owner` 指持有某个 runtime 内存单元本地回收权的 `MemoryOwner`，不是操作系统线程。协程可以迁移，worker 可以退出，owner 也可以在 topology 变更时转移；这些变化由本章定义的 token、generation 和 grace 协议处理。

## 权威范围与设计目标

### 权威关系

- [内存与对象模型](../spec/memory.md)继续规定 managed 值、移动、pin、root、resource release 和用户级存储语义。
- [运行时与运维语义](../spec/runtime.md)继续规定启动、OOM、软内存上限和公开统计。
- [调度器](scheduler.md)继续规定 runnable、park、wake、work stealing 和 scheduler control block。
- [GC 元数据](gc-metadata.md)继续规定 Mosaic GC 的三层 managed storage、Immix metadata、trace descriptor、barrier、handle representation 和 collector lease。
- 本章独占 runtime 内存返回通道、managed GC message、raw slab/span 的 owner、批量路由、范围提供者、debt 账本和相关 verifier 的内部契约。

snmalloc 是算法来源，不是 Gugu runtime 的链接依赖。Gugu runtime 仍按闭世界模型用 Gugu 编写；只有 rt0、平台系统调用、栈切换、原子和其他明确的 intrinsic 可以落到机器实现。

### 目标

1. owner 本地的分配、回收和缓存路径不访问跨 processor 的共享可写状态。
2. 跨 owner 的释放、范围返还、GC mark 和 shared graph metadata 只发布稳定消息，由 owner 在自己的上下文中处理。
3. 批量消息只用于回收、范围搬运和后台 GC 维护；channel、select、park、wake、join 和同步语义不依赖延迟可见的 memory queue。
4. managed plane 使用 `TurnRegion`、owner-local Immix `LocalHeap` 和 stable-handle `SharedHeap`；消息不得携带 managed object 裸地址。
5. 常规路径的共享状态量保持 `O(P)`，不建立 producer 到每个 owner 的 `O(P²)` 队列矩阵。
6. 延迟回收的 bytes、region transfer、mark ticket、edge delta、handle forwarding、缓存和转发消息都计入内存压力，不能以“已经发送释放请求”代替实际可复用或已经归还 OS。
7. 所有 raw link、owner token、object handle、generation、cycle credit 和 lease 都能被 verifier 检查；错误进入 `RuntimeInvariant`/`Fatal` 分类，不静默跳过损坏消息。

### 非目标

- 不把 snmalloc 的 C++ 实现、`snmalloc-rs` 或其构建系统嵌入生成的 Gugu 程序。
- 不用 raw slab free list 替换 `LocalHeap` 的 Immix object-start bitmap、mark bitmap、card table 和 forwarding metadata。
- 不向用户暴露 `free`、地址稳定承诺、processor affinity 承诺、对象 owner 或回收时序承诺。
- 不在 LocalHeap direct-pointer 热路径引入全局 `SeqCst` fence、全局共享 free list、每对象引用计数或通用 EBR。
- 不把 ordinary channel 改成 ownership transfer，也不把 arbitrary closure 交给另一个 worker 执行 flat combining。
- 不把 managed object 级别的 stable handle、region、lease 或 compressed reference 暴露成用户必须书写的 ownership 语法。
 
## 两个物理平面

Gugu 的内存实现分成两个物理平面，共享 owner、range 和消息传输的底层规则，但不共享对象寿命语义。

### Managed plane

`Managed plane` 保存语言值和所有标准库容器的 managed storage，由 Mosaic GC 按对象图可见性选择三种内部层次：

- `TurnRegion`：当前 coroutine turn 私有、由 `EscapeAndPlacement` 证明无外部 alias 的对象图，owner-local bump 分配，turn 结束时整区 reset；
- `LocalHeap`：仍使用 Immix arena、TLAB、精确 trace、hybrid barrier 和 direct pointer 的本地 managed heap；
- `SharedHeap`：跨 owner、共享身份或分析无法证明私有性的对象，使用 stable handle、owner-local mark mailbox 和 handle forwarding。

region escape、owner 转移、GC mark、edge summary、handle forwarding 和 block return 均通过本章的稳定 descriptor/generation/message 规则交接；消息不得携带 managed object 裸地址。对象死亡不会产生用户可见的 free 事件；collector 只在完成相应 lease、forwarding、access guard 和 queue grace 后产生 block/span return。
 
### Runtime raw plane

`Runtime raw plane` 保存地址稳定且不由普通 GC 移动的内部记录：

- `CoroutineSlot`、`CoroutineCold`、wait/select node、producer staging record；
- stack arena 的 stack slot、stack span descriptor 和 guard metadata；
- `ResourceCell` 以及不含 managed pointer 的 release record；
- owner directory、range descriptor、message node、free-list metadata；
- 需要明确生命周期的 runtime buffer 和平台范围记录。

raw plane 的记录可以使用 intrusive link，但 link 只存在于 non-moving storage。raw plane 不会自动成为 GC root；其中若包含 managed handle，必须通过显式 descriptor/root slot 注册。

## Owner、domain 与稳定身份

### MemoryDomain

每个物理内存单元属于一个 `MemoryDomain`。domain 是策略和账本的归属，不等同于 NUMA 节点：

| domain | 主要内容 | 寿命策略 |
|---|---|---|
| `ManagedTurn` | coroutine turn 私有 TurnRegion 与 export/transfer descriptor | owner-local reset，或 promote/transfer 后由接收 owner 管理 |
| `ManagedLocal` | LocalHeap 的 Immix arena、block、line、TLAB 和 large managed mapping | owner-local tracing、evacuation、sweep 和 block return |
| `ManagedShared` | SharedHeap payload、stable handle table、forwarding grace 和 edge summary | owner-local mark mailbox、handle forwarding、grace 后 return |
| `RuntimeRaw` | raw slab、stack span、runtime record | owner return，地址稳定 |
| `Resource` | `ResourceCell` 与其稳定 descriptor | lease/release 后回 raw pool |
| `PlatformRange` | virtual range、commit/decommit 和 guard page | 平台 adapter 管理 |
| `Foreign` | 外部系统或 FFI 所有的缓冲区 | foreign owner 或 bridge 规则管理 |

`MemoryDomain` 用编译期固定的 dense domain id 表示。domain id 是路由和统计键，不作为用户数据，也不通过字符串查找。

### OwnerRecord 与 OwnerToken

`OwnerRecord` 保存在 non-moving owner directory 中。以下是逻辑字段；实际机器布局由 backend 断言固定：

| 字段 | 约束 |
|---|---|
| `domain_id` | 指向 `MemoryDomain`，创建后不改变 |
| `owner_id` | 使用 `LogicalProcessorId` 或 domain owner id；不复用旧值 |
| `generation` | 每次 owner 接管、转移或重建时递增 |
| `route_key` | 生命周期内稳定的 64-bit 路由键，不从可移动地址推导 |
| `state` | `Active`、`Draining`、`Forwarding`、`Retired` |
| `forward_target` | `Forwarding` 时指向新的 `(owner_id, generation)` |
| `topology_epoch` | 记录当前 active processor snapshot |
| `inbox` | owner-only consumer 状态和 producer-visible tail |
| `accounting` | pending、cache、reclaimable 和 committed 计数 |

消息携带的 `OwnerToken` 至少包含 `(domain_id, owner_id, generation, route_key)`。只有 token 与 directory 当前记录同时匹配时，消息才能直接进入目标 owner 的 free list。旧 generation 只能沿已发布的 `forward_target` 转发，不能直接写入新 owner 的状态。

owner 的物理执行者可以改变，但 owner record 的稳定地址和 token 规则不能改变。owner retire 的完整协议见[Owner retire](#owner-retire)。

### 归属粒度

- raw slab/span 是默认的细粒度 owner 单元；不向 `CoroutineHot` 增加 owner 字段，避免破坏现有 128-byte hot layout。
- `StackDescriptor`、`ResourceCell` 和 `CoroutineCold` 的归属通过其 slab/span descriptor 查得，或由现有稳定 descriptor 直接提供。
- managed block 的 allocation owner 与 collector owner 分离；block descriptor 记录两者的 lease 和 generation。
- 一个 owner 可以持有多个 class 的 local cache，但一个 slot、span 或 block 在同一时刻只有一个回收 owner。

## Raw slab 与本地分配

### SlabDescriptor

每个 raw slab/span 有一个稳定的 `SlabDescriptor`，包含：

- `MemoryDomain`、`RuntimeSizeClass`、slot stride、alignment 和 span extent；
- 当前 owner token、slab generation 和状态；
- live slot count、queued slot count、free slot count 和 committed bytes；
- free-list head、pending return 统计和 poison/integrity secret 的索引；
- 与平台 range、guard page 和 queue-page grace 的关联。

descriptor 不放在用户 payload 中。descriptor 页面属于 non-moving metadata range，不能在 slot 回收时被当作普通对象覆盖。

### RuntimeSizeClass

`RuntimeSizeClass` 是按 dense id 索引的编译期表，而不是 `HashMap`。每项至少包含：

- `payload_bytes`、`slot_stride`、`alignment`；
- 每个 span 的 slot 数和 metadata bytes；
- 是否允许把 slot 本身用作 message link；
- drop/clear 策略、poison 策略和所属 adapter。

stack 的既有二次幂 size class、ResourceCell 的 class 和 control record 的 class 可以共享 class arithmetic，但不能因为尺寸相同就把不同 drop/trace 不变量的记录放进同一 free list。只有 layout、drop、scan 和 owner 规则完全相同的记录才允许物理复用。

### Metadata lookup 与 variable-sized slab

raw range 使用自然对齐的二次幂 extent，并以 dense chunk index 查找稳定 `SlabDescriptor`；连续 chunk 可以共享一个 descriptor。stack arena 的固定范围优先用 class/extent mask，动态 raw range 使用不含 `HashMap` 的分层 range map。metadata range 与 payload range 分离，已用于 payload 的 range 不能在同一生命周期中改作 owner metadata。

`RuntimeSizeClass` 可以包含非二次幂 slot stride。class 建立时生成 reciprocal division/mask 常量，并用穷举边界验证常量与精确除余等价；运行时不对不可信整数执行任意 modulus。owner 已知 descriptor/class 时，local pop 和 return 不重复做地址推导；ForeignBridge 的 checked copy 只有在需要从任意 raw pointer 解析范围时才查询 map。

### Local fast path

owner 的本地分配顺序固定为：

1. 从当前 class 的 owner-local free list pop 一个已验证 slot。
2. free list 为空时，从 owner-local span 的 bump cursor 取得 slot。
3. 当前 span 用尽时，从 owner domain 的 local range cache 取得一个新 span。
4. domain cache 用尽时，发布 typed range request，由 domain owner 或 platform range 冷路径补充。

第 1、2 步不执行原子 RMW，不进入全局锁，不调用平台。第 3 步可以访问 owner-local metadata；第 4 步才允许进入 slow path，并按 GIR/LIR 的 safepoint 分类执行。

### Local return 与 remote return

记录完成生命周期后：

- 当前执行者仍是 slab owner，且 slot 已完成唯一的 dead/returned 状态转移时，直接 push 到 owner-local free list。
- 当前执行者不是 owner 时，先完成 exactly-once 的 `ReturnQueued` 状态转移，再创建或追加 remote return message。
- `ResourceCell` 的多方 release 仍由 lease 计数和 close 状态决定唯一 release 线性化点；只有赢得 release 的执行者能发布回收消息。
- stack slot、control record 和 wait node 的完成路径不能把“当前 worker”误当作 slab owner；必须从稳定 descriptor 解析 token。

记录 stride 至少能容纳 intrusive link 时，message node 可以复用已死亡 slot 的前置区域。小于 link 所需尺寸的记录使用专用 non-moving `ReturnNode` pool，不能为每次 remote return 调用普通 allocator。

## Remote return message

### 逻辑记录

`ReturnMessage` 的逻辑字段如下：

| 字段 | 作用 |
|---|---|
| `next` | raw intrusive link，只存在 non-moving message/slab storage |
| `target` | `OwnerToken`，包含 generation 和 route key |
| `kind` | `RawSlot`、`StackSpan`、`ResourceRelease`、`HeapBlock`、`HeapLineRun`、`Extent` |
| `descriptor` | stable slab/block/range descriptor id |
| `unit` | slot index、block index 或 line-run index |
| `bytes` | 本次待处理的物理 bytes，不能为负 |
| `source_epoch` | producer topology/stop epoch |
| `state` | `Staged`、`Published`、`Forwarded`、`Consumed` |
| `integrity` | generation、class、owner 和 link 校验信息 |

消息不得包含普通 managed pointer。若回收动作必须关联 managed 对象，消息只能携带已登记的 stable handle 或 descriptor index；collector 在自己的 root/forwarding 规则内解析它。

### Producer staging

每个 producer 使用固定数量的 staging slot 或一个当前目标链，不为每个 owner 预分配队列。staging record 在进入 runtime queue primitive 前后都必须遵守 scheduler 已有的 producer gate 和 stop epoch 登记。

默认 direct mode 使用四项 temporal target cache：

- cache 只缓存最近的 `(target, shard, partial batch)`，不是所有权真相源；
- cache miss 时刷新旧 target，再切换到新 target；
- cache 的容量是实现参数，不改变消息顺序或正确性；
- producer 数量与 owner 数量增长时仍不产生 `P × P` 的长期队列状态。

### Consumer-side same-slab batching（BatchIt）

发送方按 target 聚合不能解决 producer-consumer 场景中的另一类局部性问题：一个 consumer 可能连续收到来自许多 producer、但实际属于同一 source slab 的 raw slot。Gugu 在 consumer/finisher 与 `OwnerInbox` 之间加入 `ReturnSlabCache`：

- key 是 stable `SlabDescriptor` 与 generation，不是 managed 地址；
- 默认使用 8 个 set、每 set 2 个 way 的固定关联 ring；这只缓存最近的 source slab，不建立 owner 数量大小的 hash table；
- cache hit 把 slot 追加到该 slab 的本地 `freelist::Builder`，cache miss 先关闭 victim ring，再打开新 slab ring；victim 优先选择当前积累量最大的 ring；
- ring 关闭后把同一 slab 的 slots 转成一个 `ReturnMessage` batch，再按 owner token 进入 direct 或 radix staging；
- stride 不足以容纳 link 的 slot 使用专用 ReturnNode，不能为每个命中分配临时对象；
- maintenance、owner retire、GC handoff、pressure drain 和 producer/consumer deregister 必须关闭全部 open ring；
- encoded link、generation、class 和 exactly-once 校验在 ring close 与 owner consume 两处都保留。

该 cache 只适用于 `RuntimeRaw`、stack span、ResourceCell slot 和其它地址稳定的回收单元。managed value 的 consumer 不因 BatchIt 获得对象级 free 路径；managed plane 仍按 block/span return 处理。`ReturnSlabCache` 的 pending bytes 计入 sender/consumer 的互斥账本，不能以 cache hit 作为已经回收的依据。

消息路径因此分成两种可测模式：同 owner 的 slot 直接 local return；跨 owner 的 slot 先经过 source-slab BatchIt，再经过 owner-directed batch queue。BatchIt 的收益只在 source slab temporal locality 存在时成立，cache miss、ring close、RSS 和尾延迟必须进入 benchmark。

### 批量触发

批量在以下任一条件满足时发布：

1. item 数达到现有 `BATCH_MAX = 128`；
2. bytes 达到当前 tuning profile 的 soft batch limit；
3. target、shard、domain 或 route mode 发生改变；
4. producer 即将进入 park、ForeignBridge、DirtyCpu 或 stop gate；
5. owner retire、GC handoff 或 memory pressure 要求排空 staging；
6. maintenance service 到期。

byte limit 是性能参数，不写成语言语义。实现必须同时保存 item limit 和 byte limit，避免大量小记录或单个大 extent 使批次失控。历史 snmalloc 论文中的约 1 MiB 只作为待测候选，不是 Gugu 默认承诺。

普通 ready/wake 消息不能因为 batch limit 延迟。memory return 可以延迟，但必须受 `pending_return_bytes` 和 pressure policy 约束。

### OwnerInbox 算法

`OwnerInbox` 是多 producer、单 owner consumer 的 append-only batch queue。一个 producer 发布完整 chain 的顺序为：

1. 先把 chain 的最后一个 `next` 清为 null，并以 Release 发布 chain 内容。
2. 对目标 tail 执行一次 AcqRel exchange，把自己的 last node 设为新的 tail。
3. 若 exchange 返回旧 tail，则以 Release 把旧 tail 的 `next` 链到 chain first。
4. 若旧 tail 表示空队列，则发布 front；producer 不再改动 owner-only front。

consumer 只从自己的 front 开始，以 Acquire 读取 next 并处理已经可见的 chain。producer 正在完成第 3 步时，consumer 可以暂时看到 null；这代表“稍后可见”，不是空队列，也不是错误。consumer 每次 service 有 item/byte 上限，处理后只更新自己的 front。

这个算法的性质是：

- 每个 batch 通常只需要一次共享 tail RMW；
- 同一 producer 的 chain 顺序保持；不同 producer 之间不提供全局顺序；
- 队列不是线性化 queue，不能用于同步或可观察事件排序；
- front、tail、producer staging、统计计数分离到独立 cache line，沿用 Gugu 的 64/128-byte padding 规则；
- message node 在 consumer 完成并经过 queue-page grace 前不能复用。

Gugu scheduler 的 ready/injection batch inbox 可以复用 link、producer registration、generation 和 grace substrate，但 ready 语义与 `OwnerInbox` 语义必须在类型和 verifier 中区分。

### Consumer drain

owner service 的顺序固定为：

1. 取得 bounded chain snapshot；
2. 对每个 message 校验 link、target generation、descriptor、class、unit state 和 bytes；
3. `Forwarding` token 沿 directory 转发，不能直接改写旧 owner 的 free list；
4. 当前 owner 的 raw slot 进入正确 class free list，span/extent 进入 coalescing state，GC unit 进入 `OwnedFree`；
5. 更新 pending/cache/reclaimable 账本；
6. 只有所有 message 都离开 queue 语义后，才允许释放或复用 message storage。

在正常 service 中，owner 每轮最多处理 profile 规定的 bytes 和 items；pressure service 可以提高预算，但仍必须受 `POLL_BUDGET` 和 scheduler service budget 约束。检测到不可解释的 generation、class 或范围不匹配时，进入 runtime invariant failure，不把消息丢掉当作恢复。

## Temporal radix fan-out

### 适用条件

direct mode 是默认模式，因为 Gugu 已有每 owner 的 8-shard batch inbox。只有 profile 证明以下现象成立时才启用 radix mode：

- producer 经常在大量不同 owner 之间切换；
- target cache miss 和 batch flush 明显高于实际回收工作；
- direct target 的 tail exchange/cache-line bouncing 成为可测热点；
- owner 数和 topology 规模足以抵消额外转发 hop。

radix mode 不是为低 processor 数或低 fan-out workload 永久分配 64 个 bucket。

### 路由键与 bucket

每个 active owner 注册时取得稳定 `route_key`。route key 可以来自 runtime entropy；没有 entropy adapter 时使用启动时生成的闭世界 seed。它不能来自可移动对象地址，也不能直接使用会复用的 processor slot index。

radix staging 使用固定 `2^k` bucket；高 fan-out profile 默认 `k = 6`，但 bucket 数和 levels 仍由 target workload 的 benchmark 选择。每个 bucket 只保存 batch chain，不保存 owner 的长期状态。路由目录最多使用 `MAX_RADIX_LEVELS` 个固定层级；超过目录容量时退回 domain injection，不允许无限转发。

每一跳只做以下工作：

1. 根据 route key 当前层的 bits 选择目标 bucket；
2. 把完整 batch 发布给下一级 owner/domain inbox；
3. 不解引用 managed pointer，不执行 drop，不获得 collector lease；
4. 保留原始 target token、source epoch、bytes 和 integrity。

转发 hop 必须有固定上限并计入 `remote_return_hops`。route key、directory generation 或 topology epoch 改变时，旧目录记录进入 `Forwarding`，旧消息沿旧记录排空后再释放。

### Direct 与 radix 的切换

切换只在 maintenance epoch 发生：

- direct → radix：先冻结当前 target cache，刷新所有 partial batch，再发布新 mode；
- radix → direct：先排空 bucket 和 forwarding chain，再恢复四项 target cache；
- 两种 mode 不同时消费同一个 queue head；
- mode 切换期间的 pending bytes 继续计入压力账本。

mode 是 runtime tuning profile，不是用户可观察行为。任何 profile 都必须保留 domain injection 作为 owner 不可达、owner retire 或 pressure emergency 的终点。

## Range、span 与大对象

### Range 层级

Gugu 采用以下范围层级，替代常规路径上的共享全局 free list：

1. `ProcessorRangeCache`：每个 logical processor 持有少量已 commit 的 span/extent。
2. `DomainRange`：NUMA/domain owner 持有可分割和可合并的范围。
3. `GlobalRange`：只处理 processor/domain cache miss、topology 变化和启动/退出。
4. `PlatformRange`：封装 reserve、commit、decommit、release、guard page 和低内存通知。

每一层只把整批 span/extent 交给下一层。普通小对象分配不会逐对象调用 platform adapter。

### Gugu 固有布局

- managed heap 继续使用 2 MiB arena、64 个 32 KiB block、每 block 256 条 128-byte line 和 16-byte granule。
- 每个 logical processor 的 256 KiB/8-block nursery span 继续由 TLAB 和 Immix line table 管理。
- coroutine stack 继续使用 256 MiB stack arena、二次幂 size class、per-processor cache、guard 和四窗收缩策略。
- raw control/resource slab 可以使用同样的对齐和范围层级，但不共享 managed object header 或 mark bitmap。

### Extent 与 coalescing

大 stack span、large raw record 和独立 mapping 使用稳定 `ExtentDescriptor`：

- descriptor 记录 base、length、alignment、domain、owner token、generation 和 committed state；
- 空闲 extent 按 dense extent index 进入 owner/domain 的有序结构或 bitmap；
- 相邻 extent 的 coalescing 只在 owner/domain consumer 上执行；
- remote return 先发送 extent descriptor，不让多个 producer 同时改 buddy tree；
- 只有在单 owner reducer 无法满足吞吐时，才评估带 generation 的 `AtomicU64` index stack；128-bit CAS 不是必需前提。

这种 owner reducer 优先的设计避免 Treiber stack 的 ABA 和跨平台 16-byte atomic 依赖，也保持大块回收与小对象回收的消息模型一致。

### Commit、decommit 与 trim

- virtual range 可以预留但不立即 commit；commit 以连续页批量执行。
- 小对象每次释放不触发 decommit；decommit 只在 span/extent 长期空闲、memory pressure 或 owner/domain trim 时发生。
- stack guard page 和 metadata guard page 永不作为普通 payload 返回。
- decommit 必须在所有 allocator、scanner、forwarder lease 和 queue-page grace 完成后执行。
- platform failure 映射为 `OutOfMemory`、`ResourceExhausted` 或 `RuntimeInvariant`；不能把 commit 失败当作空闲 range。
- 2 MiB huge-page hint 可以作为平台 profile，但不能成为正确性或固定延迟保证。

### 初始化、清零、重分配与诊断映射

range、owner directory 和 radix leaf 采用 lazy initialization；启动阶段只建立 active processor 所需的 descriptor，不预触碰整个 stack arena、稀疏 map 或所有 raw slab。owner record、operation record 和 ReturnNode 使用稳定 pool，pool refill 走 GlobalRange 的 typed cold operation。

raw allocation 的 alignment 不通过隐藏前置指针实现：alignment 不超过 class alignment 时进入对应 slot class，较大 alignment 直接取得对齐 extent，并把完整 extent 写入 descriptor。raw buffer 在同一 class 且 capacity 足够时可以原地 resize；跨 class 或需要改变 alignment 时执行 checked allocate-copy-return，旧 slot 仍按 owner message 回收。

commit 得到的匿名页按平台规则视为已清零；复用 raw slot 时由 class 的 `clear_mask` 指定必须清除的 pointer、length、secret 和 resource 状态字段。若 type initializer 会覆盖全部 payload，则不为无意义的全 slot 清零付费，但在 publish 前必须清除任何可能被 verifier 或 GC 读取的字段。

metadata、owner secret、queue link 和稀疏 radix map 可以通过 `PlatformRange` 的 dump policy 标记为不进入 core dump；诊断所需的 generation、class 和统计快照另行记录。dump policy 是安全/平台 profile，不改变 range 的生命周期，也不能替代 guard page。

## Managed GC 与 block/span return

### 三层 managed storage

managed plane 的对象根据 `EscapeAndPlacement` 进入 `TurnRegion`、`LocalHeap` 或
`SharedHeap`。三者共享 exact trace、generation、pressure 和 message provenance，拥有
不同的回收粒度：

- `TurnRegion` 是当前 coroutine turn 私有的 owner-local bump region。经过 export summary
  验证后可以整区 reset；它不能包含必须独立 release 的 ResourceCell，也不能留下外部
  alias。未消费的 `RegionTransfer` 使 region 处于 pending，不能 reset 或复用。
- `LocalHeap` 继续使用 2 MiB arena、32 KiB block、Immix line、TLAB、object-start bitmap、
  mark bitmap、card table 和 direct managed pointer。owner 在本地执行 mark、sweep 和
  没有 foreign incoming edge 的 evacuation。
- `SharedHeap` 保存跨 owner、共享身份或无法证明私有性的对象。shared field 使用 stable
  handle 或受保护的 compressed reference；handle forwarding 只切换 stable slot 的
  current payload，不直接改写另一个 owner 的任意 field。

### GC 工作消息

managed GC 消息与 raw return 共用 owner token、generation、epoch、batch、integrity 和
queue-page grace，但消息类型严格分离：

| 消息 | 作用 | 允许的身份字段 |
|---|---|---|
| `MarkTicket` | 跨 owner 发送可达 mark seed | cycle、target owner、stable object/block identity、credit |
| `EdgeDelta` | 批量发布跨 block edge 增删 | source/target block、generation、count、epoch |
| `CardMarkBatch` | 把 processor-local remembered-set card 标记交给 arena owner | arena descriptor、generation、card index/range、cycle epoch、bytes |
| `RegionTransfer` | 转移私有 region descriptor | region、generation、type summary、bytes、export state |
| `HandleForward` | 通知/审计 stable handle forwarding | handle slot、generation、old/new descriptor、grace |
| `HeapBlockReturn` | 返还完整空 block | arena/block descriptor、owner、generation、bytes |
| `HeapLineRunReturn` | 返还稳定 line-run | block/line descriptor、owner、generation、bytes |
| `HeapArenaReturn` | 返还完整空 arena | arena descriptor、owner、generation、bytes |
| `LargeMappingReturn` | 返还独立 mapping | extent descriptor、owner、generation、bytes |

`CardMarkBatch` 只表达 processor-local buffer 已经聚合的 remembered-set 键；card table 由 arena allocation owner 单写。它与 `MarkTicket`/`EdgeDelta` 共用 cycle credit、owner generation 和 queue-page grace，但不承担对象 mark 或同步语义。

所有消息禁止携带未 pin 的 managed object 地址、可能在 evacuation 中更新的 field 地址、
只在当前 stack frame 有效的 interior pointer、未登记 handle 或把 managed payload 重解释
为 free-list node。`RegionTransfer` 不是用户可观察的 move；若 sender 仍需依照 `f(x)`
语义继续使用值，compiler 必须先 promote 或复制，不能发布 transfer。

### MarkMailbox 与 owner credit

每个 active owner 维护 MPSC、单 owner consumer 的 `MarkMailbox`。producer 按 target、
cycle 和 generation 聚合 `MarkTicket`，一个 batch 只做一次共享发布；consumer 在 owner
上下文中验证 ticket，并把目标放入本地 mark worklist。不同 producer 没有全局顺序；同一
producer 的 batch 顺序保持。mailbox 只用于 GC 后台工作，不能用于 channel、wake、join
或其它同步语义。

coordinator 为每个 cycle 建立可追踪 owner credit。root seed、barrier、MarkTicket、
EdgeDelta 和 RegionTransfer 的未完成处理都占用 credit；owner 只有在本地 worklist、
mailbox、edge staging、barrier buffer 和转发链清空后归还 credit。coordinator 收齐所有
owner credit 并确认 producer/topology epoch 后，才能执行 remark 和终止检测；不能用单个
mailbox 为空推断全局 mark 完成。

### Block lease 与 candidate

`EdgeDelta` 由 source owner 在 card/line summary 中聚合后发送给 target owner。target
block 的 incoming lease 归零只产生 collection candidate，不能直接回收对象或 block。owner
必须继续完成 pending delta 验证、block 内 exact trace、内部 cycle/SCC 检测、pin/resource
检查和 scanner/allocator/evacuation lease 检查。不得引入普通对象级 reference counting。

### Return unit 与状态迁移

managed plane 的 return unit 按风险从低到高排列：

1. `HeapBlockReturn`：完整 32 KiB block，无 live object、pin、resource、incoming edge、
   scanner 或 allocator lease；
2. `HeapLineRunReturn`：同一 block 内连续 free line-run，line metadata、object-start
   bitmap、edge summary 和 scanner lease 已稳定；
3. `HeapArenaReturn`：64 个 block 全部为空，且 arena 没有 pin、resource、forwarding、
   handle access 或 page-covering 引用；
4. `LargeMappingReturn`：独立 mapping 完成 root/field 或 handle 更新并清除 forwarding。

允许的消息相关状态迁移为：

```text
TurnRegion: Private → Publishing → LocalPromote
TurnRegion: Private → Publishing → RegionTransfer → Received
TurnRegion: Private → ResetPending → Reset
LocalHeap:  Allocating → Sweeping → ReturnPending → OwnedFree → Allocating
LocalHeap:  Evacuating → ForwardingComplete → ReturnPending → OwnedFree
SharedHeap: Forwarding → Grace → Reclaimable → OwnedFree
Free       → Allocating
```

`ReturnPending` 只能由持有 sweep/evacuation lease 的 collector 发布。发布前必须完成
TLAB cursor/limit flush、mark/remark、barrier 和 forwarding，确认没有 scanner、allocator、
root snapshot、handle access、pin 或 pending message lease，写入 block generation、owner
token、live/free bytes，并以 Release 发布 descriptor。owner consumer 以 Acquire 验证
generation、state、lease、integrity 和 pending message，随后才放入 owner/domain free
structure；stale token 只能转发或进入 retired-domain 路径。

resource arena 不能整区丢弃。最终 release 先完成不执行用户代码的 resource-specific cleanup，再发布 stable cell id 的 `ResourceRelease` 或 raw slot return；resource lease
不能由 region reset、block candidate 或 mailbox 消费时刻替代。

## Allocation debt、pressure 与 backpressure

### 账本

每个 processor、domain 和 runtime 总账本至少维护以下逻辑计数：

- `allocated_since_cycle_bytes`：自上次完成 GC cycle 后分配的 managed bytes；
- `pending_return_bytes`：staging、inbox、forwarding chain 中尚未由 owner 消费的 bytes；
- `owner_cache_bytes`：raw slab、stack cache、range cache 中已 commit 但未被 live record 使用的 bytes；
- `reclaimable_bytes`：metadata 已确认可重用但尚未回到 owner free structure 的 bytes；
- `committed_bytes`、`reserved_bytes`、`decommitted_bytes`；
- `allocation_debt`、`mark_debt`、`gc_cpu_credit`、`pressure_epoch` 和本 pressure episode 是否已经执行 forced full cycle；
- `remote_return_batches`、`remote_return_hops` 和 `forwarded_messages`。

计数的更新可使用 per-owner counters 后再采样合并；统计可最终一致，但 pressure decision 必须读取不会低估物理占用的值。
设上个完成 cycle 的 managed live bytes 为 `last_live_bytes`，GC target 的百分比为 `target_percent`，runtime profile 的下限为 `min_growth_budget`：

```text
growth_budget = max(min_growth_budget,
                    floor(last_live_bytes × target_percent / 100))
allocation_debt = max(0,
                      allocated_since_cycle_bytes - growth_budget)
mark_debt = allocation_debt × mark_cost_per_byte + pending_mark_work
pressure_debt = max(0,
                    heap_committed_bytes + runtime_committed_bytes
                    - soft_memory_limit)
return_pressure = pending_return_bytes + owner_cache_bytes
```

`mark_cost_per_byte` 与 `assist_quantum` 来自当前 `GcPacingProfile`；它们按饱和整数 cost unit 计算，不把宿主 wall-clock 当作 debt。每次 TLAB refill、allocation slow edge 或显式 poll 最多偿还一个 `assist_quantum`，完成的 mark/card/edge 工作才减少 `mark_debt`。Local fast bump 不读取全局 debt；refill 和 slow edge 才检查本地累计 debt 并发布 assist/GC 请求。

没有配置 soft memory limit 时，`pressure_debt` 固定为 0，但 `return_pressure` 仍用于防止 producer/owner cache 无限滞留；平台分配失败仍按统一 `OutOfMemory` 规则处理。

`reclaimable_bytes` 只有在 owner 已取得对应 free unit 后，才可以作为无需新 commit 的可用容量；在完成 decommit 前它仍属于物理 committed，不能从 pressure debt 中扣除。不能把“collector 已发现死亡”直接当作可用内存。

`GUGU_RUNTIME_GC_TARGET=off` 可以关闭由 `allocation_debt` 触发的正常 cycle，但不能关闭 memory limit、pending return drain、emergency sweep 或 OOM 规则。

### Pressure hysteresis 与 forced cycle

`pressure_enter_ratio` 和 `pressure_clear_ratio` 是 profile 中固定的整数比例，且满足 `0 < clear < enter < 100`。当 `heap_committed_bytes + runtime_committed_bytes` 达到 `soft_memory_limit × enter`，runtime 开启新的 `pressure_epoch` 并进入 `Drain`；只有占用降到 `soft_memory_limit × clear` 以下，且 pending/cache/reclaimable 分类均已完成一次 owner drain，才结束该 episode。达到 soft limit 进入 `Emergency`，但一次 episode 最多启动一次 forced full cycle。

forced full cycle 完成后若仍高于 clear 水位，runtime 继续执行 bounded owner drain、forwarding grace、sweep、trim/decommit 和有限 assist，不重复启动同一完整 cycle。若仍无法为请求取得 headroom，则当前分配直接进入 `OutOfMemory`，而不是在每次临界分配上重扫整堆；占用降到 clear 水位后才允许下一 episode 再触发 forced cycle。pressure state 与 episode 标记都由 runtime owner 线性化，跨 owner 的 pending bytes 必须先合并到不会低估物理占用的快照。

GC worker 使用 `gc_cpu_fraction` 的滑动 cost window：有 runnable 压力时，超出窗口的普通 mark/evacuation work转为 debt 和后续 assist；idle processor 可以消费尚未使用的额度。emergency drain 可以暂时越过吞吐预算来恢复内存安全，但不能跳过 generation、lease、root、card batch 或 queue grace 校验。


### Backpressure 归属

- producer 只负责在自己的 staging 达到阈值时发布，不等待目标 owner 完成；
- owner service 负责消费和回收，不能由 producer 直接写目标 free list；
- owner 不活跃或正在 retire 时，消息进入 forwarding/domain injection；
- pressure emergency 下允许同步触发 domain drain，但不允许为了清空队列而执行用户代码或阻塞在 scheduler lock 上；
- `RuntimeStats` 必须能区分 live、pending、cache、reclaimable 和 committed，便于诊断延迟回收造成的峰值。

## Scheduler 接缝

### 现有实体的归属

- `CoroutineHot` 的固定布局不增加 owner 字段；其控制记录由 `CoroutineSlot`/slab descriptor 查 owner。
- `CoroutineCold`、join state、wait node 和 producer staging 使用 raw plane 的稳定 descriptor。
- coroutine stack 的 owner 归属于 stack span，而不是创建它的 coroutine；finish 在不同 worker 执行时走 local return 或 remote return。
- work stealing 可以迁移 coroutine，但不隐式转移其 raw slab/span owner。

### Wake 与 return 分离

ready、park、wake 的线性化和即时性仍遵守 scheduler 规范：

- ready/wake 消息可以使用现有 batch inbox，但被唤醒 coroutine 的可见性不能等待 memory return batch；
- stack/control slot 的回收可以异步发送；
- `join` 的完成状态必须由 join protocol 发布，不能从某个 free-list 消费事件推断；
- queue carry、staging 和 return message 的生命周期都经过 producer gate 和 queue-page grace。

### Safepoint 规则

`NoSafepointRegion` 内只允许完成已经取得的 slot/link 的状态写入和一次 batch publish：

- 不分配 message node；
- 不阻塞、不 park、不等待 owner；
- 不执行 range refill、GC assist、drop glue 或平台调用；
- 不遍历 queue，不做 radix forwarding；
- pressure check 和可能的 drain 必须在进入该 region 之前或离开后执行。

状态 bit 7 继续保留为 `BATCH_PUBLISHING`，return message 不占用该 bit；return-specific state 存在 descriptor/message state 中。

### Owner retire

owner retire 按以下顺序执行：

1. 把 owner record 从 `Active` 改为 `Draining`，以 Release 发布；
2. 停止新的 direct publish，并让 producer gate 看到 retire epoch；
3. 发布 `Forwarding` record，指定新的 owner 或 domain injection；
4. 取得 owner inbox、local cache、range cache 和 pending chain 的独占 drain lease；
5. 将未消费消息按 target generation 转发，或在 domain owner 上消费；
6. 等待所有参与 producer/consumer 通过 queue-page grace；
7. 释放 descriptor、slab page 和 route directory slot；
8. 旧 `owner_id` 永不复用；新的 owner 使用新的 generation 和 route key。

如果 retire 与 GC stop、ForeignBridge 或 DirtyCpu 重叠，必须先完成现有 producer gate 和 ABI root 可见性规则，再执行 owner handoff。

## Compiler 与 GIR/LIR 契约

### Placement 与 owner policy

`EscapeAndPlacement` 为每个分配点产生以下逻辑分类和 managed 内部 placement：

| placement | storage | return policy |
|---|---|---|
| `Managed::TurnRegion` | 当前 coroutine turn 的 owner-local bump region | export 后 promote/transfer，或验证后整区 reset |
| `Managed::LocalHeap` | processor TLAB、Immix arena、line 或 large mapping | exact GC、evacuation、sweep、block return |
| `Managed::SharedHeap` | stable handle table 管理的 shared payload | handle forwarding、grace 后 block/extent return |
| `Managed::Pinned` | non-moving pin/foreign/large region | pin/foreign lease 结束后按对应 descriptor return |
| `RuntimeRaw` | owner slab/stack span/range | local return 或 remote batch |
| `Resource` | ResourceCell + resource arena | lease release 后 cleanup/return |
| `Foreign` | foreign/platform mapping | bridge/foreign owner 规则 |

placement 不是用户可写的类型或 ownership 检查。analysis 为 `unknown` 时必须选择能够
保留现有值、引用、共享 identity 和 resource 语义的 placement，并保留完整 root、barrier、
pin 和 runtime 检查。

`AllocPlan` 需要携带 class/footprint、managed placement、representation tag、may-safepoint、
owner domain、cycle credit 和 return kind；这些是 lowering metadata，不是用户可读取的
地址、对象 owner 或 processor 信息。

### Lowering

- `Managed::TurnRegion` 产生 `RegionAlloc`；publish 前必须有 export summary，transfer 产生
  `RegionTransfer`，无法证明私有性则转 `PromoteManaged`；reset 只能在 typed root verifier
  确认没有外部 alias 后发生。
- `Managed::LocalHeap` 小对象继续走 TLAB/Immix fast path，slow edge 进入 `GcAlloc`；本地
  direct field 使用现有 `GcWriteBarrier`，跨 owner/block 追加 `EdgeDeltaBatch`。
- `Managed::SharedHeap` 产生 `ResolveSharedHandle`、`SharedAccessBegin/End` 和
  `ForwardSharedHandle`；没有 access guard 的 shared direct pointer 不合法。
- compressed reference 只在明确的 cage profile 中由 `DecodeCompressedRef` lowering，
  FFI 前必须 resolve+pin；大对象、pinned、foreign 和跨 cage object 使用完整地址或 handle。
- `MarkTicketBatch`、`EdgeDeltaBatch`、`RegionTransfer` 和 `HandleForward` 只能携带 stable
  descriptor/handle/index/generation/epoch/credit，verifier 必须拒绝 managed pointer payload。
- `RuntimeRaw` 小记录优先 lowering 到 owner-local pop/bump；cache miss 进入可 safepoint 的
  raw refill slow path，remote return 不是 managed value 的用户级 `free`。
- `Resource` drop glue 只调用规定的 release/cleanup 入口；最终回收消息由 runtime 发布。
- `HeapBlockReturn`、`Extent` 和 `ReturnNode` 的 payload 只能被标记为 raw/stable descriptor；
  compiler verifier 拒绝把它们当作 GC root 或 managed field。
- queue publish 的短序列可以落在 `OwnershipPublish`/`NoSafepointRegion`；drain、assist、
  mark mailbox、credit termination、handle resolve、radix forwarding 和 platform call 必须
  被标记为可停顿 slow path。
- `PollSummary` 必须把可能触发 region promote、GC assist、message flush、handle resolve 和
  pressure drain 的 backedge 成本纳入；`POLL_BUDGET = 4096` 不因 Mosaic message plane 放宽。

## x86_64 Backend 与内存序

### 热布局

backend 必须对以下布局执行 compile-time 和 runtime assertion：

- owner inbox 的 producer-visible tail 与 consumer-only front 不共享 cache line；
- route bucket/staging 与统计计数分离；
- `SlabDescriptor` 的 owner/generation/state 与 free-list link 的对齐满足 raw access；
- `CoroutineHot`、`StackDescriptor` 和既有 `r14`/`r15` ABI 偏移不因本章新增记录而改变；
- raw pointer、descriptor index 和 managed pointer 在 lowering 中有不同表示与 verifier 标签。

local pop、bump 和统计采样不能额外占用 `r14 = Coroutine*`、`r15 = LogicalProcessor*` 的既有内部 ABI 约定。owner lookup 和 radix hash 放在 refill/flush slow path，不进入每次 TLAB allocation。

### 原子规则

- batch tail exchange 使用 AcqRel；chain link 使用 Release，consumer link load 使用 Acquire；
- local free list 和 owner-only front 不使用原子；
- bytes/counter 只用于统计时可以 Relaxed，但不能用 Relaxed 代替 state、generation、lease 或 root publish 的同步；
- 不新增全局 SeqCst fence；只有现有 ABI/平台明确要求时才保留 SeqCst lowering；
- `OwnerToken` generation/state 的发布与读取必须形成 Release/Acquire 或等价的 CAS 配对；
- 任何 `compare_exchange` 失败都必须重新读取当前 generation/state，不能沿用旧 token 继续写。

backend 验证包含 batch publish、consumer snapshot、owner handoff、block return 和 pressure drain 的机器序列；不得只验证源码中的原子 API 名称。

## PlatformRange 与外部内存

### PlatformRange 接口

平台 adapter 提供以下固定操作，调用者不需要知道系统调用或 CRT 细节：

- `reserve_aligned(bytes, alignment)`；
- `commit(range)`、`decommit(range)`、`release(range)`；
- `protect_guard(range)`、`unprotect(range)`；
- `wait(word, expected)`、`wake(word, count)`；
- `entropy(bytes)`、`cache_line_bytes()`、`page_bytes()`；
- `zero(range)`、`set_dump_policy(range, policy)`；`commit` 若平台已保证匿名页清零可省略显式 zero。
- 可选 `low_memory_hint()` 和 huge-page hint。

Linux x86_64 和 Windows 是首批 adapter。Gugu 没有 libc 依赖时，平台调用落在已有 `PlatformAbi`/intrinsic seam；失败码必须映射到统一 runtime error 分类。

### ForeignBridge

外部 allocator 的内存不自动获得 Gugu owner：

- foreign allocation 由 foreign owner 负责释放，或显式包装成 `ResourceCell`；
- bridge 可以把“最终 release”投递到支持回调的 foreign owner，但不能假设外部指针可被 Gugu radix map 解析；
- `GuardedCopy`/checked copy 只用于 ForeignBridge 的长度和权限校验，不替代 managed object 的 trace；
- DirtyCpu、阻塞系统调用和平台 decommit 不进入 `NoSafepointRegion`。

## Provenance、free-list 完整性与安全 profile

### Raw provenance

raw pointer 只能在其所属 `RuntimeRaw`/`PlatformRange` descriptor 范围内使用。owner lookup 通过稳定 descriptor、range map 或已登记的 slab header 完成，不通过把任意整数解释成 managed pointer。

对于每个 raw return：

1. 验证地址对齐、所在 range、class、slot index 和 descriptor generation；
2. 验证 source owner 曾拥有该 slot，且状态是允许 return 的状态；
3. 验证 target owner/domain 与 descriptor 当前记录一致，或存在合法 forwarding；
4. 验证 message link 的编码、长度和下一节点范围；
5. 验证成功后才修改 free list 或 range bitmap。

### Link protection

raw free-list link 使用 per-domain secret 和 slot address 派生的 mask 编码。decode 后必须同时检查 canonical address、alignment、所属 slab、generation 和 class。back-link 或 batch tail 可以附带轻量签名/校验值，检测断链、foreign free、double return 和常见 UAF。

- secret 存在 non-moving metadata，不写进 managed payload；
- secret 初始化失败进入 runtime fatal，不使用公开常量作为安全秘密；
- debug profile 额外使用 poison、双重释放标记和全链表 verifier；
- release profile 至少保留 owner、generation、range 和 alignment 检查；
- 检查失败不能静默丢弃消息，否则会把内存损坏变成长期泄漏。

### 随机化与 guard

- raw slot 的 reuse order 可以在 security profile 中随机化，使用独立 runtime seed；不得改变语言 RNG 或可观察值顺序；
- stack、large mapping 和 metadata range 使用既有 guard page 规则；
- 不对每个小 slot 建立独立 OS mapping，避免 VMA 和系统调用爆炸；
- metadata 可在发布后改为只读或受保护映射的部分由平台 profile 决定，正确性不能依赖该优化。

## Typed combining

snmalloc 的 combining lock 只作为冷路径 adapter，不作为本章的默认同步原语。

### 允许用途

- `GlobalRange` 的罕见 refill、extent coalescing 和平台 trim；
- topology 变更时的目录重建；
- 多个同步范围请求可以被同一个 domain owner 合并执行。

### 记录与执行

- requester 在 non-moving operation slab 中创建固定类型 operation record；
- record 只含标量、stable descriptor id、已登记 handle 和 response slot，不含未登记 managed pointer 或 closure；
- requester 发布 record 后可以 park；combiner 取得记录并在自己的上下文执行；
- 每个 operation 有状态、generation、结果和取消/超时规则；response 发布使用 Release/Acquire；
- combiner 每轮有固定 item/byte budget，不能持有 range lock 跨 safepoint、await 或用户代码；
- 无争用时走单原子 fast path；有争用时使用稳定 MCS record 或 owner inbox；平台 wait/wake 由 `PlatformRange` 提供。

### 禁止用途

- TLAB allocation、raw local pop、普通 remote return；
- channel/select/park/wake 的线性化；
- 执行任意 Gugu closure、drop glue 或需要当前 coroutine stack 的函数；
- 用一个 combining lock 保护所有 domain 的共享 free list。

## 现有标准库和 arena 的适配

- `LocalArena` 继续是单 owner bump arena；reset 的引用失效和生命周期语义不改变，不为每个对象发送 remote return。
- `SyncArena` 保持共享 lease/同步语义；只有 refill、range coalescing 或 trim 可以使用 domain owner/typed combining，单次 `alloc` 不绕道消息队列。
- `Vec`、`Map`、string、Bytes 和 ByteBuffer 的 managed storage 继续由 GC 管理；COW clone 不转化为 raw reference count。
- 大型不可移动 foreign buffer 可以包装为 ResourceCell/Foreign descriptor，再使用 resource release message；这不改变普通 managed value 的移动规则。
- 用户代码看不到 owner、route key、batch 延迟、slot 地址或 decommit 时刻。

## 统计、追踪与可观测性

`RuntimeStats` 在保留既有字段的基础上增加以下逻辑维度；字段值可最终一致，但名称和口径固定：

| 字段 | 口径 |
|---|---|
| `runtime_committed_bytes` | raw slab、stack metadata、message node、region descriptor、handle table 和 runtime range 的 committed bytes |
| `pending_return_bytes` | raw staging/inbox、GC forwarding chain、MarkTicket、EdgeDelta、RegionTransfer 尚未消费的 bytes |
| `owner_cache_bytes` | owner-local raw/range/TurnRegion cache 的 committed bytes |
| `reclaimable_bytes` | metadata 已确认可重用但尚未进入 owner free structure 或 forwarding grace 的 bytes |
| `remote_return_batches` | 已发布的 raw/block return batch 累计数 |
| `remote_return_hops` | radix/owner forwarding 累计 hop 数 |
| `forwarded_messages` | 因 owner generation/topology/GC handle 转发的 message 数 |
| `mark_ticket_batches` | 已发布的跨 owner mark batch 累计数 |
| `edge_delta_batches` | 已发布的跨 block edge summary batch 累计数 |
| `mark_credit_pending` | 当前 cycle 尚未归还的 owner credit |
| `shared_handle_resolves` | SharedHeap resolve/access guard 建立累计数 |
| `shared_handle_forwards` | stable handle current payload 切换累计数 |
| `turn_region_resets` | 通过 typed root/export 验证完成的整区 reset 累计数 |
| `region_promotions` | TurnRegion 对象图提升到 LocalHeap/SharedHeap 的累计数 |
| `compressed_ref_decodes` | compression cage 内部引用解码累计数 |
| `range_reserved_bytes` | virtual reserve 但未 commit 的 bytes |

`heap_live_bytes` 仍只表示 managed live objects；`heap_committed_bytes` 不包含 raw plane。memory
limit 的物理占用判断使用 `heap_committed_bytes + runtime_committed_bytes`；`pending_return_bytes`、
`owner_cache_bytes` 和 `reclaimable_bytes` 是 runtime committed 的互斥分类，用于 pressure trigger
与诊断，不重复相加。`mark_credit_pending`、handle guard 和 region transfer 仍然对应物理
保留或不可复用状态，不能从 committed bytes 中扣除。

trace 可记录 `mark_publish`、`mark_consume`、`edge_delta_publish`、`region_promote`、
`region_transfer`、`region_reset`、`handle_resolve`、`handle_forward`、`return_publish`、
`return_consume`、`return_forward`、`block_return`、`owner_retire`、`pressure_drain` 和
`range_trim` 事件。事件 payload 只记录 domain、descriptor/slot identity、bytes、generation、
cycle、batch size、credit、hop 和 duration，不记录 managed payload 地址或用户对象内容。


## 验证契约

### 正确性模型

必须有进程内、确定性、无真实网络和无重负载的测试替身覆盖：

- 多 producer append 同一 owner inbox，包含 producer 在 tail exchange 前后被暂停的交错；
- chain 顺序、front snapshot、重复 return、generation mismatch 和 forwarding；
- owner retire 与新 producer publish 的 epoch/gate 交错；
- queue-page grace 前后 node/page reuse；
- block return 与 scanner/allocator/evacuation lease 交错；
- TLAB flush、barrier buffer、root snapshot 和 block return 的先后关系；
- direct/radix mode 切换、hop 上限、route key 碰撞和 topology epoch；
- pressure drain、pending bytes 账本和 emergency OOM 分类；
- encoded link、poison、alignment、foreign free、double return 和 descriptor corruption；
- Linux/Windows platform adapter 的 fake reserve/commit/decommit/wait/wake 行为。
- TurnRegion 的 Private/Publishing/Promote/Transfer/Reset 状态、export summary、region generation 和 pending transfer bytes；
- `MarkTicket` 的 cycle、target owner generation、stable object identity、credit、重复消息和延迟消费；
- `EdgeDelta` 的 epoch 有序性、block generation、重复/乱序 add-drop、incoming lease candidate 和 cycle/SCC 保留；
- SharedHeap handle resolve/forward/grace、access guard、pin、stale generation 和 cage range；
- `RegionTransfer`、`HandleForward`、`MarkTicket` 和 `EdgeDelta` 与普通 channel/wake/join 语义隔离；
- `f(x)`、`f(&x)`、identity handle、COW、ResourceCell 和 FFI pin 的语义在所有 placement 下保持一致；
- owner credit 的产生、传播、归还和终止检测；
- compressed reference 的 cage/base/offset/generation checked 验证和跨 cage fallback；


测试替身不能削弱 fixture、取消 generation、跳过 grace 或把 managed pointer 改成整数来绕过 verifier。

### 性能门禁

benchmark 与正确性测试分离。至少测量：

- local raw pop/bump、TLAB allocation 和 owner inbox publish；
- producer/consumer asymmetric return 与 symmetric alloc/return；
- coroutine 跨 processor create、wake、finish 和 stack reuse；
- ResourceCell release burst、wait-node churn 和 control slab recycle；
- consumer-side `ReturnSlabCache` 的 source-slab hit rate、ring occupancy、victim bytes、close 次数与 remote batch reduction；
- block return、line-run return、GC assist 和 pressure drain；
- direct target cache 与 radix mode 的 target cardinality、batch size、hop、CAS/exchange、LLC miss、cache-line bouncing、RSS、committed 和 pending bytes；
- processor 数为 1、2、6、32 时的吞吐和 p50/p99 latency；
- owner retire、动态并行度和 platform trim 的尾延迟。
- TurnRegion reset/promote/transfer 的 bytes、命中率、export summary 大小与 reset 延迟；
- MarkTicket/EdgeDelta batch size、mailbox occupancy、credit pending、跨 owner hops 和终止延迟；
- SharedHeap handle resolve/forward 次数、access guard 保留时长、forwarding grace bytes 与 stale handle；
- pointer compression 对 payload footprint、扫描带宽、decode cycles、cache miss 和 FFI 交接的影响；
- block candidate 的 lease 命中率、SCC fallback、HeapBlockReturn 延迟与 RSS；

必须在 release 构建中比较 cycles/instructions 与 wall time；没有 profile 数据时不得宣称某个 batch threshold、radix level 或 cache size 更快。

## 实施顺序

1. 固定 `OwnerRecord`、`OwnerToken`、`SlabDescriptor`、`ReturnMessage` 和 generation/state verifier。
2. 以 stack span、CoroutineSlot/Cold 和 wait node 建立 raw owner-local cache 与 owner inbox adapter。
3. 接入 consumer-side `ReturnSlabCache`，验证 source slab 聚合、victim close、integrity 和 pressure flush。
4. 接入 ResourceCell release，验证 exactly-once cleanup、generation 和 queue grace。
5. 建立 PlatformRange、ExtentDescriptor、commit/decommit 和 domain range reducer。
6. 建立 allocation debt、pending bytes、pressure drain、RuntimeStats 和 trace 口径。
7. 固定 `TurnRegion` descriptor、export summary、promote/transfer/reset state 和 region pressure accounting。
8. 将 `MarkTicket`、`MarkMailbox`、owner credit 和 `EdgeDelta` 接入现有 batch/gate/grace substrate，完成跨 owner mark 与 block candidate verifier。
9. 建立 `SharedHeap` stable handle、access guard、forwarding grace 和 generation verifier，保留 LocalHeap direct evacuation。
10. 接入 managed `HeapBlockReturn`、line-run、arena 和 large mapping return；所有 return 必须等待 handle/lease/grace 条件。
11. 在明确的 heap cage profile 中加入 checked pointer compression；再以真实 workload 评估 decode、cache 和 FFI 成本。
12. 完成 per-owner root slice、credit termination、MosaicBaseline/MosaicConcurrent stop 边界和 security profile。
13. 最后加入 typed combining，用于 GlobalRange 和 topology 冷路径，不回流到 allocation/return/GC mark 热路径。

每个步骤完成后都要同步对应的 spec/internals 条款；实现、规范和测试必须同时改变，不能只引入一个“以后再接”的空接口。

## snmalloc 对照与明确取舍

| snmalloc 思想 | Gugu 采用方式 | 不采用的原形 |
|---|---|---|
| owner-local allocator | TurnRegion bump、LocalHeap TLAB、raw slab/span owner cache | 不把当前 OS thread 当 owner |
| remote deallocation message | RegionTransfer、HeapBlockReturn、stable raw record、ResourceCell batch return | 不发送 managed object 裸地址 |
| BatchIt consumer-side batching | ReturnSlabCache 按 source slab 聚合后再投递 | 不用于 managed value 的对象级 free |
| message-driven marking | MarkTicket owner mailbox、EdgeDelta summary、owner credit termination | 不用 mailbox 空值替代全局终止检测 |
| stable forwarding | SharedHeap handle slot、access guard、forwarding grace | 不把 stale direct pointer 当合法 shared reference |
| variable-sized slab | 自然对齐 extent、共享 descriptor、预计算 reciprocal 常量 | 不把 superslab 地址算术当作 managed provenance |
| pointer compression | 不超过 4 GiB heap cage 的 checked offset，跨 cage 使用 handle | 不把 compressed reference 当 raw pointer 或用户整数 |
| lazy buddy/startup | lazy range metadata、domain 初始化与 typed cold operation | 不让首次分配持有全局锁做大批 page fault |
| zero/realloc/alignment | raw range 按需求清零、同 class 原地调整、跨 class allocate-copy-return | 不改变 Gugu managed 引用移动语义 |
| metadata separation/core-dump policy | metadata/payload 分离、稀疏 map 的 dump policy 交给平台 adapter | 不对每个小对象建立独立 OS mapping |
| intrusive free-list message node | raw slot 或 ReturnNode pool | 不覆盖 managed payload/trace field |
| MPSC batch tail exchange | OwnerInbox、MarkMailbox 和 GC message batch publish | 不用于线性化同步事件 |
| temporal radix tree | route key、bounded bucket forwarding、高 fan-out profile | 不按可移动地址或 processor slot 直接路由 |
| size class/slab | dense RuntimeSizeClass、TurnRegion class、stack class、extent class | 不跨 drop/scan 不变量混用 free list |
| address metadata map | 复用 Gugu radix/descriptor map、handle table | 不新建 P² owner mailbox |
| large range/buddy | owner/domain reducer、extent descriptor、批量 trim | 不依赖未对齐的 128-bit global CAS |
| combining lock | typed cold operation record | 不执行 arbitrary closure 或热路径锁 |
| free-list encoding/randomization | raw provenance、generation、link protection、安全 profile | 不把 x86 普通指针伪装成 capability |
| guarded copy/platform abstraction | ForeignBridge checked copy、PlatformRange seam | 不让 platform call 进入 no-safepoint leaf |

本章因此形成的差异化是：coroutine scheduler、TurnRegion、LocalHeap、SharedHeap、runtime raw allocator 和 range manager 共享一条 owner-directed message plane，同时保留各自的对象寿命和同步语义。

## 参考资料

- [snmalloc README](https://github.com/microsoft/snmalloc#readme)
- [snmalloc 论文：A Fast and Featureful malloc for Modern C++](https://doi.org/10.1145/3315573.3329980)
- [snmalloc 地址空间设计](https://github.com/microsoft/snmalloc/blob/main/docs/AddressSpace.md)
- [snmalloc remote allocator](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/remoteallocator.h)
- [snmalloc remote deallocation cache](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/remotecache.h)
- [snmalloc 0.7 BatchIt 说明](https://github.com/microsoft/snmalloc/blob/main/docs/release/0.7/README.md)
- [snmalloc MPSC free-list queue](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/freelist_queue.h)
- [snmalloc free-list 实现与防护](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/freelist.h)
- [snmalloc combining lock](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/ds/combininglock.h)
- [Gugu 内存与对象模型](../spec/memory.md)
- [Gugu GC 元数据](gc-metadata.md)
- [Gugu 调度器](scheduler.md)
- [Gugu GIR 与 LIR](gir-lir.md)
- [Gugu x86_64 后端](backend.md)
- [Gugu 运行时与运维语义](../spec/runtime.md)
