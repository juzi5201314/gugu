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

完整 block 默认以 `HeapBlockReturn` 批量返还 allocation owner；line-run、arena 和 large
mapping 只有在各自的 scanner、allocator、evacuation、forwarding、handle access 和
queue grace lease 完成后才能启用。block return、allocation debt、pending bytes 和
owner handoff 的完整协议由[内存所有权与消息通道](memory-messaging.md)独占；本章继续
独占 Immix metadata、trace、barrier、handle representation 和 collector lease 的布局
与阶段。

### Mosaic managed storage metadata


`TurnRegion` 使用稳定 region descriptor 和 owner-local bump cursor。descriptor 至少包含
owner token、region generation、capacity、used bytes、export summary、active transfer
lease 和 reset state。region payload 只容纳已由 `EscapeAndPlacement` 证明为当前 coroutine
turn 私有的 managed allocation；它不进入普通 mark bitmap，也不能包含必须独立 release
的 ResourceCell。region 的 object-start metadata 仍按 concrete `TypeId` 记录，使 promote
和验证可以使用同一 trace/value program。

region state 只能按下列方向转换：

```text
Private → Publishing → LocalPromote
Private → Publishing → RegionTransfer → Received
Private → ResetPending → Reset
```

`Publishing` 前必须登记所有可能逃逸的 root、slot 和 identity handle。若 sender 继续
需要访问对象，compiler/runtime 必须走 `LocalPromote` 或语义复制，不能发布
`RegionTransfer`。receiver 取得 region generation 后成为唯一 runtime owner；transfer
message 未消费前，region bytes 属于 pending pressure，不能 reset 或复用。

`SharedHeap` 使用 stable handle table。handle 是逻辑 object identity，不是用户可观察的
整数；其内部键至少包含 table/cage 标识、slot 或 object id 和 generation。slot 保存
当前 payload descriptor、TypeId、generation、access-guard 状态和 forwarding lease。对象
迁移只在 handle slot 的线性化点替换 current payload；旧 payload 在全部 access guard、
pin、mark ticket 和 forwarding grace 结束前不能复用。generation 变化时旧 handle 进入
`StaleHandle` verifier 路径，不能静默解析到复用对象。

`LocalHeap` 的 direct managed pointer 不经过 read barrier。只有 SharedHeap 的 handle
resolve、compressed reference decode 或 forwarding guard 需要额外访问步骤；compiler
不能把 shared direct pointer 带出 guard。handle table 和 forwarding metadata 是 runtime
动态状态，不写入镜像；trace program 仍按逻辑字段扫描，不维护第二套字段描述。

compression profile 可以将连续不超过 4 GiB 的 managed heap cage 表示为 32-bit offset，
并以 cage id、generation 和 offset 形成内部引用编码。cage base、长度、对齐、offset
加法和 generation 校验全部 checked；large、pinned、foreign、跨 cage 对象使用完整地址
或 stable handle。compressed reference 不能进入 raw plane，也不能绕过 FFI 的 resolve、
pin 和 native address 生命周期。

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

## `TypeId` 与 descriptor table

单态化闭合后，编译器按[单态化与编译缓存](monomorphization-cache.md#具体类型集合与-typeid)的 `StableTypeKey` 顺序分配 `TypeId`。每个 `0..type_id_count()` 值在 type section 中恰有一条固定 80 字节 `TypeRecord`；记录顺序就是 `TypeId`，不重复保存数字 ID。

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
version:              u16 = 1
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
`TypeRecord` 的逻辑 trace/value program。

对象存活 mark 的权威按 representation 分层：LocalHeap 使用 arena side mark bitmap 和 arena mark epoch，TurnRegion 私有对象在 export/reset verifier 中以 region root/lease 状态判定，SharedHeap 使用 handle side mark 和 cycle epoch。任何层次都不在 header 复制第二个 mark bit；collector 的 test-and-mark 原子更新对应 side metadata。

未转发时第二个 word 是实际 payload 字节数；固定大小对象必须等于 `TypeRecord.size`，动态对象必须至少覆盖 trace program 读取的所有字段。LocalHeap direct evacuation 的 `FORWARDED` 为 1 时第二个 word 临时保存新 payload 地址，原对象不再按普通 descriptor 扫描；SharedHeap forwarding 则以 stable handle slot 的 current payload 为权威，不把 slot 更新写成 payload header 地址。

`HAS_RESOURCE` 类型和 `HAS_RESOURCE_INSTANCE` 对象不进入 nursery/aging，直接分配到 old resource arena；该 arena可以在 major evacuation中逐对象移动活对象，但死亡对象必须逐一进入受限 release流程，不能整区丢弃。普通无 resource 且 footprint不超过单个 Immix block的对象：若当前 coroutine turn 私有则优先进入 TurnRegion，否则进入 nursery line fast path；large、pinned 和高对齐请求走 slow path。

嵌套 pin 次数存入 runtime pin side table，header 只保存是否非零。pin/unpin 对 side table 和 `PINNED` 的 0↔1 转换在线性化点原子完成，因而不受固定宽度计数上界限制。pin table 以 object identity 点查，属于冷路径；不得为每个未 pin 对象预留独立 entry。

最外层 `pin` 是 safepoint。target 位于 nursery/aging 时，slow path 先把 owner allocation 提升到可固定的 old region，按栈图和 heap descriptor 更新 `p` 及其它强引用，再增加 side-table 计数并设置 `PINNED`；arena 槽 pin 的 owner 是整个 arena backing allocation。已在 old/immortal region 时只更新计数。最外层 unpin 把计数降到 0并清位，但不立即搬移对象，后续 major cycle 才可选择它。

`payload_base = header_base + 16`，高对齐 padding 位于 header之前；layout consumer、FFI lowering与 GC 都只能从统一的 `payload_base` 派生地址，不能各自维护第二套偏移。对外布局是否可见只由 [`repr(C)` 与平台 ABI](../spec/platform-abi.md#reprc-结构体)定义，本节只固定官方 runtime 的私有地址关系。

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

close与最后 lease竞争 `CLOSED` CAS；首个关闭者把 `{ cell, generation, descriptor }` 送入 release queue并设置 `RELEASE_QUEUED`，其它路径只结束自身 lease。worker在 generation匹配时执行一次受限 release并设置 `RELEASE_DONE`。worker与最后 lease随后都调用 `try_reclaim`：只有观察到 `leases == 0 && RELEASE_DONE` 并成功 CAS设置 `RECLAIMING` 的一方才能递增 generation、清理 payload并归还 slot。显式 close时仍存在的 handle继续观察 closed，cell地址不会提前复用。普通 resource handle仍是一个稳定 cell pointer。

slab以 64 KiB页按 64、128、256、512、1024、2048、4096 byte class管理，class包含 header、对齐 padding和 raw payload；超过 4096或 payload对齐超过 64时使用独立 non-moving整页 mapping。每页使用连续 allocation bitmap和 intrusive free index，不把稳定地址放入移动 GC或全局 hash map。release descriptor明确证明 payload无 managed pointer，typed root visitor因此不扫描 cell bytes。

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

## 镜像根、vtable 与源码 metadata

Linux `.gugu.meta` / Windows `.ggmeta` 保存根、动态分派和运行时源码位置 metadata。header 固定为：

```text
magic:                   [u8; 8] = "GUGUMT01"
version:                 u16 = 1
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

function index与 stack-map function table相同，PC 为 function-relative 半开范围。path 是 package-relative逻辑 UTF-8路径，不含 workspace绝对路径；line/column 从 1开始。`source_strings_len` 不得超过 `u32::MAX`，每个 `path_offset/path_len` 都相对 source string pool并经 checked range验证。flags bit 0 `PANIC_SITE`、bit 1 `SYNTHETIC`，其余为 0。records 按 function index、pc_start、pc_end和路径 bytes排序，范围可以因内联 attribution嵌套；查找选择覆盖 PC 的最短范围，再按记录序打破相等。source string pool按 bytes去重排序，`.gugu.meta` 与运行时 panic/backtrace所需记录不得被 `--strip` 删除。

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

## write barrier、edge summary 与 remembered set

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

这是 Go 风格的 Yuasa deletion 与 Dijkstra insertion 混合屏障，并附带 owner-local edge
summary。shade 操作只入队第一次从 white 转 grey 的对象；不能递归扫描 mutator stack。
标记关闭时步骤 2、3 由一个 runtime flag 分支跳过；generation 条件可由 TLAB/new-object
分析消除。`EdgeDelta` 只表达经过 epoch 聚合的 block edge，不是普通对象 reference count。

### CardMarkBuffer 与 remembered-set flush

`card table` 仍是每个 arena 的 512-byte 粒度元数据，但 mutator 不在每次 old-to-young 写入时直接写共享 card byte。每个 `LogicalProcessor` 拥有固定 256 项的 `CardMarkBuffer`，每项只保存 `{ arena_descriptor, arena_generation, card_index, cycle_epoch }`；buffer 与 dedup 表均位于 processor owner-local storage，不保存 managed pointer。选择固定上界是因为 card mark 只需记录“脏过”这一位，重复键可以在本地合并；达到上界才进入 flush slow path，不把每次写入变成共享 cache-line 写。

barrier 在完成实际 field store 后，把 distinct card 键放入本地 buffer。dedup 使用固定大小的直接映射 stamp 表：冲突只会留下已经进入 buffer 的旧键，不得丢弃尚未发布的键。当前 processor 是 arena allocation owner 时，flush 可以在 owner 上按 arena/generation 合并后写 card table；其它情况发布 `CardMarkBatch` 到 arena allocation owner 的 card mailbox。batch 只携带稳定 arena descriptor、generation、card index/range、cycle epoch 和 bytes，不携带 field 地址或 managed pointer。card table 只能由 arena owner 写入，避免多个 processor 长期争用同一 card cache line。

buffer 满、processor 交接、进入 `ForeignBridge`、memory pressure、minor stop 请求和 producer stop gate 都必须 flush。flush 以 Release 发布实际 field store 之前已经完成的 buffer 内容；owner 以 Acquire 消费 batch 后再写 card table。minor cycle 在扫描 remembered set 前必须确认所有 active processor 的 buffer 已 flush、所有旧 epoch card batch 已消费或登记在 owner credit 中；因此不能以“当前 card table 已清零”代替 producer drain。卡片重复写是幂等的，按 arena/generation 不匹配的键进入 `RuntimeInvariant`，不能静默忽略。

card buffer 的 256 项、dedup stamp 和 pending batch bytes 都计入 runtime pressure；pressure flush 可以提高 batch drain budget，但不得在 `NoSafepointRegion` 内分配节点、阻塞或遍历其它 owner。若 flush 需要 refill，必须在 region 外建立 mandatory statepoint；`BarrierReserve.max_card_marks` 不足时不能把 card table直接写成共享 fast path。

编译器只有在证明 owner 是尚未发布的新 nursery object、写入发生在任何 safepoint/逃逸之前且旧 slot 未初始化时，才可省略屏障。向 global、old、共享对象、unknown alias 或 foreign 可见内存写 managed pointer 不能省略。

`NoSafepointRegion` 内需要执行 barrier时，compiler在 region外发出 `BarrierReserve { permit }`，其 `BarrierPermitData.max_shades` 由 concrete type descriptor逐 pointer word计算，`max_card_marks` 由本 region可能触及的 distinct `(arena, card)` 键上界计算；deletion+insertion每次写至多消费两个 shade slot和一个 card-mark slot。buffer不足时在 region外走 refill mandatory statepoint；成功后 compile-time permit证明下一 region拥有足够容量，该 ID不形成 machine value。region内只能使用 `GcWriteBarrierReserved { permit }`，禁止再次检查容量或连接 refill edge；verifier统计实际静态消费，未用额度无需生成归还指令。并发标记关闭时 reservation与 reserved barrier按同一 flag折叠，不给普通 store增加第二次 flag load。

无法在 `POLL_BUDGET` 内完成的 aggregate copy不能因持有 runtime lock而关闭 safepoint。channel/select等 runtime原语必须先在短 region内取得带 generation的不可见 transfer reservation，在 region外完成 descriptor copy与普通 barrier，再在第二个短 region发布；reservation由 typed visitor扫描，未发布 payload不能被 receiver或 close观察。

card table 每 512 heap 地址字节使用 1 byte；minor cycle 在 mutator 已停止且所有 processor buffer 已 flush、所有 `CardMarkBatch` 已由对应 arena owner 消费后，以 AcqRel swap 把 dirty card 取为 0并扫描。card table 的 owner 写入可使用 owner-local ordinary store；batch 发布以 Release，owner 消费以 Acquire，重复写 1 是幂等的。`EdgeDelta` staging 也只能在有足够 permit、已登记 owner generation 和可追踪 cycle credit 时发布；不能在 `NoSafepointRegion` 内临时分配消息节点。

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

## GC pacing 与 relocation pause budget

Mosaic 的 collector 以 `GcPacingProfile` 固定下列内部参数：`min_growth_budget`、`assist_threshold`、`assist_quantum`、`mark_cost_per_byte`、`gc_cpu_fraction`、`remark_cost_budget`、`evacuation_pause_bytes`、`evacuation_pause_roots`、`evacuation_pause_fields`、`pressure_enter_ratio` 和 `pressure_clear_ratio`。这些参数与 `CompilerIdentity`/runtime tuning profile 一起版本化并进入 digest；它们是实现门禁，不是用户可观察的时间单位。

每个 cycle 的 `allocation_debt` 先按[内存所有权与消息通道](memory-messaging.md#allocation-debtpressure-与-backpressure)计算，再乘以 descriptor/profile 的 `mark_cost_per_byte` 形成 mark debt。processor 在 TLAB refill、allocation slow edge 或显式 poll 处最多执行一个 `assist_quantum` 的标记/edge/card 工作；一次 assist 不能无限追债，也不能持有 runtime lock 跨 safepoint。mutator assist 和 collector worker 都归入同一 cycle credit，只有完成的 work 才能归还 credit；没有可消费 work 时不得虚构进度。

GC worker 的执行由 `gc_cpu_fraction` 的滑动 cost window 限制。空闲 processor 可以在未使用的 CPU 额度内执行 GC；有 runnable 压力时，超出额度的工作转为 allocation debt 和后续 assist，而不是创建无界 GC worker。memory pressure、cycle termination 和 lease/grace 正确性优先于吞吐预算，但每个 slow edge 仍受 scheduler 的 poll/service budget 限制。

collector 不能以一次不受限的 remark 或 evacuation 把预算转化为暂停尖峰。remark 必须在 `remark_cost_budget` 内完成；超出时保持 hybrid barrier、继续 concurrent mark/owner assist，并发布 continuation，不能在未终止的 mark cycle 中恢复普通 barrier。`MosaicThroughput` 可以使用较大的 profile budget，但仍受上述上限。`MosaicLowLatency` 只选择完整 relocation/update footprint 同时不超过 `evacuation_pause_bytes`、`evacuation_pause_roots` 和 `evacuation_pause_fields` 的 block；候选超过任一上限就整 block 延后，不能部分发布 direct pointer 更新，也不为 LocalHeap direct pointer 隐式增加 read barrier。延后的 block继续由 sweep/后续 cycle处理，直到有完整预算；SharedHeap 仍使用已有 handle forwarding。

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
- `BarrierPermitId` 的 `max_shades` 与 concrete descriptor一致、只关联一个 `NoSafepointRegion`且静态消费不超额，所有 `GcWriteBarrierReserved` 都没有 refill edge；不可见 transfer reservation具有合法 generation、trace descriptor和唯一 publish/cancel结局；
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
