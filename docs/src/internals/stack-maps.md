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
| `HeapDirect` | 指向已知对象 payload 起点的强引用/句柄 | 标记对象；移动后写回新 payload 地址 |
| `HeapInterior` | 指向 GC 对象字段、元素或切片起点 | 通过 heap span metadata 找到对象与内部偏移；移动后按偏移写回 |
| `StackInterior` | 指向当前协程栈范围内的 local/字段 | 不标记 heap；复制栈时加 relocation delta |
| `NonRoot` | 原始指针、代码、metadata、整数或已死值 | 不扫描、不修改 |

pin 状态属于目标对象头和 pin token，不是另一类位置。指向 pinned 对象的根仍编码为 `HeapDirect`/`HeapInterior`；GC 根据对象头决定不移动。

`dyn Trait`、slice、string、函数值等多字值只标记实际的 managed data word；长度、容量、vtable 和 code pointer 是 `NonRoot`。`#[repr(packed)]` 不能含 managed 引用，因此 stack root 总能落在自然对齐的完整机器字中。

安全引用若可能越过 frame 生命周期、进入另一个协程或被写入 heap，`EscapeAndPlacement` 必须先把被引用值提升到 heap/arena。成功 LIR 中不允许 heap 对象保存 `StackInterior`，也不允许一个协程保存指向另一协程 stack 的安全引用。

## safepoint 种类

编码中的 `kind` 固定为：

| 值 | 名称 | PC 含义 |
|----|------|---------|
| 0 | `CallReturn` | `call` 指令之后第一字节；扫描 caller frame 时使用 |
| 1 | `PollResume` | safepoint slow path 返回后的 resume label |
| 2 | `SuspendResume` | 协程重新变为 running 时继续执行的 label |
| 3 | `ForeignBridge` | Gugu 与 C/runtime bridge 完成寄存器保存后的 label |
| 4 | `MorestackEntry` | prologue建 frame前进入 `morestack_or_poll` 的 slow-path label |

普通/dirty `ForeignBridge`、park/suspend和其它 mandatory statepoint一定建立对应 map。直接 managed call若目标保留 entry `StackCheck`，caller return PC必须有 `CallReturn` map，因为 callee可能在建立 frame前进入 `morestack_or_poll`；`PollFreeLeaf` 调用和 `ForeignLeaf` 本身不建立专用 safepoint record。loop countdown没有 map，只有 interval到期实际读取 poll word的 resume label建立 `PollResume`；显式 `safepoint_poll()` 使用同一 kind。poisoned函数入口使用 `MorestackEntry`，同时覆盖 poll和真实 stack growth。

signal/APC handler不在任意PC直接扫描用户 stack。它只设置当前 processor的 poll word、投毒 current coroutine的 `stack_check`并唤醒 worker；真正暂停和扫描发生在 compiler登记的 `PollResume`、`MorestackEntry` 或 mandatory statepoint。dirty native stack不属于 Gugu stack map。

## 寄存器规则

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

`rsp` 不作为普通根；它由 coroutine context 单独保存。managed pointer 不放入 XMM 寄存器。`r14`/`r15` 是 runtime 保留寄存器，普通用户值不能占用；对应 mask bit 在普通函数中必须为 0，runtime bridge 只通过专用根表扫描它们。

在 `SuspendResume`、`ForeignBridge` 点，所有跨该点活跃的用户 managed/stack pointer必须 spill到 stack slot，三个寄存器 mask均为0。`ForeignBridge[DirtyCpu]` 使用相同 bridge root map，native段不建立额外 stack map；`CallReturn` map必须包含被调方 entry check期间仍活跃的 caller root、outgoing managed/stack参数word和 aggregate副本，即使它们在正常返回后已死。`PollResume`可以保留普通寄存器根。`MorestackEntry` 的 `slot_count`固定为0，register mask描述保存到 coroutine morestack scratch的 managed参数寄存器；caller stack参数和 aggregate副本由外层 `CallReturn` map追踪。`PollFreeLeaf`/`ForeignLeaf` 没有 map，caller prologue必须已把函数内所有 direct leaf `stack = N` 的最大值计入 reserve。

该限制避免 caller frame 依赖 callee-saved 寄存器的跨 frame 追踪，同时保留无调用循环 poll 中的寄存器分配质量。

## 构造流程

stack map 只能在 LIR 完成寄存器分配、spill slot 分配和 frame layout 后生成：

1. 从 safepoint 的 LIR 活跃集取得所有 `Ptr` value；`CallReturn` 另加入 outgoing 参数及其 aggregate 副本 descriptor 展开的全部 root word；
2. 按 provenance 分类为 `HeapDirect`、`HeapInterior`、`StackInterior` 或 `NonRoot`；
3. 查询分配结果得到物理寄存器或 frame slot；
4. 对需要 spill 的 safepoint插入 spill/reload，再重新计算局部活跃与 PC；
5. 把 stack slot 转成从 frame base 起的 8 字节 slot index；
6. 从函数 ABI 参数分类独立生成 `MorestackEntry` register map，不能从尚未建立的 frame 活跃集推断；
7. 对相同 root set 做全局去重；
8. 指令编码完成后填入最终 `pc_offset`。

一个位置在同一 map 的三类 bitmap/mask 中最多出现一次。重叠 spill、超出 frame、未对齐 managed slot、丢失 provenance 或一个 value 同时有两个未协调的权威位置都是后端错误。

GC 活跃性按语义值而不是 Rust/源级 lexical scope 计算。已经 `StorageDead` 或被 `MoveInternal` 消耗的位置不得保留为根；尚未初始化和 `MaybeUninit` payload 不得加入。保留无谓死根会延长对象寿命，因此也属于 verifier 错误，不是允许的保守实现。

## 二进制 section

Linux 放入 `.gugu.stackmap`，Windows 放入 `.gugustk`。section 内所有整数小端，所有 offset 相对 section 起始，table 按记录自然对齐；padding 必须为 0。

### header

字段按下列顺序编码：

```text
magic:              [u8; 8] = "GUGUSM01"
version:            u16 = 1
pointer_size:       u8 = 8
endian:             u8 = 1
function_count:     u32
safepoint_count:    u32
map_count:          u32
reserved:           u32 = 0
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

function table 按 `code_rva` 严格递增，code range 不重叠。`safepoint_start/count` 指向全局 safepoint table 的连续范围。flags 位固定为：bit 0 `RUNTIME_BRIDGE`，bit 1 `PANIC_LANDING_PAD`，bit 2 `HAS_STACK_INTERIOR`；其他位为 0。`unwind_index` 指向[后端](backend.md)生成的统一 unwind function record。

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

每条 variable record 开头固定为：

```text
slot_count:             u32
heap_direct_reg_mask:   u16
heap_interior_reg_mask: u16
stack_reg_mask:         u16
reserved:               u16 = 0
heap_direct_slots:      ceil(slot_count / 8) bytes
heap_interior_slots:    ceil(slot_count / 8) bytes
stack_slots:            ceil(slot_count / 8) bytes
zero_padding_to_4_bytes
```

bitmap 的 bit `i` 对应 `[frame_base + i * 8]`，最低有效 bit 先写。超出 `slot_count` 的尾 bit 必须为 0。`slot_count * 8` 不得超过所属函数 `frame_size`。三个位图互斥，三个 register mask 也互斥；bit 15 必须为 0。

map 去重基于完整 record bytes；map table 按 record bytes 字典序分配 `map_index`，不能按发现时序分配。

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

每步先以 PC查 function range，再用该 function内按 offset排序的 safepoint做 binary search；找不到精确 point、frame越界或 unwind index不匹配都是 `RuntimeInvariant` fatal，不能猜测相邻 map。interval countdown edge不是 safepoint，不能把它的 PC交给 scanner。

`ForeignBridge` record 的 bridge mode 决定 native 阶段的调度归属。普通 bridge可在 foreign worker上等待；`DIRTY_CPU_BRIDGE` 表示 coroutine 已脱离 processor并由 dirty CPU额度执行。两者都在进入 native 前保存 Gugu context；`ForeignBridgeState` 以 checked high-relative offset定位 coroutine stack上的 ABI frame，collector按本 record扫描其中 roots，不扫描 C/C++/asm 的 OS stack。返回时 bridge worker把结果写回同一 frame，再由 managed resume path构造 Gugu值。

找不到 function/safepoint、return PC 不在代码区、frame size 越界或 frame 链未在当前 stack range 内终止都进入 `RuntimeInvariant` fatal；禁止退回保守扫描。

panic unwinder 和 backtrace 使用同一 function range 与 frame-size 基础记录，再结合目标展开表恢复 landing pad；它们不能维护另一套可能分歧的 frame 大小表。

## GC 扫描与更新

`HeapDirect` 位置必须为空值或指向合法 managed payload 起点。`HeapInterior` 位置必须为空值或落在一个已分配 managed object 的 payload 范围内。collector 找到 owner object、记录 interior byte delta、标记/移动 object，然后把位置更新为 `new_payload + delta`。不合法地址进入 `RuntimeInvariant` fatal。

stack slot 就地更新。top register root 更新保存区，恢复用户寄存器前再从该区装载。caller frame 不含 register root。

raw pointer 即使数值落在 heap 内也不扫描。它跨 safepoint 的合法性由 unsafe/pin 规则保证；stack map 不能暗中延长 raw pointer 目标的生命周期。

## 协程栈复制

只有 safepoint flags 允许 stack copy 时才可移动 stack。复制固定按以下顺序：

1. 计算已使用半开范围 `[old_rsp, old_stack_high)`；
2. 按[调度器](scheduler.md)规则分配更大的连续 stack；
3. 按字节复制已使用范围到新 stack 顶端对应位置；
4. 计算有符号 `delta = new_rsp - old_rsp`；
5. 用旧 frame chain 的每个 map 枚举 `StackInterior` slot；值落在旧 stack range 内时加 `delta`；
6. 对 top frame 的 `stack_reg_mask` 保存寄存器执行相同更新；
7. 更新 coroutine context 的 `rsp`、stack bounds 和 guard；
8. 发布新 stack 后释放旧 stack。

任何 `StackInterior` 值不为空且不落在当前旧 stack range 内都是 compiler/runtime 契约破坏。heap roots在栈复制时不修改。return PC、code/metadata pointer 和整数不因 stack 地址变化而修改。

复制期间 coroutine 为非 Running 状态，任何 worker、GC scanner 或 debugger 只能看到旧描述或已完全发布的新描述，不能看到混合边界。

## 验证要求

编译器在写镜像前必须同时验证机器码与 stack map：

- 每个要求 safepoint 的 call/poll/suspend 和每个可增长 frame 的 morestack entry 都有 exact record；
- 每个 map 与最终活跃/分配位置逐项一致，CallReturn map 覆盖 outgoing managed/stack 参数；
- `CallReturn`/`SuspendResume`/`ForeignBridge` 点没有用户 register root；`PollResume` 与 `MorestackEntry` 只使用各自声明的 register/scratch roots；
- direct callee的 `PollSummary.entry_stack_check` 与 caller是否生成 `CallReturn` map一致；`PollFreeLeaf`/`ForeignLeaf` 不得伪造 map；
- `ForeignBridge` dirty mode设置 `DIRTY_CPU_BRIDGE` flag，且对应 `ForeignBridgeState.frame_offset + frame_size` checked落在已用 stack范围；
- frame size、return address、ABI bridge frame和 outgoing区符合后端 frame layout；
- `StackInterior` offset严格落在当前 stack allocation；
- suspend/foreign点 register mask为0；
- `MorestackEntry` 的 `slot_count == 0`，只含 ABI参数 register root，且关联函数保留 entry check；
- `PollResume` 只能位于实际 poll word检查的 resume label，不能位于 countdown-only edge；
- source/unwind index存在且范围合法。

runtime的确定性 fixture必须覆盖：空map、只有stack root、budgeted poll register root、countdown未到期、poisoned `MorestackEntry`、interior heap root、stack interior重定位、多frame调用、panic landing pad、foreign/dirty bridge、`PollFreeLeaf`无map、caller最大 leaf stack reserve和stack增长后GC。

- [LLVM Stack Maps and Patch Points](https://llvm.org/docs/StackMaps.html)
- [Go runtime stack 实现](https://go.dev/src/runtime/stack.go)
- [Go runtime stack map 生成入口](https://go.dev/src/cmd/compile/internal/ssagen/ssa.go)
- [Go runtime asynchronous preemption](https://go.dev/src/runtime/preempt.go)
