# x86_64 后端

本章规定 Gugu 首版自研后端从合法 LIR 到 x86_64 machine code、内部调用 ABI、寄存器分配、frame layout、重定位和 ELF/PE 镜像写出的完整路径。后端不调用 LLVM、系统 assembler 或系统 linker；原生静态/动态输入只通过已登记的构建元数据进入 image planner。

外部 C ABI、目标名称和稳定镜像边界以[平台与 ABI 参考](../spec/platform-abi.md)为准。本章的 Gugu 内部 ABI、符号、frame 和 metadata 可以随 `CompilerIdentity` 一起改变，但同一镜像中的 compiler/runtime 必须完全一致。

## 权威边界

[类型](../spec/types.md)、[unsafe](../spec/unsafe.md)、[程序模型](../spec/program-model.md)和[平台 ABI](../spec/platform-abi.md)唯一规定整数/浮点结果、可接受的 asm、目标、C ABI、导入导出与稳定镜像面。本章只固定官方 compiler如何把合法 LIR实现为这些结果。内部 ABI、CPU指令选择、frame、mangling、relocation计划和直接写出算法随 `CompilerIdentity` 版本化，不能扩大或缩小公开接受面。

## 目标描述符与数值 lowering

当前后端只实现平台注册表中的`x86_64-linux`和`x86_64-windows`。每个toolchain安装携带不可变`TargetDescriptor { name, object_format, page_size, cpu_baseline, linux_interpreter, sysroot_digest, import_policy_revision, runtime_tuning_profile_digest, backend_cost_profile_digest }`；目标运行时路径与宿主sysroot分离，descriptor整体进入compiler identity和action key，backend不探测宿主PATH。runtime tuning profile至少固定`LocalDequeMode`、remote/injection shard数和queue padding；backend cost profile固定寄存器保留、inline code-size与spill上限。release镜像只编入该profile选中的一种deque，不生成运行时mode分支。`runtime_tuning_profile_digest`取`RUNTIME_TUNING_PROFILE.digest()`（域`gugu-runtime-tuning-profile-v1`），`RuntimeRawContractV1::verify`交叉校验调度段与profile的容量、分片、batch、service节奏与填充字段。

CPU可接受面只读取[平台 CPU 基线](../spec/platform-abi.md#cpu-baseline)，后端 instruction verifier拒绝任何超出 descriptor的机器指令。数值 lowering只实现[类型系统](../spec/types.md)给定的整数/浮点结果；SSE2、NaN、overflow、shift和conversion选择是这些结果的机器实现，不在本章创建另一套数值规则。

## 后端阶段

LIR 后端严格按以下顺序运行：

1. `Legalize`：把剩余高层 operation 变成 baseline 可表达序列；`DecodeCompressedRef` 由此展成 cage id 抽取、generation 校验、offset/bounds 检查与基址拼回的完整机器码序列（LIR 层只产出该 op 本身，见[内存 owner lowering](gir-lir.md#memory-owner-lowering)）；
2. `SelectInstructions`：选择 x86_64 opcode、address mode 和 fixed-register constraint；
3. `ScheduleBlocks`：固定 block layout、fallthrough 和冷路径；
4. `BuildLiveIntervals`：计算物理 register class、liveness 和 call/safepoint constraint；
5. `AllocateRegisters`：全局线性扫描、单条保守区间和 spill；
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
- 不超过 16 字节的普通 aggregate 在 LIR 之前就拆成标量 lane，因此 backend 只见到这些 lane；`by_value` 登记的 aggregate 参数（footprint 超过 16 字节、COW 或 resource）一律由 caller 传地址并只占一个整数槽；
- ZST 不占寄存器或 stack slot；
- 返回需要超过两个 piece 时，caller 把隐藏 return pointer 放在第一个整数参数槽，显式整数参数整体后移；该指针就是 `signature.sret` 对应的第一个 entry 参数本身，不额外追加隐藏参数，callee 侧 `results` 同时为空。

三类目标参数不占普通参数槽且必须由分类阶段显式定位：隐藏 sret 指针复用参数 0 的位置、间接调用的目标（`Provenance::Code` 参数）只用作 `call`/`jmp` 的操作数、动态派发的 vtable 取自 `Provenance::Metadata` lane；接收者不是胖指针 lane 而是指向胖对的指针时，vtable 在该指针的 `+8`（`dyn` 值布局 `data@0`、`vtable@8`），此时借用 `r11` 读取并登记 clobber。

整数和浮点 bank 分别前进；某一 aggregate 的任意 piece 无法放入对应寄存器时，该 aggregate 的所有 piece 都放到 stack，避免半寄存器半内存。stack 参数按参数顺序放入 8 字节 slot（`V128` 占 16 字节），并满足自身更高对齐；caller 固定 outgoing 区承载。C ABI 侧 Linux 仍按 SysV 的独立 bank 前进，Windows 的 Microsoft x64 则让整数与浮点**按参数位置共用**同一组槽：位置 0..3 对应 `rcx/rdx/r8/r9` 与 `xmm0..xmm3`，例如 `(i64, f64)` 是 `rcx` + `xmm1`；位置越界的参数从 32 字节 shadow space 之后开始。

内部 ABI 的间接 aggregate 不是借用 caller 原 place。通常 caller 先按 value descriptor在 outgoing 区物化完整语义副本并把地址传入；该存储在调用期间归 callee参数 local所有，callee可以修改并必须在正常/panic出口执行相应 drop/resource cleanup，caller返回后不再 drop同一副本。若 `EscapeAndPlacement` 已证明参数地址越过调用，caller改为物化独立 managed box，callee cleanup只结束参数绑定，box内 value由 GC/resource descriptor管理。trivial bit-copy 类型也不能因地址分析猜测而把可变参数别名到原值。

sret destination在 callee执行期间视为 caller未初始化字节，不能加入 `CallReturn` root map。callee必须先在自己 frame或已提升 managed box中构造返回值，以逐 safepoint初始化图追踪；只有所有字段成功且不再经过 safepoint时，epilogue才把完整值 transfer到 sret、清除本地所有权并返回。panic只清理本地部分值，绝不让 caller观察/扫描半初始化 sret。C export thunk同样先取得完整 Gugu返回值，再按 C ABI写外部 sret。

整数和浮点 bank 分别前进；某一 aggregate 的任意 piece 无法放入对应寄存器时，该 aggregate 的所有 piece 都放到 stack，避免半寄存器半内存。stack 参数按参数顺序放入 8 字节 slot，并满足自身更高对齐；caller 固定 outgoing 区承载。

调用 lowering 必须把 outgoing stack 参数中的每个 managed/stack pointer word，以及按值/间接 aggregate 副本由类型 descriptor 展开的全部 root-bearing 字段，登记到 caller 的 `CallReturn` map，不受“返回后是否继续活跃”影响。callee 的参数分类同时生成一个 entry register-root map；发生 stack growth 时，`morestack` 用它扫描和更新保存的寄存器参数，stack 参数和 aggregate 副本继续由 caller map 扫描。

内部 caller-saved 为所有参数/返回寄存器、`r11` 和 `xmm0..xmm15`。内部 callee-saved 为 `rbp`、`r12`、`r13`；`r14`、`r15` 是必须保持的 runtime register。跨调用活跃值优先分配 callee-saved或 spill。C import/export thunk 把该 ABI 与 SysV/Microsoft x64 完整互换，不能让内部约定泄漏到 `extern "C"`。

context switch 是单独登记的 machine-intrinsic 边界，不使用上述普通函数参数分配：`rdi` 指向待保存 context，`rsi` 指向待恢复 context，`rdx` 是新的 `CoroutineHot*`，`rcx` 是新的 `LogicalProcessor*`。switch 保存 `rsp + 8`、调用者 return PC、`rbx/rbp/r12/r13`，随后重建 `r14/r15`、恢复保存寄存器与 `rsp`，直接跳到保存的 `rip`。完成 trampoline 使用同一片段的 restore-only 入口，不写出可恢复的旧 PC。Linux/Windows 共用这套内部编码；宿主 C 验收 adapter 另行遵守其 nonvolatile 规则，不能把这套内部寄存器约定声明为 C ABI。

片段的 disp8 偏移由同一 `CoroutineContext` layout 生成，`RuntimeRawModel` 携带实际代码字节与 restore 入口偏移，backend 只消费已经 verifier 校验的片段。入口 `StackCheck` 使用 `CoroutineSlot` 基址加固定 64 字节的 `stack_check`，该偏移同时由 Rust 构建时断言、Gugu record 布局和 runtime 契约校验，禁止 backend 另存一个独立数字表。

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

每个 `LegalizeX86_64` opcode descriptor必须显式携带 `poll_cost: NonZeroU8`，范围1..64；没有条目的 opcode使 backend verifier失败，不能使用隐式默认值。单个 LIR op展开多条机器指令时cost取 descriptor中各指令权重的饱和和；managed asm复用同一 descriptor表。该字段只供 `PollSummary`/`POLL_BUDGET`使用，修改属于 poll policy和backend schema变更。

### 浮点

`addss/addsd`、`subss/subsd`、`mulss/mulsd`、`divss/divsd` 和 `ucomiss/ucomisd` 实现普通操作。NaN 比较显式组合 parity/condition flags以匹配语言 `== != < <= > >=`。float 到 integer 的范围/NaN 在 conversion 前检查；不能依赖 `cvtt*` 的 indefinite result 暗中决定语言值。

### 128-bit向量

内部 `V128` 固定映射到 x86-64-v1的 XMM/SSE2，不形成公开 SIMD ABI。连续向量 load/store默认使用 `movdqu`；只有静态对齐证明或 `LoopVersioningAndUnswitching` 的 alignment fast version支配访问时才能使用 `movdqa`。逐 lane整数/浮点操作、比较、shuffle与 wrapping整数 reduction必须完全由 descriptor列出的 SSE2序列 lowering；没有基线序列、需要 pointer lane或成本不优于标量版本时，loop vectorizer必须保留标量循环，不能在 legalization阶段调用 helper或静默 scalarize。浮点 reduction不得改变源顺序，AVX及更高扩展仍由 instruction verifier拒绝。

### 原子与 fence

自然对齐 1/2/4/8 字节 atomic：

- Relaxed/Acquire load 和 Relaxed/Release store 使用普通 `mov`，但 LIR memory/effect fence阻止非法 compiler 重排；
- SeqCst store 使用 `xchg`；
- RMW 和 compare-exchange 使用 `lock` 指令；
- Acquire/Release/AcqRel fence 只作为 compiler barrier，SeqCst fence 使用 `mfence`；
- 不支持的 16 字节 atomic 在类型/目标检查阶段拒绝，不调用隐藏锁 fallback。

volatile 每次生成一次精确宽度访问，不能合并、删除或移动跨另一个 volatile/atomic/foreign/safepoint effect。

### runtime fast path

`EscapeAndPlacement` 的结果决定 managed allocation lowering：

- `Managed::TurnRegion` 只在当前 coroutine turn 私有、无外部 alias、无 ResourceCell lease 且不需要 FFI 地址时从 `[r15 + turn_region_cursor]` 做 checked bump；region export/publish、promote、transfer 和 reset 全部落到可 safepoint slow edge；
- `Managed::LocalHeap` 仅在 descriptor不含 `HAS_RESOURCE`、请求不 large/pinned、高对齐且 footprint不跨 Immix block时走 processor-local TLAB/连续空 line run；checked计算 16 byte header、对齐 padding和 payload，成功时推进 cursor并初始化 representation header。run不足调用 `gc_refill_line`，span用尽才访问 owner/domain range；其它请求调用 `gc_alloc_slow`；
- TLAB/TurnRegion 的 bump 先按 payload 地址对齐：`payload = align_up(cursor + 16, max(align, granule))`、`header = payload - 16`，与 runtime 参照实现的 `allocate` 逐字节一致（高对齐 padding 落在 header 之前）。地址加法必须用置标志位的 `add`（不能用不改 flags 的 `lea`）以便用进位检测环绕，推进后再与 span limit 比较；非规范对齐或超出 imm32 的算术走 `gc_alloc_slow`，不另立经验常量；
- `Managed::SharedHeap` 走 stable handle allocation/resolution；`SharedAccessBegin/End`、generation check、forwarding grace 和 handle table 更新必须位于带 stack map 的 slow edge，不能污染 LocalHeap direct-pointer fast path；
- `Managed::Pinned`、large、foreign 和 resource 请求走各自 non-moving/cleanup 路径；`RuntimeRaw` 仍使用 owner-local slab/span pop/bump。所有路径的 offset、size、align、generation 和 representation tag 使用 checked arithmetic，溢出进入 `OutOfMemory` 或 `RuntimeInvariant`，不静默截断。

allocation fast path不能读取 owner lookup、radix route、global gc epoch、SharedHeap handle table、mark mailbox 或 memory limit；这些只能在 refill/slow path处理。`TurnRegion` 和 LocalHeap TLAB 的普通 bump 同样不执行全局原子。

### prologue 与 stack check

runtime layout query还必须验证`CoroutineHot`与`StackDescriptor`的size/alignment均为64、`CoroutineSlot`的size/alignment为128/64、`offset_of!(CoroutineSlot, stack) == 64`，`PollControl`与`ProcessorOwnership`的size/alignment均为64，`LogicalProcessor.poll/ownership`按64对齐且范围不重叠，以及RemoteBatchHead、LocalDeque head/tail、idle event counter分别占用128-byte padded区域。query返回的`stack_check_offset`是从`CoroutineHot*`基址到descriptor字段的absolute offset，因此prologue仍从`[r14 + stack_check_offset]`读取；loop/显式poll只从`[r15 + poll_flags_offset]`读取`PollControl` line。两种fast path都恰有一次ordinary acquire memory operand，禁止附加lifecycle/global epoch load。任一layout/profile断言失败都是compiler/runtime schema不匹配，镜像构建必须失败，不能改用未对齐访问或其它queue mode。

### owner return 与 range lowering

x86_64 backend 对 owner inbox 的 tail/front、staging、route bucket 和统计字段执行独立 cache-line layout assertion。AcqRel batch tail exchange、Release chain link、Acquire consumer read 使用现有 atomic lowering；owner-only free list、front 和 local range cursor不使用原子。新路径不得占用 `r14`/`r15` 内部 ABI，也不得在每次 TLAB allocation 中执行 owner lookup 或 radix hash。

raw return、extent coalescing、commit/decommit 和 typed combining 都是 slow path；平台调用不能落在 `NoSafepointRegion` 或 `PollFreeLeaf`。完整机器序列、generation 检查和 direct/radix profile 见[内存所有权与消息通道](memory-messaging.md#backend-memory-ordering)。

BatchInbox每个publish batch先Release写producer-local`publish_active`、Acquire读一次read-mostly queue control word；这两步不能移动到节点state/link修改之后。ordinary `run_link_next/run_batch_len`写必须保持在head Release CAS之前；CAS成功后seen epoch/staging clear与Release清active不得移动到它之前。x86_64把Relaxed head load降为普通`mov`、Release/Relaxed compare-exchange降为`lock cmpxchg`、Acquire `head.swap(null)`降为`xchg`，并由LIR effect edge阻止compiler重排；不得额外插入generic epoch pin、SeqCst fence或per-node atomic link。empty-to-nonempty才生成`work_seq`的locked RMW。consumer的ordinary link读必须位于Acquire exchange之后，并在清queue ownership前保存`next`。

Batch publish的CAS retry是合法cyclic runtime CFG，不得包在`NoSafepointRegion`中；`ProducerHandle.pending_node/staging`使poll或GC edge拥有完整typed roots。LocalDeque slot普通load/store只有在所选`Classic64`或`Packed55`算法的ticket ownership证明成立时才能生成；instruction verifier检查Classic64的SeqCst fence/最后一项CAS，或Packed55的single-word AcqRel reservation/commit与`RESETTING`序列，不能把两个mode的内存序拼接。Packed55不能lower为两个独立`u32` lane或16-byte隐藏锁fallback。

`NoSafepointBegin/End` 在 instruction scheduling、poll placement和 verifier阶段保持 effect fence，encoder不为 marker写任何 byte、relocation或 stack map。marker内的 reserved-barrier、atomic和 unlock仍按各自机器指令编码；删除 marker不能允许相邻普通操作跨 region边界重排。

## block layout 与 branch relaxation

先以 entry 的 reverse postorder布局 hot block；panic、unwind、allocation/barrier slow path 和没有 hot predecessor 的 block放在冷区。条件分支优先让静态概率较高边 fallthrough：错误/越界为冷，循环 backedge为热，未知分支保持 GIR successor 顺序。

首次按 rel32 编码，计算最终 offset 后把范围适合且不会因自身缩短使其它分支失效的分支改为 rel8。判定位移必须用**缩短后**的指令终点：`jmp` 短跳少 3 字节、`jcc` 少 4 字节，前向跳的终点前移会让 `target - end` 变大，按旧终点收下的边界分支会在下一轮编码中越界。其它分支的收缩只会让位移向合法区间移动（前向跳变小、反向跳绝对值变小），因此按 code offset 顺序迭代到一轮无变化即可收敛；分支只能从长变短，保证终止和确定性。统计量 `rel8` 是最终短跳 form 的条数，不按收缩次数累加。外部/跨 fragment 目标保持 rel32 relocation 或 veneer。

## 线性扫描寄存器分配

### interval

block 按最终 layout 编号，每条 instruction 获得间隔为 2 的 position：`use_slot = 2p`、`def_slot = 2p + 1`，相邻 instruction 之间留出定义与使用分开的缝隙。一个**点位**（point）是分配与 stack map 共用的最小单位，携带 `(site, kind, block, instructions, mask, clobber, pointer_spill, fixed_physical)`：`kind` 取 `Normal`/`Call`/`Bridge`/`Prologue`，由 LIR op 的规范 safepoint 种类推出；`mask` 是该点位会写或会读的物理寄存器集合，加上 form 自带 clobber，Call 点位再加全部 caller-saved GPR（`rax, rcx, rdx, rbx, rsi, rdi, r8, r9, r10`）与全部 XMM，Bridge 点位在 Call 之上再加 pointer spill 标记。读也必须进 mask：ABI 返回寄存器（`rax`、`rbx`、`rdx`、`xmm0`、`xmm1`）在返回值搬运点只是被**读**，若只按写建 mask，转换或 scratch 仍可能覆写它们。

liveness 按 CFG fixed point 求得，block 参数与 edge copy 在 predecessor 末端建 use；某个值的 interval 是**单条保守区间**：`start` 取 `live_in` 首点与所有读点最小值，`end` 取 `live_out` 末点与所有写点最大值。这与"在每个 clobber 点把区间切开、边界插转换 move"的经典做法不同：块内存在 `Switch` trampoline、`GcAlloc` 快慢路这类**同一 block 内的条件跳转**，块参数还可能由多条边定义并跨 backedge 使用，边界 move 无法保证在所有路径上恰好执行一次；统一按值给一个位置、溢出值在使用处读回、定义处写回，可证明地对任意 CFG 成立，代价是牺牲了分裂带来的更细粒度着色。

spill weight 使用 `u64` 饱和累加：普通 use 1，含 fixed-register 或 Call/Bridge 点位的 use 4，紧跟在同 block 内 Call/Bridge 点之后的 use 再乘 2，loop depth `d` 乘 `min(10^d, 1_000_000)`（支配树回边识别自然循环，非可归约环退回 SCC 计数并按深度 1 处理）。定义是 `IConst`/`FConst`/`SymbolAddr`/`StackAddr` 的值为 rematerializable，weight 为 0；当它本来会被溢出时改为"无位置"：定义站点整段丢弃，每个使用处按原 lowering 重新物化一次。

### 分配规则

GPR 和 XMM 独立分配。可用 GPR 池是 `rax, rcx, rdx, rbx, rsi, rdi, r8, r9, r10, rbp, r12, r13`——`rsp`、`r14`、`r15` 与内部 ABI 保留的 scratch `r11` 不参与；XMM 池是 `xmm0..xmm15`。值跨 Call/Bridge 点位活跃时优先 `rbp, r12, r13`（callee-saved，被调者会保存），其余情况按上表顺序；空闲寄存器不足时，只把**根类要求必须落栈**的值（managed heap 指针、stack 指针）或跨 bridge 的指针直接判为 spill slot，不让它们挤占其它值的寄存器。

候选寄存器必须同时满足三条：不在该值覆盖范围内任一点位的 `mask` 里、当前没有别的活跃值占用、通过宽度检查（8 位操作数的值只能放在能编码 `r/m8` 的寄存器上，溢出槽不受限）。选择用确定性的候选序，不使用随机或哈希顺序。

没有可用候选时，在当前 interval 与占用冲突的候选中选择 `spill_weight / remaining_length` 最低者 spill；比例用 `u128` 交叉乘法比较，不使用宿主浮点。相等时 spill 稳定 value ID 较大者。**分类与占用都按点位推进**：候选检查使用该值覆盖范围内全部点位 mask 的并集，因此一次放置对整条区间成立。

被 spill 的值没有寄存器位置：读取它的操作数在指令前插入一次载入（`mov`/`movsd`/`movups`，目标取该点位空闲的 scratch），宽度够的操作数（`Rm8/16/32/64`、`XmmRm`）直接换成 `[rsp + slot_offset]`；只接受寄存器的操作数（和 `Mem` 的 base/index）先载入 scratch。写它的站点按宽度落地：`Rm64`/`XmmRm` 直接写内存；窄宽度或仅寄存器形式先写 scratch，再在该站点内该值最后一次写入之后补一次写回（值在此后仍活跃才发射）。槽内保持**零扩展规范形**，所以窄写必须先把槽读进 scratch 做字节合并，32/64 位写则直接覆盖整个寄存器。

stack spill slot 按 `(size, root_class)` 分组复用：heap 指针、stack 指针与 non-pointer/XMM 分开，只有 live range 不重叠才可共用。`V128` slot 固定 size/align 均为 16 并属于 non-pointer。slot 分配按 `(start, value)` 排序，在每个分组内选择 offset 最低的可用槽，因此偏移是确定性的。

### parallel copy

block 参数、call argument 和 return shuffle先构造成并行 copy图。选指阶段先把块参数拷贝摊平成串行 move 序列：优先发射目标不再作为任何剩余拷贝源的边，只剩环时把环上一条边的目标值先存进同类型的**临时虚拟寄存器**、把仍读该目标的源改指临时寄存器，再发射那条边；渲染结果必须自洽（harness 与站点文本断言读它），但临时虚拟寄存器不进入分配。

块参数拷贝属于**边**：`Branch` 只在被选中的那条路径上执行拷贝，需要拷贝的 taken 边经 trampoline 进入，两个后继不会共用同一段拷贝。形状固定为

```text
<test>
jcc L_taken            ; taken 边需要拷贝时才进 trampoline
<fall 边拷贝>
jmp fall
L_taken:
<taken 边拷贝>
jmp taken
```

`Switch` 的每个 case 各自独立：需要拷贝的 case 走自己的 trampoline，**默认（otherwise）边的拷贝落在"所有 case 都不匹配"的直落路径上**，随后才是 `jmp otherwise` 与 trampoline 段；漏掉默认边拷贝会让默认后继直接读到未初始化的槽。`Invoke` 只为 normal 边发射拷贝，unwind 边的参数由展开器和 stack map 恢复。

stitch 阶段把站点序列按累积位移重写标签：块标签编号在 `0..block_count`，站点内标签平移后在终结符编号空间之前；终结符自己的标签编号已是函数级，但**定义位置必须按终结符在函数序列里的起点重定基**，否则 trampoline 标签会指到函数开头。

寄存器分配阶段按同一批 pairs 重发整段拷贝（渲染结果被整体丢弃）：每组的物化把 rematerializable 的源按原 lowering 重建到该点位空闲且不在 mask 内的寄存器；拷贝按目标空闲顺序执行，内存到内存经 `r11` 搬机器字（16 字节值搬两个字），环用 `r11`（GPR 目标）或 frame 里预留的 **16 字节 copy scratch**（XMM/内存目标）打断。scratch 只在需要时预留，不进入 stack map，且在 safepoint 前 copy 必须全部完成。逐组统计 `copy_moves` 与 `copy_cycles` 进入 payload 的 `stats`。

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

有调用或 mandatory statepoint的函数至少得到8字节且 `frame_size % 16 == 8`。只有 entry `StackCheck`、没有 frame payload/call/safepoint的函数可以保持 `frame_size = 0`，但仍执行 poisoned guard比较。带 `StackCheck` 的函数，prologue 在任何 `rsp` 修改前执行，且形状固定为一个入口跳转、一个冷路径与一个检查点：

```text
        jmp   check
cold:   call  morestack_or_poll        ; 冷路径只由检查点条件进入
        jmp   check
check:  lea   r11, [rsp - required_frame]      ; 超出 disp32 时 mov r11, rsp; sub r11, imm64
        cmp   qword ptr [r14 + stack_check_offset], r11
        jg    cold
        sub   rsp, frame_size                  ; 检查通过后才是 frame 装载
        mov   [rsp + save_offset], <callee-saved>
```

冷路径放在最前是因为 `check` 与 `cold` 两个标签是站点内编号 0/1：站点序列重写后标签记账仍必须从 0 起连续，跳板式的两段布局会让编号出现空洞。`required_frame = frame_size + max_leaf_reserve`；`max_leaf_reserve` 是本函数所有 direct `ForeignLeaf` call的声明预算最高值，checked加法溢出直接进入 `StackOverflow` fatal。大 immediate使用 `mov r11, rsp; sub r11, materialized_required_frame`；`r11`是内部 ABI保留 scratch，不承载参数或 root。candidate计算采用机器字 wrapping语义，随后固定为一次 `cmp r11, qword ptr [r14 + stack_check_offset]`与一个 signed 冷分支 `jg cold`（`acquire(limit) > candidate` 时进冷路径）。官方 stack reservation处于低半 canonical address：正常 candidate与 `stack_low`都是非负 `isize`；容量计算发生地址下溢时 candidate解释为负值；`POLL_SENTINEL = isize::MAX as usize…

**`PollFreeLeaf` 的例外**：分类为 `PollFreeLeaf` 的函数省略栈检查，但**不**省略 frame。只要它仍然需要 frame（有溢出槽或 save slot）而 frame payload 非 0，就照常发 `sub rsp, frame_size` 与 save slot 装载——这些写落在 `rsp` 之下，省掉 `sub` 就会写到调用者的活跃栈上。只有 `frame_size == 0` 的 `PollFreeLeaf` 才完全没有 prologue。该函数真正的情形由选指阶段的 `StackCheck` 标记站点决定：标记站点存在则按上面的形状合成 prologue，不存在但有 frame 则把 frame setup 前插到入口块首个序列之前。

taken edge尚未建立 callee frame。`morestack_or_poll` 只通过 `r14`把 return PC、九个整数参数寄存器和八个浮点参数寄存器写入 coroutine控制块的固定 scratch；该过程不读取或写入 candidate以下的 user stack。随后切到 worker system stack，acquire读取 processor flags：先完成 GC stop，再处理可接受的 preempt；coroutine被重新调度后仍从同一 `MorestackEntry`恢复。全部 poll动作完成后才读取最新 `stack_low`，容量仍不足时增长，最后装载已经由 GC/stack copy修正的 scratch并重新进入原 prologue。因而 poll-first次序不依赖剩余 user-stack空间，并且增长不会漏掉已经发布的 stop请求。

每个可作为 `async` body入口的 code descriptor还发布 `entry_required_frame`，值覆盖入口 `required_frame`、ABI entry record和进入首个 checked prologue前的固定字节。runtime据此选择初始 stack class；该值使用与 frame layout相同的 checked计算并进入 backend/runtime schema，禁止另写经验常量。

### 分配产物与验证 {#allocation-artifacts}

`AllocateRegisters` → `ResolveParallelCopies` → `LayoutFrame` 由一个 `allocate` 调用完成，顺序是：点位与 liveness、线性扫描、spill slot 复用、一次不带偏移的并行拷贝**探针**（只为判定是否需要 16 字节 copy scratch）、frame layout、带真实 scratch 偏移的最终解析、序列改写、重拼与 branch relaxation、校验。产物 `Allocated` 携带 block 级序列、函数级 `sequence` 与 `rel8_count`、`frame`、点位表 `points`、逐站点 `sites`、逐值 `values`、`stats`，以及 `abi_regions`。

改写后的序列必须通过 `verify_allocated_sequence`：所有操作数（含 `Mem` 的 base/index）都是物理寄存器或内存，出现任何 `Reg::Virtual` 都是内部错误；`r14`/`r15` 不被写；`rsp` 只在 prologue/epilogue 所在的 ABI 区间里被修改。`StackAddr` 的 frame 占位基址（`Reg::Virtual(FRAME_SLOT_BASE + slot)`）在改写时全部换算成 `[rsp + local_offset]`，改写后不允许残留占位编号。

片段 payload（`CODEGEN_SCHEMA = 4`）新增 `frame`、`points`、`values`、`stats` 四段与站点级 `points` 范围：`frame` 给出 `frame_size`/`payload_bytes`/`outgoing_bytes`/`required_frame`/`max_leaf_reserve`/`locals`/`spill_slots`/`scratch_offset`/`save_offsets`/`checked`；`stats` 给出 `peak_live_gpr`、`peak_live_xmm`、`spill_slot_count`、`spill_bytes`、`spill_stores`、`reloads`、`rematerializations`、`copy_moves`、`copy_cycles`、`call_sites`、`safepoint_spills`、`allocated_values`、`frame_size_max`，其中 `frame_size_max` 必须等于该片段 `frame.frame_size`，`points` 段数必须等于站点 `points` 范围之和。这些计数就是 [Cost calibration profile](#cost-calibration-profile) 里 `CostRecord` 的 spill/reload 字段来源，因此必须取 branch relaxation 与 frame layout 之后的结果，不能用分配前估计代替。分配规则 revision 由 `ALLOCATION_REVISION = 1` 表达并进入 fragment key：规则变化会改变 query key，不会静默复用旧片段。

宿主执行验收：`Compilation::x64_fragments()` 暴露逐片段的机器字节、重定位、frame 视图，`cargo bench --bench x64_frame` 把片段按符号顺序映射为可执行页、解析内部 relocation、用 `ret` stub 兜住运行时入口，然后在真实 CPU 上按内部 ABI 调用入口片段并核对结果。它覆盖默认测试套件覆盖不到的部分：spill 槽读写、prologue/epilogue 的 `rsp` 调整、callee-saved 保存、栈参数传递与并行拷贝破环。指针值重定位到栈地址的场景需要 runtime allocator 才能构造参数，因此不在该 bench 的范围内。

`frame_size` 必须小于等于 `u32::MAX`；更大的单函数 frame在代码生成前报 `implementation-limit`，不能依赖更高 runtime stack max截断。单函数最终 code size同样必须小于等于 `u32::MAX`，以满足 stack-map、unwind和 source record的相对 offset表示。

epilogue从固定 slot恢复 callee-saved、`add rsp, frame_size`、`ret`。prologue/epilogue只能使用 Windows unwind可描述的指令子集；Linux CFI与Windows unwind record都从同一个 `FrameLayout`生成。

`TailCall` 在 frame仍存在时完成寄存器并行 copy，随后按 epilogue规则恢复 callee-saved并释放 frame，最后 `jmp` callee；调用者原 return address保持在 `rsp`顶端。eligibility已保证没有 stack argument/sret/root/cleanup；tail target若无 checked entry，形成的 backedge必须已由 poll budget pass覆盖。

只有 LIR `PollSummary` 分类为 `PollFreeLeaf` 的函数完全省略 prologue：frame payload为0、无 call/循环/safepoint/unwind且 legalized cost不超过64。取函数地址时生成带 `StackCheck` 的 checked thunk。内部 ABI不使用 SysV red zone，以保持 Linux/Windows frame和异步 signal边界一致。

## stack map、panic 与 unwind

普通`ForeignBridge` lowering把`foreign_bridge.lease_word`作为完整64-bit expected state。native返回的hot block只执行一次`lock cmpxchg qword ptr [r14 + state_offset]`，从精确`Foreign(g)`转为`Running(g)`；成功边先检查当前processor pending poll/GC再直接恢复coroutine stack，不能访问BatchInbox、injection或任何scheduler控制锁。失败边是cold block，调用runtime等待scan lock、取得idle processor或通过登记的ProducerHandle batch publish。该CAS同时线性化lifecycle与processor lease，backend不得另发`_Psyscall`式processor状态store/CAS。DirtyCpu mode在metadata中固定为detached并跳过该hot block。bridge call仍按C ABI clobber caller-saved值并在进入前物化全部pointer roots；返回快路径不削弱stack map要求。

寄存器分配后按[栈图](stack-maps.md)生成 safepoint root。`CallReturn`/suspend/普通 `ForeignBridge`/`ForeignBridge[DirtyCpu]` 点把所有用户 pointer spill；两种 bridge在 coroutine stack物化 ABI frame并以 high-relative offset登记，native段不生成 native stack map。counted inner chunk edge与 uncounted countdown-only edge都没有 map，只有实际 poll-word检查后的 resume label生成 `PollResume`；poisoned prologue复用 `MorestackEntry`。`ForeignLeaf`只有在其它 effect要求 `CallReturn` 时才建立普通调用记录。instruction offset在 branch relaxation和encoding后最终回填。

每个 function 生成唯一 `UnwindFunction { code_rva: u64, code_size: u32, frame_size: u32, saved_gpr_mask: u16, landing_start: u32, landing_count: u16, flags: u16 }`。landing table 每项固定为 `LandingRecord { pc_start: u32, pc_end: u32, landing_pc: u32, cleanup_chain: u32 }`，按 `pc_start` 严格递增且范围不重叠；offset 都相对 function code 起点，`cleanup_chain == u32::MAX` 表示只恢复传播。

Linux 按 code RVA 顺序把每个 `UnwindFunction` 写成一个 DWARF CFI FDE，并把 `LandingRecord` 写入该 FDE 引用的 LSDA call-site table；Windows 按相同顺序写一个 `RUNTIME_FUNCTION` 和对应 `.xdata`，landing 数据跟在 `UNWIND_INFO` 的 Gugu language-handler data 后。stack map `FunctionRecord.unwind_index` 就是该目标排序表中的 ordinal，必须与 function table 一一对应。Gugu panic unwinder据此选择 cleanup landing pad；外部工具/OS 使用平台表恢复寄存器。prologue code/offset必须满足 Windows UNWIND_INFO 限制。

panic 不允许越过未登记 C frame。export thunk 捕获 Gugu panic并按平台规范终止/转换，import call 内发生 foreign exception 不能伪装成 Gugu panic。landing pad 自身是普通 Gugu block，具有 stack map和禁止再次使用已消费 cleanup 的状态。

## inline asm 与 global asm

前端按[不安全边界](../spec/unsafe.md#asm-global-asm)唯一规定的 AT&T syntax解析 inline/global asm并生成 `AsmInst`；后端用同一 x86 encoder编码，不调用 `as`。本章只规定约束分配与机器 lowering，不另建一套可接受语法。无法映射到 baseline encoder的已解析指令或 relocation按公开 asm 规则诊断。

inline asm operand先由 constraint分配 fixed/任意 register或 memory，声明的 clobber加入 interval；未声明却被模板写入的 register由 parser数据流检查拒绝。managed inline asm不能读写 `rsp`、`r14`、`r15`，不能跳出模板、定义外部符号或伪造 safepoint；parser必须拒绝内部回边、间接控制转移、外部 call/ret、system/wait class和 repeat-prefixed string instruction。允许 opcode集合由公开 asm规则封闭，不能因宿主 CPU支持更多指令而变化。naked/global/dirty fragment使用 native parser模式，不受 managed有限 CFG限制，但仍必须满足目标 baseline encoder、声明 clobber和对应 ABI约束。
带函数体的 `#[ffi(dirty_cpu)] unsafe extern "C" fn` 不进入 managed fragment；backend 生成带 bridge mode 的 dirty thunk，参数和返回值只走 C ABI bit/raw-pointer representation。managed `#[naked]` 调用同样默认生成 `ForeignBridge[DirtyCpu]` entry，除非显式保留 `ForeignLeaf`；`global_asm` 符号则按对应 extern 声明 lowering。任何 dirty native fragment 都不得生成伪造的 managed safepoint或依赖 signal 在任意 PC 停止。

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

[平台 ABI](../spec/platform-abi.md#executable-image-forms)给出 Linux external image profile。static PIE路径的内部 rt0在读取待重定位 global前由 `AT_PHDR` 与首个 `PT_LOAD.p_vaddr` 计算 load bias，只解释 writer生成的 relative relocation，checked写入 `load_bias + addend`并封闭 RELRO；未知 relocation、越界 target或重复执行进入 `RuntimeInvariant` fatal。

writer把平台登记的逻辑节装入 4096-byte对齐的 RX、R和 RW segment；需要自重定位的 target只落在初始可写 `.data.rel.ro`，完成后转只读。dynamic FFI路径只消费 `TargetDescriptor` 的 interpreter/sysroot/SONAME并生成对应 dynamic tables，不能搜索宿主路径。static archive member按未解析 C symbol精确抽取。

`staticlib` 写确定性 SysV ar archive，member timestamp/uid/gid 为 0、mode固定，成员按 symbol key排序；`cdylib` 写 ET_DYN、只导出显式 C symbol并包含自有闭世界 runtime/metadata。

### Windows PE32+

PE writer消费[平台 ABI](../spec/platform-abi.md#executable-image-forms)给出的 PE32+、ASLR/NX、入口、逻辑节和导入导出要求。当前私有 writer profile使用 image base `0x0000000140000000`、section alignment 4096、file alignment 512、COFF timestamp/checksum 0和 `WINDOWS_CUI` subsystem；这些字段不扩大平台稳定面。section按逻辑节映射，绝对 VA全部进入 base-relocation table。

IAT只消费 `TargetDescriptor` 和构建元数据登记的 DLL/symbol并稳定排序；export table只含显式 C export。`staticlib` 写确定性 COFF archive，`cdylib` 写 PE DLL；不需要 `.lib`导入库作为最终写出的中间步骤。

## 直接编码与验证

encoder 对每个 `X64Inst` 先计算 exact 长度，再写 prefix、REX、opcode、ModRM、SIB、displacement和 immediate；两次计算必须一致。每条 instruction记录起止 offset，stack map、branch、relocation和 source table只引用该边界。

写镜像前必须：

- 解码或结构复核所有 emitted instruction，确认长度和 fixed register constraint；
- 验证所有 branch/relocation range、symbol resolution和 section权限；
- 验证 internal/C ABI 参数、callee-save、stack alignment和返回分类；
- 验证 frame、stack map、unwind和 landing pad逐函数一致；
- 验证runtime tuning profile digest、所选`LocalDequeMode`和layout query完全一致，release镜像中不存在未选mode或热路径mode branch；
- 验证BatchInbox每条publish路径的link store先于Release CAS、staging clear晚于CAS，consumer link load晚于Acquire exchange；Classic64/Packed55只使用各自规定的fence/CAS序列，queue common path不调用generic epoch、allocator或control mutex；
- 验证 `ScopedViewBegin/End` token在所有正常、panic和foreign-return边上成对闭合，`ScopedRead` projection没有写入或逃逸，view中的 safepoint 不跨越其 token end；
- 验证 ELF/PE header、segment/section、import/export、TLS、relocation和入口范围；
- 确认 writable section不可执行、stack不可执行、metadata只读且 strip保留；
- 对相同 image plan重放编码并比较 bytes，禁止时间戳、随机 GUID和目录顺序进入输出。

### 指令表与编码契约

`x86_64-v1` 的编码器只认自己那张封闭指令表，不接收 opcode 文本、不调用系统 assembler。表项按 `(助记符, 操作数形状, 第一操作数访问语义)` 唯一确定，操作数按**编码位置**排列（ModRM 的 r/m 在前、ModRM.reg 在后，立即数/`rel32` 最后），同形状反方向的两个 form（如 `mov r/m64, r64` 的 `0x89` 与 `mov r64, r/m64` 的 `0x8B`）只能靠访问语义区分。表维护以下不变量，且由 `encoder_contract` 与表审计测试共同强制：

- 每个 form 声明 `lock_allowed`、`OperandSize66`/`RepF3`/`RepF2` 前缀、REX.W、固定 ModRM 与立即数字段宽度；未登记的形式在 verifier 阶段被拒绝，不会退化到 MMX 或更宽的扩展指令；
- SSE2 整型向量的形式必须带 `66` 前缀——缺前缀会编码成 MMX 寄存器的形式，只在执行期暴露；
- 固定 ModRM 只出现在无操作数形式上（`0F AE /5`、`/6`、`/7` 的 `lfence`/`mfence`/`sfence`），有操作数时 ModRM 由操作数派生；
- 超出 baseline 的形式（`pshufb`、`pmulld`、`pextrd`、`pinsrd`、VEX `vaddps`）登记在表内并携带特性标记，只在 descriptor 允许该特性时才可选；`x86_64-v1` 下它们计入 `beyond_baseline_forms` 而不被选中；
- `lock` 只允许出现在声明了 `lock_allowed` 的形式上，且必须落在内存目标；前缀顺序固定为 `lock`、段/操作数前缀、REX、opcode。

编码契约（`ENCODER_SCHEMA = 2`、`ENCODER_REVISION = 2`、`LOWERING_REVISION = 3`、`ALLOCATION_REVISION = 1`）把表、形状不变量与 lowering/分配规则版本一起冻结：`EncoderContract::verify` 校验每个 form 的形状、助记符、特性名与 baseline 归属，`fingerprint` 取域 `gugu-x64-encoder-v1` 下对规范化字节的哈希（域 `gugu-x64-encoder-catalog-v1` 单独标记目录），任何表改动都会改变指纹并进入 action key，分配规则 revision 与 lowering revision 一并进入 fragment key。`Rel8` 是独立操作数种类，覆盖 `jmp`/`jcc` 的 17 个短跳 form；`assemble` 按 form 声明的字段宽度（1 或 4 字节）回填局部标签，超出 rel8 的距离是编码错误。`SelectInstructions` 对封闭 LIR opcode 穷尽 lowering：函数级序列先按 rel32 编码，再按 code offset 只许长变短地收缩为 rel8；外部符号、冷边与 `call` 保持 rel32 reloc。内部符号文本为 `__gugu_<kind>_<64 lowercase hex>`（kind 为 `fn`、`static`、`vtable`、`glue`、`runtime`、`const` 或 `veneer`），C import/export 不加该前缀；符号 key 与 mangled 文本同源，不把已 mangled 的字符串再哈希一次。`LegalizeX86_64` 与函数级选指产出的序列连同约束、clobber、冷边、mangled 符号、**重定位引用的符号名集合**、热/冷块计数、encoded bytes 与[分配产物](#allocation-artifacts)组成 codegen fragment（`CODEGEN_SCHEMA = 4`，键域 `gugu-x64-fragment-key-v1`，世界指纹域 `gugu-x64-fragments-v1`）；fragment 校验会重算该集合，并对带 `__gugu_` 前缀的符号强制上述形状，供镜像规划做内部符号冲突检查。query key 还并入 scheduler/coroutine 契约指纹、`TypeUniverse` 指纹、`LOWERING_REVISION` 与 `ALLOCATION_REVISION`；逐站点记录指令、字节区间、重定位、约束、clobber 与点位范围，并在验证时重算站点序、字节区间、重定位范围、frame 一致性（`stats.frame_size_max == frame.frame_size`、点位段数与站点一致）与片段指纹。镜像计划与 CLI JSON 暴露 `x64-rel8-count`、`x64-hot-block-count`、`x64-cold-block-count`、`x64-entry-symbol`、`x64-frame-size-max`、`x64-spill-slot-count`、`x64-spill-bytes`、`x64-saved-gpr-count`、`x64-reload-count`、`x64-spill-store-count`、`x64-copy-move-count`、`x64-copy-cycle-count`、`x64-peak-live-gpr`、`x64-peak-live-xmm`、`x64-allocated-values`、`coroutine-stack-check-offset` 与 `scheduler-poll-flags-offset`；`x64-entry-symbol` 是 `MonoRoots` 里 `RootCategoryV1::Entry` 实例的那个片段，不能按片段顺序推断（片段按实例键升序，runtime helper 会排在入口前面）。


### Cost calibration profile {#cost-calibration-profile}

每个 `TargetDescriptor` 关联版本化 `BackendCostProfile`，至少包含 `{ baseline_digest, inline_hot_bytes, inline_cold_bytes, max_spill_slots, max_spill_bytes_per_call, max_reload_stores, code_size_budget, regression_percent, vector_lowering }`。当前两个目标共用 `baseline_cost_profile()`：`baseline_digest` 由域 `gugu-backend-cost-baseline-v1` 对 `x86_64-v1` 哈希得到，`inline_hot_bytes = 256`、`inline_cold_bytes = 128`、`code_size_budget = 4096`、`max_spill_slots = 64`、`max_spill_bytes_per_call = 512`、`max_reload_stores = 256`、`regression_percent = 5`，且 `vector_lowering = false`。`vector_lowering = false` 表示该目标尚未提供校准的向量 lowering：`LoopVectorizationAndUnrolling` 此时只做 unroll 与 scalar remainder，不得生成任何 `V128`；只有后端提供 `vector_lowering = true` 的已校准 profile 后，vectorizer 才允许生成 vector main loop。profile固定当前 `r14/r15/r11` 保留寄存器决定，不允许后端在单个函数上临时释放 runtime register或以不同寄存器集逃避成本记录；换寄存器集必须产生新的 compiler/runtime schema和独立 profile。

backend 对每个 monomorphized function 输出 `CostRecord { instruction_bytes, hot_bytes, cold_bytes, peak_live_gpr, peak_live_xmm, spill_slots, spill_bytes, reloads, safepoint_spills, call_count, inline_decisions }`。spill slot、reload/store和 code size使用最终 branch relaxation、frame layout、stack-map生成后的结果，不能用 legalization前估计代替。`inline_decisions`记录 caller/callee stable key、估算与最终 bytes、hot/cold权重和是否因 profile拒绝；跨函数 inline不得只因局部函数短而绕过 caller预算。

profile发布前必须在固定 calibration corpus 上重复生成代码：空/标量热循环、指针写入和 safepoint、长生命周期高寄存器压力、aggregate ABI、runtime glue、深调用链、向量循环、panic/unwind和真实 scheduler/GC/select bridge workload。每类至少记录 release+debuginfo 的 instruction bytes、spill/reload、frame bytes、I-cache/code-size proxy、poll load/branch和端到端吞吐/尾延迟；不得用空函数或单一 microbenchmark 代表全部 workload。结果与同一 profile 的 baseline artifact 比较，任一硬上限超出、校准 corpus 缺少 category 或回归超过 `regression_percent` 时 profile validation失败，release构建不能静默选择该 profile；只允许回到已验证的 `Classic64`/既有 profile，并把失败原因写入诊断。

`r14/r15` 的收益与寄存器压力必须分开报告：至少比较 current-coroutine/current-processor access cycles、peak live GPR、spill/reload、IPC、I-cache miss和 GC/scheduler p99。inline、vectorization、unroll和 instruction scheduling的 cost model只能消费 profile中已校准的权重；后端没有 profile数据时不宣称吞吐、延迟或 spill 改善。

### 机器成本验证

固定LIR fixture和C对照fixture覆盖整数/floating边界、聚合ABI、register压力、spill、critical edge、branch relaxation、i128、atomic、panic unwind、stack growth、GC safepoint、no-safepoint marker零编码、ELF relocation和PE unwind/import。机器码fixture还必须逐指令断言普通prologue只有一次`[r14 + stack_check]`load与共享容量/poll冷分支，budget poll只有一次`[r15 + poll_flags]`load，并且两者没有lifecycle/global epoch访问；BatchInbox每个publish batch只有producer-local active/seen store、一次queue-control load、普通link写、head `lock cmpxchg`和可选empty-transition通知，consumer只有Acquire exchange与普通link读；所选LocalDeque mode的机器序列与profile一致。真实执行smoke test分别直接启动Linux ELF和Windows目标环境中的PE；只比较反汇编文本不能证明镜像可运行。

### 编码执行验收

除逐字节 fixture 外，x86_64 后端还有一条在真实宿主 VM 上执行编码产物的验收路径：`X64Harness` 用与生产同源的 lowering、verifier 和编码器为每条 lowering 规则构造确定性用例（输入寄存器、期望寄存器/内存结果、冷边是否触发、原子与向量 lane 覆盖），`cargo bench --bench x64_encoding` 把用例片段映射为可写→可执行页在宿主上真跑，逐例比较寄存器、内存槽与冷边计数，失败以非零退出码结束。同一条路径还核对三件事：解码序列在真实控制记录上的 `decodes`/`rejections` 与 `CompressionPlane` 的 checked 解码逐个吻合；编码器生成的上下文切换片段与 runtime 的 `ContextSwitchCode::fixed()` 逐字节相同（含 restore offset）；harness 自检（用例数、契约 form 数与 baseline 之外的形式数）成立。

宿主 VM 与裸适配器只存在于 `benches/support/vm.rs`，compiler 与 runtime 仍保持 `forbid(unsafe_code)`。这条路径的作用不是替代 fixture，而是在真实机器上暴露只有执行才可见的错误：SSE2 形式缺 `66` 前缀落到 MMX、固定 ModRM 未写出、`adc`/`sbb` 重复计入上一段 CF、向量比较误用「无序为真」谓词、插入掩码移位量错、32 位归约丢位，都是先由它发现再修在归属层的。

## 参考实现资料

- [Rust 编译器开发指南：代码生成](https://rustc-dev-guide.rust-lang.org/backend/codegen.html)
- [Go x86 instruction assembler](https://go.dev/src/cmd/internal/obj/x86/asm6.go)
- [Go internal ABI](https://go.dev/src/internal/abi/abi-internal.md)
- [System V AMD64 ABI](https://gitlab.com/x86-psABIs/x86-64-ABI)
- [Microsoft x64 调用约定](https://learn.microsoft.com/en-us/cpp/build/x64/x64-calling-convention)
