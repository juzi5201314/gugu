# GC 元数据

本章规定编译器和 Gugu runtime 共享的具体类型描述符、heap object header、Mosaic 三层 managed storage、trace program、全局根、vtable/glue 引用、handle representation 和写屏障 metadata。语言层的移动、pin、root 与 resource 语义见[内存与对象模型](../spec/memory.md)；本章固定当前实现，不构成跨编译器版本 ABI。

## 权威边界

[内存与对象模型](../spec/memory.md)、[值传递](../spec/passing.md)和[运行时](../spec/runtime.md)唯一规定对象寿命、引用有效性、pin、resource release、OOM与公开统计。本文独占官方 runtime 的 Mosaic managed storage、Immix布局、collector阶段、object header、handle representation 和 compiler/runtime metadata编码；这些参数不能回写成用户可依赖的地址、时序或回收保证。

## 总体约束

当前官方 runtime 规范采用精确、分代、并发标记且能够移动对象的 Mosaic GC。managed plane 由
三个可互相提升、但不随意降级的层次组成：

- `TurnRegion`：compiler 证明属于当前 coroutine turn 的私有 managed allocation group，使用 owner-local bump/slab，region 结束时批量 reset；
- `LocalHeap`：继续使用 Immix arena、TLAB、object-start bitmap、mark bitmap、card table 和 direct managed pointer，owner-local 完成 trace、sweep 和不涉及 foreign incoming edge 的 evacuation；
- `SharedHeap`：保存跨 owner、共享身份或无法证明私有性的对象，使用 stable handle/forwarding representation，collector 可以并发迁移 payload。

共同约束如下：

- stack/root 只扫描编译器明确登记的位置，不做保守扫描；
- nursery 的私有对象优先进入 `TurnRegion`，无法证明私有性或已发布的对象进入 `LocalHeap`；
- `LocalHeap` nursery 对象在 minor cycle 复制，old Immix arena 在 owner-local 并发标记后按 block/line 存活率选择 evacuation；
- `SharedHeap` 的跨 owner mark 使用 `MarkTicket` mailbox，跨 block 活跃边使用批量 `EdgeDelta`，全局终止使用 owner credit；
- pinned、正在普通 `ForeignBridge` 或 `ForeignBridge[DirtyCpu]` 边界暴露和超大对象可以留在 non-moving region；`ForeignLeaf` 传递的可移动对象地址仍必须由调用方 pin；
- `LocalHeap` 的普通 direct pointer 不使用 read barrier；`SharedHeap` 的 handle resolve/access guard 是共享对象路径的必要屏障；
- managed pointer、stable handle 或 compressed reference 移动后，安全引用仍必须通过本地 field/root 更新或 handle forwarding 指向原语义对象；
- raw pointer 不参与追踪，跨 safepoint 必须由 pin 或规范允许的短生命周期保证；
- 对象级 incoming reference 不使用普通 reference counting；block-level incoming lease 只用于生成 collection candidate，不能直接宣告对象死亡。

runtime 和 compiler 只能通过本章定义的 section/schema 交换静态 metadata。runtime 动态
heap bitmap、mark queue、mark mailbox、card table、handle table、lease summary 和 pin
side table 不写入镜像。

## owner-directed block return

managed plane 的消息分为 GC 工作消息与内存归还消息。`MarkTicket`、`EdgeDelta`、
`RegionTransfer` 和 `HandleForward` 用于共享图的标记、发布、转发和压缩；
`HeapBlockReturn`、`HeapLineRunReturn`、`HeapArenaReturn` 和 `LargeMappingReturn` 用于
归还已经完成 lease/forwarding 的物理范围。所有消息只携带稳定 descriptor、ObjectId、
stable handle、slot/index、generation、epoch、bytes 和 integrity 字段，不能携带 managed
object 裸地址、未登记 interior pointer 或指向可移动 field 的地址。

`TurnRegion` 的 `RegionTransfer` 是 managed ownership descriptor，不是 raw free 消息。
发送前必须发布 export summary 并确认 region 内没有 sender 仍需访问的外部 alias；如果
语言语义要求 sender 继续使用，compiler 必须先 promote 或复制，不能发布 transfer。

完整 block 默认以 `HeapBlockReturn` 批量返还 allocation owner；四类 unit 全部经 owner inbox
走 `ReturnPending → OwnedFree → Free`，同 owner 也发消息，exactly-once 与字节账本只认
consume。`HeapBlockReturn` 的发布门禁是八项归零：incoming/allocator/scanner/evacuation
四类 lease、pin 计数、resource 实例数、块内 live line 与候选绑定 `candidate_job`；任一项未
归零都拒绝发布，因此归还不会与仍在进行的扫描、分配、疏散或 pin 竞争同一块。
`HeapLineRunReturn` 只恢复块内 bump 区间（把 `LINE_QUEUED` 还原成可分配、并把 bump 游标
退回 run 起点），不移动物理页，因此不入字节账本。`HeapArenaReturn` 表示整区已空：发布前
要求 64 个 block 全部 `Free`，消费时复核状态、结清仍未归还的 extent，并摘除世界级 arena
登记与 LocalHeap arena（块槽位与位图整体丢弃，descriptor 永不复用）。`LargeMappingReturn`
覆盖整段连续块，由 span 起始块统一发布；成员块没有 object-start 与独立标记，候选与 dirty
通知一律折回起始块，避免成员块被单独试验删除判死。

四类 unit 的 decommit 都只发生在 allocator/scanner/forwarder 三路 lease 归零、没有 live 或
queued slot、没有在途 return 且 queue-page grace 走完之后；extent 在归还门禁之前就从块上
解绑，复用已 trim 的块时由分配路径重新提交物理页。managed 物理页的字节账本跟随**页的提交
owner**，不随消息路由或管理权转移迁移。block return、allocation debt、pending bytes 和
owner handoff 的完整协议由[内存所有权与消息通道](memory-messaging.md)独占；本章继续
独占 Immix metadata、trace、barrier、handle representation 和 collector lease 的布局
与阶段。

### Mosaic managed storage metadata


`TurnRegion` 使用稳定 region descriptor 和 owner-local bump cursor。descriptor 至少包含
owner token、region generation、capacity、used bytes、export summary、active transfer
lease 和 reset state。region payload 只容纳已由 `EscapeAndPlacement` 证明为当前 coroutine
turn 私有的 managed allocation；它不进入普通 mark bitmap，也不能包含必须独立 release
的 ResourceCell。region 的 object-start metadata 仍按 concrete `TypeId` 记录，使 promote
和验证可以使用同一 trace/value program。当前控制区为 384 字节：104 为 next、112 为 major epoch、120 为活动对象 pin 数，128 起的 256 字节保存 32 KiB 区域的 object-start 位图。coordinator+176 保存登记链；快慢分配都在同一分配操作中发布对象起点，reset 和 transfer 均要求 pin 数为零。

`std.mem.LocalArena`/`SyncArena` 的用户区域与 TurnRegion 复用同一控制页布局与全局登记链：数据段按区域字节数对齐、控制页紧随，对象头（16 字节）与 object-start 位图公式一致，因此 `coordinator_region_*` 的标记/扫描/清扫原样适用。差别是三个槽位按 arena 语义重载：+32 记 owner 协程槽（LocalArena 校验，`gc_arena_owner_check`；SyncArena 恒 0 放行），+40 记 arena 自己的 bump cursor（替代 processor 侧 cursor，SyncArena 用 CAS 推进），+48 记 arena 内部区域链（`gc_arena_grow` 追加、reset/destroy 遍历整条链）；+96 仍是真实的 payload 小计，+104 仍是全局登记链 `heap_region_lookup` 所需。句柄 `_region` 指向数据段末尾（control-16），不是对象起点——`heap_region_extent_lookup` 只做区域覆盖范围的查找，内部根命中即经 `coordinator_region_mark` 整区保活；句柄不可达后区域按 `mark_epoch != epoch` 清扫。

区域链方向固定为**旧尾指向新尾**：`gc_arena_grow` 打开的新区域 NEXT 为 0，由调用方把 `control.ARENA_NEXT` 写成新区域——`gc_arena_alloc` 为无竞争路径直存，`gc_arena_alloc_sync` 用 CAS 竞争 0→grown 槽位，失败的分配者沿用胜者链接的区域；反方向链接会在多协程 grow 下形成环。新建区域把 `mark_epoch` 预置为 `(coordinator+144 当前 epoch + 1) | REGION_MARK_BORN_BIT`（1<<63）：裸存 `epoch+1` 会让 `coordinator_region_mark` 的 `== epoch` 早退，跳过区域对象的字段扫描而漏标受管引用；预置"带 born 位的 epoch"使 mark 路径先扫描对象再写入净 epoch，sweep 比较时对 born 位取掩码（`stamped & ~BORN != epoch`），保证新建区域免被同轮 sweep 回收但在下一轮正常老化。控制页只有 256 字节 object-start 位图（2048 granule × 16B = 32 KiB 覆盖），`region_bytes` 因此钳位到 `REGION_BITMAP_COVER_BYTES = 32768`；`bytes + object_header` 超出 32 KiB 的单对象在 `gc_arena_alloc`/`gc_arena_alloc_sync` 入口走 `gc_arena_alloc_managed` 回退到受管堆（`gc_alloc_slow` LocalHeap），位图不得写越控制页。STATE(u32) 高位 `REGION_STATE_ARENA_BIT` 标记 arena 区域——+48 对 TurnRegion 是 RESETS 计数、对 arena 才是链指针，判别只能靠该位；`coordinator_region_mark` 标记到 arena 区域后沿 `ARENA_NEXT` 把整链逐个标记+扫描对象，扩容区域随句柄可达的首区域同生共死（只根首区域时后续区域不再两轮后被误收）。工作列表容量只受宿主内存限制：coordinator 页内队列仍只容纳 384 项，写满后 `coordinator_push` 把条目追加到宿主页链（每页 510 槽，`[0]=next、[8]=count`），minor/major drain 先扫页内条目再按链序扫溢出页并逐页 `mapping.release`；扫描中的追加只发生在链尾，`next` 在扫描后读取、页内上界逐次重读，多区域 arena 或任意 >384 活对象的图不再让回收中止。collect.gg 已依赖 heap.gg 不能反向引用，`ARENA_MAJOR_CYCLES_OFFSET` 与 born 位以本地常量镜像。

minor 扫描登记区域的对象字段并更新年轻引用。major 按内部根解析精确对象，按 epoch 保留所在区域并将其对象加入同一 trace 工作列表；当前分配区和活动 transfer/pin 区域同样保活。无根、无门禁且已退役的区域先从登记链摘除，再整体释放数据与控制页，分别扣减 heap 与非 heap committed，并撤销 region payload 小计。字段屏障按登记范围识别区域，不推算不存在的 arena metadata。

region state 只能按下列方向转换：

```text
Private → Publishing → LocalPromote
Private → Publishing → RegionTransfer → Received
Private → Publishing → ResetPending → Reset
```

export summary 是五位掩码：`external-alias`、`resource-lease`、`ffi-address`、`pending-transfer`、`live-root`。
只有五位全为 0 时 summary 才算闭合。`Publishing` 前必须登记所有可能逃逸的 root、slot 和
identity handle；编译器声明的位来自 `RegionPublish` 的操作数，运行时观察到的位来自 owner-local
事实（外部 alias、ResourceCell lease、pin/foreign 地址、在途 transfer、live root root set）。
`reset` 因此必须同时满足：状态为 `Publishing`、transfer lease 为 0、summary 闭合。任一条不满足
时不得释放 region，调用方必须走 `LocalPromote`。若 sender 继续需要访问对象，compiler/runtime
必须走 `LocalPromote` 或语义复制，不能发布 `RegionTransfer`。receiver 取得 region generation 后
成为唯一 runtime owner；transfer message 未消费前，region bytes 属于 pending pressure，不能
reset 或复用，两侧 owner 账本在接收方采纳后才一起移动。

### LocalHeap arena 与 LargeMapping {#localheap-large-mapping}

LocalHeap 的数据段按 `arena_bytes` 对齐；当前记录布局的 side metadata 为 55,576 字节，随后是每个 block 64 字节的 `HeapBlockRecord`，最后是独立的 `LargeMappingRecord` 表。表容量固定为 64，每条记录 48 字节：`base:u64`、`payload:u64`、`bytes:u64`、`generation:u32`、`state:u32`、`mapped_bytes:u64` 和 `mark_epoch:u64`。这些字段是 range identity、payload identity 与生命周期账本，不是可跨 safepoint 保存的用户引用；表不参加普通 block 的 object-start、mark 或 line-live 位图。

请求的 `header + align - 1 + payload_bytes` 超过单个 Immix block，或 `align` 超过配置的 `large_align_limit` 时，分配器进入 LargeMapping slow path。它以平台页为最小粒度向上取整总范围，按 `max(align, page_bytes)` reserve 并 commit 整段，在 payload 前写入统一的 16 字节 object header、清零 payload，再以 release 顺序将记录状态置为 `ACTIVE`。`payload = align_up(base + header, mapping_align)`；header 的 `payload_size_or_forward` 保存实际 payload 字节数，不能以 block size 或类型静态 size 替代运行时请求。空间或记录槽不足走既有 runtime fatal，不截断、回退到普通 block 或返回伪造地址。
普通 block slow path 通过 `gc_refill_line(kind, payload_bytes, align, flags)` 取得已对齐 run 起点：先检查当前 TLAB 的 `cursor/limit`，只有请求放不下时才提交并打开下一个 block，同时回写 processor 的 TLAB。返回 0 只表示没有可用 block 或请求不能容纳；`gc_alloc_slow` 不得把它解释成可继续写入的伪地址。
动态数组或 `alloc_bytes` 的逻辑长度为零时，LIR 仍保留零长度，但把实际分配 payload 钳位为 8 字节；对象头必须记录非零物理大小，collector 才能解析和搬迁该 backing。不得把最小物理 payload 当作语言级元素数，也不得以放宽 collector 的零大小校验掩盖非法对象头。

镜像当前还提供 `gc_move_one(root)` 这一受限复制原语：读取真实 header 的 type/size，按请求字节数复制 payload，先在新对象完成初始化，再以 forwarding 标志和新 payload 地址发布旧 header；重复命中直接返回既有 forwarding。LargeMapping 不伪装成普通 block 搬迁，调用方必须走其独立 range 生命周期。该原语只负责一个对象的复制与 forwarding 发布，root/field 的回写和 cycle 级 work/credit 仍由 collector 协议负责。
收集器通过 nursery/old 登记表解析 `payload <= address < payload + bytes` 的首地址与内部引用，按本轮 epoch 去重并加入同一 descriptor 工作列表。minor 扫描有效大对象字段以更新年轻引用，major 只标记根图可达的大对象；pin 与普通对象共用 allocation 租约表。major 在字段遍历闭合后，把无本轮标记且无固定租约的记录撤销，再以登记的实际 `mapped_bytes` 整体释放映射，扣减 reservation、committed 和 heap 分区，不影响文件小计。重复扫描跳过已撤销记录；槽重用递增 generation。大对象不能通过普通 block candidate 或 line return 单独释放。字段写屏障先查询已登记的大对象范围，命中时不读取地址附近假定的 arena metadata，也不生成普通 block 卡片；其年轻字段由 minor 的大对象表扫描更新。

### 镜像执行边界与 block 账本 {#image-execution-boundary-block-ledger}

LocalHeap 的分配、minor evacuation、major exact mark/sweep、trace cursor、barrier scratch、
MarkTicket、EdgeDelta、candidate、pacing 与四类 block return 均由 `std/runtime/*.gg` 执行。
compiler 侧只保留 `local_heap_schema`、header 编码、metadata section、descriptor verifier 与
负向契约检查；不存在与镜像算法并行的 Rust world/LocalHeap 执行体。arena side metadata 在
固定 side 区后追加 `HeapBlockRecord`：record 保存 block identity、generation、commit 状态和
当前 state；打开 block 只允许 `Free -> Allocating`，minor reset 只有在 scanner/allocator/
forwarder lease、pin、resource 与 candidate 全部清零后才可回到 `Free`。major sweep 逐 object
检查 object-start 与 mark bitmap，再重建 line-live、page-cover、card 与 block state；活对象
不因 block 扫描顺序改变，未标记对象只在 forwarding/candidate 门禁闭合后清除。
每次 major 进入时必须先清空 coordinator 的 worklist count，再开始本轮 mark epoch 和根遍历；
minor 留下的扫描项以及上一轮 major 的对象地址都不是本轮根。固定三对象回归执行真实
major 根遍历、descriptor trace、mark bitmap 与 sweep：可达自环容器保留文件资源，
不可达文件触发一次终结；撤销全部强根后的下一轮回收容器内文件，第三轮不重复终结。
根记录由真实 `gc_walk_roots` 生成：权威编码器构造两帧栈图，按非零加载偏移解析 section
与代码基址，首帧消费 suspend-resume 点的寄存器根和初始化门控槽，调用者按 call-return 点
枚举内部指针根，直到返回地址落在函数表外。未登记字和失效门控槽即使残留资源地址也不参与
保活；容量不足时 `needed` 保留完整根数，截断的 `written` 不交给 GC。类型段、帧和寄存器
保存区由夹具提供，关闭提交与平台释放由钩子控制；此项覆盖扫描器的 LIR 执行。
实际文件 API 镜像另经 `decode_tables` 检查：存在带根的 CallReturn 图，CallReturn/Suspend
图的五类寄存器掩码均为零，符合调用点强制根落栈的分配规则。因此 `gc_dispatch` 扫描
CallReturn 帧时传空寄存器保存区；合成 SuspendResume 寄存器根用例验证 walker 的能力，
不表示当前文件镜像使用该保存协议。单 managed owner 的活跃协程通过 processor+4000 注册，
启动主协程与子协程入队前登记，完成后摘除。dispatcher 合并发起者和动态根后，继续枚举其它
已启动协程保存的 context.rsp/rip，以 CallReturn 栈图追加根；未启动协程的入口包由动态根表保活，
不扫描入口哨兵。追加复用原栈根缓冲，容量不足进入不变量失败，不接受截断根集合。
固定暂停栈回归验证仅从另一协程栈可达的文件在 major 中保留，注销后下一轮才关闭；Join 结果根
与栈登记独立。`gc_enter` 的同一生产指令序列在编码前执行确定性验证：
发布 mutator RSP/R14、切换系统栈与 descriptor、传递 request/entry_rsp，dispatcher 钩子
改写栈根并破坏易失寄存器后，入口重新加载 coordinator、恢复 RSP/R14，保留 R15 和返回
状态；成功和非零状态都覆盖。该验证不执行宿主机器码，亦不声称测试了独立寄存器根保存协议。

`std.mem.pin` 先经 `pin_prepare` 在系统栈完成一次 minor 提升，调用方安全引用在精确根图内回写，之后才转换地址并执行 `pin_acquire`。old arena 内首地址与内部地址解析为同一个对象，16 字节记录保存 payload 与嵌套计数；4 KiB 登记页通过 coordinator+168 的双向链管理，空页立即归还共同 provider，不能计入文件小计。对象首次取得租约时置 `CONTROL_PINNED`，最后释放清位；major 在用户根之外扫描活动 pin 记录并追踪字段。标准库 defer 在正常返回和 panic 展开时各走一次 release；清理重新读取安全引用槽，禁止提前把根寿命折叠为整数。静态和协程栈已有非移动存储，继续由原安全引用保持生命周期；TurnRegion、LargeMapping 与 ordinary old 对象共用显式 pin 租约。需求统计依据冻结 HIR 中 `std/runtime/pinning.gg` 的定义身份解析实际 acquire/release 实例，统计优化后 LIR 的普通调用、Invoke 与 TailCall；同名用户函数不计入。无生成方、无执行入口的 GIR/LIR Pin/Unpin 伪操作已移除。

minor 在重置 nursery 之前 flush 当前 owner 的 card staging，按脏卡片覆盖的完整 old 对象运行同一 descriptor trace，并将新复制对象加入既有工作列表直到闭合。已进入 old 存储的 aging 对象也必须登记新写的年轻引用。卡片只在覆盖对象扫描后清零，不能仅清 staging 后就宣称消费完成；固定对象图回归验证 pinned 容器的新字段经 minor 搬迁后仍保持值，容器本身地址不变。

x64 普通对象快分配在对象头写入之后、返回 payload 之前，用 arena 相对 header granule
置 object-start 位；整个序列没有 safepoint。lowering revision 7 失效旧片段，侧元数据位图
偏移与 Gugu 常量交叉校验。机器序列解释并装配回归覆盖位图字/块边界和 arena 尾部，
置位保留其它对象的标志。慢分配与 collector 搬运继续使用同一位图。
`gc_heap_live_bytes` 读取 nursery/old 的 object-start，跳过 forwarded 旧副本，累加载荷长度，
并加入有效大对象记录的 payload 字节；同一 arena 不重复遍历。只读取置位对象头，空位不访问
未提交数据页；不把页提交量、对象头和对齐间隙算作 payload。minor/major 完成后将观测写入
coordinator+152。三对象真实 major 回归验证载荷 32→24→0，大对象回归验证 40000 字节
载荷与更大的页提交量分开；采样不进行额外收集。TurnRegion 打开时先提交数据页再写对象头，
数据归 heap 分区、控制记录归非 heap；快慢分配都累加实际 payload，区域控制+96 保存本区总量，
processor 的区域载荷计数保留全部尚未 reset 的区域。reset 门禁通过后扣载荷并清本区计数，
promote 保留载荷，已提交页不因逻辑 reset 被假装释放。机器快路径同时更新区域对象数，
测试覆盖动态七字节载荷；实际 LIR 测试区分 384 字节控制描述符与一页物理计费。

Gugu image 的 coordinator 以固定 worklist 和 owner credit 追踪标记收敛：根、barrier、
MarkTicket 与 EdgeDelta 各自增加可追踪 credit，消费完成后归还；只有 worklist、已发布 batch、
mailbox、barrier scratch、producer epoch、forwarding work 与 pending credit 同时归零才结束
cycle。`gc_image.gg` 通过 action key 的 `runtime_gc_case` 选择 Linux PIE 探针；探针输出不是
算法后门，而是对每条状态转移、错误边界和成功输出的固定观察面。
屏障在 processor 页的 `+3560` 保存固定 scratch 地址；scratch 头部为 `count:u64`、`overflow:u64`、`publish_epoch:u64`，`+64` 起为 512 条 32 字节记录（source block、target block、kind、sequence）。ordinary/reserved 两个入口都复用同一追加路径，满载只置 overflow 而不覆盖旧记录；flush/credit owner 尚未接入前，overflow 会阻止把该 scratch 误报为已清空。

`SharedHeap` 使用 stable handle table。handle 是逻辑 object identity，不是用户可观察的
整数；其内部键至少包含 table/cage 标识、slot 或 object id 和 generation。slot 保存
当前 payload descriptor、TypeId、generation、access-guard 状态和 forwarding lease。对象
迁移只在 handle slot 的线性化点替换 current payload；旧 payload 在全部 access guard、
pin、mark ticket 和 forwarding grace 结束前不能复用。generation 变化时旧 handle 进入
`StaleHandle` verifier 路径，不能静默解析到复用对象。

当前镜像执行体以 8 KiB 表页保存 64 个既有布局的 handle slot、独立的物理地址登记和 payload identity 记录；coordinator+184 为表链，+192 为单调表编号。共享 payload 走 LargeMapping 非移动分配，GC 复用同一 descriptor/mark/sweep，handle 根先 checked 解析，访问与初始化租约作为强根。映射实际回收后 slot 才进入 OwnedFree，下次复用推进 generation；过期 handle 不能按槽号访问新对象。`SharedAccessBegin` 返回载荷地址，`SharedAccessEnd` 显式携带同一 handle，成功 begin 更新共同 HANDLE_RESOLVES 计数。字段屏障验证当前访问租约，minor 扫描非移动映射字段。首个 pin 记录增加共享 slot 的 pin 门禁，最后一个释放同步清门禁。表+16 保存活跃/初始化 slot 数；空表计入共同 reclaimable 字节，复用时扣除，压力 sweep 先摘链再归还空表，两笔计数都只转换一次。

`LocalHeap` 的 direct managed pointer 不经过 read barrier。只有 SharedHeap 的 handle
resolve、compressed reference decode 或 forwarding guard 需要额外访问步骤；compiler
不能把 shared direct pointer 带出 guard。handle table 和 forwarding metadata 是 runtime
动态状态，不写入镜像；trace program 仍按逻辑字段扫描，不维护第二套字段描述。

compression profile 可以将连续不超过 4 GiB 的 managed heap cage 表示为 32-bit offset，
并以 cage id、generation 和 offset 形成内部引用编码。cage base、长度、对齐、offset
加法和 generation 校验全部 checked；large、pinned、foreign、跨 cage 对象使用完整地址
或 stable handle。compressed reference 不能进入 raw plane，也不能绕过 FFI 的 resolve、
pin 和 native address 生命周期。同一引用类型的值可指向压缩对象也可指向 cage 外对象，
因此类型驱动的压缩表示不健全：引用值的 provenance、ABI 车道与栈根声明始终按完整地址
声明，压缩只发生在被 header 标记为 `COMPRESSED_REF` 的对象内存字节里（当前即压缩闭包
环境的 capture 槽）。

### GC 工作消息与终止信用

每个 owner 维护单 consumer 的 `MarkMailbox`。跨 owner 的可达边发布 `MarkTicket`，跨
block 的新增/删除引用发布聚合后的 `EdgeDelta`。消息只携带 cycle、owner generation、
stable object/block identity、epoch、credit 和 integrity，不携带可移动 payload 地址。

每个 cycle 由 coordinator 分配有限且可追踪的 owner credit。root seed、mutator barrier、
MarkTicket 和 EdgeDelta 消费都会产生或归还 credit；只有本地 worklist、已发布 batch、
所有 mailbox、barrier buffer 和 pending credit 同时清空，coordinator 才能宣布标记终止。
“当前 mailbox 为空”不是 cycle 完成条件。

block-level incoming lease 只用于把 block 标记为 collection candidate，不是对象级
reference counting。candidate block 仍需 exact local trace、cycle/SCC 检测、pin/resource
检查以及 scanner/allocator/evacuation lease 验证，才可以进入 `HeapBlockReturn`。

## compressed reference 与 handle verifier

每次 SharedHeap resolve 必须建立 access guard；guard 结束前 current payload 保持有效，
并发 forwarding 只能将 guard 指向新 payload 或保留旧 payload。pin 把 access guard 提升
为不移动 lease，传给 foreign code 的地址在 lease 结束前不能改变。没有 guard 或 pin 的
旧 direct pointer 不属于合法 managed reference。

handle、compressed reference、MarkTicket、EdgeDelta 和 RegionTransfer 的 verifier 必须
共同检查 cycle/topology epoch、owner generation、object/block generation、cage range、
TypeId、state、lease、integrity 和 exactly-once 状态；检查失败进入 `RuntimeInvariant`，
不能退化为普通对象扫描或 raw free。

cage profile 开启时 `MANAGED_LOCAL` arena 以 island 形式从登记 cage 切出（粒度是 GC arena
字节数），压缩引用槽保存的是 cage 相对的 `cage id | generation | offset` 编码字；根种类
判别值 5（`compressed-ref`）与栈图判别值同源。world 在 root slice 校验、mark seeding 与
搬迁回写三处只经同一条 checked 解码/编码路径读写它：空字解码为空引用，未登记 cage、
过期 generation、越界 offset 或落在 canonical hole 的地址进入 `RuntimeInvariant`，回写时
指向 cage 外对象的地址同样失败，绝不把编码字当地址使用。`MANAGED_SHARED` arena 永不成岛，
因此压缩引用不能绕过 handle resolve；cage reservation 只增加 `range_reserved_bytes`，
island 内 extent 提交才进入 `runtime_committed_bytes`，`compressed_ref_decodes` 与
`compression_decode_rejections` 分别统计成功解码与拒绝。

`RuntimeRawModel`（query 30）当前为 schema 23，在同一 `RuntimeRawContractV1` 中并入
`SharedHeapRuntimeContract`（schema 1，profile `mosaic-shared-handle` revision 1）：stable
handle 身份按高 4 位 tag `0xA`、12 位 table、24 位 generation、24 位 slot 编码，table 是逻辑
表身份而不是宿主地址；`SharedHandleSlot` 固定 64 byte / 64 byte 对齐，登记 generation、状态、
current/old payload identity、forward generation、grace epoch、access guard、pin、mark ticket、
forwarding lease 与 owner/block/payload 字节数，`SharedPayloadRecord` 固定 32 byte 且只含
payload identity、block/offset、generation、owner、bytes 与状态，两者由内建
`std/runtime/heap.gg` 逐字段交叉校验。slot 状态目录固定为
`free`/`live`/`forwarding`/`grace`/`reclaimable`/`owned-free`，唯一允许的迁移是
`free -> live`、`live -> forwarding`、`forwarding -> grace`、`grace -> live`、
`live -> owned-free`、`grace -> reclaimable -> live` 与已释放槽的下一次发布
`owned-free -> live`（发布时推进 handle generation，因此复用不会让旧 handle 重新生效）；
forwarding grace 步数为 4，只有 grace 走满且 access guard、pin lease、mark ticket 与
forwarding lease 全部为 0 才释放旧 payload。`SharedHeapDemand` 由优化后 LIR 推导并强制
`alloc_sites == resolve_sites == handle_slots`、`access_begin_sites == access_end_sites`、
`payload_copy_sites == forward_sites`。`HandleForward` 作为 GC 工作消息族的判别值 5 登记
16 个字段（handle table/slot、handle/forward generation、old/new payload identity、
cycle/topology epoch、target owner 身份、bytes、state、integrity、family），字段集合禁止携带
managed 或 raw 地址；mark ticket 的 `ticket_sites` 与 `mark_sites`、共享字段屏障站点与
`barrier_sites` 由契约交叉校验。`runtime/shared_heap.rs` 是这套身份的确定性参照实现：
payload 与 slot 都是稠密编号，free 槽用稠密栈复用，generation 是唯一的 ABA 防线，
`resolve_payload` 只接受当前 heap 产生的 fresh identity，重复解析、过期 table/slot/generation、
被 pin 的 forward、非递增 forward generation 与提前回收都返回不变量失败。

共享对象的 block 身份由世界级 registry 分配：descriptor 从 `SHARED_DESCRIPTOR_BASE`（`1 << 24`）
以上的独立编号段单调推进，同一 owner 的 payload 依次落在同一个共享 block 的不同偏移上，
因此共享 block 与 LocalHeap arena 共用同一条 `descriptor * 64 + index` 编码却永不撞车。registry
按 handle slot 稠密索引，`table` 与 `generation` 一起构成索引键；跨 owner 的 mark ticket 在写任何
标记前先经它校验目标 owner 与是否已交还，handle 过期或 slot 未登记都直接失败。`MarkTicket`
的目标因此分成两种形式：owner-local 用 arena descriptor + header 偏移 + block generation，
跨 owner 共享对象用 handle table + slot + handle generation；`target-kind` 与两者一起进入
integrity 摘要，消费端按判别值分支：local 目标入队到 owner worklist，shared 目标只调用
`SharedHeap::mark_ticket` 并在同一分支内 `finish_mark_ticket` 结清 lease。`HandleForward` 使用
独立的域分离摘要键 `gugu-handle-forward-integrity-v1`（取字节 12..16，与 card/region/mark 三族
不共用前缀），车道承载 handle table/slot、handle/forward generation、old/new payload identity、
cycle/topology epoch 与 bytes，两条 payload identity 各占一条 64-bit 车道并原样往返。

共享 payload 的字段写入与本地字段走**同一条** hybrid barrier 路径：共享 block 在分配时就以
payload owner 为 manager 登记 card table，字段写入先经 guard 读旧值、再写 payload、最后执行
old/new shade、card 记账与 edge summary 记账；写入者不是 payload owner 时，冲刷会以 `CardMark`
批次投给 payload owner 而不是写进写入者自己的表。共享 payload 是稳定存储（不随 minor 搬迁），
因此它永远按 old generation 记账；trace 中的 `SharedFieldBarrier` 与这里的记账是同一次写入的
两个视图。

共享平面的世界侧闭环与 LocalHeap sweep 同一步，都在 mark cycle 收敛并关闭之后：世界 registry
除按 handle slot 索引的登记项外，还按 descriptor 稠密索引 block 记录，记录 bump 游标
（`used_bytes`）、仍在使用的字节（`live_bytes`，含仍在 grace 中的旧 payload）、已真正释放的字节
（`dead_bytes`）、存活 payload 数与是否已封口（`sealed`），切换填充 block 时就地给旧 block 封口。
搬迁的其余步骤都在这一层：`forward_shared_payload` 先过登记项、slot 状态与 pin 门禁，用 registry
预留目标位置并分配 payload，再让参照实现完成复制与 slot 切换，最后把旧位置记成待结清记录并发布
`HandleForward`；`service_handle_forward` 由 payload owner 消费，在目标 token、integrity、
topology/cycle epoch、登记项与在飞记录全部通过之后才结清 forwarding lease、推进 grace；
`settle_shared_forwards` 负责重试同一轮未结清的记录——guard、pin 或票据仍持有旧 payload 时只把
对象标成已交还，留给后续 cycle。sweep 的释放条件是「本 cycle 未标记 + 状态 live + 无旧 payload +
全部 lease 归零 + 无在飞搬迁」，搬迁的判据是「已封口、有存活 payload、`dead_bytes > 0` 且
`live_bytes <= dead_bytes`」，被 pin 的 payload 计入推迟并让该 block 本轮无法归零。`HandleForward`
的投递目标是 payload/block owner（与 mark ticket、card batch 同源）；owner 交接时 registry owner
与 card table manager 在同一个线性化点一起改，因此 CardMark 与 mark ticket 都跟着新 manager
路由。block 身份解析因此按编号段分流：共享 descriptor 的 owner/manager 由 registry 回答，
共享 block 不进入 LocalHeap 块状态与候选回收。在飞的 handle 搬迁与 region transfer 同属
「已经离开生产者、未被目标消费」的 GC 工作，一起计入 mark 终止的 forwarding work 条件。

`runtime::world::shared_forward_tests` 是这一层的确定性回归：搬迁发布通知并在目标 owner 消费后
结清 lease 与 grace、guard 期间旧 payload 保持有效且 guard 结束后才回收、pin 推迟搬迁并让 block
无法归零、错误目标/篡改校验/过期 handle/跳号 generation/重放全部干净拒绝且状态不变、sweep 只释放
未标记对象、guard 或在飞搬迁推迟释放、死字节不少于活字节的 block 被搬空、管理权转移带走
registry owner 与 card table manager、在飞搬迁阻塞 mark 终止，以及只有 LocalHeap 对象的世界零共享
状态。`SharedForwardHarness` 与 `benches/shared_forward.rs` 用真实夹具编译产物驱动同一条路径并打印
吞吐，不进默认测试套件。

`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `shared-heap-contract-fingerprint`、`shared-heap-demand`、`shared-heap-profile`、`shared-heap-profile-revision`、
`shared-heap-handle-tag`、`shared-heap-slot-bytes`、`shared-heap-payload-record-bytes`、
`shared-heap-forwarding-grace-steps`、`shared-heap-state-count`、
`shared-heap-transition-count`、`shared-heap-forward-fields` 与 `shared-heap-records`。
`runtime::shared_heap_tests` 覆盖 guard 期间旧 payload 可读、四步 grace 后才能回收、
guard/pin/mark ticket 阻止提前回收、pin 推迟 forward 且不改变 generation、重复解析与过期
身份拒绝、释放后复用推进 generation、搬迁保持字节并线性化 current payload，以及每个 cycle
至多一次 side mark。

## `TypeId` 与 descriptor table

单态化闭合后，编译器按[单态化与编译缓存](monomorphization-cache.md#concrete-type-set-typeid)的 `StableTypeKey` 顺序分配 `TypeId`。每个 `0..type_id_count()` 值在 type section 中恰有一条固定 80 字节 `TypeRecord`；记录顺序就是 `TypeId`，不重复保存数字 ID。

`TypeRecord` 按小端编码：

```text
size:                 u64
align:                u32
flags:                u32
name_offset:          u32
name_len:             u32
trace_offset:         u32
trace_len:            u32
value_program_offset: u32
value_program_len:    u32
copy_glue_rva:        u64
drop_glue_rva:        u64
publish_glue_rva:     u64
reserved0:            u64 = 0
reserved1:            u64 = 0
```

`size` 是普通值的 payload 大小；动态 backing object 设置 `VARIABLE_SIZE`，其实际 allocation size 由 object header 保存。`align` 必须是非零二的幂且可由目标表示。ZST 的 `size` 为 0、`align` 至少为 1；ZST 不单独分配 heap object。

flags 位固定为：

| bit | 名称 | 含义 |
|-----|------|------|
| 0 | `HAS_HEAP_DIRECT` | trace 中存在直接 managed pointer |
| 1 | `HAS_HEAP_INTERIOR` | trace 中存在 interior managed pointer |
| 2 | `HAS_VALUE_ACTIONS` | 语义复制/销毁不是纯 bit copy/no-op |
| 3 | `HAS_RESOURCE` | 包含 resource 租约或 owner |
| 4 | `VARIABLE_SIZE` | allocation payload 大小运行时决定 |
| 5 | `ZERO_SIZED` | `size == 0` |
| 6 | `UNSIZED_VIEW` | 类型只能作为引用/dyn/slice view 的 pointee metadata |
| 7 | `HAS_DEFERRED_RELEASE` | 对象死亡时需进入受限 resource release 队列 |
| 8 | `PIN_SENSITIVE` | 对象 pin/unpin 需要类型专用 glue |

其余位必须为 0。name 是规范要求的 UTF-8 `TypeId.name()` 文本，指向 type section 的 name pool；同名不代表同一类型。

`name_offset/len` 相对 name pool，`trace_offset/len` 相对 trace pool，`value_program_offset/len` 相对 value pool。每个 pool 长度不得超过 `u32::MAX`，每个非空半开范围都必须 checked 位于对应 pool 内；空范围的 offset 和 len 必须同时为 0。`value_program` 描述字段级 copy/drop/publish/resource 动作。三个 glue RVA 为 0 分别表示 bitwise copy、无 drop、无 publish；非零时必须指向本镜像只使用 Gugu 内部 ABI 的 compiler-generated 函数。

## type section

Linux 使用 `.gugu.types`，Windows 使用 `.gugutyp`。header 字段顺序为：

```text
magic:                [u8; 8] = "GUGUTY01"
version:              u16 = 3
pointer_size:         u8 = 8
endian:               u8 = 1
type_count:           u32
record_size:          u32 = 80
reserved:             u32 = 0
records_offset:       u64
trace_pool_offset:    u64
trace_pool_len:       u64
value_pool_offset:    u64
value_pool_len:       u64
name_pool_offset:     u64
name_pool_len:        u64
section_len:          u64
```

所有 pool offset 8 字节对齐、位于 section 内且不重叠。record、program 和 name 按内容稳定排序后布局，但 `TypeRecord` 本身仍按 `TypeId` 顺序。所有 padding 为 0。

type section 的版本 3 把记录固定为 80 byte：`size u64`、`align u32`、`flags u32`（九位类型旗标，见上表）、`name_offset/name_len u32`、`trace_offset/trace_len u32`、`value_offset/value_len u32`、`copy/drop/publish_glue_rva u64` 与两个保留字，记录里不再存放稳定键，记录顺序即 `TypeId`。本表的编码是规范性定义：encoder 只负责产出能被本表解码且语义等价的字节，不要求与表逐字节一致；`trace_kind` 与 type flag 的一致性由 verifier 交叉校验。
零长度数组没有 pointer word，即使元素含 managed 指针，其 direct/interior flags 也必须为零。

`Symbol::TypeDescriptor(key)` 与 `type section` 是同一条记录：符号地址等于 section 起点加 `records_offset` 再加 `type_id * 80`，记录本身就是描述符。编译器不再为每个类型另写一份只含 `size`/`align` 的副本，运行期按该地址读到的字节与 verifier 校验过的记录逐字节相同。`TypeId` 是真空编号（`spec/types.md`）：`type_id[T]()` 在 LIR 期就物化为 `u32` 常量，`as_int()` 之后可作数组下标，不依赖任何链接期地址。

### compiler-storage 类型

闭包环境与 spawn 参数包不是语言类型，但它们是真实分配出去的 managed 对象，因此有 `TypeId`：
在实例闭合期间，编译器为每个「具名定义 + 具体捕获类型列表」生成存储类型记录，与其它具体类型
一起排序编号并冻结；LIR 之后只按同一份键查表，不再自造 descriptor。规范字节沿用 GBC1 风格、
经 `gugu-mono-v1` 域摘要（与语言类型同域），字段为：

```text
u16 tag            = 0x8000（与语言类型 tag 空间不重叠）
u8  role           = 1 闭包环境 / 2 spawn 参数包
u8  representation = 0 direct / 1 compressed / 2 shared-handle
u64 target_len; [u8] target     角色目标稳定身份（定义稳定键）
u64 size
u64 align
u64 field_count
[u64 offset, u8[32] key] * field_count   按 offset 升序
```

闭包环境的槽宽恒为 8 byte：direct 保存 capture cell 地址、compressed 保存 cage 压缩字、
shared-handle 保存 stable handle。direct 与 compressed 两种表示的每个槽都是一个 managed
**直接指针字**，因此它们的 trace metadata 是「从字 0 起连续 `field_count` 个受管直接指针」
（`Pointers { count }`，发出一条覆盖全部槽的 `Direct`），**不得**声明成内联的被捕获类型：
那会让 trace 递归展开该类型的布局，捕获 `[int; 512]` 时展开出 512 个标量、一个受管字都不
登记，descriptor 退化成 `TraceKind::None`，收集器扫不到捕获对象，nursery 重置后它就是复用
内存。shared-handle 表示的槽是身份而不是地址，按「无 direct managed pointer」登记，扫描由
descriptor 驱动的 handle visitor 负责。因此同一闭包的不同具体实例只要有不同捕获类型，就得到不同
`TypeId`；捕获类型与表示策略一起进入身份，不允许仅按 `DefId` 复用。capture cell 与 async 结果
不另发记录：它们的 payload 就是被捕获类型 / async 结果类型本身，逃逸与 placement 已经把它们
物化成该类型的 managed box，为同一布局再发一条 descriptor 只会在类型表里制造第二个身份。

spawn 参数包是原始 ABI 车道缓冲（9 个整数车道 + 8 个 16 byte 浮点车道，车道区 200 byte，加 8
byte 保留共 208 byte、16 byte 对齐），managed 车道由子协程入口的 `MorestackEntry` 栈图描述，
因此包里不登记字段。布局只允许在 `runtime/coroutine_pack.rs` 的 `COROUTINE_PACK_*` 常量里定义：
入口片段、LIR 发包点、冻结类型 descriptor 与调度器复制点都从这同一份常量取大小、对齐和偏移，`runtime/tests.rs` 的镜像断言把它与
`std/runtime/scheduler.gg` 的 `COROUTINE_PACK_BYTES` 钉在一起。

**复制先于入队。** LIR 只把包做成调用方帧上的临时区，调度器在子协程入队前把整包复制到子协程
自持的原始内存，初始帧只携带 `(code, 包副本地址)`：`spawn` 返回后调用方帧即可被复用，子协程
第一次运行往往发生在更晚的 park 之后，读父帧就是悬垂读。复制源与目标在交接期间都可定位，包
的 managed 车道由 typed 根登记负责（`gc:<case>` 探针与 `spawn-reuse` 用例覆盖这条边界）。

## typed 动态根登记 {#typed-dynamic-root-registry}

跨栈存活的值（spawn 参数包、协程结果、channel 槽与等待载荷、select 获胜项、Join 结果、
active TurnRegion、消息、pin 记录）不能靠扫描 scheduler 的整片 raw 区找出来。登记表给出
稳定入口：`[容量, 已发布条数][固定容量记录]`，处理器页只有一个槽位指向表首
（`PROCESSOR_DYNAMIC_ROOTS_OFFSET`）。一条记录是 `identity / slot / descriptor / count /
kind / flags` 六个字：`identity` 是发布者的稳定身份（热记录地址，raw 控制地址本身不是根），
`slot` 是发布者自持、不可移动的真实载荷槽，`descriptor` 是**按类别解释**的载荷描述，`count`
是载荷内的车道/元素数，`flags` 位 0 表示载荷已完整初始化。

spawn 参数包的描述就是 **lane map**：每个车道 3 位，`0` 表示不是根，`n + 1` 表示栈图根种类
判别值 `n`（0=Direct、1=Interior、2=Handle、3=Compressed、4=StackInterior），车道顺序为整数
车道 0..8（步长 8）再接浮点车道（起点 72、步长 16）。编译器在发包点按机器值的 provenance
分类（`coroutine_pack_trace_code`），把 map 作为 `spawn(code, env, trace, result_kind)` 的第三参
交进调度器，第四参是**结果字**里根的 lane map 编码（结果类型不含根车道时为 0）。调度器在
**入队之前**把参数包复制进子协程自持的原始内存、登记参数包记录，并把结果种类记进冷记录的
载荷状态字：包的复制品才是子协程入参的唯一真相，帧里不会再引用父协程的临时缓冲。

结果载荷的死亡窗口是「结束到被取走」：子协程结束时结果值只存在于冷记录里，标量结果内联，
含根车道的结果按机器车道内联在结果槽里（`CoroutineCold::result` 起的 16 字节，正是子协程返回
ABI 携带的两个机器字），根的 lane map 记在载荷状态字里。`finish_hot` 先按状态字就地结清 spawn
参数包登记，再按结果种类发布结果登记（`DYNAMIC_ROOT_KIND_RESULT`，`count` 为 1，车道 0 就是
结果槽本身）；`take_result` 是结果字的唯一读取入口，Join 与 select 都经它取走结果并结清登记，
因此「发布→结清」严格一次。取走时按元素描述符（`TypeRecord`，首字是 payload size）把结果区
整份复制进目标槽，而不是只搬一个机器字：两字载荷（例如带捕获的闭包）必须两字都送达。描述符
size 超过结果区上界时直接 fatal——更大的结果载荷需要子侧装箱，属于尚未接管的迁移缺口。
载荷状态字（`CoroutineCold::payload_state`，偏移 504）位 0 是参数包
发布位、位 1 是结果发布位、位 3 起 3 位是结果根的 lane map 编码；未发布过载荷的协程
（bootstrap/main）三处为零，不允许也无从结清。

固定访客 `gc_visit_dynamic_roots`（`roots.gg`）按 `kind` 解释描述，把载荷里的受管车道逐条写成
标准根记录（`address / kind / value`）：spawn 载荷逐车道读 lane map，只对非零编码的车道取值，
车道地址由包基址加车道偏移算出；结果载荷只有一个车道，地址就是结果槽本身。未知类别、未初始化
载荷、越过车道数的 `count`、结果载荷里非 0 的车道号都直接 fatal——访客宁可失败也不保守扫描。
产出的根记录与帧扫描共用同一缓冲区契约（`needed`/`written` 头与 32 字节记录），因此标记阶段
可以无缝串接栈根与动态根。

发布与结清都以身份为单位，并要求**恰好一次**：身份重复、表满、结清不存在的身份都是不变量
错误，直接 fatal——已发布的载荷绝不能被静默丢弃或重复结清。同一协程的两种载荷在时间上不重叠
（参数包死于结束、结果生于结束），因此可以复用同一个身份：`finish_hot` 先结清参数包、再发布
结果，`take_result` 结清结果，顺序由载荷状态字保证。

第一版只接线 spawn 参数包（`DYNAMIC_ROOT_KIND_SPAWN_PAYLOAD`）与结果字（`DYNAMIC_ROOT_KIND_RESULT`）；
其余类别（channel 已占用槽/等待载荷、select 获胜项、active TurnRegion、消息、pin 记录）的判别值
在该类别的生产者落地时按同一枚举追加，届时各自定义自己的描述形态，并在访客里给出对应解释。
表布局与 `.gg` 侧常量由 `runtime/tests.rs` 的镜像断言钉死，记录布局由 `dynamic_roots_schema.rs`
的 const 断言钉死。

## heap object header

每个非 ZST managed object 的 payload 前紧邻 16 字节 header，payload 地址始终满足类型对齐。高对齐对象所需 padding 位于 allocation 起点与 header 之前，不改变 `header = payload - 16`。

```text
control: AtomicU64
payload_size_or_forward: AtomicU64
```

`control` 位布局固定为：

- bits 0..31：`TypeId`；
- bits 32..35：age，范围 0..15；
- bits 36..37：generation，0 nursery、1 aging、2 old、3 immortal；
- bit 38：`FORWARDED`；
- bit 39：`PINNED`；
- bit 40：`RELEASE_QUEUED`；
- bit 41：`LARGE_OBJECT`；
- bit 42：`HAS_RESOURCE_INSTANCE`；
- bits 43..44：managed representation，0 `LOCAL_DIRECT`、1 `TURN_REGION`、2 `SHARED_HANDLE`、3 `COMPRESSED_REF`；
- bits 45..63：保留，必须为 0。

`TURN_REGION` 对象的 region descriptor、generation、export summary 和 reset lease 位于
动态 metadata，不在 header 复制；`SHARED_HANDLE` 对象的 stable slot 是语言身份的内部
权威，`COMPRESSED_REF` 只能在已登记 cage profile 中使用。representation 不影响
`TypeRecord` 的逻辑 trace/value program，但 header representation 决定字段字的解释：
collector 按 header 选择把字段读成完整地址还是 checked 压缩字，因此 header 与字段
表示必须同源。cage 开启时只有压缩闭包环境（capture 槽确为压缩字的 LocalHeap 分配）
写 `COMPRESSED_REF`；其余 LocalHeap 对象——包括 large、pinned 与其它非压缩分配——
仍然写 `LOCAL_DIRECT`，TurnRegion 对象继续写 `TURN_REGION` 并按 region 私有语义
解释字段。`COMPRESSED_REF` 对象在 LocalHeap 搬迁中必须保留同一表示。

对象存活 mark 的权威按 representation 分层：LocalHeap 使用 arena side mark bitmap 和 arena mark epoch，TurnRegion 私有对象在 export/reset verifier 中以 region root/lease 状态判定，SharedHeap 使用 handle side mark 和 cycle epoch。任何层次都不在 header 复制第二个 mark bit；collector 的 test-and-mark 原子更新对应 side metadata。

未转发时第二个 word 是实际 payload 字节数；固定大小对象必须等于 `TypeRecord.size`，动态对象必须至少覆盖 trace program 读取的所有字段。LocalHeap direct evacuation 的 `FORWARDED` 为 1 时第二个 word 临时保存新 payload 地址，原对象不再按普通 descriptor 扫描；SharedHeap forwarding 则以 stable handle slot 的 current payload 为权威，不把 slot 更新写成 payload header 地址。

`HAS_RESOURCE` 类型和 `HAS_RESOURCE_INSTANCE` 对象不进入 nursery/aging，直接分配到 old resource arena；该 arena可以在 major evacuation中逐对象移动活对象，但死亡对象必须逐一进入受限 release流程，不能整区丢弃。普通无 resource 且 footprint不超过单个 Immix block的对象：若当前 coroutine turn 私有则优先进入 TurnRegion，否则进入 nursery line fast path；large、pinned 和高对齐请求走 slow path。

嵌套 pin 次数存入 runtime pin side table，header 只保存是否非零。pin/unpin 对 side table 和 `PINNED` 的 0↔1 转换在线性化点原子完成，因而不受固定宽度计数上界限制。pin table 以 object identity 点查，属于冷路径；不得为每个未 pin 对象预留独立 entry。

最外层 `pin` 是 safepoint。target 位于 nursery/aging 时，slow path 先把 owner allocation 提升到可固定的 old region，按栈图和 heap descriptor 更新 `p` 及其它强引用，再增加 side-table 计数并设置 `PINNED`；arena 槽 pin 的 owner 是整个 arena backing allocation。已在 old/immortal region 时只更新计数。最外层 unpin 把计数降到 0并清位，但不立即搬移对象，后续 major cycle 才可选择它。

`payload_base = header_base + 16`，高对齐 padding 位于 header之前；layout consumer、FFI lowering与 GC 都只能从统一的 `payload_base` 派生地址，不能各自维护第二套偏移。对外布局是否可见只由 [`repr(C)` 与平台 ABI](../spec/platform-abi.md#repr-c-struct)定义，本节只固定官方 runtime 的私有地址关系。

## ResourceCell slab

ResourceCell不进入 managed heap；它从地址稳定、无 managed pointer的专用 slab分配。每个 cell以 64-byte header开始，字段顺序固定为：

```text
leases:                AtomicU64
state:                 AtomicU32
payload_size:          u32
owner_coroutine:       AtomicU64
release_glue:          u64
release_descriptor_id: u32
slab_class:            u16
payload_align_log2:    u8
flags:                 u8
next_free:             AtomicU64
generation:            u64
reserved:              u64 = 0
```

state bit 0 `SHARED`、bit 1 `CLOSED`、bit 2 `RELEASE_QUEUED`、bit 3 `RELEASE_DONE`、bit 4 `RECLAIMING`，其它位为 0。Local状态下 `owner_coroutine` 是唯一可更新者，lease以 relaxed atomic增减；publish先 release发布 raw payload，再 CAS设置 SHARED并把 owner写为 `u64::MAX`，之后所有 lease增减使用 AcqRel。状态不能返回 Local。lease从 `u64::MAX` 再增进入 `RuntimeInvariant` fatal。

close与最后 lease竞争 `CLOSED` CAS；首个关闭者把 `{ cell, generation, descriptor }` 送入 release queue并设置 `RELEASE_QUEUED`，其它路径只结束自身 lease。worker在 generation匹配时执行一次受限 release并设置 `RELEASE_DONE`。worker与最后 lease随后都调用 `try_reclaim`：只有观察到 `leases == 0 && RELEASE_DONE` 并成功 CAS设置 `RECLAIMING` 的一方才能递增 generation、清理 payload并归还 slot。显式 close时仍存在的 handle继续观察 closed，cell地址不会提前复用。ResourceHandle 同时保存 slab descriptor/index、slab generation 与 cell generation；复用相同 slot 后，旧 generation 的 handle 与 release ticket 必须拒绝，避免 ABA。

slab以 64 KiB页按 64、128、256、512、1024、2048、4096 byte class管理，class包含 header、对齐 padding和 raw payload；超过 4096或 payload对齐超过 64时使用独立 non-moving整页 mapping。每页使用连续 allocation bitmap和 intrusive free index，不把稳定地址放入移动 GC或全局 hash map。release descriptor明确证明 payload无 managed pointer，typed root visitor因此不扫描 cell bytes。

这些 class 阶梯、64-byte header、状态位与 release 描述符 schema 由 compiler 侧 `RuntimeRawContractV1`（schema 2）固定并由 verifier 检查；LIR 资源隔离闸门拒绝 resource 描述符进入 managed region，违规诊断为 `E0059`。runtime 侧实现复用同一 schema 与 verifier，不在本章另立第二份定义。

## heap arena 与 side metadata

普通 heap 以 2 MiB 对齐 arena管理；每个 arena固定含 64 个 32 KiB Immix block，每个 block含 256 条 128 byte line，基础 allocation granule为 16 byte，宿主页按 4 KiB计算。arena state封闭为 `Free`、`Nursery`、`Aging`、`Old`、`Resource`、`Pinned` 和 `Evacuating`；block另有 `Free`、`Allocating`、`Marked`、`Sweeping`、`Evacuating`。普通对象的 header+padding+payload不得跨 32 KiB block；超过该 footprint、对齐超过 4096或显式 large/pinned 的请求使用独立整页 mapping。

每个 2 MiB arena 的 side metadata 固定包含：

- 131072 bit object-start bitmap，每个 16 byte granule一 bit；
- 131072 bit mark bitmap；
- 512 个 `u32 page_covering_object`，记录跨过各 4 KiB页首的对象在 arena内的起始偏移，没有则为 `u32::MAX`；
- 4096 byte card table，每 byte覆盖 512 heap byte；
- 64 个 block state/live-byte项与 `64 * 256` 个 line mark/live-byte项；
- arena state、allocation cursor、pin count、mark epoch和 evacuation owner。

这些数量由 arena/block/line/granule常量推导，使用连续数组而不是 hash容器；构造时以 `debug_assert_eq!` 检查 64、256、128、16和 bitmap/card长度。object allocation先设置 start bit和跨页 covering offset，再 release发布 header。并发 sweep只在对应 block状态为 `Sweeping` 且没有 scanner/allocator lease时清除死亡 object bit、line状态和可归还页面。

地址到 arena descriptor 使用四级 radix page map，按低 48-bit canonical地址的 4 KiB page number每 9 bit索引一层；节点是 512 个原子指针的 4 KiB页并按需分配。managed heap mapping必须满足 `base + len <= 0x0000_8000_0000_0000`；LA57宿主也只能接受该范围，OS返回更高地址时 unmap并重试，耗尽后进入 `OutOfMemory`。managed/interior-pointer热路径固定做四次稠密索引，不使用全局 `HashMap` 或区间树。独立 large/pinned mapping的叶项直接指向唯一 descriptor。

解析 `HeapInterior` 时先通过 radix map找 descriptor；独立 large/pinned mapping直接用其 payload起点验证。普通 arena在当前 4 KiB页的 start-bitmap范围向前找最近 start bit；本页没有或该对象未覆盖目标地址时使用 `page_covering_object`。找到 header后 checked验证 `payload <= ptr < payload + payload_size`。一页只有 256 个 granule、4 个 `u64` bitmap word，因此最多扫描 4 个 word。

每个 logical processor从全局 nursery一次取得 8 个 block组成的 256 KiB本地 span，但 bump cursor/limit始终只覆盖当前可用 line run；对象不得越过 block。run耗尽时先在本地 span的 line表推进，无需全局同步；8 个 block用完才 refill。old allocation同样从 line表选择连续空 line，不能退化为不看 Immix line的整段 bump。含 resource、需要 pin、独立 large或高对齐请求绕过 nursery。2 MiB/32 KiB/128 byte/16 byte与 8-block span的关系写入同一 `HeapLayout`常量并逐项断言。

block 身份是全局稠密的 `ManagedBlockId`（`descriptor * 64 + block`，arena 内下标只占低 6 位），`BlockRef` 在它之上再带 block generation：管理权转移不改变 payload 的 heap/arena 定位，消费端也必须按全局身份解析来源块，只带 arena 内下标的旧编码会被拒绝。arena 登记在世界级 `managed_arenas` 表里，每项记录 descriptor、heap owner、heap 内 arena 槽、extent arena id 与 arena 基址；`allocate_managed` 只在已登记且仍有空 block 的 arena 上分配，`commit_managed_block` 按调用方指定的 extent arena 提交 block，不再隐式落到「第一个 arena」。

本阶段的实现证据：`local_heap_schema.rs` 把上述常量固定成 `LOCAL_HEAP_SCHEMA = 6` 契约段（版本 6 新增实际映射长度与独立标记 epoch，统一大对象 pin、字段追踪和整映射释放；版本 5 相对 4 的变化：新增独立 `LargeMappingRecord` 表、固定大映射页粒度，并把 page-cover/object-start/mark 的运行期消费者接入分配与屏障路径；版本 4 相对 3 的变化：块状态目录扩为 `allocating`/`candidate`/`sweeping`/`evacuating`/`return-pending`/`owned-free`/`free`，归还入队位、large span 成员与 span 起止位置进入 `reserved`，块记录成为 managed extent 与归还门禁的唯一账本；版本 3 相对 2 的变化：块记录新增 `candidate_job` 绑定与候选绑定语义，lease 计数成为候选、扫描、迁移和 allocator 之间的共同门禁）。构造时继续以 arena/block/line/granule 常量推导 bitmaps、page-cover、card 与 side metadata 字节数，并由 source-layout verifier 逐字段拒绝漂移。

## trace descriptor

### 两种表示

trace descriptor 第一个字节是 kind：

- `0`：`None`，后面无字节；
- `1`：`Bitmap`，用于固定大小、pointer word 数不超过 256 的对象；
- `2`：`Program`，用于大数组、动态 backing 和含 enum 分支的对象。

compiler 必须选择语义等价且编码更短的表示；相同长度时优先 `Bitmap`，保证输出确定。

`Bitmap` 编码为：

```text
kind:             u8 = 1
reserved:         [u8; 3] = 0
word_count:       u32
direct_bitmap:    ceil(word_count / 8) bytes
interior_bitmap:  ceil(word_count / 8) bytes
zero_padding_to_4_bytes
```

bit `i` 对应 payload 的 `[i * 8, i * 8 + 8)`。两个 bitmap 互斥；尾部无效 bit 为 0。所有 managed 字段必须自然对齐，因而不会跨 word。

### trace program

`Program` 在 kind 后保存 `u32 program_len` 和一串指令。每个 program 的地址基准是当前 payload/子对象起点；offset 和 stride 以 8 字节 word 计。所有无符号可变整数使用 canonical ULEB128：禁止多余的前导零组。

opcode 固定为：

| opcode | 操作数 | 行为 |
|--------|--------|------|
| `0x00 END` | 无 | 结束当前 program |
| `0x01 DIRECT` | `offset, count` | 扫描连续 `count` 个 `HeapDirect` word |
| `0x02 INTERIOR` | `offset, count` | 扫描连续 `count` 个 `HeapInterior` word |
| `0x03 REPEAT` | `base, count, stride, body_len, body` | 对固定数量元素，以 `base + i*stride` 为子基准执行 body |
| `0x04 REPEAT_FIELD` | `base, count_byte_offset, count_width, stride, body_len, body` | 从 payload 字段读取运行时元素数后重复 body |
| `0x05 SWITCH` | `tag_byte_offset, tag_width, case_count, cases, default_len, default` | 按判别值选择一个子 program |
| `0x06 ARENA_SLOTS` | 无 | 按固定 arena backing 记录逐槽应用运行时 `TypeId` descriptor |

`body_len`、`default_len` 为小端 `u32`；其余整数除 `count_width`/`tag_width` 外使用 ULEB128。field/tag width 只允许 1、2、4、8，按目标小端读取。

`SWITCH` 的每个 case 依次编码 `tag_value: u64`、`body_len: u32`、`body`，case 按无符号 tag 严格递增。子 program 使用当前 payload 作为基准，因而字段偏移是绝对的；nested `REPEAT` 才改变子基准。
在 Repeat 元素内部，Switch 的 tag 和 RepeatField 的 count 均相对当前元素基址读取；游标的元素偏移以 word 保存，读取这些 byte offset 字段前必须乘以 8，不能重新以最外层对象地址为基准。

program 必须恰以 `END` 结束，END 后无非 padding 字节；嵌套深度不超过 32，单 program 小于 4 GiB。每次 direct/interior 范围和每个动态 repeat 的最终范围都必须在 object payload size 内。动态 count 与 stride 的乘加使用 checked arithmetic；越界进入 `RuntimeInvariant` fatal。

编译器对结构体/元组按具体字段偏移发出 DIRECT/INTERIOR；固定数组优先 REPEAT；动态 Vec/string backing 使用 REPEAT_FIELD；enum 使用 SWITCH。普通递归类型只通过 managed pointer 间接，descriptor 不沿 pointer 递归扫描。只有内建 arena backing 可以用 `ARENA_SLOTS` 对异构 inline value 做受限的 `TypeId` descriptor dispatch。

`LocalArena`/`SyncArena` backing 的动态 payload 头固定为 `{ slot_count: u64, records_offset: u64, data_offset: u64, capacity: u64 }`。`records_offset` 指向 payload 内连续的 16 字节记录：

```text
value_offset: u64
type_id:      u32
flags:        u32
```

flags bit 0 为 `INITIALIZED`，其余位为 0。记录按 allocation 顺序排列；`value_offset` 必须位于 data 区、满足目标类型对齐且完整值不越过 payload/capacity。`ARENA_SLOTS` 只能是内建 backing program 的第一条有效指令并紧接 `END`。scanner 对每个 initialized slot取得 `TypeRecord`，以 `payload + value_offset` 为 inline base解释其 trace descriptor；含 resource 的类型在 arena allocation 前已被拒绝。reset/destroy 必须先在同步边界清除相应 initialized bits，再让 backing 不可扫描/回收。

`MaybeUninit[T]` 的 payload 不发出 trace 指令。只有 `assume_init` 消耗后形成的 `T` 值才按 `T` descriptor 进入 root/heap；unsafe 代码把唯一强引用藏在未初始化 payload 中不建立 GC 可达性。

### 可恢复的解释器

`runtime/gc_trace.rs` 的 `TraceCursor` 是 trace descriptor 的唯一解释器，`LocalHeap`、mark
与候选平面共用它，不再保留递归实现：

- **按工作预算推进**：每次 `step` 至多消费预算给出的工作单位，游标保存帧栈（`Body`、
  `Words`、`Repeat`、`Switch`、`Bitmap`）与各自的位置，跨调用不借用类型表、不复制 descriptor、
  不保存裸地址。一次性入口 `walk_descriptor` 的上界是
  `descriptor_len × (TRACE_MAX_DEPTH + 1) + payload_words + 1024`：额度耗尽即
  `RawInvariant`，因此损坏的 descriptor 不会把驱动变成不终止的循环或无限增长的访问列表。嵌套
  深度超过 `TRACE_MAX_FRAMES` 同样是失败，而不是栈溢出。
- **Bitmap 位号语义**：descriptor 头部的 `word_count` 后紧跟两张等长位图，direct 与 interior
  的同位号位指向**同一个** payload word（interior 不额外偏移八个 word）。帧里保存的始终是
  「当前字节尚未访问的位」，位耗尽后推进到下一字节，绝不重访同一位。
- **`SWITCH` 的返回位置**：分支体执行完必须回到整段 case 编码之后，而不是 tag 或某个 case 的
  中间；case 的 tag 必须严格递增。
- **`ARENA_SLOTS` 的展开**：该指令本身不访问任何 word，只标记语义。程序执行完后，同一个预算下
  按 32 字节 backing 头（`slot_count`、`records_offset`、`data_offset`、`capacity`）与 16 字节
  槽记录逐个处理 initialized 槽：`value_offset` 相对 data 区，inline base 是
  `data_offset + value_offset`，inline descriptor 的字段偏移则相对整个 backing payload。未初始
  化槽不是 root；记录区越过 payload、未知 flags、越界或未对齐的值、缺失的 `TypeId`、含 resource
  的类型都是 `RawInvariant`，不得当成「没有指针」跳过。
- **对象级游标**：`ObjectTraceCursor` 组合程序部分与可选的槽展开，因此调用方只需按
  `TraceProgress` 推进一个游标，就能得到 `(payload_base + offset, word)` 形式的精确访问序列。

对象枚举沿用分配时维护的 object-start 位图：`LocalHeap::committed_blocks_of` 给出已提交
block，`block_objects` 给出「payload 地址 + block 内 header 偏移」，`marked_in_current_epoch`
只认当前 epoch 的标记位（本 block 尚未在本 epoch 标记过时返回 false，不能把上一轮的陈旧位当成
当前结果）。三者都不扫描 payload，也不依赖对象大小。

## value program 与 glue
value program 用于编译器在 GIR 中展开语义复制、销毁、发布和 resource 动作。它不是 GC trace program；collector 不解释普通 copy/drop 操作。

每条 value instruction 固定为：

| opcode | 操作数 | 含义 |
|--------|--------|------|
| `0x00 END` | 无 | 结束 |
| `0x10 COPY_FIELD` | byte offset、`TypeId` relocation | 调用字段 copy 语义 |
| `0x11 DROP_FIELD` | byte offset、`TypeId` relocation | 逆序销毁字段 |
| `0x12 PUBLISH_FIELD` | byte offset、`TypeId` relocation | 发布 COW/resource 图 |
| `0x13 ACQUIRE_RESOURCE` | byte offset、glue relocation | 获得租约 |
| `0x14 RELEASE_RESOURCE` | byte offset、glue relocation | 释放租约 |
| `0x15 REPEAT_VALUE` | base、count、stride、body_len、body | 对固定数组重复字段动作 |
| `0x16 SWITCH_VALUE` | tag 描述与 case body | 按 enum 活跃变体执行动作 |

整数编码与 trace program 相同。drop 顺序由 compiler 生成的 instruction 顺序完全决定；结构体字段逆声明顺序、数组逆索引、enum 只处理活跃变体。copy/publish 使用声明顺序。

常见小类型直接在 GIR/LIR 中 inline value program；较大或多个调用点共用时调用 `copy_glue`/`drop_glue`/`publish_glue`。两条路径必须由同一个 value program 生成，不能维护第二份字段规则。glue 的 safepoint/effect 由 LIR 显式标注并有正常 stack map。

不可达且仍含最后 resource lease 的对象进入受限 release queue 并保持到 descriptor 已执行；队列只运行 compiler/runtime 生成的 resource drop glue，不执行任意用户析构或 finalizer，不允许复活、分配、panic 或等待。同一对象最多排队一次，release 完成后才可回收。

镜像内 `value_copy/drop/publish/forget/transfer`、`cow_snapshot` 与 resource glue 已绑定到 Gugu `value` 模块。解释器直接消费 80 字节 TypeRecord 的 value offset/length；零长度表示无动作。ResourceCell 的单 END 程序是资源叶，Acquire 增租约，Release 先清空槽再减租约，Forget 只清空槽。结构 wrapper 按当前模式选择前向或逆向动作，enum 只处理当前 tag。控制页登记在 owner 上，GC 释放最后租约只提交 close 队列；原生 close 完成后才能回收控制页，自然退出排空剩余登记。`finalize_resource` 跳过无资源标志及已 forwarding 的旧对象，先清除资源标志再执行 descriptor，重复访问不会再次释放。进程内 LIR 测试执行该入口、ResourceCell 叶程序、租约释放、完成发布、登记表摘除及 native_memory 账本扣减；测试提供固定类型段，并在最终平台释放处记录事件。该测试不代替整个 GC 可达性扫描或原生线程的验证。

值动作解释器的 ULEB128、定宽整数读取和嵌套 Repeat/Switch 均按值传递游标并返回续点，不对游标取地址、不给游标建立托管盒。GC 的资源终结会执行同一解释器；若在这里触发托管分配，慢路径再次切到 collector 栈顶会覆盖尚未返回的终结帧。因此这条路径必须保持无托管分配，嵌套程序结束后仍从父程序的正确续点执行后续动作。

value glue 的符号也受后端的真实发射门禁约束：只有 lowering 实际发出 `CallTarget::Runtime` 或专用 value op 的入口才检查，不会因为 `RuntimeCall` 枚举中存在一个由 lowering 内联的名字而误报。未接管的 `value_*`、COW、resource、pin 和动态擦除入口必须留在显式迁移缺口清单；它们不是定义，不能被 bench 当作镜像运行期证明。实现接管时，入口必须绑定到 `GLUE_DEFS` 的 Gugu `Item`/machine `Fragment`，并同步移除缺口记录。

## 镜像根、vtable 与源码 metadata

Linux `.gugu.meta` / Windows `.ggmeta` 保存根、动态分派和运行时源码位置 metadata。header 固定为：

```text
magic:                   [u8; 8] = "GUGUMT01"
version:                 u16 = 2
pointer_size:            u8 = 8
endian:                  u8 = 1
root_count:              u32
vtable_count:            u32
source_record_count:     u32
reserved0:               u32 = 0
reserved1:               u32 = 0
root_records_offset:     u64
vtable_index_offset:     u64
vtable_data_offset:      u64
source_records_offset:   u64
source_strings_offset:   u64
source_strings_len:      u64
section_len:             u64
```

每个 `RootRecord` 固定 32 字节：

```text
location:        u64
type_id:         u32
kind:            u16
flags:           u16
count:           u64
stride:          u64
```

kind 为 0 global、1 OS-thread-local template、2 coroutine-local template、3 runtime static root slot。kind 0/3 的 `location` 是 image RVA，kind 1 是 module TLS block byte offset，kind 2 是 coroutine-local layout byte offset。count 至少为 1；单值 stride 为 0，数组 stride 必须不小于类型大小。flags bit 0 为 `READ_ONLY_AFTER_INIT`，bit 1 为 `LAZY_SLOT`，其他位为 0；kind 1/2 必须设置 lazy，runtime 只在对应 thread/coroutine initialized bitmap 的 bit 已发布后扫描。records 按 kind、location、TypeId 排序且同一实例内存范围不重叠。

非零/非纯常量 global 初始化必须设置 `LAZY_SLOT`：初始化器先在自己 GIR local中构造完整值，release写 global并最后设置 initialized bit；失败/panic时 bit保持 0并清理 local。GC只扫描 bit已设置的 global，因而不会读取半初始化 managed字段。纯静态常量可以在镜像加载时视为已初始化。

### 静态块与初始化器发布协议

每个 `Symbol::Global { key, thread_local, bytes }` 对应一块镜像内静态存储，布局固定为
「`initialized` 位字 8 byte + 值槽 `bytes` byte」，符号地址是**块首**；取静态地址的指令
（`StaticRef`）返回的是**值槽**地址，即块首加 8。块尺寸只由定义的声明类型决定，
`StaticRef`、`StaticInitialized` 与初始化器发布三处必须给出同一个符号：尺寸分歧在编译期失败。
位置由 writer 决定：非局部项落在可写数据区（Linux 保留区 / Windows `.data`），线程/协程局部项
落在对应 TLS 模板（TLS 存储与模板块放置仍待接线）。

初始化器（`MonoKind::StaticInit` 实例）在返回前发布结果：聚合结果按字节从自己的地址复制进
值槽，多车道标量按车道依次落槽，最后用 release 语义写位字。因此

- 位字只在值完整后可见，读者要么看到旧的 0（随后走初始化路径），要么看到完整值；
- 初始化器 panic 时位字保持 0，值槽可能含半构造内容，收集器不会扫描它；
- 值槽只在位字发布后才被 `RootRecord`（`LAZY_SLOT`）纳入扫描。

读点守卫由 GIR 在静态引用处合成：`StaticInitialized` 读块首位字，为 0 时以 `CallSite::StaticInit`
（局部 static 的声明点用 `CallSite::Initializer`）调用初始化器，再经 `StaticRef` 取得值槽地址。
守卫的调用边先落到独立块再跳进读点，避免「带结果参数的调用边」与「无分支参数的普通边」指向同一块。
初始化器实例**不可被 GIR 内联**：它的发布动作挂在自身 `Return` 降级上，内联只搬语句会让初始化
静默变成空操作。

持有 `StaticRef` 结果的 local 在具体类型表中物化为 `*T`（`T` 是静态的声明类型），包括 `let` 绑定
静态的槽位和直接解析到静态定义的表达式两种来源；实例类型收集在冻结前把该指针类型一并登记进
类型宇宙，否则具体 body 的类型键无法在冻结后解析。

局部 `static` 的绑定槽只保存存储地址（供闭包捕获），本 owner 内的读写都经同一存储 place，
因此写入跨调用可见。受管值类型（值槽含受管指针）的静态仍被拒绝，待 `RootRecord` 与 `LAZY_SLOT`
扫描接线后再放开。

尚未接线：TLS 局部项的模板块放置与运行期扫描，以及受管静态值的根记录生产者。

根记录的 `location` 是镜像内真实地址，布局后才知道：编译器只产出稳定符号名，写入
`BlobRelocation { offset, symbol }` 清单（blob 内字节偏移 + 已登记符号），由两个 writer 在
`defined_layout`/`assign_rvas` 之后回填 8 字节地址；符号缺失或偏移越界/未对齐一律链接失败，
不留零值让运行期读到无效地址。action key 只含符号名与偏移，因此不含运行地址。

vtable 是 variable record，由 `vtable_count + 1` 个 `u64` index 定界：


```text
concrete_type_id: u32
method_count:     u32
trait_key:        [u8; 32] StableDefKey
size:             u64
align:            u32
flags:            u32
copy_glue_rva:    u64
drop_glue_rva:    u64
method_rvas:      [u64; method_count]
```

method 按 trait 声明槽顺序排列。相同 `(concrete_type_id, trait_key)` 只能有一条。`dyn` data pointer 由 stack/object descriptor 追踪，vtable pointer 是 metadata pointer，不加入 GC root。
arena allocation owner 同时维护该 arena 的 card mailbox；`CardMarkBatch` 消费只合并 card index/range 并设置 dirty byte，不重新扫描对象，也不改变 `MarkTicket` 的 mark work。card mailbox 为空不是 minor cycle 完成条件，未消费 batch 必须由 owner credit 和 pressure 账本继续保留。

每个 `SourceRecord` 固定 32 字节：

```text
function_index: u32
pc_start:       u32
pc_end:         u32
path_offset:    u32
path_len:       u32
line:           u32
column:         u32
flags:          u32
```

function index与 stack-map function table相同，PC 为 function-relative 半开范围。path 是 package-relative逻辑 UTF-8路径，不含 workspace绝对路径；line/column 从 1开始。`source_strings_len` 不得超过 `u32::MAX`，每个 `path_offset/path_len` 都相对 source string pool并经 checked range验证。flags bit 0 `PANIC_SITE`、bit 1 `SYNTHETIC`，其余为 0。records 按 function index、pc_start、pc_end和路径 bytes排序，范围可以因内联 attribution嵌套；查找选择覆盖 PC 的最短范围，再按记录序打破相等。source string pool按 bytes去重排序。这组 PC 记录由代码生成写入源码位置节（Linux `.gugu.src`，Windows `.gugusrc`），魔数 `GUGUSRC1`；`.gugu.meta` 继续保存类型、根与 vtable 的来源条目。两处记录都不得被 `--strip` 删除。

## scheduler non-moving slab 与 queue-page grace

scheduler raw控制对象不进入moving GC heap。`CoroutineSlot` 固定为128 byte，由相邻的64-byte `CoroutineHot`与64-byte `StackDescriptor`组成；`CoroutineCold`按编译期固定size class分配。两者使用64 KiB分段slab page，page扩容只追加，slot地址和`cold_index`解析在page存活期间不变。`run_link_next`、remote/injection head、producer staging和detached carry只保存`CoroutineHot*`，不得指向可移动对象或另建GC forwarding indirection。hot slot、cold record、wait-node和stack的完整已提交bytes都计入runtime memory limit与内部统计。

slot成为`Dead`后，只有在stack已归还、最后一个Join/handle与runtime root释放、state不含`ENQUEUED|BATCH_PUBLISHING|STACK_SCAN_LOCKED`且所有queue位置都不再引用它时，才能回到同page free list。复用slot必须分配新的`CoroutineId`并推进相应generation；`CoroutineHot*`的allocation/provenance不变。普通复用不等待queue epoch，因为batch producer只把旧head当不透明pointer值、consumer只用atomic exchange摘整链且不会基于旧head执行consumer CAS。

整页解除映射使用独立的queue-page grace，而不是每次enqueue执行generic EBR pin：

1. allocator只选择全部slot均free、未出现在任何head/staging/carry/local/run_next/registry的page，先从free page集合摘除；
2. coordinator在queue control word中Release发布新的`slab_epoch`与reclaim gate，并阻止新queue participant登记；epoch发布时`publish_active`为true的processor、worker、poller和foreign/callback producer必须完成当前head CAS或detached遍历、flush `pending_node/staging`，到达不持有raw queue pointer的checkpoint后Release写`slab_epoch_seen`并清active；当时inactive的participant不能越过gate开始新batch；
3. coordinator Acquire等待全部旧epoch participant确认，并再次验证page仍为空且没有queue ownership；此后旧opaque head、局部`next`或staging pointer都不可能重新发布该page；
4. 才能decommit/unmap hot与cold page并重新开放participant登记。

该grace只在完整GC后的内存回收、memory-limit压力、processor/worker teardown或runtime终止触发；普通publish/pop不读全局slab epoch、不写共享participant计数、不发SeqCst fence。无法让任一registered participant越过checkpoint时保留page，不能用超时猜测安全。

## root 枚举

一个 GC cycle 的根来源封闭为：

1. 已停协程按[栈图](stack-maps.md)给出的 stack/register root；`Foreign` 与 `DirtyWaiting` 使用保存 PC 的 `ForeignBridge` map扫描coroutine stack上的ABI bridge frame；
   其中压缩槽（`compressed-ref`，根种类判别值 5）保存 cage 相对编码字，必须先经 checked 解码才能参与 owner 解析与标记；
2. `RootRecord` 声明的global，以及所有已登记OS thread/TLS实例和全部live `CoroutineCold`的已初始化 coroutine-local payload；
3. scheduler/runtime的强句柄表、`SharedHeap` handle table、`ProducerHandle.pending_node/staging`、remote/injection head、detached carry、`run_next`、LocalDeque、等待队列载荷、Join结果和resource release queue；
4. 当前 active coroutine 的 `TurnRegion` descriptor、export summary、transfer reservation 和尚未 reset 的 region root；
5. `MarkMailbox`、`EdgeDelta` staging、`RegionTransfer`、`HandleForward` 和其它仍未消费的 GC message 所引用的 stable descriptor 或 handle；
6. 外部线程回调桥建立的临时 root handle；
7. 正在执行的 pin side table entry 和 SharedHeap access guard。

root snapshot开始前，coordinator除停止active processor外，还发布producer stop epoch和 GC
owner credit epoch：每个registered producer完成当前batch CAS、把`pending_node/staging`
留在登记record并确认；remote/injection consumer完成当前detached节点的`next`保存或把
carry登记后确认；每个 owner 发布自己的 root slice、TurnRegion registry、handle access
guard 和本地 worklist 边界。未进入runtime调用的native线程没有staging；正处于runtime
callback/waker的线程必须在返回native前经过该checkpoint。全部确认后，queue head、GC
mailbox 和所有owner-only位置在本次snapshot内稳定；恢复时先Release发布metadata，再解除
producer gate。不能扫描任意native OS stack来替代该协议。

runtime私有结构必须通过固定typed root visitor枚举，不允许对其内存做保守扫描。visitor以live
registry和queue ownership定位`CoroutineHot`，再由`cold_index`解析`CoroutineCold`；
`TurnRegion` registry、SharedHeap handle table、`MarkMailbox`、`EdgeDelta` staging、
`run_link_next`、`run_batch_len`、processor pointer和slab free metadata都不是隐式 managed
root，只有登记的 descriptor/handle payload 才能被扫描。`ForeignBridgeState`自身不保存
managed pointer；`lease_word`是generation-tagged lifecycle整数，其余字段以
`(CoroutineHot*, stack_high-relative frame_offset)`定位ABI frame。collector在
`STACK_SCAN_LOCKED`下根据调用点map扫描和更新其中的managed root。

普通`ForeignBridge`与`ForeignBridge[DirtyCpu]`都只通过已保存的Gugu stack/map和显式pin暴露根。attached普通bridge遇到GC stop时由collector按完整generation立即retake并转为detached，不等待native线程合作；foreign/dirty worker的OS stack、C/C++ stack和opaque asm寄存器绝不保守扫描。传给native的managed地址必须在进入前pin，或复制到non-moving storage。native work永不返回时，相关coroutine frame/pin会一直保留；普通processor lease仍可被GC/scheduler取回，因此该native work不阻止其它heap的mark、relocation或stop epoch完成。
stack arena、processor stack cache和已经从live coroutine registry摘除的stack slot不属于root。coroutine完成defer后，必须先在旧stack上用GC barrier把result或panic payload移入cold control record，再由`finish_coroutine`单向切到worker system stack；持有`STACK_SCAN_LOCKED`停止typed visitor遍历旧stack并发布空descriptor后，stack slot才能交给cache，随后发布`Dead`。仍存活的Join/handle只保留hot/cold control slot与结果。缓存字节中的旧pointer pattern绝不保守扫描。Waiting/Runnable stack的冷压缩同样必须持有scan lock，用旧map完成全部`StackInterior`修正并发布新descriptor后，旧stack slot才可进入cache。

## write barrier、edge summary 与 remembered set {#write-barrier-edge-summary-remembered-set}

所有可能覆盖 heap managed field 的写入由 LIR `GcWriteBarrier` lowering 成统一 hybrid barrier。
LocalHeap 和 TurnRegion 使用 direct field barrier；SharedHeap handle field 和跨 block edge
还必须维护 owner-local edge summary：

1. 读取旧值；
2. 并发标记开启时，若旧值非空则 shade 旧 target；
3. 当前 coroutine stack 尚为 grey 时，若新值非空则同时 shade 新 target；没有 current coroutine 的 runtime/system write 一律按 grey 处理；
4. 执行实际 store；
5. owner 在 old/immortal generation 且新值指向 nursery/aging 时，把 owner 所在 512 字节 card 的稳定键追加到当前 processor 的 `CardMarkBuffer`；不得直接写共享 card table；
6. 若 source 与 target 属于不同 owner/block，按本地 card/line summary 聚合 `EdgeAdd` 或
`EdgeDrop`，由 owner batch 发布给 target；同一 edge 的删除不能早于其已经发布的 add 被
纳入同一或更晚的 epoch。

edge summary 是**多重计数**而不是布尔标志：同一 `(source block, target block)` 对的重复 add
累加计数，drop 只扣减当前计数，扣到零且没有保留记录时该 block 对才从活跃边集合清退；扣减超过
当前计数是不变量失败。一条 hybrid barrier 写入最多产生两项边变更（deletion 与 insertion），与
每次写入最多消费的两个 shade slot 同源，因此 `edge_deltas_per_write == shade_slots_per_write`。
热路径把边变更写进 processor owner-local 的固定 `edge scratch`（每 processor 512 项），只有
scratch 满或需要跨 owner 合并时才走慢路径发布 `EdgeDelta`；scratch 项数与每次写入的边变更上界
都进入 barrier 契约，`BarrierReserve` 的额度必须同时覆盖 shade slot 与 edge scratch，不能在
`NoSafepointRegion` 内为边变更临时分配消息节点。

这是 Go 风格的 Yuasa deletion 与 Dijkstra insertion 混合屏障，并附带 owner-local edge
summary。shade 操作只入队第一次从 white 转 grey 的对象；不能递归扫描 mutator stack。
标记关闭时步骤 2、3 由一个 runtime flag 分支跳过；generation 条件可由 TLAB/new-object
分析消除。`EdgeDelta` 只表达经过 epoch 聚合的 block edge，不是普通对象 reference count。

### CardMarkBuffer 与 remembered-set flush

`card table` 仍是每个 arena 的 512-byte 粒度元数据，但 mutator 不在每次 old-to-young 写入时直接写共享 card byte。每个 `LogicalProcessor` 拥有固定 256 项的 `CardMarkBuffer`，每项只保存 `{ arena_descriptor, arena_generation, card_index, cycle_epoch }`；buffer 与 dedup 表均位于 processor owner-local storage，不保存 managed pointer。选择固定上界是因为 card mark 只需记录“脏过”这一位，重复键可以在本地合并；达到上界才进入 flush slow path，不把每次写入变成共享 cache-line 写。

barrier 在完成实际 field store 后，把 distinct card 键放入本地 buffer。dedup 使用固定大小的直接映射 stamp 表：冲突只会留下已经进入 buffer 的旧键，不得丢弃尚未发布的键。当前 processor 是 arena allocation owner 时，flush 可以在 owner 上按 arena/generation 合并后写 card table；其它情况发布 `CardMarkBatch` 到 arena allocation owner 的 card mailbox。batch 只携带稳定 arena descriptor、generation、card index/range、cycle epoch 和 bytes，不携带 field 地址或 managed pointer。card table 只能由 arena owner 写入，避免多个 processor 长期争用同一 card cache line。

buffer 满、processor 交接、进入 `ForeignBridge`、memory pressure、minor stop 请求和 producer stop gate 都必须 flush。flush 以 Release 发布实际 field store 之前已经完成的 buffer 内容；owner 以 Acquire 消费 batch 后再写 card table。minor cycle 在扫描 remembered set 前必须确认所有 active processor 的 buffer 已 flush、所有旧 epoch card batch 已消费或登记在 owner credit 中；因此不能以“当前 card table 已清零”代替 producer drain。卡片重复写是幂等的，按 arena/generation 不匹配的键进入 `RuntimeInvariant`，不能静默忽略。
flush 后必须同时使 stamp 的键与 buffer 索引失效，不能只把 epoch 清零：非标记期间的合法 epoch 也是零。镜像实现清除整条 32 字节记录；有效 card 键包含非零 arena 地址，因此零键不可能误命中，后续同一卡片必须重新追加到已清空的 buffer。

card buffer 的 256 项、dedup stamp 和 pending batch bytes 都计入 runtime pressure；pressure flush 可以提高 batch drain budget，但不得在 `NoSafepointRegion` 内分配节点、阻塞或遍历其它 owner。若 flush 需要 refill，必须在 region 外建立 mandatory statepoint；`BarrierReserve.max_card_marks` 不足时不能把 card table直接写成共享 fast path。

编译器只有在证明 owner 是尚未发布的新 nursery object、写入发生在任何 safepoint/逃逸之前且旧 slot 未初始化时，才可省略屏障。向 global、old、共享对象、unknown alias 或 foreign 可见内存写 managed pointer 不能省略。

`NoSafepointRegion` 内需要执行 barrier时，compiler在 region外发出 `BarrierReserve { permit }`，其 `BarrierPermitData.max_shades` 由 concrete type descriptor逐 pointer word计算，`max_card_marks` 由本 region可能触及的 distinct `(arena, card)` 键上界计算；deletion+insertion每次写至多消费两个 shade slot和一个 card-mark slot。buffer不足时在 region外走 refill mandatory statepoint；成功后 compile-time permit证明下一 region拥有足够容量，该 ID不形成 machine value。region内只能使用 `GcWriteBarrierReserved { permit }`，禁止再次检查容量或连接 refill edge；verifier统计实际静态消费，未用额度无需生成归还指令。并发标记关闭时 reservation与 reserved barrier按同一 flag折叠，不给普通 store增加第二次 flag load。

无法在 `POLL_BUDGET` 内完成的 aggregate copy不能因持有 runtime lock而关闭 safepoint。channel/select等 runtime原语必须先在短 region内取得带 generation的不可见 transfer reservation，在 region外完成 descriptor copy与普通 barrier，再在第二个短 region发布；reservation由 typed visitor扫描，未发布 payload不能被 receiver或 close观察。

card table 每 512 heap 地址字节使用 1 byte；minor cycle 在 mutator 已停止且所有 processor buffer 已 flush、所有 `CardMarkBatch` 已由对应 arena owner 消费后，以 AcqRel swap 把 dirty card 取为 0并扫描。card table 的 owner 写入可使用 owner-local ordinary store；batch 发布以 Release，owner 消费以 Acquire，重复写 1 是幂等的。`EdgeDelta` staging 也只能在有足够 permit、已登记 owner generation 和可追踪 cycle credit 时发布；不能在 `NoSafepointRegion` 内临时分配消息节点。

屏障与边缓存的实现证据：`barrier_schema.rs` 把上述缓存与预留固定成 `BARRIER_SCHEMA = 2` 契约段——每 processor 256 项 `CardMarkBuffer`（`CARD_MARK_BUFFER_ENTRIES = 256`）、每次写入 2 个 shade slot（`SHADE_SLOTS_PER_WRITE = 2`）、每 processor 512 项 edge scratch（`EDGE_BUFFER_ENTRIES = 512`，同时也是 `EdgeDemand.reserve_slots` 的预留证明）、每次写入至多 2 项边变更（`EDGE_DELTAS_PER_WRITE = 2`）、`CardMarkBatch` 的 13 个规范字段（`CARD_MARK_BATCH_FIELDS = 13`，不含 field 地址与 managed pointer）与 6 个 flush 原因（`CARD_MARK_FLUSH_REASONS`：`buffer-full`、`processor-handoff`、`foreign-bridge`、`memory-pressure`、`minor-stop`、`producer-stop-gate`）。verifier 拒绝「边变更上界不等于 shade 上界」「scratch 项数小于每次写入上界」「batch 字段集合与消息 schema 不同源」的契约；`barrier-contract-fingerprint`、`barrier-demand`、`barrier-card-granularity-bytes`、`barrier-card-mark-buffer-entries`、`barrier-card-mark-stamp-entries`、`barrier-flush-reason-count`、`barrier-card-mark-batch-fields`、`barrier-record-count` 与 `barrier-runtime` 进入 `ImagePlan`、`-Zdump-runtime` 与 CLI JSON。

### 镜像侧的六步执行体

`barrier.gg` 在独立 raw 页上实现完整六步（read-old → shade-old → shade-new → store → card-mark →
edge-summary）。页面布局是 64 字节 `CardMarkBufferHead` 头、256 条 32 字节 `CardMarkEntry` 与 256 条
32 字节直接映射 `CardMarkStamp`；processor 页 `+3576` 保存该页地址，`+3584` 是栈扫描 epoch，
`+3592` 是 arena 基址，`+3600` 是标记活跃标志。read-old 由编译器在配对 `Load` 完成，store 仍由
配对 `Store` 执行（把两者合成单一物理发布组是后续单元）。

shade 只在标记周期开启时发生：`mark_epoch` 单调推进，周期开始时清空 arena 的 mark 位图，活跃
标志单独表达「当前有标记周期」，因此旧周期的 shade 不会与新周期混淆。删除屏障对非空旧值 shade，
插入屏障只在新值非空且当前栈仍为 grey（栈扫描 epoch 与 `mark_epoch` 不等）时 shade。card 键只在
owner 位于 old/immortal 且新值落在 nursery/aging 时登记，append 命中 stamp 只累加 `marks`，冲突
只淘汰 stamp 索引而不丢已登记键；buffer 满时先把已有键 flush 到各 arena 的 card table（owner-local
写，幂等）并清空 stamp，再落新键。跨 block 的删除（旧目标与本对象不同 block 且不等于新值）与插入
各记一条 32 字节 edge delta，满载只置 overflow 账本。`gc_barrier_reserve(max_shades, max_card_marks)`
提供同一页上的额度预留；六原因中除 `buffer-full` 之外的五个由 cycle 调用同一个 `gc_barrier_flush`
入口。

## collector 使用 metadata 的阶段

minor cycle 对仍位于 LocalHeap nursery 的对象停止 mutator，复制可移动对象并更新根/字段；
TurnRegion 私有图在 owner 确认无 export 后直接 reset，不进入 minor mark；已经 publish、
old、SharedHeap、pinned、large 和 resource object 不因 minor cycle 直接移动。对象 age
增加到 2 后提升到 old，达到 15 饱和。

每个 major cycle 按以下阶段执行：

1. coordinator 固定 cycle、topology epoch 和 owner credit，建立 per-owner root slice；
2. 短暂 root snapshot 发布 hybrid barrier、MarkMailbox、handle access 和 region transfer
   gate；
3. 每个 owner 并发解释本地 descriptor，处理本地 worklist，并通过 `MarkTicket` 向其它
   owner 发布跨 owner mark；
4. mutator barrier 将跨 block edge 聚合为 `EdgeDelta`，owner 消费后更新自己的 lease
   summary；所有 pending message、worklist 和 credit 都必须可追踪；
5. coordinator 等待全部 owner credit、mailbox、barrier buffer 和 producer epoch 收敛，
   再执行 remark；不能用单个队列为空作为完成条件；
6. LocalHeap 按 block/line live bytes、pin、resource、foreign incoming edge 和 lease
   状态选择 owner-local evacuation；没有安全 incoming edge 的对象可 direct forwarding；
7. SharedHeap 复制对象后只在 stable handle slot 的线性化点切换 current payload。旧 payload
   保留到 access guard、pin、mark ticket 和 forwarding grace 全部结束；不能把 shared
   direct pointer 带出 guard；
8. 资源对象逐对象完成受限 release，不能整区丢弃；TurnRegion 经过 export、transfer 和
   typed root 验证后进入 reset；
9. 重建 LocalHeap card/edge summary、发布 handle metadata、关闭本轮 barrier 并恢复
   mutator；MosaicConcurrent 可以跳过与 shared heap 大小相关的全局 pointer-update stop，
   但仍保留必要的局部 handshake、pin 和 access guard；
10. GC worker 与 mutator 并发 sweep 未 evacuate 的 LocalHeap/resource block，处理 candidate
   block 的 exact local trace 与 cycle/SCC 检测，把完整空 block/页通过 owner-directed
   return 返还 owner/domain。

## GC pacing 与 relocation pause budget {#gc-pacing--relocation-pause-budget}

Mosaic 的 collector 以 `GcPacingProfile` 固定下列内部参数：`min_growth_budget`、`assist_threshold`、`assist_quantum`、`mark_cost_per_byte`、`gc_cpu_fraction`、`gc_cpu_window_cost`、`remark_cost_budget`、`evacuation_pause_bytes`、`evacuation_pause_roots`、`evacuation_pause_fields`、`pressure_enter_ratio`、`pressure_clear_ratio`、`pressure_poll_bytes`、`owner_drain_items`、`owner_drain_bytes` 和 `owner_drain_interval_bytes`。这些参数与 `CompilerIdentity`/runtime tuning profile 一起版本化并进入 digest；它们是实现门禁，不是用户可观察的时间单位。当前版本只登记唯一 profile `mosaic-default`，参数不允许被环境变量覆盖：任何参数变动都必须递增 profile revision 并同时更新契约、预算数字与端到端 GC workload，否则 verifier 在镜像写出前拒绝。`GcPacingRuntimeContract` 把这些参数、pressure 状态目录、必须各自 drain 的账本分类、assist/remark/evacuation 结局与 credit 来源目录固定为带版本对象；`GcPacingDemand`（分配站点、屏障站点、assist slow edge、受管类型数）由优化后 LIR 与冻结类型表推导并进入契约指纹。

每个 cycle 的 `allocation_debt` 先按[内存所有权与消息通道](memory-messaging.md#allocation-debt-pressure-backpressure)计算，再乘以 descriptor/profile 的 `mark_cost_per_byte` 形成 mark debt。processor 在 TLAB refill、allocation slow edge 或显式 poll 处最多执行一个 `assist_quantum` 的标记/edge/card 工作；一次 assist 不能无限追债，也不能持有 runtime lock 跨 safepoint。mutator assist 和 collector worker 都归入同一 cycle credit，只有完成的 work 才能归还 credit；没有可消费 work 时不得虚构进度：assist 必须报告 `none`/`within-quantum`/`quantum-truncated`/`no-work` 四种结局之一，可消费 work 为零时不得记账。偿还量按一个 cost unit 只扣一次的口径分摊：先冲抵拖欠的 `pending_mark_work`，余量再按 `mark_cost_per_byte` 折成 allocation 字节；assist 的真实交接是它自己 owner 的 processor 账本交出的 dirty card 键数，edge summary 留给 cycle 边界取走，未收敛的 credit 只表示本轮 cycle 未完成，不会把分配变成错误。

GC worker 的执行由 `gc_cpu_fraction` 的滑动 cost window 限制。空闲 processor 可以在未使用的 CPU 额度内执行 GC；有 runnable 压力时，超出额度的工作转为 allocation debt 和后续 assist，而不是创建无界 GC worker。窗口预算为 `gc_cpu_window_cost × gc_cpu_fraction / 100`，必须至少能容纳一次 `assist_quantum`；cycle 完成时窗口复位。memory pressure、cycle termination 和 lease/grace 正确性优先于吞吐预算，但每个 slow edge 仍受 scheduler 的 poll/service budget 限制。

collector 不能以一次不受限的 remark 或 evacuation 把预算转化为暂停尖峰。remark 必须在 `remark_cost_budget` 内完成；超出时保持 hybrid barrier、继续 concurrent mark/owner assist，并发布 continuation，不能在未终止的 mark cycle 中恢复普通 barrier；未开启 barrier 时不得执行 remark。`MosaicThroughput` 可以使用较大的 profile budget，但仍受上述上限。`MosaicLowLatency` 只选择完整 relocation/update footprint 同时不超过 `evacuation_pause_bytes`、`evacuation_pause_roots` 和 `evacuation_pause_fields` 的 block；候选超过任一上限就整 block 延后，不能部分发布 direct pointer 更新，也不为 LocalHeap direct pointer 隐式增加 read barrier。延后的 block继续由 sweep/后续 cycle处理，直到有完整预算；SharedHeap 仍使用已有 handle forwarding。

每次 stop 都记录实际 remark、evacuation、root-update cost 和 copied bytes。profile 只能在确定性 model、release generated-code 和端到端 GC workload 均通过后改变；没有这些数据不能宣称 Immix/Mosaic 组合达到某个吞吐或 p99 暂停目标。

GC mutator stop 的 managed 执行确认只统计 active `LogicalProcessor`；root snapshot 还
必须等待当时已登记且正在 runtime queue primitive 中的 producer/consumer 确认 producer
stop epoch，并收集所有 owner credit。停留在普通`ForeignBridge`、`ForeignBridge[DirtyCpu]`
或`DirtyWaiting`的 native work 没有 processor 且不执行 queue primitive 时不确认 stop；其
ABI frame roots 保持可见，native 线程随后进入 callback/waker 前必须先经过 producer gate。
native work 完成后再通过普通 resume safepoint 回到 managed heap。

GC worker解释 trace program时使用显式小栈；嵌套上限 32，采用固定 `[TraceFrame; 32]`。
每个 owner 的 mark worklist、MarkMailbox 和 edge staging 使用分段 pool；collector 维护
显式 credit 终止状态。并发 sweeper 只能取得 block 的 `Sweeping` lease，allocator 只能取得
`Allocating` lease，handle forwarding 只能取得对应 slot 的 forwarding lease。

完全空 block/页进入 owner-directed return 后，只有 owner/domain consumer 验证 generation、
state、lease、pending message、handle access 和 queue grace，才能重新标为 `Free`；在此
之前仍计入 committed 与 pending pressure。resource arena 仍逐对象执行受限 release，不能
用整区 return 替代 resource lease。

## metadata 验证

镜像写出前和 runtime `Booting` 时都必须验证：

- section magic/version/target、offset、长度、对齐、排序和保留位；
- `type_count` 与 `type_id_count()`、name UTF-8 和每个 record 的 size/align/flags；
- trace/value program opcode、canonical ULEB128、nested length、范围和唯一 END；
- glue/method RVA 位于合法 code range并具有匹配内部签名；
- root range 位于相应 data/TLS section且 descriptor 可覆盖；
- vtable 的 trait/concrete 类型组合唯一、槽数与 trait 相同；
- source record 的 function index、PC range、UTF-8逻辑路径、行列和 flags 合法，`SourceRecord32` 指向 exact record；
- object header TypeId、payload size、forward 地址和 generation 状态合法；
- strip 后所有 type/root/vtable/source record、stack map 和 glue 仍存在。
- `BarrierPermitId` 的 `max_shades`/`max_card_marks` 与该 region 重算的静态消费上界一致、只关联一个 `NoSafepointRegion`且静态消费不超额，所有 `GcWriteBarrierReserved` 都没有 refill edge；不可见 transfer reservation具有合法 generation、trace descriptor和唯一 publish/cancel结局；
- `CoroutineHot`、`StackDescriptor`、`CoroutineSlot`的size/alignment/offset与scheduler/backend schema完全一致；所有live cold index可解析，queue root的state/ownership唯一，`run_batch_len`与chain边界合法；
- slab free slot不含queue/root/scan ownership，page candidate从allocation集合隔离；queue-page grace的participant集合、epoch确认和二次空页验证全部完成后才出现decommit/unmap action；普通queue trace中不能出现per-publish epoch pin、全局refcount或SeqCst fence；
- representation tag 与 `TurnRegion`/`LocalHeap`/`SharedHeap` placement 一致；region export/reset、handle slot、access guard、forwarding grace、pin 和 compressed cage 的 generation/range/state 合法；
- `MarkTicket`、`EdgeDelta`、`RegionTransfer`、`HandleForward` 和 block return 的 cycle/topology epoch、owner/object/block generation、credit、integrity、exactly-once 状态与 pending bytes 账本一致；
- candidate block 已完成 local exact trace、cycle/SCC 处理、incoming lease、scanner、allocator、evacuation、resource、handle access 和 queue grace 验证；

GC metadata verifier 还必须检查 `BarrierReserve.max_card_marks` 与 concrete descriptor/region 的 distinct card 上界一致；每个 `CardMarkBuffer`、`CardMarkBatch` 的 arena generation、cycle epoch、owner token 和 pending bytes 都能闭合到一次 flush/consume 结局；remark continuation 不得在 barrier 已关闭时残留；每次 evacuation 的 bytes/root/field cost 不得越过当前 `GcPacingProfile`。

任何静态验证失败阻止产出镜像；Booting 中发现损坏进入 `RuntimeInvariant` fatal。runtime 不能忽略未知 opcode 或把未知类型按无指针对象扫描。

## 参考实现资料

- [Go runtime heap bitmap 与类型扫描](https://go.dev/src/runtime/mbitmap.go)
- [Go runtime GC program 与 metadata](https://go.dev/src/runtime/mgcdata.go)
- [Go runtime hybrid write barrier](https://go.dev/src/runtime/mbarrier.go)
- [Rust 编译器类型布局与 ABI](https://rustc-dev-guide.rust-lang.org/backend/abi.html)

## 实现接入证据 {#implementation-evidence}

`RuntimeRawModel`（query 30）在该阶段升到 schema 12、当前为 schema 23，在同一 `RuntimeRawContractV1` 中并入
`GcMetadataRuntimeContract`：schema 3（schema 2 起 trace descriptor 带 kind 字节、value program 带两阶段动作；schema 3 起 root 段是镜像根记录，四类真实位置加 `READ_ONLY_AFTER_INIT`/`LAZY_SLOT` 标志）、section 主版本 2、section 魔数 `GUGUGC01`、
arena 2 MiB / block 32 KiB / line 128 byte，与 slab/extent 参数同源。
`GcMetadataDemand` 由冻结类型表（`TypeUniverse.records` 与 `vtables`）推导，
类型数/vtable 数/trace 与 value program 字节数/根范围计数进入契约 fingerprint，
并并入 action key 与 query 键，使闭世界内容变化时整体契约身份同步变化。
`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `gc-metadata-type-count`、
`gc-metadata-trace-bytes`、`gc-metadata-value-bytes`、`gc-metadata-vtable-count`、
`gc-metadata-root-count`、`gc-metadata-arena-bytes`、`gc-metadata-block-bytes`、
`gc-metadata-line-bytes`、`gc-metadata-contract-fingerprint` 与
`gc-metadata-demand`；dump 输出三行 `gc-metadata schema=...`、
`gc-metadata-types ...` 与 `gc-metadata-fingerprint ...`，冷/热编译一致。

`gc_metadata_tests` 覆盖：最小 `GcMetadataWorldV1` 自洽、`boot_verify` 拒绝
缺 trace/value `End` 与 dangling child key、`GcMetadataDemand` 指纹稳定且随
字段变化、`GcMetadataRuntimeContract` 拒绝 arena 漂移、`RawModelError` 文本
展示，以及契约在 `RuntimeRawContractV1::build` 内的端到端集成。
`tests::image_plan_reports_gc_metadata_contract` 验证镜像计划含完整
`gc-metadata-*` 字段、dump 行存在、类型表扩张时 GC metadata 指纹变化。
当前 trace/value program 对每条 entry 只发单字节 `End`，编码与
`boot_verify` 均按规范运行；REPEAT_FIELD/ARENA_SLOTS 与 `String`/COW/`ResourceCell`
资源字段的扩展在 `placement` 与 `LocalHeap` 接入后补齐，不改变上述定位规则。

同一对象中的 `GcPacingRuntimeContract`（pacing schema 3）把
`GcPacingProfile` 的 16 个参数固定为唯一 `mosaic-default` revision 3：
`min_growth_budget`、`assist_threshold`、`assist_quantum`、`mark_cost_per_byte`、
`gc_cpu_fraction`、`gc_cpu_window_cost`、`remark_cost_budget`、
`evacuation_pause_bytes`、`evacuation_pause_roots`、`evacuation_pause_fields`、
`pressure_enter_ratio`、`pressure_clear_ratio`、`pressure_poll_bytes`、
`owner_drain_items`、`owner_drain_bytes` 与 `owner_drain_interval_bytes`；
契约还登记三个 pressure 状态
（`steady`/`drain`/`emergency`）、三个必须各自完成 owner drain 的账本分类
（`owner-cache-bytes`/`pending-return-bytes`/`reclaimable-bytes`，与
`LedgerSchemaV1` 的 committed 分区独立计数器逐项同源）、四种 assist 结局、
两种 remark 结局、两种 evacuation 结局与九个 credit 来源
（`barrier-buffer`/`card-mark-batch`/`edge-delta`/`pending-return`/`producer-staging`
加上 mark 阶段接入的 `mark-credit`/`mark-mailbox`/`mark-worklist`/`forwarding-work`）。
verifier 拒绝参数漂移、`0 < clear < enter < 100` 之外的 hysteresis、非 block 整数倍的
evacuation payload 上界、小于 extent 阶梯顶层的 evacuation payload 上界、装不下一次
assist quantum 的窗口预算、四个 drain 节奏参数的零值、小于一个 return node 的
`owner_drain_bytes`、与账本不一致的 drain 分类，以及内容与登记指纹不一致的契约。`GcPacingDemand`（分配站点、屏障站点、
assist slow edge、受管类型数）由优化后 LIR 与冻结类型表推导并进入契约指纹。
`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `pacing-contract-fingerprint`、
`pacing-profile`、`pacing-profile-revision`、`pacing-min-growth-budget`、
`pacing-assist-threshold`、`pacing-assist-quantum`、`pacing-mark-cost-per-byte`、
`pacing-gc-cpu-fraction`、`pacing-gc-cpu-window-cost`、`pacing-remark-cost-budget`、
`pacing-evacuation-pause-bytes`、`pacing-evacuation-pause-roots`、
`pacing-evacuation-pause-fields`、`pacing-pressure-enter-ratio`、
`pacing-pressure-clear-ratio`、`pacing-credit-source-count`、
`pacing-pressure-poll-bytes`、`pacing-owner-drain-items`、`pacing-owner-drain-bytes`、
`pacing-owner-drain-interval-bytes` 与 `pacing-demand`，
dump 输出 `pacing`/`pacing-budget`/`pacing-cpu`/`pacing-remark`/`pacing-evacuation`、
`pacing-pressure`/`pacing-drain-classes`/`pacing-drain`/`pacing-assist-outcomes`/
`pacing-remark-outcomes`/`pacing-evacuation-outcomes`/`pacing-credit-sources`/
`pacing-demand`/`pacing-fingerprint`，冷/热编译逐字节一致。

`MarkRuntimeContract`（mark schema 3，profile `mosaic-mark` revision 2，域 `gugu-mark-runtime-v1`）
把 mark 阶段的执行门禁固定成带版本对象：每 owner 单 consumer 的 `MarkMailbox`、`generation(32) |
slot(32)` 的 credit id、六个 cycle 状态（`idle`/`snapshot`/`marking`/`converging`/`remark`/
`complete`）、三个 credit 转移（`acquire`/`consume`/`return`）、六类 root snapshot 参与者
（`producer-stop-epoch`/`remote-consumer`/`root-slice`/`region-registry`/`handle-access-guard`/
`local-worklist`）、七项收敛条件（`local-worklist`/`published-batch`/`mailbox`/`barrier-buffer`/
`producer-epoch`/`forwarding-work`/`pending-credit`）以及「条件 → credit 来源」绑定。verifier
要求条件来源的并集恰好覆盖九个 credit 来源（既不遗漏也不重复绑定）、`mailbox_consumers == 1`、
`credit_slot_bits + credit_generation_bits == 64`、credit 池为正，并逐字段校验
`MarkMailboxHead`（64 字节 / align 64）、`MarkCreditHead`（64 / 64）、`MarkTerminationRecord`
（72 / 8）、`EdgeDeltaHead`（96 / 32）与 `CandidateCursorHead`（64 / 64）五条 record 布局与
`std/runtime/mark.gg` 的 Gugu 布局一致。`MarkTicket` 的 15 个字段只含稳定 arena descriptor、
对象偏移、目标 block 世代、source block 全局身份、cycle/topology epoch、credit 与 bytes，任何
managed 地址都会被 `verify_family` 拒绝。`source_block` 是全局块身份（`descriptor * 64 + block`）
而不是 arena 内下标：消费端据此解析来源 owner 并确认该 block 仍然提交在对应 heap 里，只带下标的
编码会被拒收。目标 block 世代同样在消费端复核：目标对象解析成功后其所属 block 的当前世代必须
等于 ticket 携带值，复用过 block 的 arena 不能接受旧 ticket。消费一条 ticket 的顺序是「完整性 →
目标目录解析 → 对象与世代 → 来源身份 → 消费 credit」，
因此被拒绝的 ticket 不会留下已经被扣掉的 credit，也不会把来源记到错误的 arena 上。

credit 池上界取「常驻 message node 容量 + 根槽数」：在飞的 ticket 与 edge delta 各占一个
non-moving node（两者共用同一 node pool 与同一 credit 池），根 seed 不占 node 但每个根槽每 cycle
至多一次，因此池耗尽就是契约违约。

`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `mark-contract-fingerprint`、`mark-runtime`、
`mark-cycle-states`、`mark-conditions`、`mark-snapshot-participants`、`mark-credit-pool`、
`mark-mailbox-consumers`、`mark-ticket-fields`、`mark-records` 与 `mark-demand`，dump 输出
`mark`/`mark-cycle-states`/`mark-conditions`/`mark-condition-sources`/`mark-credit-sources`/
`mark-credit-transitions`/`mark-snapshot-participants`/`mark-mailbox`/`mark-record`/`mark-field`/
`mark-ticket-fields`/`mark-demand`/`mark-fingerprint`。`mark_demand` 完全由 `GcMetadataDemand`
的 `root_range_count`、`BarrierDemand` 的 `card_mark_sites`/`edge_summary_sites` 与
`LocalHeapDemand` 的 `shared_sites` 推导，不新增 LIR 遍历，跨段相等性由 `RuntimeRawContractV1`
的 verifier 强制。

`EdgeRuntimeContract`（edge schema 1，profile `mosaic-edge` revision 2）固定候选回收的推进协议：
候选 job 的 10 个相位（`discover`/`trace`/`trial`/`scc`/`validate`/`commit`/`sweep`/`release`/
`complete`/`invalidate`，顺序即状态机推进顺序）、七个 block 候选状态（`allocating`/`candidate`/
`sweeping`/`evacuating`/`return-pending`/`owned-free`/`free`，与 `HeapBlockRecord.state` 的
判别值同源）、默认推进 quantum 4096、
`candidate_schema = 1`、`edge_buffer_entries = 512`（与 barrier 契约的 edge scratch 同值同源）、
`deltas_per_write = 2` 与精确追踪执行器 revision，并逐字段校验 18 个 `EdgeDelta` 字段
（含全局 source/destination 块身份、两个 generation 槽、delta 方向、cycle/topology epoch、
credit 与 integrity，不含任何 managed 地址）。`EdgeDemand` 由 `BarrierDemand` 派生并与
`MarkDemand` 交叉校验：`edge_sites` 必须等于 barrier 的 `edge_summary_sites` 与 mark 的
`edge_delta_sites`，`reserve_slots` 取 barrier 的 `shade_slots`——三份契约不允许各自记一份站点
计数。`edge-contract-fingerprint`、`edge-demand`、`edge-runtime`、`edge-candidate-quantum`、
`edge-candidate-schema`、`edge-phase-count`、`edge-block-state-count` 与 `edge-delta-field-count`
进入 `ImagePlan` 与 CLI JSON，dump 输出 `edge`/`edge-demand`/`edge-phases`/`edge-states`/
`edge-fingerprint` 行。

`runtime/pacing.rs` 的 `PacingPlane` 是契约的运行时对偶：`growth_budget` 取
`max(min_growth_budget, last_live × target / 100)`，`allocation_debt`、`mark_debt` 与
`pressure_debt` 按[内存所有权与消息通道](memory-messaging.md#allocation-debt-pressure-backpressure)
的公式以饱和整数 cost unit 计算；assist 只在真实 slow edge 上借用且不超过一个 quantum，
没有可消费 work 时返回 `no-work` 而不记账，偿还量先冲抵 `pending_mark_work`、余量再按
`mark_cost_per_byte` 折算 allocation 字节；`gc_cpu_fraction` 窗口内的普通 worker 工作
超出预算的部分转为 debt，emergency drain 可以越过吞吐预算；cycle 工作量取累计计数器
相对上一次基线的差值，因此 remark 门禁不会因为累计值增长而永久拒绝后续 cycle；
remark 在 barrier 已开启的前提下按预算给出 `complete`/`continuation`；relocation 的
bytes/root/field 三项任一超出即整 block `defer`。pressure episode 由平面线性化：enter
水位开启、达 soft limit 进入 `emergency`、请求字节与 committed 之和越过 limit 只进入
`drain`，降到 clear 水位且三类分类都被一次真实 owner drain 覆盖后才结束，一个 episode
至多授权一次 forced full cycle，用尽 drain 与 forced cycle 仍无 headroom 才进入
`OutOfMemory`；committed 快照按 `pressure_poll_bytes` 间隔刷新，有界 drain 按
`owner_drain_interval_bytes` 节奏推进。credit 账本用真实结构观测九个来源并要求全部归零
才能推进 credit epoch，credit epoch 只单调前进：未收敛的 cycle 不推进它，下个完成的
cycle 会追平 barrier epoch。mark 阶段的四个来源（`mark-credit`/`mark-mailbox`/
`mark-worklist`/`forwarding-work`）由 `MarkPlane` 与 owner worklist 直接观测，并同样进入
`credit_snapshot`，因此 per-cycle cost 窗口包含整堆标记工作量。

`world/pacing_impl.rs` 把这些决定接到真实路径：分配成功后累加 allocation debt；分配上的
唯一慢路径 `pacing_slow_edge(owner, bytes)` 由 `PacingPlane::slow_edge_due` 把关（assist 阈值、
cost window、`pressure_poll_bytes` 的 poll 节奏与 `min_growth_budget` 的 cycle 尝试节奏都不需要
全局读取，未触发时普通分配不付任何查询），先借一次真实 assist、再按节奏推进 hysteresis、在
episode 内执行一次有界 owner drain、按 allocation debt 启动自动 cycle，最后用「committed 估计
+ 本次请求字节」判断是否需要真正推进 headroom；只有可能越过 limit 时才刷新快照并走
drain → forced cycle → `OutOfMemory` 链，无法取得 headroom 时写入 rt0 的 fatal 并让本次分配
失败。drain 分 `Cycle` 与 `Bounded` 两种作用域、共用一条实现：都先交出未发布的 return
staging、再关闭 owner 真实的 source-slab cache（关闭顺带以 memory-pressure 原因冲刷该 owner
的 processor 账本，同一次 drain 不再重复冲刷、也不虚增空 flush 统计）、排空 owner inbox、
推进 queue-page grace epoch、取出 owner-local edge delta；只有 full cycle 过 remark 终止门禁、
把 per-cycle 的 card 工作计入滑动窗口（emergency 越过吞吐预算，普通 cycle 把超出部分转为
mark debt，拖欠的 mark 工作跨 cycle 存活由 assist 归还）并推进 epoch。随后对所有空载 extent
重跑 lease、live/queued slot 与在途 return 四条门禁，pause footprint 取候选自身撤销的
committed 字节与 descriptor 数而不是合成值；未过门禁的 extent 保持 committed 并计入
`blocked_extents`，超出一轮 relocation pause 预算的候选整块计入 `deferred_extents`、由下个
cycle 继续（单个 extent 至多等于预算上界，因此每轮至少推进一个候选）。通过门禁的 extent 在
`decommit` 的同一步让名下空载 descriptor 离开 committed 口径，物理页与账本不会各走一边。
cycle 推进 barrier epoch 后立即排空新 epoch 发布的批次并取走新 edge delta，再要求九个 credit
来源收敛：收敛则 credit epoch 同步前进并调用 `complete_cycle` 记录真实 live record 字节
（`committed − pending − reclaimable − cache` 的残差，由 `ledger_invariant` 在运行时强制守恒），
未收敛则本轮 cycle 未完成、credit epoch 保持落后并在下个完成 cycle 追平。软上限口径直接取 provider 的 committed 总量：
stack arena 与 raw plane 共用同一 provider，加一次 stack committed 会把同一物理页计入两次，
因此 `pressure_committed_bytes` 只做覆盖校验。`PacingPlane`/`CreditPlane`/`PressureEpisodeStats`
提供 `pacing-state`/`pacing-debt`/`pacing-pressure`/`pacing-credit`/`pacing-assists`/
`pacing-outcomes`/`pacing-gc-cpu` 各段 dump。

pacing 契约在 `RuntimeRawContractV1::build`、镜像计划与 CLI JSON（`build_json_reports_barrier_contract_keys`）
内的端到端集成中验证；`8b21ce3` 改由镜像内 `LocalHeap` 承载回收后，旧 world 参照实现的
`pacing_tests` 随之删除。
TLAB 的 arena 归属统一由已发布的 `limit - 1` 反推：cursor 可以停在块首，也可以恰好到达 arena 末端。minor nursery 定位、large mapping 查询/分配和 refill 共用这一条规则，避免末端 cursor 选择未提交的下一 arena 元数据或错误回收存活尾链。
契约还同样固定 Callable 的稳定类型键修复，该修复在提交 `0813ad6` 中独立完成：
[`mono/keys.rs`](monomorphization-cache.md) 统一 `Ty::Callable` 类型
稳定键，`concrete/layout.rs` 删除 `Callable` 特例，`mono/universe.rs` 改用
`MonoContext::type_key`，`INSTANCE_SCHEMA` 与 `MONO_SCHEMA` 递增到 5，使旧缓存
失效；新增 `function_item_type_key_matches_frozen_universe` 回归保证
`type_id[函数项]()` 不再报 `E0054`。
