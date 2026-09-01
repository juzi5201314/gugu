# GC 元数据

本章规定编译器和 Gugu runtime 共享的具体类型描述符、heap object header、trace program、全局根、vtable/glue 引用和写屏障 metadata。语言层的移动、pin、root 与 resource 语义见[内存与对象模型](../spec/memory.md)；本章固定当前实现，不构成跨编译器版本 ABI。

## 权威边界

[内存与对象模型](../spec/memory.md)、[值传递](../spec/passing.md)和[运行时](../spec/runtime.md)唯一规定对象寿命、引用有效性、pin、resource release、OOM与公开统计。本文独占官方 runtime 的 Immix布局、collector阶段、object header和 compiler/runtime metadata编码；这些参数不能回写成用户可依赖的地址、时序或回收保证。

## 总体约束

官方 runtime 当前使用精确、分代、并发标记且能够移动对象的 Immix collector：

- stack/root 只扫描编译器明确登记的位置，不做保守扫描；
- nursery对象在 minor cycle复制，old Immix arena在并发标记后按 block/line存活率选择机会性 evacuation；
- pinned、正在普通 `ForeignBridge` 或 `ForeignBridge[DirtyCpu]` 边界暴露和超大对象可以留在 non-moving region；`ForeignLeaf` 传递的可移动对象地址仍必须由调用方 pin，但不会建立 bridge handle。
- managed pointer 移动后由 collector 更新所有已登记 root/field；
- raw pointer 不参与追踪，跨 safepoint 必须由 pin 或规范允许的短生命周期保证；
- collector 不使用 read barrier，mutator write 通过统一 hybrid barrier 维持并发标记和分代 remembered set。

runtime 和 compiler 只能通过本章定义的 section/schema 交换静态 metadata。runtime 动态 heap bitmap、mark queue、card table 和 pin side table不写入镜像。

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
- bits 43..63：保留，必须为 0。

对象存活 mark 只以 arena side mark bitmap和 arena mark epoch为权威，不在 header复制第二个 mark bit；collector的 test-and-mark原子更新对应 bitmap word。

未转发时第二个 word 是实际 payload 字节数；固定大小对象必须等于 `TypeRecord.size`，动态对象必须至少覆盖 trace program 读取的所有字段。`FORWARDED` 为 1 时第二个 word 临时保存新 payload 地址，原对象不再按普通 descriptor 扫描。

`HAS_RESOURCE` 类型和 `HAS_RESOURCE_INSTANCE` 对象不进入 nursery/aging，直接分配到 old resource arena；该 arena可以在 major evacuation中逐对象移动活对象，但死亡对象必须逐一进入受限 release流程，不能整区丢弃。普通无 resource且 footprint不超过单个 Immix block的对象才进入 nursery line fast path；large、pinned和高对齐请求走 slow path。

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

## root 枚举

一个 GC cycle 的根来源封闭为：

1. 已停协程按[栈图](stack-maps.md)给出的 stack/register root；`Foreign` 与 `DirtyWaiting` 使用保存 PC 的 `ForeignBridge` map 扫描 coroutine stack上的 ABI bridge frame；
2. `RootRecord` 声明的 global，以及所有已登记 OS thread/TLS 实例和全部 coroutine 的已初始化 local payload；
3. scheduler/runtime 自己的强句柄表、等待队列载荷和 resource release queue；
4. 外部线程回调桥建立的临时 root handle；
5. 正在执行的 pin side table entry。

runtime 私有结构必须通过固定的 typed root visitor 枚举，不允许对其内存做保守扫描。`ForeignBridgeState` 自身不保存 managed pointer；它以 `(Coroutine*, stack_high-relative frame_offset)` 定位 ABI frame，collector在 `STACK_SCAN_LOCKED` 下根据调用点 map扫描和更新其中的 managed root。

普通 `ForeignBridge` 与 `ForeignBridge[DirtyCpu]` 都只通过已保存的 Gugu stack/map和显式 pin暴露根。foreign/dirty worker 的 OS stack、C/C++ stack和 opaque asm寄存器不在精确 metadata 范围内，collector绝不对其做保守扫描；传给 native 的 managed地址必须在进入前 pin，或改为复制到 non-moving storage。native work永不返回时，相关 coroutine frame/pin会一直保留，但它不阻止其它 heap的 mark、relocation或 stop epoch完成。

## write barrier 与 remembered set

所有可能覆盖 heap managed field 的写入由 LIR `GcWriteBarrier` lowering 成统一 hybrid barrier：

1. 读取旧值；
2. 并发标记开启时，若旧值非空则 shade 旧 target；
3. 当前 coroutine stack 尚为 grey 时，若新值非空则同时 shade 新 target；没有 current coroutine 的 runtime/system write 一律按 grey 处理；
4. 执行实际 store；
5. owner 在 old/immortal generation 且新值指向 nursery/aging 时，把 owner 所在 512 字节 card 标脏。

这是 Go 风格的 Yuasa deletion 与 Dijkstra insertion 混合屏障。shade 操作只入队第一次从 white 转 grey 的对象；不能递归扫描 mutator stack。标记关闭时步骤 2、3 由一个 runtime flag 分支跳过；generation 条件可由 TLAB/new-object 分析消除。

编译器只有在证明 owner 是尚未发布的新 nursery object、写入发生在任何 safepoint/逃逸之前且旧 slot 未初始化时，才可省略屏障。向 global、old、共享对象、unknown alias 或 foreign 可见内存写 managed pointer 不能省略。

card table 每 512 heap 地址字节使用 1 byte；0 clean，1 dirty。minor cycle 在 mutator 已停止后以 AcqRel swap 把 dirty card 取为 0并扫描，所有 card 处理完成后才恢复 mutator；因此不存在恢复后清零覆盖新写入的窗口。mutator 用 release store 写 1，collector acquire 取得；重复写 1 是幂等的。

## collector 使用 metadata 的阶段

minor cycle 停止需要扫描 nursery 的 mutator，复制可移动对象并更新根/字段；old/pinned/large object 不因 minor cycle 移动。对象 age 增加到 2 后提升到 old，达到 15 饱和。

major cycle：

1. 短暂 root snapshot，启用 hybrid barrier；
2. 与 mutator 并发解释 descriptor、完成 mark；
3. remark safepoint 扫描未完成 grey stack 和 barrier buffer；
4. 根据 block/line live bytes、pin和 resource状态选择可机会性 evacuate的 old arena；
5. 在 stop/update阶段搬移所选活对象，利用 forwarding pointer更新全部 root/field；resource arena逐对象处理，不能整区丢弃；
6. 重建 card、关闭 barrier并恢复 mutator；
7. GC worker与 mutator并发 sweep未 evacuate的 old/resource block，按 line回收死亡对象、把受限 resource动作送入 release queue，并把完全空 block/页归还全局池或 OS。

GC stop 的完成条件只统计 active `LogicalProcessor`。普通 `ForeignBridge`、`ForeignBridge[DirtyCpu]` 和 `DirtyWaiting` 都没有 processor，因而不需要确认 stop；它们的 ABI frame roots在根枚举阶段保持可见，native work完成后再通过普通 resume safepoint回到 managed heap。

GC worker解释 trace program时使用显式小栈；嵌套上限 32，采用固定 `[TraceFrame; 32]`。mark queue和 release queue无固定上界，使用分段 work deque/pool；并发 sweeper只能取得 block的 `Sweeping` lease，allocator只能取得 `Allocating` lease。

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

任何静态验证失败阻止产出镜像；Booting 中发现损坏进入 `RuntimeInvariant` fatal。runtime 不能忽略未知 opcode 或把未知类型按无指针对象扫描。

## 参考实现资料

- [Go runtime heap bitmap 与类型扫描](https://go.dev/src/runtime/mbitmap.go)
- [Go runtime GC program 与 metadata](https://go.dev/src/runtime/mgcdata.go)
- [Go runtime hybrid write barrier](https://go.dev/src/runtime/mbarrier.go)
- [Rust 编译器类型布局与 ABI](https://rustc-dev-guide.rust-lang.org/backend/abi.html)
