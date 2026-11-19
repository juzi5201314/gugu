# 栈图

本章规定 compiler、runtime、移动 GC、协程栈复制和 panic unwinder 共享的 stack map 契约。编码只保证与同一 `CompilerIdentity` 构建的 runtime 配套；它不是平台 C ABI，也不能跨编译器版本拼接。

官方 runtime 的精确移动 collector与可复制协程栈要求 compiler提供完整活跃位置和 frame信息；runtime不允许把“看起来像地址”的机器字当作根。替代实现若不消费该格式也不能把 bytes误当平台 ABI。

## 权威边界

[内存](../spec/memory.md)、[并发](../spec/concurrency.md)、[运行时](../spec/runtime.md)和[平台 ABI](../spec/platform-abi.md)唯一规定引用有效性、pin、safepoint行为、panic边界和外部栈约束。本章只固定同一 compiler/runtime build 内如何描述和更新已经由这些语义判定为活跃的机器位置；stack-map bytes、frame公式和寄存器编号不是用户或外部工具 ABI。

## 前提与术语

- **safepoint**：runtime 可以暂停该协程、扫描/更新其根或切换调度状态的机器位置。
- **frame base**：函数完成 prologue 后保持不变的 `rsp`。
- **frame size**：从 frame base 到调用者压入的 return address 之间的字节数。
- **slot**：从 frame base 起、8 字节对齐的一个机器字位置。
- **top frame**：当前被暂停协程最内层的 Gugu 用户 frame。
- **caller frame**：通过 return address 逐层恢复的更外层 frame。

普通 Gugu 函数禁止动态 `alloca`。完成 prologue 后到 epilogue 前不得改变 `rsp`；最大的 outgoing 参数区、Windows shadow space、spill、局部对象和保存寄存器都预留在固定 frame 中。编译器生成的 runtime bridge 可以有专用 frame flag，但也必须提供等价的固定 frame record。

有 safepoint 或调用的函数使用 `frame_size % 16 == 8`，使 x86_64 函数入口的 `rsp % 16 == 8` 在 `sub rsp, frame_size` 后得到 16 字节对齐。return address 固定在 `[frame_base + frame_size]`。真正无调用、无 safepoint、无 stack root 的 leaf 可以使用零 frame，不进入 stack map 表。

## 根类别

每个活跃指针位置恰属于以下一种类别：

| 类别 | 含义 | GC/栈复制动作 |
|------|------|---------------|
| `HeapDirect` | 指向 LocalHeap 已知对象 payload 起点的强引用 | 标记对象；移动后写回新 payload 地址 |
| `HeapInterior` | 指向 LocalHeap 对象字段、元素或切片起点 | 通过 heap span metadata 找到对象与内部偏移；移动后按偏移写回 |
| `SharedHandle` | 指向 SharedHeap stable handle slot 的 managed 引用 | 标记 handle 当前 payload；forwarding 只切换 slot，访问必须处于 access guard |
| `CompressedRef` | 指向登记 heap cage 内 offset 的压缩 managed 引用；根种类判别值 5，与 GC 根槽目录的 `compressed-ref` 同源 | 经压缩平面 checked 解码 cage id/generation/offset 与 canonical 地址；对象搬迁后按新 payload 重新编码；失败进入 `RuntimeInvariant` |
| `StackInterior` | 指向当前协程栈范围内的 local/字段 | 不标记 heap；复制栈时加 relocation delta |
| `NonRoot` | 原始指针、代码、metadata、整数或已死值 | 不扫描、不修改 |

pin 状态属于目标对象头和 pin token，不是另一类位置。指向 pinned 对象的根仍编码为
`HeapDirect`、`HeapInterior`、`SharedHandle` 或 `CompressedRef`；GC 根据对象/handle 状态决定不移动。

`dyn Trait`、slice、string、函数值等多字值只标记实际的 managed data word；长度、容量、
vtable、cage id 和 code pointer 是 `NonRoot`。`#[repr(packed)]` 不能含 managed 引用，
因此 stack root 总能落在自然对齐的完整机器字中；compressed reference 只在 compiler
登记的 cage profile 中作为 managed root 解码。

安全引用若可能越过 frame 生命周期、进入另一个协程或被写入 heap，`EscapeAndPlacement`
必须先把被引用值提升到 LocalHeap/SharedHeap 或稳定 arena。成功 LIR 中不允许 heap 对象
保存 `StackInterior`，也不允许一个协程保存指向另一协程 stack 的安全引用。TurnRegion
中的引用必须随 region export summary 一起登记；region reset 前不能存在外部 root。

## safepoint 种类

编码中的 `kind` 固定为：

| 值 | 名称 | PC 含义 |
|----|------|---------|
| 0 | `CallReturn` | `call` 指令之后第一字节；扫描 caller frame 时使用 |
| 1 | `PollResume` | safepoint slow path 返回后的 resume label |
| 2 | `SuspendResume` | 协程重新变为 running 时继续执行的 label |
| 3 | `ForeignBridge` | Gugu 与 C/runtime bridge 完成寄存器保存后的 label |
| 4 | `MorestackEntry` | prologue建 frame前进入 `morestack_or_poll` 的 slow-path label |

普通/dirty `ForeignBridge`、park/suspend、runtime lock acquire的 contention edge和其它 mandatory statepoint一定建立对应 map。直接 managed call若目标保留 entry `StackCheck`，caller return PC必须有 `CallReturn` map，因为 callee可能在建立 frame前进入 `morestack_or_poll`；`PollFreeLeaf` 调用和 `ForeignLeaf` 本身不建立专用 safepoint record，即使它们带 unwind 边。这些叶调用的落地链仍从 unwind 边生成，不依赖 safepoint 记录。counted inner chunk edge与 uncounted countdown-only edge没有 map，只有实际读取 poll word后的 resume label建立 `PollResume`；显式 `safepoint_poll()` 使用同一 kind。poisoned函数入口使用 `MorestackEntry`，同时覆盖 poll和真实 stack growth。`NoSafepointRegion` 内没有 map或 resume PC，begin前/end后的实际 poll仍使用 `PollResume`。

LIR 侧的映射补充：无 default 的 `select` 经挂起路径提交，走 `SuspendResume`；有 default 的非挂起 `select` 走 `CallReturn`。分配操作（`GcAlloc`/`RegionAlloc`/`PromoteManaged`）进入函数级分配站点计数，并在所在指令上留下强制 `CallReturn` 记录：bump 快路径本身不回收，但同一站点发射的 slow call 可以启动回收，返回 PC 必须能按本帧 map 扫描根；**裸**屏障（`GcWriteBarrier`/`SharedFieldBarrier`，无 permit）在 lowering 里落成 runtime 调用，slow path 会 shade，必须留下 `CallReturn` 记录并同时计入屏障站点计数；带 permit 的预留屏障只在对应的 `NoSafepointRegion` 内出现、由 `BarrierReserve` 额度保证不进入 slow path，与不产出指令的 `BarrierReserve` 标记一样只计数。`DecodeCompressedRef` 为纯解码，不建立记录；`ResolveSharedHandle`、`ForwardSharedHandle`、`SharedAccessBegin/End` 调用走 `CallReturn`。访问 begin/end 同时登记输入 handle，尚未写回的 payload 结果不充当当前点的根；该推导规则进入逻辑栈图 schema 9。

`MorestackEntry` 的固定 scratch发布发生在任何 GC/preempt处理之前，且不写 candidate以下地址；因此 slow path即使同时由容量不足和 poison触发，也能先停机或调度出去。恢复后必须以 GC/stack copy已经修正的 scratch和最新 `stack_low`重试原 `StackCheck`，不能从旧 candidate直接建立 frame。

signal/APC handler不在任意PC直接扫描用户 stack。它只设置当前 processor的 poll word、投毒 current coroutine的 `stack_check`并唤醒 worker；真正暂停和扫描发生在 compiler登记的 `PollResume`、`MorestackEntry` 或 mandatory statepoint。dirty native stack不属于 Gugu stack map。

## 寄存器规则

### 内部 A64 runtime-site 边界（4c.1）

4c.1 最初只发布无根运行时站点的最终 BL 返回 PC 与 CFA 诊断；4c.2b.1 的序列化接线见后文。
返回 PC 必须精确匹配，不能用最近站点代替；MorestackEntry 的 frame 尚未建立，
原始 LR 在 x10；PollResume 按 CFA=SP+frame.size，原始 LR 在 SP+record+8 恢复。
x29 指向 FP/LR record，但 poll 后不能假设 x29 的值有效，因此诊断使用 SP-based CFA。

下表的 v4 寄存器编号仅适用于 x64，不能把 xN 强行解释为同号 x64 bit。4c.2a 的 Rust
codec/walker 已支持下面的 A64 v5 契约；4c.2b.1 已接通标量根 spill、managed call 与 safepoint
CFA。coroutine handoff 与镜像中的 Gugu runtime decoder 尚未切换到 A64，不承诺可运行用户镜像。

### A64 v5 寄存器与布局

v5 header byte 24 的 u32 为架构 tag 183，byte 28 的 u32 仍为零。v4 隐式为 x64，
两个保留字保持零；编码默认入口与 RuntimeRawContract 的 x64 指纹/输出字节不变。
只接受 (version=4, tag=0) 与 (version=5, tag=183)，未知组合拒绝。

A64 用五个 u32 register mask，xN 的位号就是 N（0..30）；SP/bit31 不编码。
x16/x17 scratch、x18 platform、x27/x28 runtime context、x29 FP 与 x30 LR 不得为用户根。
CallReturn/PollResume/SuspendResume/ForeignBridge 的 mask 全零；MorestackEntry 仅可
登记 x0-x8 参数根，slot_count=0 且无 typed root。非入口点的根槽与初始化字节必须在 frame 内。
frame_size 是 16 的倍数，code RVA/size 与非零返回 PC 均须 4 字节对齐，PC 严格小于 code_size。

v5 root record 头为 slot_count:u32 + 五个 u32 mask，共 24 字节，无额外保留字；其后五类
slot bitmap、4 字节 padding、typed_count/typed root 与 v4 共用同一算法。v4 保持原 16 字节头。
DecodedSection 携带 architecture，mask 统一扩为内存中的 u32，不把 A64 截成 u16。
scan_frame 按精确 return PC 找站点，分别扫描 frame words 与调用方提供的入口寄存器 scratch；
两者复用原有五类 scan_roots、初始化门控、handle 与 compression 处理，寄存器结果 offset
是该架构的寄存器编号。找不到精确 PC、缺少 frame/register snapshot 均报错，不猜相邻 map。

stack map 的通用寄存器编号固定为：

| bit | 寄存器 | bit | 寄存器 |
|-----|--------|-----|--------|
| 0 | `rax` | 8 | `r9` |
| 1 | `rbx` | 9 | `r10` |
| 2 | `rcx` | 10 | `r11` |
| 3 | `rdx` | 11 | `r12` |
| 4 | `rsi` | 12 | `r13` |
| 5 | `rdi` | 13 | `r14` |
| 6 | `rbp` | 14 | `r15` |
| 7 | `r8` | 15 | 保留，必须为 0 |

`rsp` 不作为普通根；它由 coroutine context单独保存。managed pointer不放入 XMM寄存器，`V128` 也禁止 pointer lane并始终属于 non-root；跨 taken poll slow edge活跃的向量值由普通16字节 spill保存，但不进入 root map。`r14`/`r15` 是 runtime保留寄存器，普通用户值不能占用；对应 mask bit在普通函数中必须为0，runtime bridge只通过专用根表扫描它们。

在 `SuspendResume`、`ForeignBridge` 点，所有跨该点活跃的用户 managed/stack pointer必须 spill到 stack slot，三个寄存器 mask均为0。`ForeignBridge[DirtyCpu]` 使用相同 bridge root map，native段不建立额外 stack map；`CallReturn` map必须包含被调方 entry check期间仍活跃的 caller root、outgoing managed/stack参数word和 aggregate副本，即使它们在正常返回后已死。`PollResume` 同样把跨点 managed/stack pointer 落栈，五类 register mask 为零；压力慢路由 system-stack collector 直接回写这些槽。`MorestackEntry` 的 `slot_count`固定为0，register mask描述保存到 coroutine morestack scratch的 managed参数寄存器；caller stack参数和 aggregate副本由外层 `CallReturn` map追踪。`PollFreeLeaf`/`ForeignLeaf` 没有 map，caller prologue必须已把函数内所有 direct leaf `stack = N` 的最大值计入 reserve。

该限制避免 caller frame 依赖 callee-saved 寄存器的跨 frame 追踪；poll 中的非根标量仍按通常规则分配。

## 构造流程

stack map 只能在 LIR 完成寄存器分配、spill slot 分配和 frame layout 后生成。逻辑推导的 schema 是 `STACKMAP_SCHEMA = 9`：v9 为共享访问 begin/end 登记 `CallReturn` handle 根；回调与压力服务在 poll 慢路尾跳到 managed 让出入口，父帧仍按原始返回 PC 精确匹配 `PollResume`，挂起期间同样保留根。v8 起普通和 dirty bridge 的指令与 Invoke 都登记 outgoing 参数包根；整数地址车道从包偏移 16 开始，原 C 栈参数从 176 加平台栈偏移开始，32 字节 fixup 中的栈深度仍是标量。分配器与物理栈图共用 lowering 的车道换算；`ALLOCATION_REVISION = 6`、`METADATA_REVISION = 2` 使旧机器片段失效。托管调用上的分配种类仍产出 `CallReturn`，按值副本只展开已有 provenance 的根字；裸屏障调用与其它 mandatory statepoint 一样产出 `CallReturn`（v3 只计数、不产记录）；v5 起调用点还登记本帧活跃的槽根字；v6 起分配 op（`GcAlloc`/`RegionAlloc`/`PromoteManaged`）的返回 PC 也是强制 `CallReturn`——同一站点发射的 slow call 可以启动回收，回收要在该返回 PC 上按本帧 map 扫描根；v7 起根集合改由 SSA 逆向活跃集（`LiveValues`）枚举，跨点存活但既不是本条指令操作数、也不在 LIR 栈槽的受管值同样登记 `Value` 根。

1. 从 safepoint 的跨点活跃集（`LiveValues`，SSA 逆向、may 语义）取得所有 `Ptr` value，并入本条指令自己的操作数与 LIR 栈槽根字；`CallReturn` 另加入 outgoing 参数，以及本帧按值聚合副本槽上已有 provenance 的 root word。活跃集沿边把后继块参数替换为对应实参（参数是被持有的接收入口，值在实参一侧），并扣除位于该点位的定义——安全点在该指令执行期间触发，该指令的结果尚未写入。块内由指令定义的值不构成 live-in（SSA 下同块定义必然支配同块使用），否则定义点之前的前驱会凭空多出根。**块参数不受这两条扣除影响**：它是块的输入，在块入口处就已可用，且只有留在 live-in 里才能被边实参替换；把它当成「本块产生的定义」一并扣掉，会让替换永不触发，于是跨块携带的受管值——未取地址的切片恰好退化成一条块参数链——在前驱块里凭空消失，安全点上不再登记根。没有 provenance 的位字是 `NonRoot`，不进入 bitmap；副本地址不属于本帧栈槽时，字根由持有副本的外层帧登记；
2. `CallReturn` 还必须登记本帧在该点仍活跃的槽根字：被调方执行期间调用方帧仍按调用返回 PC 的 map 扫描，而借出地址的槽（`let view = &value`）没有活跃值、只有活跃地址，槽根表是它唯一的登记来源。未写入的根字由逐根初始化字节在下游门控，因此登记本身不会引入假根；
3. 按 provenance 和 representation 分类为 `HeapDirect`、`HeapInterior`、`SharedHandle`、`CompressedRef`、`StackInterior` 或 `NonRoot`；
4. 查询分配结果得到物理寄存器或 frame slot；
5. 对需要 spill 的 safepoint插入 spill/reload，再重新计算局部活跃与 PC；
6. 把 stack slot 转成从 frame base 起的 8 字节 slot index；
7. 从函数 ABI 参数分类独立生成 `MorestackEntry` register map，不能从尚未建立的 frame 活跃集推断；
8. 对相同 root set 做全局去重；
9. 指令编码完成后填入最终 `pc_offset`。调用返回点取该站点最后一条 `call` 的下一条指令，入口检查取 `morestack_or_poll` 返回后的第一字节。编码后的 section 字节进入镜像计划；`--strip` 必须保留 [平台 ABI](../spec/platform-abi.md) 登记的栈图、展开与源码位置节。

一个位置在同一 map 的五类 bitmap/mask 中最多出现一次。重叠 spill、超出 frame、未对齐 managed slot、丢失 provenance 或一个 value 同时有两个未协调的权威位置都是后端错误。

分配阶段为上述查询发布三样东西：`FrameLayout`（`frame_size`、`payload_bytes`、逐局部对象的 `local_offset`、spill slot 的 `offset/size/root`、16 字节 copy scratch 偏移与 callee-saved save 偏移）、点位表（每个点位的 `site`、`kind`、`block`、指令区间与 mask）以及逐值结果（`Gpr`/`Xmm`/`Slot`/rematerializable）。stack map 生成只消费这些结果，不重新推导位置。spill slot 统一是 8 或 16 字节、按 root class 分开、偏移都从 frame base 起算，因此第 5 步的 slot index 就是 `offset / 8`。

分配阶段的**强制落槽**判定以 LIR 逻辑根集为权威，不另立一套活跃性口径：出现在禁止寄存器根的安全点（`CallReturn`/`Suspend`/`ForeignBridge`）根集里的值，其活跃区间必须覆盖该点位并强制落 frame slot。分配器自己用「触碰点 + 块边界」造的是凸区间，它够不到「最后一次触碰之后、但仍处于逻辑活跃集里」的点（单块无后继、活跃性只在块级可见等情形）；若以该凸区间为准，值会留在寄存器，或虽已落槽却被后端按「区间不覆盖该点」判死而丢掉根，对象被搬走而引用不更新。这里补齐区间时，点位区间按**点位下标**换算（`use_slot = 下标 * 2`），且同一 LIR 指令可能降级成多个站点、需取并集。

需要从 frame 里取地址的局部对象在选指阶段写成占位基址 `Reg::Virtual(FRAME_SLOT_BASE + slot)`（`FRAME_SLOT_BASE = 1 << 30`）：占位编号空间高于值编号与 lowering 临时编号，`select` 会断言值数量低于该上界。写回 store/load 的占位在分配阶段全部换算成 `[rsp + local_offset]`，验证阶段拒绝任何残留的 `Reg::Virtual`，所以 stack map 不会看到占位寄存器。

GC 活跃性按语义值而不是 Rust/源级 lexical scope 计算。已经 `StorageDead` 或被 `MoveInternal` 消耗的位置不得保留为根；尚未初始化和 `MaybeUninit` payload 不得加入。保留无谓死根会延长对象寿命，因此也属于 verifier 错误，不是允许的保守实现。

### 逐根初始化事件 {#root-init-events}

LIR 为每个栈槽维护逐根字段的初始化事件流 `RootInit { slot, position, bit, initialized }`，`bit` 是 `slot.roots` 的下标：`slot.roots` 是**多变体聚合的并集超集**（同一偏移只在部分变体里是指针），位图把它收窄成「该字在当前值里已经写成指针」。

- 置位：写入完整覆盖该根字的 8 字节。构造点已知变体，**只有自身携带根的字段才置位**——标量字段即使覆盖了并集条目所在的偏移也不置位，因此构造 `Scalar(int, int)` 变体不会把 `Callable(fn() int)` 变体在 `payload + 8` 的指针字当成已初始化；
- 整值写入但来源变体不可知时（从别的槽、参数或返回值复制整值），按覆盖范围保守置位全部并集条目。这类动态多变体副本要收窄到实际变体需要类型 trace，属于后续集成变更；
- 复位：`StorageLive`（建立新值）与 `StorageDead`（结束占用）清空该槽全部位，旧值的位不得被新值继承；
- 位置顺序是语义的一部分：`Reset` 之后必须有新的 `Set`，该根字才能被当成指针，所以重写 pass 必须用同一份锚点映射精确定位每个事件，不能丢弃或钳位到块尾；
- 初始化证明使用块内指令位置（安全点 instruction 减去所属块的 instructions.start），不能使用带块排序前缀的 LogicalSafepoint.position；安全点之后的 Set 不得影响当前图；
- 消费者只追踪置位的根字；未置位的并集条目表示该字在当前值里是标量或尚未写入。

帧内位图已接线：`FrameLayout::init_bases` 为每个带根字的栈槽在 frame 内分配**每根一字节**的区域（`base + bit`），`alloc/rewrite.rs` 在每个站点代码之前按块内位置游标发射 `mov byte [rsp + offset], 1/0`。选一字节一根而不是位图字，是因为置位与复位都不需要读改写、不占用 scratch 寄存器；站点游标保证这些写入落在那次读取位图的安全点之前，块尾把剩余事件全部刷新。每帧位图与普通值存储分开，未初始化 payload 不会被读到。

静态完整初始化证明已落地：`gugu_lir::root_init::InitSlots` 以 **must** 语义求块级不动点（汇合点取前驱交集），`fully_initialized(block, position, slot)` 只在该点沿所有路径都写全该槽根字时为真——它是 `mask = 0`（无需读帧内位图的判定依据）、不是过滤器：未证明已初始化**不得**据此丢根。帧内位图与证明当前由回归测试与真实镜像行为覆盖，站点条目（消费方）随后续集成变更接入。

## 二进制 section

Linux 放入 `.gugu.stackmap`，Windows 放入 `.gugustk`。section 内所有整数小端，所有 offset 相对 section 起始，table 按记录自然对齐；padding 必须为 0。

### A64 lowering 到 map/CFA 的接线（4c.2b.1）

4c.2b.2a 同时消费 LiveSlots 的局部槽根和 InitSlots 的逐根 must 证明。一个 union root
表项对应一个 frame byte；含相同字偏移的不同 provenance 必须有独立 gate，不能同时折叠
进多个 fixed bitmap。尚未静态证明的根在 PollResume 和 CallReturn 都使用 TypedRoot。
gate 在帧建立后只清零一次，RootInit 事件保持同位置的源顺序（例如 reset 后 set）；
未置位、已复位或 lifetime 已结束的项不参与扫描。入口有回边时保守使用 typed gates，
避免把回边事实当作第一次进入的初始化证明。gate 也计入物理 frame，但不是被扫描的根字。
确定性 VM 使用真实生成的 STRB、序列化 map 和 reference scan_frame 验证 GC 写回后 reload；
这不是生产 Gugu decoder 已接通的声明。

CallReturn 的 poll-free leaf 豁免不能与 slow-capable callee 入口检查共存。A64 runtime
使用 4096 字节的 managed leaf 帧预算：caller 检查自己的 frame 加预算，callee 帧必须不大于
预算且不发射自动 morestack/poll 入口。reserve 不计入 frame_size/CFA/slot_count，也不新增
caller 的调用图；过大的叶帧必须报错。空帧叶同样不检查，非叶的自动检查仍保留。

标量根已从权威 LIR anchors 经共享 pointer_spill 落槽；五类 provenance 逐站点映射，
不允许用 register 根绕过调用/poll 的强制落槽要求。managed call 的 BL 返回标签必须位于
结果物化之前；该调用新定义的结果不在该返回 PC 上被当作已有根。实参根登记 source spill，
传出栈参数另登记实际 outgoing slot；pre-frame 入口的传入栈参数由 caller 图覆盖。
自动生成的 checked entry 也有 v5 入口图，不依赖 LIR 必须保留 StackCheck marker。

逐片段 CFA 段 magic 为 GUGUCF01：固定 40 字节头依次为 magic:[u8;8]、arch:u32=183、
code_size:u32、frame_size:u32、fp_offset:u32、code_rva:u64、count:u32、reserved:u32=0。
每个 16 字节记录为 pc_offset:u32、cfa_offset:u32、lr:u32、flags:u32。
flags=1 是 MorestackEntry（cfa=0、lr=10 表示 x10）；flags=0 为建帧后的站点
（cfa=frame_size、lr=fp_offset+8 表示 SP-relative 字节偏移）。其他 flags 拒绝。
PC 严格递增、4 字节对齐且在 code range 内；与 v5 map 的每个 PC/kind 必须一一匹配。
caller 恢复只接受精确返回 PC；缺失 snapshot 或 SP 溢出都报错，不读宿主地址。
此格式是 safepoint walker 的记录，不是平台 EH unwind；gated local、协程 handoff 与
镜像内的 Gugu decoder 仍未接入。x64 v4 编码、raw contract 与 runtime 源码均不变。

### header

字段按下列顺序编码：

```text
magic:              [u8; 8] = "GUGUSM01"
version:            u16 = 4 (x64) / 5 (A64)
pointer_size:       u8 = 8
endian:             u8 = 1
function_count:     u32
safepoint_count:    u32
map_count:          u32
architecture:       u32 = 0 (v4) / 183 (v5)
reserved2:          u32 = 0
functions_offset:   u64
safepoints_offset:  u64
map_index_offset:   u64
map_data_offset:    u64
section_len:        u64
```

所有 offset 必须 8 字节对齐、位于 `section_len` 内且相应 table 不重叠。`map_index` 含 `map_count + 1` 个 `u64`，最后一个是 map data 末端，因而第 `i` 个 variable record 是半开范围 `[index[i], index[i + 1])`。

### function record

每条固定 32 字节：

```text
code_rva:           u64
code_size:          u32
frame_size:         u32
safepoint_start:    u32
safepoint_count:    u32
unwind_index:       u32
flags:              u16
reserved:           u16 = 0
```

function table 按 `code_rva` 严格递增，code range 不重叠。`code_rva` 相对片段区基址，且必须与[镜像 writer](backend.md) 的放置同源：片段世界的函数体与 `registered_fragments` 登记的机器片段一起按符号名排序、16 字节对齐，glue Item 实例与机器片段共用 `__gugu_runtime_*` 前缀因而必然交错。只排片段世界的算法会让排在机器片段之后的函数拿到偏小的 `code_rva`，运行期 `PC → 函数` 查找落到错误区间；展开表（`.eh_frame`/`.xdata`）用同一组 `code_rva`。`safepoint_start/count` 指向全局 safepoint table 的连续范围。flags 位固定为：bit 0 `RUNTIME_BRIDGE`，bit 1 `PANIC_LANDING_PAD`，bit 2 `HAS_STACK_INTERIOR`；其他位为 0。`unwind_index` 指向[后端](backend.md)生成的统一 unwind function record。

### safepoint record

每条固定 12 字节：

```text
pc_offset:          u32
map_index:          u32
kind:               u8
flags:              u8
reserved:           u16 = 0
```

同一函数内按 `pc_offset` 严格递增。flags bit 0 表示该点允许 stack copy，bit 1 表示该点允许 GC scan，bit 2 表示 register mask 有效，bit 3 表示该 `ForeignBridge` record 的 native work 属于 `DirtyCpu`；bit 3 只能与 kind 3 同时出现。未定义位为 0。`pc_offset` 必须严格小于 `code_size` 并指向已登记的指令边界/resume label。

### root map record

v4 的每条 variable record 开头固定为（v5 的 mask/header 宽度见上文）：

```text
slot_count:             u32
heap_direct_reg_mask:   u16
heap_interior_reg_mask: u16
shared_handle_reg_mask: u16
compressed_ref_reg_mask: u16
stack_reg_mask:         u16
reserved:               u16 = 0
heap_direct_slots:      ceil(slot_count / 8) bytes
heap_interior_slots:    ceil(slot_count / 8) bytes
shared_handle_slots:    ceil(slot_count / 8) bytes
compressed_ref_slots:   ceil(slot_count / 8) bytes
stack_slots:            ceil(slot_count / 8) bytes
zero_padding_to_4_bytes
typed_count:            u32
typed_roots:            typed_count × 12 bytes
```

每个逐根初始化条目 12 字节：

```text
word:       u32   帧内机器字下标，与固定位图同一坐标
root_kind:  u32   五类根之一（0=Direct、1=Interior、2=Handle、3=Compressed、4=StackInterior）
init_byte:  u32   帧内逐根初始化字节的字节偏移；非零表示该字已成写指针
```

条目按 `word` 非递减排列：同一 `word` 允许携带多个 `root_kind` 条目——多变体槽的同一个字在不同变体下可以是不同种类的根，由各自的 `init_byte` 在运行期择一门控；同 `word` 的条目按 `init_byte` 严格递增，完全重复或乱序一律拒绝。条目 `word` 不得同时出现在任一固定位图里；编码侧发现同一 offset 登记了多种根种类时，全部改写成逐根初始化条目而不进固定位图。`root_kind` 越界一律拒绝。没有条目的记录写 `typed_count = 0`。

bitmap 的 bit `i` 对应 `[frame_base + i * 8]`，最低有效 bit 先写。超出 `slot_count` 的尾 bit 必须为 0。`slot_count * 8` 不得超过所属函数 `frame_size`。五个位图互斥，五个 register mask 也互斥；bit 15 必须为 0。

逐根初始化字节位于槽位图范围**之外**的帧内区域（每个根字一字节，由 `FrameLayout::init_bases` 分配），因此按 `slot_count` 合成帧的消费者必须先按 `frame_size` 开表再扫描：`slot_count * 8` 不是帧的上界，`frame_size` 才是。

门控根只对**栈驻留**的并集值存在：SSA 常驻的并集值走 `Value` 根，不经过栈槽、也没有初始化字节。判定由 `gugu_lir::root_init::InitSlots`（must 语义块级不动点）给出，`mask = 0` 之外的根一律写成条目。

map 去重基于完整 record bytes；map table 按 record bytes 字典序分配 `map_index`，不能按发现时序分配。version 2 的 `SharedHandle` 和 `CompressedRef` 位置仍是 managed root；decoder 必须拒绝 version 1（`SharedHandle`/`CompressedRef` 表示不同）、version 2（缺少裸屏障调用的 `CallReturn` record）与 version 3（map record 没有逐根初始化尾字段）记录，避免把旧 world 当当前 world 使用。

## stack walk

runtime 扫描一个已停在 safepoint 的协程时：

1. 从 coroutine context 取得 top `pc`、`rsp` 和保存寄存器；
2. 用排序 function table 查找包含 `pc` 的 function record；
3. 根据 safepoint kind恢复当前 frame并继续外层遍历：
- `CallReturn`：当前SP加 `frame_size` 得到 caller SP；caller PC是当前返回地址；
- `PollResume`：当前 frame不变，从 `resume_pc` 继续；
- `SuspendResume`：context中的SP/PC已经指向 suspended frame；
- `ForeignBridge`：先从 bridge context恢复 user SP/PC；dirty mode不改变扫描算法；
- `MorestackEntry`：当前函数 frame尚未建立，scratch中的 return PC是 caller PC，scratch GPR由当前 map扫描；slow path可以先处理 poll再决定是否复制 stack。

`SharedHandle` 位置由 handle table 的 current payload 和 generation 解析；scanner 不把 slot
里的 payload 当作另一个 stack root。全零字是空引用，不查表也不计数；槽编号 0 不能表示活动 handle。`CompressedRef` 位置经压缩平面的单一 checked 解码路径
解析：空字是空引用、不解码也不计数；未登记 cage、过期 generation、越界 offset 与落在
canonical hole 的地址都进入 `RuntimeInvariant`，不能当成 `HeapDirect` 猜测。成功解码计入
`compressed_ref_decodes`，被拒绝的解码计入 `compression_decode_rejections`。压缩字是 cage
相对量，对象在 cage 内搬迁后由本地槽更新重新编码，因此栈复制不重定位它。

每步先以 PC查 function range，再用该 function内按 offset排序的 safepoint做 binary search；找不到精确 point、frame越界或 unwind index不匹配都是 `RuntimeInvariant` fatal，不能猜测相邻 map。counted inner chunk edge与 uncounted countdown-only edge都不是 safepoint，不能把它们的 PC交给 scanner。

`ForeignBridge` record的 bridge mode决定 native阶段的调度归属。普通 bridge进入 native时可以保留 generation-tagged processor lease，return、GC、callback、retirement或 scheduler retaker通过同一完整 coroutine state竞争后也可以变为 detached；`DIRTY_CPU_BRIDGE` 从开始执行起就 detached并由 dirty CPU额度运行。两者都在进入 native前保存 Gugu context；`ForeignBridgeState` 以 checked high-relative offset定位 coroutine stack上的 ABI frame，`lease_word` 只是 non-root整数。collector不论 attached/detached都按本 record扫描 frame roots，不扫描 C/C++/asm 的 OS stack。返回 worker把结果写回同一 frame；赢得 lease可直接走 managed resume，失去 lease才经 idle processor或 runnable queue恢复。

找不到 function/safepoint、return PC 不在代码区、frame size 越界或 frame 链未在当前 stack range 内终止都进入 `RuntimeInvariant` fatal；禁止退回保守扫描。

panic unwinder 和 backtrace 使用同一 function range 与 frame-size 基础记录，再结合目标展开表恢复 landing pad；它们不能维护另一套可能分歧的 frame 大小表。

## GC 扫描与更新

`HeapDirect` 位置必须为空值或指向合法 LocalHeap/TurnRegion managed payload 起点。`HeapInterior` 位置必须为空值或落在一个已分配 LocalHeap/TurnRegion object 的 payload 范围内。collector 找到 owner object、记录 interior byte delta、标记/移动 object，然后把位置更新为 `new_payload + delta`。`SharedHandle` 位置解析 stable handle slot 的 current payload；`CompressedRef` 位置 checked 解码 cage、offset 和 generation。handle forwarding 只切换 slot，compressed/shared stale generation 或越界进入 `RuntimeInvariant` fatal，不能当成 direct pointer 猜测。

stack slot 就地更新。top register root 更新保存区，恢复用户寄存器前再从该区装载。caller frame 不含 register root。

raw pointer 即使数值落在 heap 内也不扫描。它跨 safepoint 的合法性由 unsafe/pin 规则保证；stack map 不能暗中延长 raw pointer 目标的生命周期。

## 协程栈复制 {#coroutine-stack-replication}

只有 safepoint flags 允许 stack copy 时才可移动 stack。复制固定按以下顺序：

1. 计算已使用半开范围 `[old_rsp, old_stack_high)`；
2. 按[调度器](scheduler.md)规则分配更大的连续 stack；
3. 按字节复制已使用范围到新 stack 顶端对应位置；
4. 计算有符号 `delta = new_rsp - old_rsp`；
5. 用旧 frame chain 的每个 map 枚举 `StackInterior` slot；值落在旧 stack range 内时加 `delta`；
6. 对 top frame 的 `stack_reg_mask` 保存寄存器执行相同更新；
7. 更新 coroutine context 的 `rsp`、stack bounds、正常 `stack_check`和收缩观察字段；
8. 发布新 stack 后释放旧 stack。

任何 `StackInterior` 值不为空且不落在当前旧 stack range 内都是 compiler/runtime 契约破坏。heap roots在栈复制时不修改。return PC、code/metadata pointer 和整数不因 stack 地址变化而修改。

## 验证要求

编译器在写镜像前必须同时验证机器码与 stack map：

- 每个要求 safepoint 的 call/poll/suspend 和每个可增长 frame 的 morestack entry 都有 exact record；
- 裸屏障调用点必须有 `CallReturn` record，且根集非空；预留屏障与 `BarrierReserve` 标记在 `NoSafepointRegion` 内不得留下 record；
- 每个 map 与最终活跃/分配位置逐项一致，CallReturn map 覆盖 outgoing managed/stack 参数；
- `CallReturn`/`PollResume`/`SuspendResume`/`ForeignBridge` 点没有用户 register root；`MorestackEntry` 只使用声明的 register/scratch roots；
- 根完备性：`CallReturn`/`SuspendResume`/`ForeignBridge` 点上，分配器为**跨点存活**的受管值准备的 frame slot 必须出现在该点的根位图或逐根条目里（`metadata.rs::verify_live_pointers_are_rooted`）。这些点位按寄存分配规则不允许保留用户寄存器根，跨点受管值必然落 frame slot；逻辑根集合漏登记时物理层不会有任何报错，收集器只是扫不到仍被持有的对象，因此漏登记必须是编译期错误而不是运行期现象；“跨点存活”的判定是分配区间同时覆盖点位前与点位后，该点位自身定义（例如调用结果）不算；
- direct callee的 `PollSummary.entry_stack_check` 与 caller是否生成 `CallReturn` map一致；`PollFreeLeaf`/`ForeignLeaf` 不得伪造 map；
- 普通 bridge的 `lease_word` 固定保存该 invocation原始 attached `Foreign(g)`；当前 lifecycle只能是该 exact word、带 scan lock的竞争态或同 generation的 detached Foreign。dirty mode设置 `DIRTY_CPU_BRIDGE` flag：active Foreign必须带 `FOREIGN_DETACHED`，DirtyWaiting保持同 generation且不能带该 bit。所有模式的 `ForeignBridgeState.frame_offset + frame_size` checked落在已用 stack范围；
- frame size、return address、ABI bridge frame和 outgoing区符合后端 frame layout；
- `HeapDirect`、`HeapInterior`、`SharedHandle`、`CompressedRef` 和 `StackInterior` 的 bitmap/mask 互斥，所有 handle/cage generation 与所属 metadata 一致；
- `SharedHandle` 访问必须有 access guard 或 pin，compressed reference 必须有登记 cage；
- `StackInterior` offset严格落在当前 stack allocation；
- `suspend/foreign`点 register mask为0；
- `MorestackEntry` 的 `slot_count == 0`，只含 ABI 参数 register root，且关联函数保留 entry check；
- `PollResume` 只能位于实际 poll-word检查后的 resume label，不能位于 counted inner chunk 或 uncounted countdown-only edge；
- `NoSafepointRegion` 的机器范围内不存在 safepoint record、call-return map 或 poll resume label，紧邻 region 的 poll map 只能指向 marker 外的真实 poll instruction；
- source/unwind index 存在且范围合法。

runtime的确定性 fixture必须覆盖：空map、只有stack root、budgeted poll register root、counted inner chunk、uncounted countdown未到期、poisoned `MorestackEntry`同时处理GC与增长、interior heap root、shared handle root、compressed reference root、stack interior重定位、多frame调用、panic landing pad、no-safepoint region邻接poll、attached foreign快返回、foreign retake后扫描、dirty bridge、`PollFreeLeaf`无map、caller最高 leaf stack reserve、stack增长、四窗迟滞收缩和512 B/1 KiB冷 stack重定位后GC。

- [LLVM Stack Maps and Patch Points](https://llvm.org/docs/StackMaps.html)
- [Go runtime stack 实现](https://go.dev/src/runtime/stack.go)
- [Go runtime stack map 生成入口](https://go.dev/src/cmd/compile/internal/ssagen/ssa.go)
- [Go runtime asynchronous preemption](https://go.dev/src/runtime/preempt.go)
