# x86_64 后端

本章规定 Gugu 首版自研后端从合法 LIR 到 x86_64 machine code、内部调用 ABI、寄存器分配、frame layout、重定位和 ELF/PE 镜像写出的完整路径。后端不调用 LLVM、系统 assembler 或系统 linker；原生静态/动态输入只通过已登记的构建元数据进入 image planner。

外部 C ABI、目标名称和稳定镜像边界以[平台与 ABI 参考](../spec/platform-abi.md)为准。本章的 Gugu 内部 ABI、符号、frame 和 metadata 可以随 `CompilerIdentity` 一起改变，但同一镜像中的 compiler/runtime 必须完全一致。

## 权威边界

[类型](../spec/types.md)、[unsafe](../spec/unsafe.md)、[程序模型](../spec/program-model.md)和[平台 ABI](../spec/platform-abi.md)唯一规定整数/浮点结果、可接受的 asm、目标、C ABI、导入导出与稳定镜像面。本章只固定官方 compiler如何把合法 LIR实现为这些结果。内部 ABI、CPU指令选择、frame、mangling、relocation计划和直接写出算法随 `CompilerIdentity` 版本化，不能扩大或缩小公开接受面。

## 目标描述符与数值 lowering

当前后端只实现平台注册表中的 `x86_64-linux` 和 `x86_64-windows`。每个 toolchain安装携带不可变 `TargetDescriptor { name, object_format, page_size, cpu_baseline, linux_interpreter, sysroot_digest, import_policy_revision }`；目标运行时路径与宿主 sysroot分离，descriptor整体进入 compiler identity和 action key，backend不探测宿主 PATH。

CPU可接受面只读取[平台 CPU 基线](../spec/platform-abi.md#cpu-基线)，后端 instruction verifier拒绝任何超出 descriptor的机器指令。数值 lowering只实现[类型系统](../spec/types.md)给定的整数/浮点结果；SSE2、NaN、overflow、shift和conversion选择是这些结果的机器实现，不在本章创建另一套数值规则。

## 后端阶段

LIR 后端严格按以下顺序运行：

1. `Legalize`：把剩余高层 operation 变成 baseline 可表达序列；
2. `SelectInstructions`：选择 x86_64 opcode、address mode 和 fixed-register constraint；
3. `ScheduleBlocks`：固定 block layout、fallthrough 和冷路径；
4. `BuildLiveIntervals`：计算物理 register class、liveness 和 call/safepoint constraint；
5. `AllocateRegisters`：全局线性扫描、interval split 和 spill；
6. `ResolveParallelCopies`：消除 block 参数、call/return shuffle；
7. `LayoutFrame`：分配 outgoing、local、spill、save slot 并生成 prologue/epilogue；
8. `BuildStackMapsAndUnwind`：在最终位置构造根、frame 和 landing metadata；
9. `RelaxBranches`：选择 rel8/rel32 并迭代到大小固定；
10. `Encode`：直接写 machine bytes 和逻辑 relocation；
11. `PlanImage`：布局 fragment、section、import/export 和 metadata；
12. `ApplyRelocationsAndEmit`：写 ELF/PE/static archive/shared library。

一个阶段失败后不能退回较低优化级、外部 toolchain 或解释执行。目标不支持的 LIR/inline asm/native relocation 是编译错误；内部不变量破坏是 compiler internal error。

## 后端内存表示

instruction-selected body 仍属于 LIR 的目标形态，不建立可被其他阶段误认成第五层语义 IR。每个 instruction 是封闭 `X64Inst` enum，operand 使用 `VirtualReg(u32)`、`PhysicalReg`、`StackSlotId`、immediate 或 `AddressMode`。body 内 instruction、operand、relocation 和 source record 分别存入连续 arena；basic block 保存 range。

`AddressMode` 只允许 x86_64 可编码的 `base + index * scale + disp32`，scale 为 1/2/4/8，base/index 可缺一但不能都是无意义值。RIP-relative symbol address使用独立 variant，不能伪装成普通 base register。encoder 不接收任意文本 opcode。

## Gugu 内部 ABI

### 保留寄存器

所有 Gugu 用户函数和 compiler-generated glue 固定：

- `rsp`：stack pointer；
- `r14`：当前 `Coroutine*`；
- `r15`：当前 `LogicalProcessor*`；
- `r11`：instruction lowering、parallel copy 和 long branch 的后端 scratch。

allocator 永远不把普通 value 分配到这四个寄存器。`r14`/`r15` 在 coroutine resume 后由 scheduler 重建，内部调用必须保持。进入 C ABI 前由 bridge 按平台 nonvolatile 规则保存，返回后验证/恢复。

### 参数与返回

内部整数/pointer 参数寄存器依次为：

```text
rax, rbx, rcx, rdx, rdi, rsi, r8, r9, r10
```

浮点参数寄存器依次为 `xmm0` 到 `xmm7`。整数/pointer 返回使用 `rax`、`rbx`，浮点返回使用 `xmm0`、`xmm1`。

参数分类固定为：

- integer、bool、char、pointer、reference、handle、code/metadata pointer 使用整数槽；
- `f32`/`f64` 使用浮点槽；
- fat pointer 拆成两个整数槽；
- 不超过 16 字节的普通 aggregate 按 8 字节 piece 拆分；piece 只含一个浮点 scalar 时用浮点槽，否则用整数槽；
- 大于 16 字节、含 resource/COW 特殊传递动作或无法自然拆分的 aggregate 由 caller 传地址；
- ZST 不占寄存器或 stack slot；
- 返回需要超过两个 piece 时，caller 把隐藏 return pointer 放在第一个整数参数槽，显式整数参数整体后移。

内部 ABI 的间接 aggregate 不是借用 caller 原 place。通常 caller 先按 value descriptor在 outgoing 区物化完整语义副本并把地址传入；该存储在调用期间归 callee参数 local所有，callee可以修改并必须在正常/panic出口执行相应 drop/resource cleanup，caller返回后不再 drop同一副本。若 `EscapeAndPlacement` 已证明参数地址越过调用，caller改为物化独立 managed box，callee cleanup只结束参数绑定，box内 value由 GC/resource descriptor管理。trivial bit-copy 类型也不能因地址分析猜测而把可变参数别名到原值。

sret destination在 callee执行期间视为 caller未初始化字节，不能加入 `CallReturn` root map。callee必须先在自己 frame或已提升 managed box中构造返回值，以逐 safepoint初始化图追踪；只有所有字段成功且不再经过 safepoint时，epilogue才把完整值 transfer到 sret、清除本地所有权并返回。panic只清理本地部分值，绝不让 caller观察/扫描半初始化 sret。C export thunk同样先取得完整 Gugu返回值，再按 C ABI写外部 sret。

整数和浮点 bank 分别前进；某一 aggregate 的任意 piece 无法放入对应寄存器时，该 aggregate 的所有 piece 都放到 stack，避免半寄存器半内存。stack 参数按参数顺序放入 8 字节 slot，并满足自身更高对齐；caller 固定 outgoing 区承载。

调用 lowering 必须把 outgoing stack 参数中的每个 managed/stack pointer word，以及按值/间接 aggregate 副本由类型 descriptor 展开的全部 root-bearing 字段，登记到 caller 的 `CallReturn` map，不受“返回后是否继续活跃”影响。callee 的参数分类同时生成一个 entry register-root map；发生 stack growth 时，`morestack` 用它扫描和更新保存的寄存器参数，stack 参数和 aggregate 副本继续由 caller map 扫描。

内部 caller-saved 为所有参数/返回寄存器、`r11` 和 `xmm0..xmm15`。内部 callee-saved 为 `rbp`、`r12`、`r13`；`r14`、`r15` 是必须保持的 runtime register。跨调用活跃值优先分配 callee-saved或 spill。C import/export thunk 把该 ABI 与 SysV/Microsoft x64 完整互换，不能让内部约定泄漏到 `extern "C"`。

普通内部调用的 panic 能力由 `Call`/`Invoke` 决定，不额外传隐式错误码。coroutine、GC 和 resource context 通过保留寄存器与显式 metadata 取得，不追加隐藏普通参数。

## instruction selection

### integer 与地址

selector 优先使用能直接编码的 immediate 和 address mode，只有不满足 sign-extended imm32 或合法 scale 时才物化常量。`lea` 只做地址/无 flags integer 组合；不能用于绕过语言 overflow 检查，因为普通加减本来按位宽环绕。

固定约束：

- 除法/余数使用 `rax:rdx` dividend，结果 `rax`/`rdx`；有符号 `MIN / -1` 在指令前显式分支到规范结果；
- variable shift count 使用 `cl`，constant shift 直接编码 imm8并先按位宽规范化；
- `cmpxchg` expected 使用 `rax`；
- byte setcc 使用可编码低 8 bit register，再 zero-extend 为规范 bool；
- i128 加减使用两条 `add/adc` 或 `sub/sbb`，乘除调用 compiler-generated baseline glue；
- bounds/除零/非法状态进入共享冷 panic stub，携带稳定 source location ID。

RIP-relative code/data addressing是镜像内默认。超过 rel32 距离时 image planner 在调用者附近生成 16 字节对齐 veneer：`movabs r11, target; jmp/call r11`。veneer 按 `(source fragment, target symbol, kind)` 去重并进入 stack/unwind 验证。

### 浮点

`addss/addsd`、`subss/subsd`、`mulss/mulsd`、`divss/divsd` 和 `ucomiss/ucomisd` 实现普通操作。NaN 比较显式组合 parity/condition flags以匹配语言 `== != < <= > >=`。float 到 integer 的范围/NaN 在 conversion 前检查；不能依赖 `cvtt*` 的 indefinite result 暗中决定语言值。

### 原子与 fence

自然对齐 1/2/4/8 字节 atomic：

- Relaxed/Acquire load 和 Relaxed/Release store 使用普通 `mov`，但 LIR memory/effect fence阻止非法 compiler 重排；
- SeqCst store 使用 `xchg`；
- RMW 和 compare-exchange 使用 `lock` 指令；
- Acquire/Release/AcqRel fence 只作为 compiler barrier，SeqCst fence 使用 `mfence`；
- 不支持的 16 字节 atomic 在类型/目标检查阶段拒绝，不调用隐藏锁 fallback。

volatile 每次生成一次精确宽度访问，不能合并、删除或移动跨另一个 volatile/atomic/foreign/safepoint effect。

### runtime fast path

只有 descriptor不含 `HAS_RESOURCE`、请求不 large/pinned、高对齐且 footprint不跨 Immix block时才走 allocation fast path。当前 `[r15 + tlab_cursor_offset]`/limit只覆盖 processor本地 span中的一个连续空 line run；checked计算 16 byte header、对齐 padding和 payload，成功时推进 cursor并初始化 header。run不足调用 `gc_refill_line`，它先在本地 8-block span推进 line表，span用尽才访问全局 heap；其它请求调用 `gc_alloc_slow`。offset与 `HeapLayout`由 runtime layout query固定并进入 backend schema。

safepoint poll 读取 `[r14 + state_offset]` 的 preempt/GC bits，通常分支不 taken；slow path 保存登记寄存器并切 system stack。write barrier 先测试全局 marking flag 和 generation/card 条件，调用/inline 规则必须与[GC 元数据](gc-metadata.md#write-barrier-与-remembered-set)相同。`ForeignLeaf` 直接按目标 C ABI 发出调用，不执行 processor 释放或 system-stack bridge；调用前的 `StackCheck` 必须覆盖 caller frame 与声明的 leaf stack budget。未标注和 `ForeignBridge` 调用发出完整交接桩。

## block layout 与 branch relaxation

先以 entry 的 reverse postorder布局 hot block；panic、unwind、allocation/barrier slow path 和没有 hot predecessor 的 block放在冷区。条件分支优先让静态概率较高边 fallthrough：错误/越界为冷，循环 backedge为热，未知分支保持 GIR successor 顺序。

首次按 rel32 编码，计算最终 offset 后把范围适合且不会因自身缩短使其它分支失效的分支改为 rel8。按 code offset 顺序迭代，直到一轮无变化；分支只能从长变短，保证终止和确定性。外部/跨 fragment 目标保持 rel32 relocation 或 veneer。

## 线性扫描寄存器分配

### interval

block 按最终 layout 编号，每条 instruction 获得间隔为 2 的 position；奇数 position 留给 split/spill。liveness 按 CFG fixed point 求得，block 参数/edge copy 在 predecessor 末端建 use。interval 由有序不相交 range 和 use position 组成。

spill weight 使用 `u64` 饱和累加：普通 use 1，fixed-register use 4，loop depth `d` 乘 `min(10^d, 1_000_000)`，safepoint 后立即使用再乘 2。rematerializable 常量/symbol address 的 spill cost 为 0，但每次重建仍计入 code-size 成本。

### 分配规则

GPR 和 XMM 独立分配。值跨 call 活跃时 GPR 首选 `rbp,r12,r13`；不跨 call 时首选顺序为：

```text
rax, rcx, rdx, rbx, rsi, rdi, r8, r9, r10, rbp, r12, r13
```

XMM 顺序为 `xmm0..xmm15`，内部调用全部 clobber。fixed instruction constraint 先占用要求寄存器并在前后 split 冲突 interval。

allocator 维护按结束 position 排序的 active/inactive 集。没有空闲 register 时，在当前 interval 与占用候选中选择 `spill_weight / remaining_length` 最低者 spill；比例用 `u128` 交叉乘法比较，不使用宿主浮点。相等时 spill 稳定 value ID 较大者。interval 在下一 use、call、safepoint和 fixed constraint 前后 split，不能在 instruction 中间 split。

stack spill slot 按 `(size, align, root_class)` 分组复用，只有 live range 不重叠才可共用。root class 为 heap pointer、stack pointer 或 non-pointer；不同 root class 不共用 slot，使 stack map 和栈复制验证不依赖某时刻残留位。slot 分配按 interval 起点/ValueId 排序，选择最低 offset 可用槽。

### parallel copy

block 参数、call argument 和 return shuffle先构造成并行 copy图。无环边按目标空闲顺序执行；cycle 使用 `r11`（GPR）或一个为该函数预留的 16 字节 stack scratch（XMM/内存）打断。scratch 不进入 stack map，且在 safepoint 前 copy 必须全部完成。

## frame layout

frame 从完成 prologue 后的 `rsp` 低地址向高地址固定排列：

1. outgoing call area；Windows 发生任何调用时至少 32 字节 shadow space；
2. address-taken local 与 stack aggregate，按对齐从高到低、stable slot ID 排列；
3. heap-pointer spill；
4. stack-pointer spill；
5. non-pointer/XMM spill；
6. parallel-copy scratch；
7.实际使用的 `rbp`、`r12`、`r13` save slot；
8. 零 padding；
9. 调用者压入的 return address，不计入 frame size。

outgoing area大小是函数所有 callsite 所需最大值，因而 body 中 `rsp` 不变化。payload 布局结束后：

```text
frame_size = align_up(payload_size + 8, 16) - 8
```

有调用/safepoint的函数至少得到 8 字节且 `frame_size % 16 == 8`。prologue 在 sub 前执行：

```text
candidate = rsp - frame_size
if candidate < current_coroutine.stack_guard: morestack(frame_size)
rsp = candidate
store used callee-saved registers to fixed slots
```

`frame_size` 必须小于等于 `u32::MAX`；更大的单函数 frame 在代码生成前报 `implementation-limit`，不能依赖更高 runtime stack max截断。单函数最终 code size同样必须小于等于 `u32::MAX`，以满足 stack-map、unwind和 source record 的相对 offset表示。

`morestack` 把 return PC、九个整数参数寄存器和八个浮点参数寄存器保存到 coroutine 控制块的固定 scratch，并以该函数的 `MorestackEntry` map 登记其中哪些 GPR 是 heap/stack root；随后切换 worker system stack并按[调度器](scheduler.md#可复制协程栈)增长。复制过程用 caller `CallReturn` map 修正 outgoing stack 参数，恢复后装载已更新 scratch并重新进入原 prologue。scratch 每个 coroutine 一份且只在该 coroutine Running 的 prologue 使用；嵌套增长说明契约已破坏，进入 `RuntimeInvariant` fatal。

epilogue 从固定 slot恢复 callee-saved、`add rsp, frame_size`、`ret`。prologue/epilogue只能使用 Windows unwind 可描述的指令子集；Linux CFI 与 Windows unwind record 都从同一个 `FrameLayout` 生成。

`TailCall` 在 frame仍存在时完成寄存器并行 copy，随后按 epilogue规则恢复 callee-saved并释放 frame，最后 `jmp` callee；调用者原 return address保持在 `rsp` 顶端。eligibility 已保证没有 stack argument/sret/root/cleanup，后端不得临时借用已释放 frame保存参数。

真正 leaf 且 frame payload 为 0、无 safepoint、无 unwind cleanup时完全省略 prologue。内部 ABI 不使用 SysV red zone，以保持 Linux/Windows frame和异步 signal 边界一致。

## stack map、panic 与 unwind

寄存器分配后按[栈图](stack-maps.md)生成 safepoint root。`CallReturn`/suspend/`ForeignBridge` 点把所有用户 pointer spill；`ForeignLeaf` 只有在其它 effect 要求 `CallReturn` 时才建立普通调用记录，不能作为 bridge safepoint。leaf 的 pre-call `StackCheck` 仍是独立 safepoint，若增长 stack 必须先完成复制和 root 修正。poll可以记录 register root。instruction offset在 branch relaxation 和 encoding 后最终回填。

每个 function 生成唯一 `UnwindFunction { code_rva: u64, code_size: u32, frame_size: u32, saved_gpr_mask: u16, landing_start: u32, landing_count: u16, flags: u16 }`。landing table 每项固定为 `LandingRecord { pc_start: u32, pc_end: u32, landing_pc: u32, cleanup_chain: u32 }`，按 `pc_start` 严格递增且范围不重叠；offset 都相对 function code 起点，`cleanup_chain == u32::MAX` 表示只恢复传播。

Linux 按 code RVA 顺序把每个 `UnwindFunction` 写成一个 DWARF CFI FDE，并把 `LandingRecord` 写入该 FDE 引用的 LSDA call-site table；Windows 按相同顺序写一个 `RUNTIME_FUNCTION` 和对应 `.xdata`，landing 数据跟在 `UNWIND_INFO` 的 Gugu language-handler data 后。stack map `FunctionRecord.unwind_index` 就是该目标排序表中的 ordinal，必须与 function table 一一对应。Gugu panic unwinder据此选择 cleanup landing pad；外部工具/OS 使用平台表恢复寄存器。prologue code/offset必须满足 Windows UNWIND_INFO 限制。

panic 不允许越过未登记 C frame。export thunk 捕获 Gugu panic并按平台规范终止/转换，import call 内发生 foreign exception 不能伪装成 Gugu panic。landing pad 自身是普通 Gugu block，具有 stack map和禁止再次使用已消费 cleanup 的状态。

## inline asm 与 global asm

前端按[不安全边界](../spec/unsafe.md#asm-与-global_asm)唯一规定的 AT&T syntax解析 inline/global asm并生成 `AsmInst`；后端用同一 x86 encoder编码，不调用 `as`。本章只规定约束分配与机器 lowering，不另建一套可接受语法。无法映射到 baseline encoder的已解析指令或 relocation按公开 asm 规则诊断。

inline asm operand先由 constraint 分配 fixed/任意 register或 memory，声明的 clobber加入 interval；未声明却被模板写入的 register由 parser 数据流检查拒绝。普通 inline asm不能读写 `rsp`、`r14`、`r15`，不能跳出模板、定义外部符号或伪造 safepoint。`options(naked)` 只用于平台规范允许的 naked function，完整负责 C/rt0 ABI且不能含普通 Gugu value、GC root、panic或调用。

global asm 输出独立 fragment，只能引用显式 export/import 和 compiler提供的稳定逻辑符号句柄；不能按字符串猜 Gugu mangled name。

## 符号与 relocation

内部符号文本固定为：

```text
__gugu_<kind>_<64 lowercase hex stable key>
```

kind 为 `fn`、`static`、`vtable`、`glue`、`runtime`、`const` 或 `veneer`。C import/export 使用用户/属性指定名称，不加该前缀。内部 key 冲突在 image planning 阶段报 internal error。

fragment relocation 封闭为：

- `PcRel8`、`PcRel32`；
- `Abs64`；
- `Rva32`；
- `GotPcRel32`；
- `ImportSlotPcRel32`；
- `TypeId32`；
- `TypeRecordRva32`；
- `SourceRecord32`。

地址 relocation 含 offset、addend、target stable symbol和 field width；`TypeId32`/`TypeRecordRva32` 的 target 是完整 `StableTypeKey`。`SourceRecord32` 的 target 是 `{ MonoKey, fragment_source_ordinal }`；ordinal 按 fragment 内 `(instruction offset, logical path, start byte, end byte, synthetic kind)` 排序分配，因而相同源码 span 的多个机器范围仍可区分。image planner 在闭世界类型/源码表排序后分别写入稠密 `TypeId`、type record RVA 或 source record index，使每实例 fragment不依赖本次集合的临时编号。应用前 checked 验证范围；溢出只可通过规范 veneer/GOT/import slot解决，不能截断。

## image planner

逻辑 section 按目标规范映射，片段在 section 内按 `(alignment descending, stable symbol key, fragment kind)` 排序。alignment padding 全为 0 或 x86 NOP（只限 executable）。相同 code folding 只合并 machine bytes、relocation target序列、unwind/stack map、source location records和可见性都相同的内部函数；C export和取地址身份不同的 function不合并。

### Linux ELF64

[平台 ABI](../spec/platform-abi.md#可执行镜像形式)给出 Linux external image profile。static PIE路径的内部 rt0在读取待重定位 global前由 `AT_PHDR` 与首个 `PT_LOAD.p_vaddr` 计算 load bias，只解释 writer生成的 relative relocation，checked写入 `load_bias + addend`并封闭 RELRO；未知 relocation、越界 target或重复执行进入 `RuntimeInvariant` fatal。

writer把平台登记的逻辑节装入 4096-byte对齐的 RX、R和 RW segment；需要自重定位的 target只落在初始可写 `.data.rel.ro`，完成后转只读。dynamic FFI路径只消费 `TargetDescriptor` 的 interpreter/sysroot/SONAME并生成对应 dynamic tables，不能搜索宿主路径。static archive member按未解析 C symbol精确抽取。

`staticlib` 写确定性 SysV ar archive，member timestamp/uid/gid 为 0、mode固定，成员按 symbol key排序；`cdylib` 写 ET_DYN、只导出显式 C symbol并包含自有闭世界 runtime/metadata。

### Windows PE32+

PE writer消费[平台 ABI](../spec/platform-abi.md#可执行镜像形式)给出的 PE32+、ASLR/NX、入口、逻辑节和导入导出要求。当前私有 writer profile使用 image base `0x0000000140000000`、section alignment 4096、file alignment 512、COFF timestamp/checksum 0和 `WINDOWS_CUI` subsystem；这些字段不扩大平台稳定面。section按逻辑节映射，绝对 VA全部进入 base-relocation table。

IAT只消费 `TargetDescriptor` 和构建元数据登记的 DLL/symbol并稳定排序；export table只含显式 C export。`staticlib` 写确定性 COFF archive，`cdylib` 写 PE DLL；不需要 `.lib`导入库作为最终写出的中间步骤。

## 直接编码与验证

encoder 对每个 `X64Inst` 先计算 exact 长度，再写 prefix、REX、opcode、ModRM、SIB、displacement和 immediate；两次计算必须一致。每条 instruction记录起止 offset，stack map、branch、relocation和 source table只引用该边界。

写镜像前必须：

- 解码或结构复核所有 emitted instruction，确认长度和 fixed register constraint；
- 验证所有 branch/relocation range、symbol resolution和 section权限；
- 验证 internal/C ABI 参数、callee-save、stack alignment和返回分类；
- 验证 frame、stack map、unwind和 landing pad逐函数一致；
- 验证 ELF/PE header、segment/section、import/export、TLS、relocation和入口范围；
- 确认 writable section不可执行、stack不可执行、metadata只读且 strip保留；
- 对相同 image plan重放编码并比较 bytes，禁止时间戳、随机 GUID和目录顺序进入输出。

目标测试使用固定 LIR fixture和 C 对照 fixture覆盖整数/floating边界、聚合 ABI、register压力、spill、critical edge、branch relaxation、i128、atomic、panic unwind、stack growth、GC safepoint、ELF relocation和 PE unwind/import。真实执行 smoke test分别直接启动 Linux ELF和 Windows目标环境中的 PE；只比较反汇编文本不能证明镜像可运行。

## 参考实现资料

- [Rust 编译器开发指南：代码生成](https://rustc-dev-guide.rust-lang.org/backend/codegen.html)
- [Go x86 instruction assembler](https://go.dev/src/cmd/internal/obj/x86/asm6.go)
- [Go internal ABI](https://go.dev/src/internal/abi/abi-internal.md)
- [System V AMD64 ABI](https://gitlab.com/x86-psABIs/x86-64-ABI)
- [Microsoft x64 调用约定](https://learn.microsoft.com/en-us/cpp/build/x64/x64-calling-convention)
