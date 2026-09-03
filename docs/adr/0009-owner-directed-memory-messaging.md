# ADR-0009：面向 owner 的 runtime 内存消息通道

- 状态：已接受
- 日期：2026-09-02
- 领域：runtime、GC、调度器、平台内存

## 背景

Gugu 的设计同时包含有栈协程、M:N 调度、per-processor TLAB、可移动 Immix、non-moving runtime slab、stack arena、ResourceCell 和动态 processor topology。当前这些内存来源的回收与范围管理分别依赖各自的 owner、queue 或 pool 规则；跨 processor 的释放、GC block 返还和大范围归还如果共享一个全局 free structure，会把本地快路径重新变成 cache-line 和原子竞争点。

snmalloc 提供了适合这一问题的算法思想：每个 allocator 保持本地状态，远程释放不直接改拥有者的 free list，而是由发送方按目标聚合成 batch，通过 MPSC 消息返回拥有者；拥有者在自己的上下文中完成回收。其 temporal radix 路由、intrusive free-list、size class、范围管理和 link protection 还可以减少高 fan-out、分配和元数据损坏的成本。

但 Gugu 与 malloc 有不可合并的语义差异：managed object 可移动、由精确 GC 管理、没有用户级 `free`，且 compiler/runtime 必须闭世界编译。因此不能把 snmalloc 作为全局 allocator 直接替换 Immix，也不能把 C++ 实现作为生成程序的依赖。

## 决策

Gugu 采用“owner-directed memory messaging”作为 runtime 内部统一的所有权返回与 GC 后台工作模型，分为一个逻辑 managed plane 和一个 runtime raw plane：

1. `Managed plane` 采用 Mosaic GC：`TurnRegion` 负责已证明私有的 coroutine-turn 对象图，`LocalHeap` 保留 Immix、TLAB、write barrier、direct pointer 和 owner-local tracing，`SharedHeap` 使用 stable handle、mark mailbox、edge summary、owner credit 和 forwarding grace。消息只携带稳定 descriptor、handle、object/block identity、generation、epoch、credit 和 bytes，不发送 managed object 裸地址。
2. `Runtime raw plane` 用 non-moving slab/span 保存 coroutine control、stack slot、wait node、ResourceCell、range descriptor 和 message node。跨 owner 的生命周期结束通过 batched return message 交给 owner，owner 在本地 free list 或 range reducer 中处理。

该模型使用稳定 `OwnerToken`、descriptor generation、topology epoch、cycle credit 和现有 queue-page grace 处理 owner 转移、region transfer、GC message 与消息存活。direct owner shard 是默认路由；temporal radix fan-out 作为高 fan-out tuning profile，只在 benchmark 证明 direct target switching 是热点时启用。

所有待发送、待转发、待消费、handle grace、region transfer 和 owner cache 的物理 bytes 都进入 memory pressure 账本。allocation debt、pending bytes、credit drain 和 pressure drain 是同一个设计的一部分，不能单独延迟释放而不限制内存峰值。

snmalloc 的 combining lock 只允许作为 `GlobalRange`、extent coalescing、topology 和 platform trim 的 typed cold-path adapter。Gugu 不执行 arbitrary closure，不在 LocalHeap direct allocation、SharedHeap resolve、GC mark 或 allocation/return 热路径使用全局 combining lock。

## 细节

### Owner 与身份

owner 是 logical processor 或 stable domain owner，不是 OS thread。`OwnerToken` 包含 domain、owner id、generation 和稳定 route key。owner id 不复用；owner retire 先进入 `Draining`/`Forwarding`，排空 producer gate、inbox、local/range cache 后才释放 descriptor。

raw slab/span 的 owner 元数据放在稳定 descriptor，而不是扩展 `CoroutineHot` 的固定布局。managed block 同时记录 allocation、scanner、sweep 和 evacuation lease，collector 只有在 lease 完成后才能发布 return message。

### 消息与队列

raw return message 只能携带 stable descriptor、slot/block/line/extent index、bytes、generation、epoch 和 integrity 信息。stride 足够时可复用已死亡 raw slot 作为 intrusive link；否则使用专用 non-moving `ReturnNode` pool。managed pointer、未 pin 的 interior pointer 和未登记 handle 不得进入 raw message。

owner inbox 是多 producer、单 owner consumer 的 append-only batch queue：producer 发布一条完整 chain，每 batch 只做一次 AcqRel tail exchange；consumer 以 Acquire 读取 chain 并在自己的 front 上处理。该队列故意不提供跨 producer 的全局顺序或线性化保证，所以只用于回收、范围搬运和后台维护，不用于 wake、join、channel 或同步。

Gugu scheduler 已有的 `ProducerHandle`、8-shard batch head、producer gate、`BATCH_MAX = 128` 和 queue-page grace 作为传输底座。ready/wake 与 memory return 在类型和 verifier 上分离，不能因复用底层 link 就混淆语义。

在 owner inbox 之前加入 BatchIt 风格的 `ReturnSlabCache`：consumer/finisher 按 source slab descriptor 和 generation 聚合 raw slots，再发布一个 return batch。默认使用固定 8-set、2-way 关联 ring；它不适用于 managed value，也不改变 exactly-once、integrity、pressure 或 grace 规则。

### 路由

direct mode 使用 owner directory、8 shard 和小型 temporal target cache，不建立 producer×owner 的长期队列矩阵。route key 不从可移动地址或复用的 processor slot 推导。

radix mode 使用固定 `2^k` bucket、有限 levels 和 forwarding record，把高 fan-out batch 转发到目标 domain/owner。每跳计数并受上限约束；旧 topology epoch 只能转发到新 token 或 domain injection。mode 切换在 maintenance epoch 完成，并先排空旧 staging/bucket。

### 范围管理

reserve、commit、decommit、release、guard、wait/wake、entropy、zero 和 dump policy 由 `PlatformRange` adapter 提供。decommit 只在 span/extent 经过所有 allocator/scanner/forwarder lease 和 grace 后按批执行；huge-page hint 不能成为正确性保证。


### GC 融合

managed GC 使用三个内部层次：`TurnRegion` 的 owner-local bump/reset、`LocalHeap` 的 Immix
tracing/evacuation/sweep，以及 `SharedHeap` 的 stable handle/mark mailbox/forwarding grace。
跨 owner mark 使用 `MarkTicket`，跨 block edge 使用 `EdgeDelta`，cycle 终止使用 owner credit；
block-level incoming lease 只生成 candidate，不替代 exact tracing，也不引入对象级 reference
counting。

`TurnRegion` 只有在 export summary 证明无外部 alias、无 ResourceCell lease 且没有 FFI 地址
需求时才能 reset。私有 region 可以通过 `RegionTransfer` 交给新 owner；若 sender 仍需依据
`f(x)` 语义继续使用，必须先 promote 或复制。普通 channel 不获得 transfer 语义。

`SharedHeap` 对象通过 stable handle slot 表示逻辑身份。forwarding 只在 slot 线性化点切换
current payload；旧 payload 必须等待 access guard、pin、mark ticket 和 forwarding grace
完成。`LocalHeap` direct pointer 保持无 read barrier；只有 shared resolve/compressed decode
路径承担额外访问成本。

完整 block 在 `Sweeping` 或 `Evacuating` 完成、incoming edge、forwarding/root、handle access、
scanner、allocator 和其它 lease 全部结束后进入 `ReturnPending`，再通过 owner inbox 返还
`OwnedFree`。line-run、arena 和 large mapping return 必须各自通过 metadata verifier；不能
因为低 live ratio 直接释放仍被 message 或 grace 使用的范围。

resource arena 不整区丢弃。最终 ResourceCell release 先完成不执行用户代码的 cleanup，再把
stable cell id 返还 raw pool。普通 managed COW、Vec、Map、string 和 ByteBuffer 仍由 GC 管理，
不引入用户可见的引用计数或显式 free。

### Debt、屏障与压力

runtime维护 `allocated_since_cycle_bytes`、`pending_return_bytes`、`owner_cache_bytes`、
`reclaimable_bytes`、`mark_debt`、`gc_cpu_credit`、committed/reserved bytes和GC message batch/hop
计数。pending、cache、credit保留和reclaimable是runtime committed的互斥分类；allocation debt按
GC target的增长预算计算，mark debt按 descriptor/profile的 `mark_cost_per_byte` 计算，pressure
decision读取不会低估物理占用的跨 owner快照。

LocalHeap old-to-young写入先完成field store，再把 `(arena, generation, card, cycle)` 键写入
processor-local、固定256项的 `CardMarkBuffer`；不在mutator路径直接写共享 card table。buffer满、
processor交接、ForeignBridge、memory pressure和minor stop前必须 flush；非 arena owner发布
`CardMarkBatch`，card table只能由arena owner消费写入。batch不携带managed pointer，重复card标记幂等，
generation/epoch/credit必须由verifier闭合。

`GcPacingProfile`固定 `min_growth_budget`、`assist_threshold`、`assist_quantum`、
`mark_cost_per_byte`、`gc_cpu_fraction`、`remark_cost_budget`、evacuation pause上限和pressure
enter/clear比例。mutator assist与collector worker共享cycle credit；remark或evacuation超过预算
发布continuation或延后完整block，不关闭hybrid barrier、不为LocalHeap direct pointer增加隐式
read barrier。一个pressure episode至多执行一次forced full cycle，未降到clear水位时继续有界
drain而不重复整堆重扫，无法取得headroom才报告OOM。

### 安全与 provenance

raw link 使用 per-domain secret 和 slot address 派生的编码，decode 时检查 canonical address、alignment、range、class、owner 和 generation。debug profile 额外执行 poison、double return 和全链表检查；release profile 保留必要的 provenance 和 generation 检查。random reuse、guard page 和 checked copy 只应用于 raw/foreign 适用范围，不能改变 managed trace。

### Typed combining

需要同步结果的 GlobalRange 或 topology 操作使用稳定 operation record、固定 tag、标量参数、stable descriptor 和 response slot。combiner 可以合并有限数量的同类请求，但不能执行用户 closure、drop glue、await 或跨 safepoint 持锁。无争用路径保持单原子 fast path，有争用路径使用 owner inbox/MCS record 和平台 wait/wake。

## 影响

### 正面影响

- coroutine control、stack、resource、TurnRegion 和 GC block 的跨 processor 回收不再要求释放者写入拥有者的内部 free structure；
- scheduler、raw allocator、range reducer、Mosaic collector 和 SharedHeap handle manager 可以共享 generation、gate、credit、grace 和 batch publish 的实现知识；
- 短命私有对象可按 TurnRegion 整区 reset，本地对象按 LocalHeap tracing，跨 owner 共享对象按 MarkTicket/EdgeDelta/handle forwarding 分工处理；
- managed heap 继续保留精确移动语义，消息化只作用于经过稳定 identity/descriptor 保护的 GC 工作和回收粒度；
- pending bytes、owner cache、mark credit、handle grace、region transfer 和 forwarding hop 可见，内存上限下的延迟回收不会被隐藏；
- 高 fan-out 路由、raw link integrity、shared handle provenance、pointer cage 和 platform range 形成可独立测量的 tuning/security profile。

### 代价

- TurnRegion 需要闭世界 escape/export summary；分析保守时会减少整区 reset 命中率；
- MarkTicket、EdgeDelta 和 owner credit 减少共享 mark queue 竞争，但增加 batch、路由、终止检测和 pending bytes；
- stable handle/access guard 让 SharedHeap 能并发压缩，但共享对象访问会增加间接访问、缓存压力和 grace 保留；
- compressed reference 降低引用宽度，但增加 cage 约束、decode 成本和 FFI 交接规则；
- block candidate 不能直接解决跨 block cycle，需要受限 trial deletion/SCC fallback；
- remote return、handle grace 和 region transfer 会延迟复用并可能增加 RSS；symmetric alloc/return 可能比 local reuse 更差；
- owner retire、generation、credit、forwarding、queue grace 和 GC lease 的交互增加验证负担；
- radix mode 会增加转发 hop 和 metadata；只有 profile 证明 direct mode 成为热点时才启用；
- 论文 benchmark 的 batch、cage、slab、credit 和 size class 参数不能外推到 Gugu；所有性能结论必须来自 release workload benchmark。
 
## 排除的替代方案
1. **直接链接 snmalloc 或 snmalloc-rs**：违反 Gugu 闭世界 runtime 方向，并不能处理 Mosaic managed object 移动。
2. **用 snmalloc free list 替换 LocalHeap Immix**：丢失 object-start、mark、card、forwarding、edge summary 和 exact root 不变量。
3. **为每个 producer/owner 建立长期队列**：内存为 `O(P²)`，与 Gugu 的动态 processor 设计冲突。
4. **用 non-linearizable return/mark queue 做 wake/channel**：会把 GC 回收延迟错误地暴露为同步语义。
5. **所有 managed object 使用 stable handle 或 read barrier**：会把 LocalHeap direct-pointer 热路径的成本扩大到不需要共享的对象。
6. **用每对象引用计数替代 exact tracing**：无法处理普通 managed cycle，也违背 block lease 只作 candidate 的约束。
7. **所有操作统一走 combining lock**：把本地快路径重新变成共享 cache-line 热点，也无法安全执行 Gugu closure。
8. **固定照搬论文中的 1 MiB、bucket、cage 或 size class 参数**：这些是特定实现和 workload 的调优结果，不是语义契约。
 
## 后续实现约束
 
实现顺序固定为：raw slab owner return、ResourceCell release、PlatformRange/extent、debt/pacing、TurnRegion、MarkTicket/MarkMailbox、EdgeDelta/block candidate、SharedHeap handle forwarding、GC block return、pointer compression、credit termination、radix profile、security profile、typed combining。每一步必须同时更新对应规范和确定性测试替身；不得先添加没有真实消费者的兼容接口。
 
验证至少覆盖 MPSC 交错、owner retire、generation forwarding、queue-page grace、GC lease、TLAB/region flush、root snapshot、MarkTicket credit、EdgeDelta 乱序、SCC fallback、handle access grace、compressed cage、pressure drain、radix hop bound、link corruption、platform fake 和 symmetric/asymmetric workload。默认测试保持快速、进程内和确定性；重负载与性能测量放在 benchmark。

## 参考

- [snmalloc README](https://github.com/microsoft/snmalloc#readme)
- [snmalloc 论文](https://doi.org/10.1145/3315573.3329980)
- [snmalloc remote allocator](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/remoteallocator.h)
- [snmalloc remote deallocation cache](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/remotecache.h)
- [snmalloc MPSC queue](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/freelist_queue.h)
- [snmalloc free-list protection](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/mem/freelist.h)
- [snmalloc combining lock](https://github.com/microsoft/snmalloc/blob/main/src/snmalloc/ds/combininglock.h)
- [Gugu 内存消息化内部规范](../src/internals/memory-messaging.md)
