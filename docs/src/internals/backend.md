# x86_64 后端

本章规定 Gugu 首版自研后端从合法 LIR 到 x86_64 machine code、内部调用 ABI、寄存器分配、frame layout、重定位和 ELF/PE 镜像写出的完整路径。后端不调用 LLVM、系统 assembler 或系统 linker；原生静态/动态输入只通过已登记的构建元数据进入 image planner。

外部 C ABI、目标名称和稳定镜像边界以[平台与 ABI 参考](../spec/platform-abi.md)为准。本章的 Gugu 内部 ABI、符号、frame 和 metadata 可以随 `CompilerIdentity` 一起改变，但同一镜像中的 compiler/runtime 必须完全一致。

## 权威边界

[类型](../spec/types.md)、[unsafe](../spec/unsafe.md)、[程序模型](../spec/program-model.md)和[平台 ABI](../spec/platform-abi.md)唯一规定整数/浮点结果、可接受的 asm、目标、C ABI、导入导出与稳定镜像面。本章只固定官方 compiler如何把合法 LIR实现为这些结果。内部 ABI、CPU指令选择、frame、mangling、relocation计划和直接写出算法随 `CompilerIdentity` 版本化，不能扩大或缩小公开接受面。

## 目标描述符与数值 lowering

`TargetDescriptor` 分别保存 `arch`、`os`、`object_format`、`pointer_width`、`rt0`、`cpu_baseline` 与 `pointer_compression`；架构、操作系统与格式的规范 `name()` 同时用于 descriptor 身份编码与 cfg 比较（cfg 只比较架构和操作系统）。内部 `X86_64Macos` 描述符绑定 Mach-O64、Darwin 平台 profile 和构建期 runtime artifact。image planner 按 `object_format` 选择 ELF64、PE32+ 或 Mach-O64 writer；公开目标注册表和宿主目标选择仍只支持 Linux 与 Windows 两个 x86_64 目标。

每个toolchain安装携带不可变`TargetDescriptor { name, object_format, page_size, cpu_baseline, linux_interpreter, sysroot_digest, import_policy_revision, runtime_tuning_profile_digest, backend_cost_profile_digest }`；目标运行时路径与宿主sysroot分离，descriptor整体进入compiler identity和action key，backend不探测宿主PATH。runtime tuning profile至少固定`LocalDequeMode`、remote/injection shard数和queue padding；backend cost profile固定寄存器保留、inline code-size与spill上限。release镜像只编入该profile选中的一种deque，不生成运行时mode分支。`runtime_tuning_profile_digest`取`RUNTIME_TUNING_PROFILE.digest()`（域`gugu-runtime-tuning-profile-v1`），`RuntimeRawContractV1::verify`交叉校验调度段与profile的容量、分片、batch、service节奏与填充字段。

CPU可接受面只读取[平台 CPU 基线](../spec/platform-abi.md#cpu-baseline)，后端 instruction verifier拒绝任何超出 descriptor的机器指令。数值 lowering只实现[类型系统](../spec/types.md)给定的整数/浮点结果；SSE2、NaN、overflow、shift和conversion选择是这些结果的机器实现，不在本章创建另一套数值规则。`I8` 与 `I16` 在寄存器中保持零扩展到 64 位。内存 `Load` 用 `movzx` 写入 32 位目的寄存器来建立这个规范形；只覆盖低 8 位或低 16 位的 `mov` 不能作为载入。

## 标准库系统依赖

`NativeLink.with_standard_library` 在形成最终 action key 前扫描已验证 LIR 的私有机器操作。NetCall 的 Windows 路径加入 ws2_32 与 IOCP 所需 kernel32 符号；ResolverSymbol 在 Linux 加入 libc.so.6 的 getaddrinfo/freeaddrinfo 和既有 pthread/TLS 符号，在 Windows 加入 Winsock 解析入口。EnvironmentSymbol 在 Linux 引入 environ(Data)、getenv/setenv/unsetenv(Function) 与 pthread/TLS，Windows 引入环境块与 ordinal 比较 API；ProcessWorker 使用既有 ForeignBridge 创建事务，Linux 子分支只发射 syscall，Windows 使用系统 DLL。缺省 Linux 解释器取目标 descriptor，用户显式解释器保持原值。保留显式导入顺序后按固定目录追加内建依赖，完全相同的项不重复添加；库或 Function/Data 种类冲突拒绝。规范编码进入 action key、backend plan、ELF PT_INTERP/DT_NEEDED 或 PE IAT。import policy revision 为 4，不探测宿主库、NSS 配置或 PATH。StartupCall 不引入 libc。

net_call 和 resolver_symbol 机器片段独立按实际机器重定位的可达引用选择。ELF、PE 和 stackmap/unwind 的 RVA 布局共用同一判定；未使用 Resolver 的数字 bind 不包含 resolver 动态依赖。网络请求布局为九个八字节参数，value 位于72、error位于80，Windows provider 扩展函数地址位于88；transport 的 connect/shutdown/getsockopt/recvfrom/sendto/WSARecvFrom/WSASendTo/WSARecv/WSASend 继续复用同一 raw request。流读写与 datagram 交给 epoll/IOCP，Windows 的 WSABUF、地址缓冲和 OVERLAPPED 保存在请求页直到 completion。全部请求为 raw 存储。Winsock SOCKET 保持64位，SOCKET_ERROR 只比较32位返回，错误读取 WSAGetLastError；IOCP/句柄操作读取 GetLastError。Linux 负 syscall 返回转换为正 errno。Windows 关闭 socket 的回调使用 C ABI，回传捕获的 WSA 错误供共享资源完成路径消费。

内部调用的 outgoing 栈实参必须在任何参数寄存器改写前保存，随后统一解析寄存器并行拷贝组。否则十参数 native_call 的最后一个取消实参可能被第一个操作码覆盖；固定回归覆盖这一情况，真实 Resolver 的 freeaddrinfo 路径也消费同一规则。

进程私有机器目录为 StartupCall(op,request)、EnvironmentSymbol(op)、ProcessCall(op,request) 与 ProcessWorker()，全部要求 runtime 注入源身份及 unsafe。ProcessRequest 为 88 字节、8 字节对齐，字段依次是 operation、八个 arguments、value、error；SpawnPacket 为十个机器字、80 字节。process_schema=1 将操作目录和 Child/pipe 控制记录纳入 RuntimeRaw=47 的指纹与恢复验证；冻结 HIR/具体 GIR 对照 repr(C) 与 offset_of 逐字段验证。Child/pipe 扩展从资源控制页 +200 开始，不覆盖资源 header、关闭队列和 registry。LIR world/body 为 19/20，x64 lowering 为 21；旧缓存不回退。

Rt0Payload revision=5、144 字节；Linux 独立寄存器保存初始 argc/argv，Windows 获取 GetCommandLineW 原始地址，分别写偏移 128/136，不重定位。Gugu process_context 在 boot/gc_boot 之后、main 之前复制 raw argv/cwd，创建专用进程锁，退出时在 mapping.verify_drained 前归还。三个镜像布局消费者统一用 fs_native.required 按重定位可达性选择网络、环境与进程片段，不能把可选进程或 Winsock 放进启动地板集。

原生导入明确分 Function/Data。ELF 为每项提供 `__gugu_got_<name>` 槽符号，Data 的 dynsym 为 STT_OBJECT、重定位为 GLOB_DAT，不生成 PLT；environ 操作加载槽中的变量地址，再由 Gugu 读取当前 char**。导入 environ 时，rt0 使用参数块 reserved_14 定位 GOT，仅在变量为 NULL 时以启动 envp 初始化，保留 loader/构造器修改。没有此数据导入便不发射初始化代码。PE 使用 IAT 槽，普通 C 外调只接受 Function。

权限门在 Linux 读取宿主环境变量不导入 environ：process_context 在 boot_args 时把 `argv[argc]` 之后紧随的 envp 指针表记入上下文槽（偏移 56），permission 模块直接扫描该表匹配 `NAME=` 前缀。这样 std.fs 等带权限检查点的模块闭包不携带 environment 依赖，不会通过 with_implied 向每个使用它们的程序注入 glibc/pthread/interpreter 链接要求。Windows argv 无 envp 布局，permission 仍走 environment 模块的 GetEnvironmentVariable。

## 后端阶段

cage 控制记录含机器更新的解码计数，ELF 将其放在 rt0 只读收尾范围之外的保留可写区，PE 放在 `.data`；两者都不可执行。PE 的 cage 重定位直接使用数据 RVA，不叠加文本 RVA。私有 `cage_control` 访问器与解码序列共用 `CageControl` relocation，因此公开统计与机器更新消费同一份记录。栈深度读取/复制片段在入口根据当前 r14 的 stack_high 重建地址，复制尾跳 glue 域的既有 memcpy；不能误用 runtime 域同名符号。时钟由两个片段分别提供：`monotonic_nanos` 在 Linux 使用 CLOCK_BOOTTIME（计入系统挂起时间）、在 Windows 使用 GetTickCount64；`wall_nanos` 在 Linux 使用 CLOCK_REALTIME、在 Windows 读取 GetSystemTimePreciseAsFileTime 并把 FILETIME 换成 Unix epoch 纳秒（输出缓冲位于 shadow space 之上）。双目标镜像布局与登记片段重定位回归分别验证页权限、地址及尾跳定义。

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

AArch64 `invoke` 的 landing PC 必须指向 unwind 边本身的机器块，而不是目标 LIR block：边块先执行 block 参数 parallel copy，再落入目标块；直接落到目标块会让 cleanup 路径读取未拷贝的旧寄存器或栈槽。

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

双目标一致性回归固定在 `x64/abi_tests.rs` 与 `gugu-compiler/tests/integration/abi_image_consistency.rs`：前者是 SysV/Microsoft x64 的槽位黄金表（0/1/2/16/17-byte aggregate 边界、shadow space、栈对齐填充、sret 隐藏指针、寄存器耗尽回退、按位置共享槽、混合返回），后者让同一组 FFI fixture（`repr(C)`/`packed`/`transparent`/`union`/`enum` 按值穿越 extern 边界、leaf/dirty/ordinary bridge、`#[os_thread_local]`/`#[coroutine_local]`、`#[export_name]`、panic 路径）在两个目标上各生成结构完整的 ELF/PE 镜像，并按字节断言入口、导入库/符号、导出符号、PT_INTERP 与 strip 后保留的运行时字符串。外部线程接入经 `NativeLink::THREAD_SYMBOLS` 的 glibc 导入登记，缺导入时 `BackendInvariant` 拒绝镜像。

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
- TLAB/TurnRegion 的 bump 先按 payload 地址对齐：`payload = align_up(cursor + 16, max(align, granule))`、`header = payload - 16`，与 runtime 参照实现的 `allocate` 逐字节一致（高对齐 padding 落在 header 之前）。地址加法必须用置标志位的 `add`（不能用不改 flags 的 `lea`）以便用进位检测环绕，推进后再与 span limit 比较：`cmp limit, new_cursor` 之后 `jb` 表示 new_cursor 超过 limit，进入 slow path。非规范对齐或超出 imm32 的算术走 `gc_alloc_slow`，不另立经验常量；
- **payload 尺寸只取 LIR 的 bytes 操作数**：它是 `GcAlloc`/`RegionAlloc` 的第一个操作数，先搬进 scratch 寄存器（并声明其破坏），bump 的推进量与对象头的 payload 字段都写这个值。运行期算出的尺寸（变长数组的 `length * element_size`）与常量尺寸走同一寄存器通道，任何情况下都不拿 descriptor 的静态布局尺寸覆盖运行期请求；
- 普通对象不得跨 Immix block 的约束按**运行期**判断：`cmp bytes, block - header - max(align, granule) + 1` 之后 `ja` 进 slow path。静态判断对变长分配没有意义，因此不作为准入条件；
- slow path 的 ABI 固定为 `rax` = type_id、`rbx` = payload_bytes、`rcx` = align、`rdx` = flags，`rax` 返回 payload；`r14`/`r15` 保持。`flags` bit0 为 compressed，bits1..2 是 placement 编码（TurnRegion=0、LocalHeap=1、SharedHeap=2、Resource=3），与对象头位布局分开。bytes 必须**先**搬进 `rbx`，之后的立即数装载不得在 bytes 仍要读时覆盖它的原寄存器。descriptor 未登记、不适用分配 ABI 的 placement 都是编译期诊断，不放行到镜像；
- `Managed::SharedHeap` 走 stable handle allocation/resolution；`SharedAccessBegin/End`、generation check、forwarding grace 和 handle table 更新必须位于带 stack map 的 slow edge，不能污染 LocalHeap direct-pointer fast path；
- `Managed::Pinned`、large、foreign 和 resource 请求走各自 non-moving/cleanup 路径；`RuntimeRaw` 仍使用 owner-local slab/span pop/bump。所有路径的 offset、size、align、generation 和 representation tag 使用 checked arithmetic，溢出进入 `OutOfMemory` 或 `RuntimeInvariant`，不静默截断。

函数值的 `code` 指向独立 checked 适配入口。LIR `Symbol::FunctionAdapter` 记录目标实例及其是否捕获环境；world 在固定 pass 前为每个实际取址的目标生成一次适配 body。适配入口接收 sret（若有）、environment、用户参数，去掉无捕获目标不需要的 environment，再用原目标签名 `Invoke` 并传播正常结果与 unwind。它经过普通 poll、寄存器分配、stack map 与机器码管线，不在所有普通函数上增加隐藏参数，也不改变 runtime glue ABI。

allocation fast path不能读取 owner lookup、radix route、global gc epoch、SharedHeap handle table、mark mailbox 或 memory limit；这些只能在 refill/slow path处理。`TurnRegion` 和 LocalHeap TLAB 的普通 bump 同样不执行全局原子。镜像内 `gc_write_barrier` 直接返回：引用存储在调用前已经写入，这一阶段没有收集器去消费卡表。

### runtime glue 发射门禁 {#runtime-glue-emission-gate}

runtime 符号只能在 lowering 的真实发声点发射，并必须先通过 compiler 持有的 glue 目录查询。检查覆盖：

- `PlatformCall`、`RuntimeMachineCall` 以及隐含 runtime op 的专用 lowering；
- 普通 `Call`/`ForeignCall` 的 `CallTarget::Runtime`，包括 `Invoke` 与 `TailCall`；
- `GcAlloc`/`RegionAlloc` 的 slow ABI、region 生命周期入口和 safepoint slow edge。

由 lowering 内联的 `wide_div`、`format` 等目录名字不会凭名字触发检查。真正发射的名字若不在 `GLUE_DEFS`，也不在带注释的迁移缺口清单 `PENDING_GLUE_IMPL`，立即产生 `LoweringError::UnregisteredGlue`，禁止生成 relocation；后端不能把缺失符号交给 ELF/PE writer 的退出桩兜底。`PENDING_GLUE_IMPL` 只是迁移期间的显式缺口记录，不是 runtime 实现证明；对应消费者接管后必须从清单移除并登记真实 `Item` 或 `Fragment`。

`SafepointPoll` 的 cold edge 只调用登记的 `safepoint_slow` fragment：它在 `r15 + poll_flags_offset` 上保留 `GC_STOP` 位、清除已消费的 `PREEMPT` 位并 `ret`，不分配、不切换栈、不伪装成普通 Gugu 函数。`RuntimeMachineIntrinsic` 的四个定宽原子入口则直接落到既有 `Atomic` lowering；其 fetch-and/or CAS 循环在高寄存器压力点预留地址与候选值，必要的索引常量和栈地址先折叠成内存位移，不能因 scratch 不足退化为错误宽度或 runtime 调用。
`GcWriteBarrierReserved` 使用独立的 `gc_write_barrier_reserved` glue 入口；它与普通入口共享同一 `(pointer, old, new)` ABI 和 card 写入顺序，但不允许在屏障函数内重新申请额度。该入口只有 `BarrierReserve` 建立的 permit 才可达；当前镜像实现已完成有界 card 记账，shade 与 edge-summary 消费者仍由后续 mark/trace 单元接管，不能把 card 置位本身当作完整回收证明。

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

共享算法位于 `gugu-backend/src/regalloc/`：`live` 消费规范化 CFG、use/def 位图与点位事件，完成活跃定点、凸区间及循环加权；`scan` 执行带 mask、拷贝提示与重物化的确定性加权线性扫描；`slots` 按 `(size, align, root_class)` 复用不相交区间的槽；`parallel` 按稳定顺序调度物理拷贝并打断环。共享层不依赖 x64、LIR 类型或指令编码。

`RegisterFile` 提供架构局部的物理寄存器身份、寄存器类、普通/跨调用偏好池、spill size/align 与 copy scratch。操作数读写、固定读写和调用 clobber 由架构适配器规范化为事件及候选 mask；每值 `allowed` 集合表达低字节等约束。x64 保留 form 解释、权威逻辑根集与 outgoing-root 扩展、ABI/frame、spill rewrite，以及 scratch 保存序列；共享调度器只决定搬运顺序。局部身份不代替 payload 的寄存器编码，x64 通过显式转换保留既有编号、分配统计及指纹。后续 A64 适配器复用这些算法，不另建分配路径；本次提取不接入 A64 lowering 或登记目标。

### interval

block 按最终 layout 编号，每条 instruction 获得间隔为 2 的 position：`use_slot = 2p`、`def_slot = 2p + 1`，相邻 instruction 之间留出定义与使用分开的缝隙。一个**点位**（point）是分配与 stack map 共用的最小单位，携带 `(site, kind, block, instructions, mask, clobber, pointer_spill, fixed_physical, outgoing_roots)`：`kind` 取 `Normal`/`Call`/`Bridge`/`Prologue`，由 LIR op 的规范 safepoint 种类推出；`mask` 是该点位会写或会读的物理寄存器集合，加上 form 自带 clobber，Call 点位再加全部 caller-saved GPR（`rax, rcx, rdx, rbx, rsi, rdi, r8, r9, r10`）与全部 XMM，`CallReturn`、会分配的调用、`SuspendResume`、普通与 dirty `ForeignBridge`、会挂起的 `Select` 以及 `Invoke` 把 `pointer_spill` 置位；`PollResume` 同样置位，`ForeignLeaf`、`PollFreeLeaf` 不置位。调用点另在 `outgoing_roots` 记录 outgoing 区里的指针字。读也必须进 mask：ABI 返回寄存器（`rax`、`rbx`、`rdx`、`xmm0`、`xmm1`）在返回值搬运点只是被**读**，若只按写建 mask，转换或 scratch 仍可能覆写它们。

liveness 按 CFG fixed point 求得，block 参数与 edge copy 在 predecessor 末端建 use；某个值的 interval 是**单条保守区间**：`start` 取 `live_in` 首点与所有读点最小值，`end` 取 `live_out` 末点与所有写点最大值。这与"在每个 clobber 点把区间切开、边界插转换 move"的经典做法不同：块内存在 `Switch` trampoline、`GcAlloc` 快慢路这类**同一 block 内的条件跳转**，块参数还可能由多条边定义并跨 backedge 使用，边界 move 无法保证在所有路径上恰好执行一次；统一按值给一个位置、溢出值在使用处读回、定义处写回，可证明地对任意 CFG 成立，代价是牺牲了分裂带来的更细粒度着色。

spill weight 使用 `u64` 饱和累加：普通 use 1，含 fixed-register 或 Call/Bridge 点位的 use 4，紧跟在同 block 内 Call/Bridge 点之后的 use 再乘 2，loop depth `d` 乘 `min(10^d, 1_000_000)`（支配树回边识别自然循环，非可归约环退回 SCC 计数并按深度 1 处理）。定义是 `IConst`/`FConst`/`SymbolAddr`/`StackAddr` 的值为 rematerializable，weight 为 0；当它本来会被溢出时改为"无位置"：定义站点整段丢弃，每个使用处按原 lowering 重新物化一次。

### 分配规则

GPR 和 XMM 独立分配。可用 GPR 池是 `rax, rcx, rdx, rbx, rsi, rdi, r8, r9, r10, rbp, r12, r13`——`rsp`、`r14`、`r15` 与内部 ABI 保留的 scratch `r11` 不参与；XMM 池是 `xmm0..xmm15`。值跨 Call/Bridge 点位活跃时优先 `rbp, r12, r13`（callee-saved，被调者会保存），其余情况按上表顺序；`pointer_spill` 点位上，根类要求必须落栈的值（managed heap 指针、stack 指针）直接占用 spill slot，整条活跃区间不再进寄存器；`PollResume` 也置该标记，由系统栈上的 collector 回写 spill 槽。调用点另记录 outgoing 区里的指针字（值编号、根类、相对 outgoing 区起点的偏移），寄存器参数仍留给 `MorestackEntry`。

候选寄存器必须同时满足三条：不在该值覆盖范围内任一点位的 `mask` 里、当前没有别的活跃值占用、通过宽度检查（8 位操作数的值只能放在能编码 `r/m8` 的寄存器上，溢出槽不受限）。选择用确定性的候选序，不使用随机或哈希顺序。
机器点位的 use/def slot 同序递增；候选掩码及跨调用查询先二分跳过 `def_slot < interval.start` 的前缀，只遍历 `use_slot <= interval.end` 的覆盖部分，区间端点语义保持不变。
重载内存地址后，全宽 GPR 纯写输出可复用读取地址所用的临时寄存器，因为 x64 先读取源/有效地址再写目标；其他已选输出仍独占临时位置。窄写和 ReadWrite 不复用，也不能以预先重载未初始化的旧输出值占用额外 scratch。该规则使高压力下的 `lea dst,[base+index]` 即便 base 与 dst 都溢出，也能用唯一保留的 `r11` 完成，不牺牲活跃寄存器。
指令表必须真实描述旧目标的读取：二操作数 `imul` 与 `cmovcc` 的目标是 ReadWrite，前者参与乘积，后者在条件不成立时保留原值。若错标为纯 Write，溢出重写可能用源重载槽覆盖旧目标，导致乘积变成源值平方或丢失选择的默认分支；三操作数 `imul` 才是纯写目标。
仅寄存器目标的 ReadWrite 指令在 spill 后必须重载旧值，并在最后一次写入后把仍活跃的完整结果写回原槽；不能因为 `imul`/`cmovcc` 有读写语义就省略回写。可直接使用内存目标的指令仍在原槽上更新，不重复搬运。两地址 lowering 在同一站点内先写结果、再把结果作为 ReadWrite 输入继续运算时，spill rewrite 必须在每次 scratch 写入后立刻保持 spill slot 可见值同步；不得让后续指令从中途的 spill slot 重载尚未包含前序写入的旧值。最后一次写入之后仍可按活跃性省略不再需要的结果写回，但最终结果已死不能省略此前仍供站点内读取的中间写回。
逐根初始化查询直接检查当前块中该根的最后一条有效事件；没有对应事件时读取块入口的 must 位图，不为单个根复制全部槽状态。根为未初始化时仍保留动态位图检查，不能通过性能优化放宽精确根条件。
块入口初始化事实按槽顺序拼接为连续根位图，槽起点由前缀和定位；零根槽不单独分配位图。前驱交集与事件应用仍使用相同根编号和 must 语义。

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

寄存器分配阶段按同一批 pairs 重发整段拷贝（渲染结果被整体丢弃）：先按目标空闲顺序完成全部已有位置之间的搬运，再将 rematerializable 源按原 lowering 重建到最终目标。内存目标通过 `r11` 或 `xmm15` 重建并立即写入，禁止把临时重建值跨后续搬运保存，因为内存到内存搬运也会破坏 `r11`。环用 `r11`（GPR 目标）或 frame 里预留的 **16 字节 copy scratch**（XMM/内存目标）打断；剩余搬运需要 `r11` 时，环上旧值必须放入 copy scratch。同一条指令里的读取 scratch 互斥，全宽纯写输出可在源读取后复用它。scratch 只在需要时预留，不进入 stack map，且在 safepoint 前 copy 必须全部完成。逐组统计 `copy_moves` 与 `copy_cycles` 进入 payload 的 `stats`。

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

冷路径放在最前是因为 `check` 与 `cold` 两个标签是站点内编号 0/1：站点序列重写后标签记账仍必须从 0 起连续，跳板式的两段布局会让编号出现空洞。`required_frame = frame_size + max_leaf_reserve`；`max_leaf_reserve` 是本函数所有 direct `ForeignLeaf` call的声明预算最高值，checked加法溢出直接进入 `StackOverflow` fatal。大 immediate使用 `mov r11, rsp; sub r11, materialized_required_frame`；`r11`是内部 ABI保留 scratch，不承载参数或 root。candidate计算采用机器字 wrapping语义，随后固定为一次 `cmp qword ptr [r14 + stack_check_offset], r11`与一个 signed 冷分支 `jg cold`（`acquire(limit) > candidate` 时进冷路径）。官方 stack reservation处于低半 canonical address：正常 candidate与 `stack_low`都是非负 `isize`；容量计算发生地址下溢时 candidate解释为负值；`POLL_SENTINEL = isize::MAX as usize…

**`PollFreeLeaf`**：LIR 分类发生在分配之前，看不到溢出槽和 callee-saved 保存槽。`frame_size > 0` 时无论入口还有没有 `StackCheck` 站点，都按上面的形状补发栈检查，再 `sub rsp` 与保存——这些写落在 `rsp` 之下，省掉 `sub` 会写到调用者的活跃栈上，省掉比较则会减过栈底。只有 `frame_size == 0` 才完全没有 prologue。没有预留标签的叶使用函数里让出的连续标签号，前插到入口块首个序列之前。

taken edge尚未建立 callee frame。`morestack_or_poll` 只通过 `r14`把 return PC、九个整数参数寄存器和八个浮点参数寄存器写入 coroutine控制块的固定 scratch；该过程不读取或写入 candidate以下的 user stack。随后切到 worker system stack，acquire读取 processor flags：先完成 GC stop，再处理可接受的 preempt；coroutine被重新调度后仍从同一 `MorestackEntry`恢复。全部 poll动作完成后才读取最新 `stack_low`，容量仍不足时增长，最后装载已经由 GC/stack copy修正的 scratch并重新进入原 prologue。因而 poll-first次序不依赖剩余 user-stack空间，并且增长不会漏掉已经发布的 stop请求。

每个函数的 frame 都发布 `entry_required_frame = required_frame + 8`。8 是调用方已经压上、又不计入 `frame_size` 的返回地址。可复制栈的 runtime 用这个值选择初始 stack class；镜像内主协程先提交 64 KiB，不够时由首次 `MorestackEntry` 按这个需求增长，不能绕过入口检查。计算与 frame layout 相同，checked 加法，不另写经验常量。

镜像内主协程先保留低半 canonical 地址空间的逻辑栈窗口，只提交靠近高地址端的页；`morestack_or_poll` 在尚未建立目标 frame 时保存参数和返回 PC，切回 system stack 查验精确 `MorestackEntry` 栈图，再向低地址连续提交页面并更新同一个 `CoroutineSlot.stack_check`。窗口内已使用的栈地址不移动，故本路径无需复制帧或修正 `StackInterior`；其它可复制栈仍按[栈图](stack-maps.md#coroutine-stack-replication)规定处理。请求超过逻辑上限必须在提交之前进入不可捕获的栈溢出 fatal，栈图缺失、损坏或平台提交失败也不能回到用户 frame。

Linux 用不可访问的匿名映射取得虚址窗口，再对末端页调用 `mprotect` 提交；Windows 用 `NtAllocateVirtualMemory(MEM_RESERVE, PAGE_NOACCESS)` 的 in/out 基址和尺寸取得窗口，再用 `MEM_COMMIT` 提交。两边的失败结果都在镜像 runtime 提升为不可恢复的运行时故障；Windows 入口必须传栈上描述符的地址，不能把内部 ABI 的 `bytes` 机器字当作指针。

### 分配产物与验证 {#allocation-artifacts}

`AllocateRegisters` → `ResolveParallelCopies` → `LayoutFrame` 由一个 `allocate` 调用完成，顺序是：点位与 liveness、线性扫描、spill slot 复用、一次不带偏移的并行拷贝**探针**（只为判定是否需要 16 字节 copy scratch）、frame layout、带真实 scratch 偏移的最终解析、序列改写、重拼与 branch relaxation、校验。产物 `Allocated` 携带 block 级序列、函数级 `sequence` 与 `rel8_count`、`frame`、点位表 `points`、逐站点 `sites`、逐值 `values`、`stats`，以及 `abi_regions`。

改写后的序列必须通过 `verify_allocated_sequence`：所有操作数（含 `Mem` 的 base/index）都是物理寄存器或内存，出现任何 `Reg::Virtual` 都是内部错误；`r14`/`r15` 不被写；`rsp` 只在 prologue/epilogue 所在的 ABI 区间里被修改。`StackAddr` 的 frame 占位基址（`Reg::Virtual(FRAME_SLOT_BASE + slot)`）在改写时全部换算成 `[rsp + local_offset]`，改写后不允许残留占位编号。

片段 payload（`CODEGEN_SCHEMA = 5`）新增 `frame`、`points`、`values`、`stats` 四段与站点级 `points` 范围：`frame` 给出 `frame_size`/`payload_bytes`/`outgoing_bytes`/`required_frame`/`entry_required_frame`/`max_leaf_reserve`/`locals`/`spill_slots`/`scratch_offset`/`save_offsets`/`checked`；点位另带 `outgoing_roots`；`stats` 给出 `peak_live_gpr`、`peak_live_xmm`、`spill_slot_count`、`spill_bytes`、`spill_stores`、`reloads`、`rematerializations`、`copy_moves`、`copy_cycles`、`call_sites`、`safepoint_spills`、`allocated_values`、`frame_size_max`，其中 `frame_size_max` 必须等于该片段 `frame.frame_size`，`points` 段数必须等于站点 `points` 范围之和。这些计数就是 [Cost calibration profile](#cost-calibration-profile) 里 `CostRecord` 的 spill/reload 字段来源，因此必须取 branch relaxation 与 frame layout 之后的结果，不能用分配前估计代替。分配规则 revision 由 `ALLOCATION_REVISION = 2` 表达并进入 fragment key：规则变化会改变 query key，不会静默复用旧片段。站点 payload 同时记录 `call_return_pc` 与 `morestack_pc`，栈图生成规则由 `METADATA_REVISION = 1` 一并进入 fragment key。

`CODEGEN_SCHEMA = 8` 起 payload 不再携带 LIR world 指纹：输入身份只由 `CodegenFragment` query key（world 指纹、实例、body 指纹、目标、encoder、契约、来源自动 poll 策略与 revision）绑定，restore 只核对 schema、目标、实例与 encoder 指纹。同一 body 在任何 world 里产出同一份 absorb 前 payload，这是内建片段物化安装的前提。`codegen::fragments` 按 `lir.bodies()` 顺序产出全部 absorb 前 payload（LIR 已安装的 body 优先从[内建函数表](monomorphization-cache.md#builtin-function-table)解码 x64 层，其余走 `CodegenFragment`），每份都经过 `validate_fragment`；`codegen::build` 在其上完成 absorb、入口符号、metadata、类型描述符与全局区，构建期先选定录制集，再通过 `record_fragment` 复用同一 `assemble_fragment` 与 `validate_fragment`，不为丢弃实例编码或建立一次性片段 query 缓存。

宿主执行验收：`Compilation::x64_fragments()` 暴露逐片段的机器字节、重定位、frame 视图，`cargo bench --bench x64_frame` 把片段按符号顺序映射为可执行页、解析内部 relocation、用 `ret` stub 兜住运行时入口，然后在真实 CPU 上按内部 ABI 调用入口片段并核对结果。它覆盖默认测试套件覆盖不到的部分：spill 槽读写、prologue/epilogue 的 `rsp` 调整、callee-saved 保存、栈参数传递与并行拷贝破环。指针值重定位到栈地址的场景需要 runtime allocator 才能构造参数，因此不在该 bench 的范围内。

`frame_size` 必须小于等于 `u32::MAX`；更大的单函数 frame在代码生成前报 `implementation-limit`，不能依赖更高 runtime stack max截断。单函数最终 code size同样必须小于等于 `u32::MAX`，以满足 stack-map、unwind和 source record的相对 offset表示。

epilogue从固定 slot恢复 callee-saved、`add rsp, frame_size`、`ret`。prologue/epilogue只能使用 Windows unwind可描述的指令子集；Linux CFI与Windows unwind record都从同一个 `FrameLayout`生成。

`TailCall` 在 frame仍存在时完成寄存器并行 copy，随后按 epilogue规则恢复 callee-saved并释放 frame，最后 `jmp` callee；调用者原 return address保持在 `rsp`顶端。eligibility已保证没有 stack argument/sret/root/cleanup；tail target若无 checked entry，形成的 backedge必须已由 poll budget pass覆盖。

只有分配后 `frame_size == 0` 的 `PollFreeLeaf` 完全省略 prologue：无 call、无循环、无 safepoint/unwind且 legalized cost不超过64。分配后又长出帧的叶补发栈检查。取函数地址时生成带 `StackCheck` 的 checked thunk。内部 ABI不使用 SysV red zone，以保持 Linux/Windows frame和异步 signal边界一致。

## stack map、panic 与 unwind

普通`ForeignBridge` lowering把`foreign_bridge.lease_word`作为完整64-bit expected state。native返回的hot block只执行一次`lock cmpxchg qword ptr [r14 + state_offset]`，从精确`Foreign(g)`转为`Running(g)`；成功边先检查当前processor pending poll/GC再直接恢复coroutine stack，不能访问BatchInbox、injection或任何scheduler控制锁。失败边是cold block，调用runtime等待scan lock、取得idle processor或通过登记的ProducerHandle batch publish。该CAS同时线性化lifecycle与processor lease，backend不得另发`_Psyscall`式processor状态store/CAS。DirtyCpu mode在metadata中固定为detached并跳过该hot block。bridge call仍按C ABI clobber caller-saved值并在进入前物化全部pointer roots；返回快路径不削弱stack map要求。

寄存器分配后按[栈图](stack-maps.md)生成 safepoint root。`CallReturn`/suspend/普通 `ForeignBridge`/`ForeignBridge[DirtyCpu]` 点把所有用户 pointer spill；两种 bridge在 coroutine stack物化 ABI frame并以 high-relative offset登记，native段不生成 native stack map。counted inner chunk edge与 uncounted countdown-only edge都没有 map，只有实际 poll-word检查后的 resume label生成 `PollResume`；poisoned prologue复用 `MorestackEntry`。`ForeignLeaf`只有在其它 effect要求 `CallReturn` 时才建立普通调用记录。instruction offset在 branch relaxation和encoding后最终回填。回填使用站点上的 `call_return_pc`（最后一条 `call` 的下一条指令）和 `morestack_pc`（`morestack_or_poll` 返回后的第一字节）。生成的三节进入镜像计划：Linux 为 `.gugu.stackmap`、`.eh_frame`、`.gugu.src`，Windows 为 `.gugustk`、`.xdata`、`.gugusrc`。栈图表只收录至少一个安全点的函数，展开表按同一符号序收录全部函数，`unwind_index` 指向展开表序号。源码记录使用 [GC metadata](gc-metadata.md) 的 32 字节 `SourceRecord` 布局，function index 是栈图表下标。`--strip` 丢掉 `.debug*` 与 `.symtab`，上述三节必须留下。写出前用 runtime walker 消费每一条函数与安全点（整张栈图只解码一次），再逐函数校验落地链的最内层选择（`check_landings` 只读落地记录，不重解码 section）。

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

[平台 ABI](../spec/platform-abi.md#executable-image-forms)给出 Linux external image profile。static PIE路径的内部 rt0在读取待重定位 global前由 `AT_PHDR` 与链接期 program header 虚址计算 load bias：`bias = AT_PHDR - phdr_vaddr`。首个 `PT_LOAD` 的 `p_vaddr` 与 `p_offset` 都是 0，`phdr_vaddr` 等于 `e_phoff`（64）。rt0 只解释 writer 生成的 relative relocation，checked 写入 `bias + addend` 并 `mprotect` 封闭 RELRO；未知 relocation、越界 target、重复槽或入口缺失在写出前失败。

镜像固定为 `ET_DYN`、页大小 4096。无动态导入时程序头是 `PT_PHDR`、三个 `PT_LOAD`（RX 含文件头与 rt0、R 为只读逻辑节、RW 为 `.data.rel.ro`）和 `PT_GNU_RELRO`，不写 `PT_INTERP`。RX 前缀依次是 `_start`、8 个 `u64` 的参数块、`ud2` 冷陷阱、48 字节 cage 控制、`write`/`mmap` syscall stub，以及 `ret` / `exit` 兜底。用户片段与 `registered_fragments` 登记的机器片段一起按符号名排序、16 字节对齐接在前缀之后（栈图与展开表的 `code_rva` 由同一次排序得出：glue Item 实例与机器片段共用 `__gugu_runtime_*` 前缀因而交错，只排片段世界会给排在机器片段之后的函数算出偏小的偏移）；栈图里的 `code_rva` 仍相对该片段区，绝对虚址是片段区基址加 `code_rva`。只读节装入下一页，含 `.gugu.stackmap`、`.eh_frame`、`.gugu.src`、`.gugu.types`、`.gugu.meta` 和 4096 字节的处理器前缀。RW 页前 128 字节为 `0xFF` 栈界限，随后是 16 字节一组的 `(slot, addend)`；文件偏移等于虚址。`PcRel32` 与 `Rva32` 在写出前修完，`Abs64` 进入这张表。未解析的 `morestack_or_poll` 接到 `ret` stub，其余未解析的 `__gugu_*` 与未带归档的 C 符号接到 `exit` stub。编译请求目前不携带归档字节；非空 SysV ar 按未解析 C 符号名精确抽取成员，时间戳、uid、gid 为 0。

显式动态导入才写 `PT_INTERP` 与 `PT_DYNAMIC`。`CompileRequest::with_native_link` 把库/符号列表和目标工具链提供的解释器传到 writer，并用规范编码加入 action key，不搜索宿主 PATH。`.dynstr` 以 NUL 开头并收纳 SONAME 与符号名，`.dynsym` 含零项及 undefined GLOBAL/FUNC 项，SysV `.hash` 描述符号表。`.dynamic` 登记字符串、符号、hash、RELA 表地址及各自长度/项宽，依声明顺序写 `DT_NEEDED`，使用即时绑定。每个导入符号在 RX 有一条 `jmp [rip+GOT]`，GOT 初值为零；loader 通过 `R_X86_64_GLOB_DAT` 写入函数地址。这些槽不进入 rt0 自重定位表，避免覆盖 loader 的结果。动态镜像的 loader RELRO 范围为空，rt0 完成自重定位后才按既有精确范围执行 `mprotect`；运行时可写区保持可写。机器 rt0 只做重定位、`mprotect`、调用入口和 `exit`；它不解析启动环境变量，也不发布生命周期状态。`r14` 指向这 128 字节，栈检查因此放行。镜像 runtime 的 `rt0_start` 另外分配一页可写 processor 作为 `r15`：`boot` 清零队列，把 TLAB 与 TurnRegion 的 cursor/limit 指到已提交的堆，不把这两对字清成 0。只读节里的 `0xFF` 处理器前缀只作为未进入镜像 runtime 时的兜底。语言级 panic、GC 与 channel 的运行时例程由镜像内 Gugu runtime 实现。

`staticlib` 写确定性 SysV ar archive，member timestamp/uid/gid 为 0、mode固定，成员按 symbol key排序；`cdylib` 写 ET_DYN、只导出显式 C symbol并包含自有闭世界 runtime/metadata。

### 内部 AArch64 Linux ELF64

整程序入口在 A64 装配前完成 `NativeLink.with_standard_library` 的 leaf 默认导入登记。`program::assemble` 合并内建与用户导入，复用 `definitions::linux_import_definition` 为每个新增 Function 建立调用桩和有文件内容的 GOT 槽；`program::linux_image` 从同一份 `World.imports` 生成动态绑定记录，不能重新枚举内建目录而漏掉用户符号。进程内镜像测试逐项核对动态符号、GLOB_DAT 的目标槽与 DT_NEEDED，构建成功本身不代表 loader 会填写新增槽。

`a64_linux_resolver_image <output> [<interpreter> <libc>]` 将可选输入直接传入 `DynamicLink` 和所有 libc 导入；缺省仍为 musl，glibc 验证使用 `/lib/ld-linux-aarch64.so.1 libc.so.6`。外部 QEMU runner 以 `AARCH64_RUNNER_LIBC=musl/glibc` 选择 initramfs 的运行库；glibc 模式装入 Debian arm64 libc6 包的 loader、libc、NSS/files/DNS 与 libresolv，并设置 `hosts: files dns`。此手工验证不改变静态 writer 默认值，也不进入默认测试套件。

`DynamicLink`/`Import` 为同一 writer 增加显式 interpreter 和 Function/Data 的命名库及 GOT 槽；`link_dynamic`、`link_entry_dynamic` 复用原有布局、指令修补和启动数据路径。动态形式增加 PT_INTERP/PT_DYNAMIC、DT_NEEDED、dynstr/dynsym、单 bucket SysV hash 与 eager R_AARCH64_GLOB_DAT（1025）；动态表只读，导入槽在有文件内容的 RW 段。共享 RELA 表按 RELATIVE、GLOB_DAT 顺序写出，DT_RELASZ 覆盖全部记录，启动参数的 RELOC_COUNT 仅计 RELATIVE 前缀。符号名全局唯一、库依声明顺序去重，不支持 Library-only 项、符号版本、导出或 lazy PLT；interpreter/libc 来自调用方而非宿主探测。下面的无 PT_INTERP/自重定位描述指默认静态路径。

`a64::elf` 是不依赖目标注册的纯库 static-PIE writer。`ElfRequest` 给出节、入口、指令重定位和绝对指针槽；复用 `macho` 的 `Section`、`SectionContent`、`Segment` 与 `Location` 容器，但 `DataConst` 在这里从装载开始就只读，RELATIVE 槽只能位于有文件内容的 `Data`。`layout` 按节大小和请求形状计算链接期虚址（同时是文件偏移），`link` 写出确定性的 `ET_DYN`、`EM_AARCH64 = 183` 镜像与 BLAKE3 指纹。此层不接入 target registry、CLI 或闭世界 image planner。

程序头依次为 `PT_PHDR`、RX/R/RW 三个 `PT_LOAD` 与不可执行的 `PT_GNU_STACK`；不写 `PT_INTERP`。三个装载段按 64 KiB 对齐，RX 包含 ELF 和程序头，R 包含 `.rela.dyn` 与节表，RW 的零填充节排在有文件内容的节之后且不增加文件大小。`.rela.dyn` 每项为标准 ELF64 RELA（24 字节）：槽虚址、符号索引为零的 `R_AARCH64_RELATIVE = 1027`、目标链接期地址加 addend；没有重定位时仍保留零长度的表。重复槽、非可写或未对齐的指针槽、越界位置、布局溢出和不支持的指令重定位在写出前报错。

`a64::relocation` 与 Darwin 启动路径共享 decode/encode 修补器；支持 `Call26`/`Branch26`、`AdrpPage21`、`AddLo12` 与 `LdStLo12`，按指令自身的 4 KiB 页计算 ADRP 和页内偏移，与 ELF 段的 64 KiB 对齐无关。修补时检查指令形式、访存缩放、位移范围和对齐，不用掩码截断无效值。

`a64::elf::link_entry` 接收 `LinuxEntry`（`rt0_start`、可写载荷、初始槽与 processor），追加只读参数块、Linux `_start` 和共享 `a64::fragments::rt0`。参数块保存链接期值，入口由参数块运行时地址减去 `bias_baseline` 得到加载偏移，遍历 RELA 表把 `bias + addend` 写入 `bias + slot`。零条目跳过循环；内核不负责这批自重定位。入口从原始 sp 读取 argc，以 `sp + 8` 为 argv、`argv + 8 * (argc + 1)` 为 envp，保留它们直到以 x0–x4 = params、bias、envp、argc、argv 调用 rt0。rt0 初始化 x27/x28，写入载荷启动参数并调用 `rt0_start`，返回的 x0 直接交给 `exit_group`（x8 = 94，`svc #0`）。入口写入载荷 revision，并为载荷的 `initial_slot` 字段自动登记一项 RELATIVE；其他载荷指针由调用方提供。`LinuxEntry::runtime` 为 `Some(LinuxRuntime)` 时由链接器补齐其余启动契约，与 Darwin 侧同构：参数块追加写入 main `entry`、`runtime_state` 与 `coroutine_enter` 三个链接期 VA（rt0_start 运行时加加载偏移），`Rt0Payload` 的 `main_entry`/`runtime_state`/`main_return`/`stackmap`/`fragment_base`/`unwind`/`gc_types`/`gc_meta` 地址字段登记为 RELATIVE（rt0 的 RELATIVE 循环把它们补成运行时地址），对应长度字段按偏移截到节尾写出；`fragment_base` 必须是代码节偏移 0 的起点，空 GC blob 记为指针和长度皆 0。`runtime_state` 必须落在可写 Data 节且容下 `state::BYTES`；`None` 保留只含 `rt0_start` 的探针形态。`examples/a64_linux_image.rs` 在默认测试之外生成读取已重定位指针槽、经 Linux write 输出消息并返回 42 的验证镜像；单元测试只在进程内检查结构与指令。

`a64::fragments::linux_fd_dispatch(exit_thread)` 是无导入、无外部重定位的 Builder 片段，在原生栈上保留 x19/x20/x29/x30，并预留 128 字节 arm64 kernel stat 暂存区。`exit_thread = true` 的变体注册为 `fs_native_worker`（`linux_fs_start` clone 子的 `b` 入口），写出结果后以 `exit(93)` 终结线程，内核清 `CLONE_CHILD_CLEARTID` 的 tid 字完成发布；`false` 变体注册为 `fd_dispatch`，恢复寄存器后 `ret`，作 `callback_file_worker` 尾巴。它接收 x0 = `NativeRequest*`，加载 `foreign_schema` 的 operation/arguments，按 Linux x86_64 请求号翻译为 asm-generic syscall：read/write/close/lseek = 63/64/57/62，open→openat 56，stat/lstat/newfstatat→newfstatat 79，fstat→80，fcntl→25，fsync/fdatasync/ftruncate→82/83/46，rename/mkdir/rmdir/link/unlink/symlink/readlink→38/34/35/37/35/36/78，getdents64→61。*at 转换插入 -100、`AT_REMOVEDIR` 或 `AT_SYMLINK_NOFOLLOW`；传入 newfstatat 的 dirfd/flags 原样保留。open 的 flags 与 fcntl 的 `F_SETFL` 输入、`F_GETFL` 输出交换位组 14–15 和 16–17，其他 flags 保持不变。负返回值转成正 errno，原返回值保留在 VALUE；未支持请求为 EINVAL。信号请求委派给同一片段内的共享信号分派生成器；此层不实现调度与完成发布。

`linux_fs_start` 以 x64 相同标志位（VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|PARENT_SETTID|CHILD_CLEARTID）发 clone 220，parent/child tid 指针都指向 packet 首字（`thread_id` 字段即 tid 字），子线程从栈上取 packet 后 `b fs_native_worker`；`op == −1` 的 foreign 请求改为 `pthread_create` + `foreign_native_worker`，线程属性在原生栈上构造。`linux_fs_poll` 先按 `OPERATION`（+8）分流：−1 走 `pthread_tryjoin_np`，join 成功清 `THREAD_HANDLE` 并完成、EBUSY 返回 pending、其余错误 `exit_group(2)`；其余操作读 packet 首字 tid：非零 pending、清零完成。operation 与 tid 字是两个字段，不能用首字同时判断两者。

stat 成功后把暂存区复制为 x86_64 kernel stat：mode/nlink/uid/gid/rdev 调整偏移，32 位 nlink 零扩展、blksize 符号扩展，dev/ino/size/blocks/时间戳保持偏移，填充和保留字段清零。Linux `fs_platform::linux_stat` 从 +88/+96 读取修改时间秒/纳秒。解码往返与确定性 syscall 替身执行测试覆盖请求翻译、flags、完整 stat、负返回、零字节读取/目录 EOF 和不支持请求；不进行真实 I/O。`examples/a64_linux_fd_image.rs` 在默认测试之外生成经 worker 以 `O_EXCL` 创建 /tmp 文件（已存在则失败，不覆盖或删除已有文件）并写/关/stat/重开/读、目录 getdents、unlink 和 errno 检查的镜像；所有检查成功返回 0，失败返回 1。

`a64::fragments::linux_process_call/worker` 使用 `process_schema` 的 ProcessRequest/SpawnPacket，独立于 NativeRequest。call 校验 x0 与请求 operation 相同并在原生栈保存非易失寄存器，try_wait/kill/close/read/write 翻译为 wait4 260、kill 129、close 57、read 63、write 64；观察使用 WNOHANG 并将 wait status 归一为 runtime 的 kind/code。worker 地址由 ADR 返回，同片段 worker 执行 pipe2 59、fcntl 25、openat 56 和 spawn。parent 端 F_SETFL(O_NONBLOCK)，F_DUPFD_CLOEXEC 的最小 fd 为 3；管道 flags 配置失败同时关闭两端并保留原 errno。

spawn 的错误管道和 stdio 临时副本均为 CLOEXEC；用 clone 220 的 arm64 参数顺序 flags/newsp/parent_tid/tls/child_tid（17/0/0/0/0）模拟 fork，不使用共享 VM 的 vfork。`linux_process_spawn` 的子分支只编码 syscall：chdir 49、fcntl 25、dup3 24、close 57、rt_sigaction 134、execve 221，失败在错误管道写 4 字节 errno 并用 exit 93 退出 127。低错误管道写端先以 F_DUPFD_CLOEXEC 提升到 3 以上；三个 stdio 来源全部复制到安全临时端后再 dup3，原始 0/1/2 不被 cleanup 误关。parent 重试 EINTR，EOF 后才发布 SpawnPacket 的 handle/id 与请求 value；setup/exec 错误先 wait4 回收，错误管道 read 自身失败则 kill 后回收，clone 失败关闭错误管道。子分支不调用 Builder 之外的运行时服务、libc 或分配器。

pipe_write 以 8 字节 kernel sigset 和 rt_sigprocmask 135 保存/恢复线程 mask，用 rt_sigpending 136 观察旧 SIGPIPE；EPIPE 时仅在无旧 pending 的情况下以零超时 rt_sigtimedwait 137 消费新信号，EINTR 可重试。这里的 4096 是 SIGPIPE 的位掩码，不是页面大小。`examples/a64_linux_process_image.rs` 为默认测试之外的 BusyBox echo/pipe/read/wait/ENOENT 验证；nextest 仅使用解码后的指令解释器和 syscall 脚本，覆盖 parent/child、cleanup、状态解码和 SIGPIPE，不启动子进程。片段仍未接通 foreign bridge、native worker 线程启动与公开目标。

`a64::fragments::linux_net_dispatch` 接收 x0 操作号和 x1 网络 packet，以 Builder 生成无导入的系统调用分派：socket/setsockopt/bind/listen/accept4/getsockname/getpeername/close = 198/208/200/201/242/204/205/57，epoll_create1/ctl/pwait = 20/21/22，connect/shutdown/getsockopt/recvfrom/sendto = 203/210/209/207/206。value/error 仍在 packet 的 +72/+80；负内核返回转换成 value = 0 与正 errno。操作 19 以 ADR 返回同一片段中的原生 close 回调地址，成功返回 0、失败返回正 errno。

epoll_ctl 将调用方的打包 12 字节事件展开到原生栈，DEL 不解引用 event；epoll_wait 在 2048 字节暂存区接收最多 128 个 arm64 事件，成功后逐项压缩为 x86_64 12 字节格式，保留完整 64 位 data。零事件或失败不写输出缓冲。maxevents 按有符号 int 解释，非正返回 EINVAL，正数批量上限为 128；epoll_pwait 的 sigmask = 0、sigsetsize = 8。片段不使用动态栈分配或 mmap，不在原生栈上按不可信 maxevents 分配内存。

`linux_signal_dispatch` 和 `linux_fd_dispatch` 共享同一信号分派生成器；前者单独处理信号 NativeRequest，后者也路由现有运行时的 14/289。13/14/289 分别调用 134/135/74；39/62/186/200/234 分别调用 172/129/178/130/131。rt_sigaction 将输入复制到 32 字节栈缓冲，清除 SA_RESTORER 位并清零 restorer；成功时才复制旧 action。文件/信号保留原始负 VALUE 并写正 ERROR，与网络 packet 的错误归零约定不同。`linux_syscall` 共享原生栈保存、比较分支、svc 和取负辅助生成器。`signal.gg::arm` 修正 signalfd4 的参数：原先把 524288 放到 sigsetsize、flags 留零；现为 size = 8、flags = 524288，避免 x64 与 arm64 内核返回 EINVAL。

Linux DNS 的 `net_resolver.gg` 经 `resolver_symbol` 获得 getaddrinfo/freeaddrinfo，再使用 foreign packet 调用 libc；Linux addrinfo 的 sockaddr 在 +24、next 在 +40。取消不提前释放仍被原生调用读取的 host/hints/result；迟到结果仍须 freeaddrinfo。内部 `linux_resolver_imports(library)` 声明两个 C 名字，`linux_resolver_symbol` 将 x0 的 0/1 映射到 `linux_import_stub_symbol` 命名的导入桩，越界陷阱；`linux_import_stub` 以 ADRP/ADD 指向 `linux_import_slot_symbol(name)` 的 GOT 槽，LDR 后 BR x16。它与 `foreign_native_worker(Platform::Linux)` 共用同一套桩符号；worker 的 gettid、__errno_location 也以同样方式定义桩与槽。`examples/a64_linux_resolver_image.rs` 通过真实 A64 packet worker 调用 musl，验证 localhost、/etc/hosts 的 resolver.test、成功结果的 freeaddrinfo 及 AI_NUMERICHOST 错误的空结果，不进入 nextest。完整 worker 线程启动、调度和目标登记仍未接通；没有另一套 UDP DNS，也未改动取消/迟到结果所有权。

`examples/a64_linux_net_signal_image.rs` 手工生成通过适配器在 loopback 上 bind/listen/connect/accept、sendto/epoll/recvfrom，以及 block SIGUSR1、kill 自身、signalfd 读取的镜像；另外通过 sigaction 忽略 SIGUSR2、自发信号并恢复旧 action，同时检查网络 EINVAL/EBADF。默认 nextest 只运行解码/编码往返和确定性 syscall 替身，覆盖布局、完整 data、成功/零事件/错误、信号 action 转换和 signalfd 的运行时参数回归，不运行真实网络或子进程。

`a64::fragments::linux_vm(PlatformOp)` 为原生 C ABI 的独立范围片段工厂，不是 NativeRequest worker。reserve 的 x0/x1 对应 x64 reserve_aligned 的 bytes/alignment；其他操作以 x0 接收 NativeExtent 的 base/+8 bytes。使用 mmap 222、munmap 215、mprotect 226、madvise 233，保留负 errno。reserve 检查 bytes/align、二次幂和总长溢出，以 PROT_NONE 和 MAP_PRIVATE|MAP_ANONYMOUS 过量预留，按位对齐并分别 munmap 首尾。任一 trim 失败释放仍拥有的映射范围，返回原错误；成功解除前缀后先推进 owned base/length，再尝试尾部，避免错误清理解除其他线程在已释放前缀新建的映射。Commit/Unprotect 为保护位 3，Guard 为 0，Decommit 为 MADV_DONTNEED 4；未使用 MAP_FIXED 覆盖其他映射。

`linux_page_size()` 用 x0 的 Linux envp 扫描环境终止项与 auxv，返回 AT_PAGESZ，不调用 libc/getauxval，不依赖宿主构建机器页大小，缺少时返回 0。该片段以 `page_size` machine intrinsic 登记（aarch64-linux 专属），`rt0_start` 在第一次 `reserve_aligned` 之前用 `Rt0Payload.boot_environment` 清标记位后的 envp 调用；返回 0 或非 4/16/64 KiB 值 fatal，合法值经 `adopt_native_page_size`/`adopt_large_mapping_page_size` 写入两个 `pub static`：`native_memory.gg` 的 `NATIVE_PAGE_BYTES` 与 `heap.gg` 的 `LARGE_MAPPING_PAGE_BYTES`。aarch64-macos 上二者仍为 16384 const、其余已登记路径仍为 4096 const。runtime 内所有页对齐的 `reserve_aligned` 对齐实参统一改经 `native_memory.os_page_bytes()`（const 目标返回常量、a64-linux 返回探测值），rt0 自身的 startup/栈/a64 控制块/processor 页预约用同一探测值作 alignment。`aarch64-linux` 已登记；尚未在 16/64 KiB 内核真机上验证完整 GC/allocator。

进程内指令/syscall 脚本覆盖 4/16/64 KiB auxv、缺失值、guard/commit/decommit/release 的原始参数与错误、reserve 的 prefix/tail、空输入/溢出和清理错误；不做真实 I/O。`examples/a64_linux_vm_image.rs` 在默认测试之外从真实 auxv 取得页大小，预留一页、提交、写入、guard/unprotect、验证内容、MADV_DONTNEED 后验证零填充并 munmap，同时检查不对齐 munmap 的 EINVAL。此层不需要 FP、原子或 foreign-call worker。

### Windows PE32+

PE writer消费[平台 ABI](../spec/platform-abi.md#executable-image-forms)给出的 PE32+、ASLR/NX、入口、逻辑节和导入导出要求。当前私有 writer profile使用 image base `0x0000000140000000`、section alignment 4096、file alignment 512、COFF timestamp/checksum 0和 `WINDOWS_CUI` subsystem；这些字段不扩大平台稳定面。`DllCharacteristics` 固定带 dynamic base、high-entropy VA 和 NX。逻辑节按登记名各写一节，名字长于 8 字节则写出失败。用户片段仍与 `registered_fragments` 登记的机器片段一起按符号名排序、16 字节对齐接在 rt0 之后（栈图 `code_rva` 与 `.pdata` 都取自这次排序，两者必须与放置同源）；栈图 `code_rva` 相对该片段区，`.pdata` 里的 `RUNTIME_FUNCTION` 再加上片段区 RVA。`.xdata` 保留完整 `GUGUUN01` 容器，其平台尾里的 `UNWIND_INFO` 偏移改写成镜像 RVA。

绝对地址写成 preferred VA 并进入 `IMAGE_REL_BASED_DIR64`；`PcRel32` 与 `Rva32` 在写出前修完，不进重定位表。重复槽、未知种类、超出 i32 的 PC 相对、可写可执行节和缺失入口在写出前失败。实际节表省略零长度节，不能把 `VirtualAddress == SizeOfImage` 的空尾节交给 Windows loader。导入表默认包含 runtime 所需的 `kernel32.dll` 与 `ntdll.dll` API，不搜索宿主、不隐式写入 CRT。显式 `NativeLink` 按声明顺序追加 DLL/符号，DLL 名大小写无关地合并，同一符号跨库登记报错。普通 C 调用和取址使用 `jmp [rip+IAT]` 桩，机器 runtime 的 `__gugu_iat_*` 引用仍直接指向槽。IAT 初值是两字节对齐的 hint/name RVA，数据目录只覆盖 IAT 槽，不包含后续 runtime 状态区。rt0 用 RIP 相对地址取得可写初始槽和启动载荷，捕获原生环境与命令行，再调用 Gugu `rt0_start`，按实际返回状态调用 `ExitProcess`。载荷内所有代码地址都包含 `.text RVA`，初始化仍在宿主栈上时不得激活 main 栈的 stack-check limit；首次切栈前最后发布。Win64 报告长度保存在既有栈槽中跨过 `GetStdHandle`，不依赖 volatile `r11`。未解析入口按登记的 runtime glue 门禁拒绝，不写退出桩或 `ret`。export directory 只含显式 C export；`staticlib` 使用确定性 COFF archive（时间戳、uid、gid 为 0），`cdylib` 置 `IMAGE_FILE_DLL`；不生成 `.lib` 导入库。

C 导出入口的 `.pdata` 在常量符号 RVA 发布后追加，`UNWIND_INFO` 指向实际 `.rdata` 常量；不得在常量尚未进入符号表时查找导出展开信息。追加仅改变后置节，不移动此前的文本与常量 RVA。

### macOS Mach-O 64

`gugu-backend::macho` 是不依赖目标注册的纯库 writer：输入架构（`Arm64` 页大小 16 KiB、`X86_64` 页大小 4 KiB）、节列表、dylib 导入、指针槽 fixup、本地符号与入口位置，输出确定性的已签名镜像。段固定为 `__PAGEZERO`（4 GiB，镜像基址 `0x1_0000_0000`）、`__TEXT`（映射文件头与 load command，只放代码与只读数据）、`__DATA_CONST`（加载后只读，`SG_READ_ONLY`）、`__DATA`（零填充节只能在此段并排在有文件内容的节之后）和 `__LINKEDIT`；没有节的数据段不写出。段的虚址与文件偏移按架构页对齐并首尾相接，节按请求顺序与对齐排列，节序号从 1 起按段顺序编号。

load command 依次为各段、`LC_DYLD_CHAINED_FIXUPS`、`LC_DYLD_EXPORTS_TRIE`（空 trie）、`LC_SYMTAB`、`LC_DYSYMTAB`（只有本地符号）、`LC_LOAD_DYLINKER`、`LC_UUID`、`LC_BUILD_VERSION`（macOS 12.0）、`LC_MAIN`、每个 dylib 一条 `LC_LOAD_DYLIB` 和最后的 `LC_CODE_SIGNATURE`。导入使用 chained fixups：指针格式 `DYLD_CHAINED_PTR_64_OFFSET`、导入格式 `DYLD_CHAINED_IMPORT`，每个含 fixup 的段按页写链起点，链内 stride 为 4 字节且不跨页；rebase 槽保存目标相对镜像基址的 36 位偏移，bind 槽保存导入序号与 8 位 addend。dylib 序号 8 位且 `0xF0..=0xFF` 是 dyld 的负数特殊序号，因此镜像最多链接 239 个 dylib。fixup 槽必须是 `__DATA_CONST`/`__DATA` 内 8 字节对齐的有文件内容槽，重复槽、越界目标与缺失导入在写出前失败。`LC_UUID` 取签名前字节 BLAKE3 摘要的前 16 字节。

签名是 linker-signed ad-hoc SuperBlob，只含一个 CodeDirectory（版本 `0x20400`、`CS_ADHOC | CS_LINKER_SIGNED`、SHA-256、无 special slot），`execSeg` 指向 `__TEXT` 并标记 main binary；CodeDirectory 的 pageShift 取镜像架构页大小（Arm64 = 14，即 16 KiB；X86_64 = 12，即 4 KiB），页哈希覆盖 `[0, 签名偏移)` 且按该页大小分块，因此签名最后写入。

`macho::append_native_imports` 将后端的 `NativeImport` 转换为两级导入及 `__DATA_CONST,__got` 中的 bind 槽，保留声明顺序；纯库依赖仅加入 dylib 列表。返回的槽位置和导入种类不依赖架构，供后端生成调用入口或引用数据。x86_64 在节大小及 dylib 列表固定后为 Function 写出 `jmp [rip+GOT]`，不为 Data 创建代码入口。

内部 x86_64 链接器根据闭世界片段的重定位登记 libSystem 调用，布局确定后修补代码位移和元数据地址。LC_MAIN 启动片段保存启动参数，由参数块地址推导加载偏移并调用 `rt0_start`，正常返回交给 libSystem 的 `exit`。Darwin 文件和进程 worker 通过 pthread 及 C ABI 运行，套接字等待使用 kqueue。该路径不提供 Mach-O C 导出、strip 或 arm64 机器入口。

`a64::fragments::darwin_fd_dispatch` 是独立的文件请求片段；x0 指向现有 `NativeRequest`，操作 0–10 为描述符请求，11–21 为目录与路径请求，编号与 Darwin x86_64 一致。入口使用 16 字节对齐的原生栈，保存请求指针和返回地址，用 `darwin_fd_imports` 列出的 libSystem 调用桩处理请求；C 的 32 位整数返回值先扩展为 64 位，失败立即通过 `__error` 读取 errno。`readdir` 先清零线程 errno，并在返回空指针时也把零写入 value，使调用方能区分目录末尾与失败。调用方预清零请求错误字段并负责目录项解析和路径缓冲区管理；未知操作返回 EINVAL。该片段不接入 pthread worker、托管调用或目标注册，也不处理 16 KiB 原生页上的 VM 操作。

`a64::macho::append_import_stubs` 为每个 Function 导入在 `__TEXT,__stubs` 追加一条 `adrp x16` / `ldr x16, [x16, #pageoff]` / `br x16` 序列。page 和 pageoff 在镜像布局后按指令的 4 KiB 页解析，不使用 Mach-O 段的 16 KiB 页。调用方须先固定其余布局输入，然后使用返回的符号位置修补调用指令，不能再改变节的大小或对齐。内部符号名为 `__gugu_darwin_c_<C 声明名>`（例如 `__gugu_darwin_c_exit`、`__gugu_darwin_c_write`），不加 dyld 导入使用的前导下划线；`a64::macho::import_stub_symbol` 生成该名字。调用桩只占用 x16，不改变返回地址；调用方仍负责 AAPCS64 参数和系统栈切换，arm64 foreign/extern lowering 尚未接入。

`a64::macho::link_entry` 在调用方提供的 arm64 镜像节上添加 Darwin `LC_MAIN`、参数块和共享的 `a64::fragments::rt0`。入口通过 `ADRP/ADD` 定位 `Rt0Params`，减去块中记录的镜像相对地址得到加载基址，并将参数块、基址、`envp`、`argc`、`argv` 放入 x0–x4。共享 rt0 初始化 x27/x28、保存启动参数到可写 `Rt0Payload`，然后调用 `rt0_start`。入口的页、页内偏移和 `BL` 重定位在布局后、签名前解析；退出经 `__gugu_darwin_c_exit`。链接器复用已有的 libSystem `_exit` 导入及其无偏移绑定槽。在添加入口前记录 `append_import_stubs` 已生成的每个调用桩所指的绑定槽，再按最终布局重新编码这些桩。调用方提供 runtime 载荷、初始槽、processor 和 `rt0_start` 位置；此路径不接入闭世界编译或 4d 的调用方 lowering。`DarwinEntry::runtime` 为 `Some(DarwinRuntime)` 时由链接器补齐其余启动契约：参数块追加写入 main `entry`、可写 `runtime_state` 与 `coroutine_enter` 三项 RVA，`Rt0Payload` 写入 `main_entry`、`runtime_state`、`main_return`、`stackmap`/`unwind`/`gc_types`/`gc_meta`（长度按偏移截到节尾）与 `fragment_base`（代码节起点而非 image base）；`RELOC_TABLE`、`PROTECT_*` 保持 0，重基址交给 dyld chained fixups，空 GC blob 记为指针和长度皆 0。`runtime_state` 必须落在 `__DATA` 节且容下 `state::BYTES`，入口与哨兵须为代码节内 4 字节对齐且不越界的地址，`fragment_base` 须为代码节偏移 0；`None` 保留只含 `rt0_start` 的探针形态。

`a64::fragments::darwin_net_dispatch` 在原生栈上用 x0 接收操作号，x1 接收 `net_platform.gg` 的请求地址。请求参数从偏移 0 开始，value 与 error 分别位于 72 和 80，入口清零两者并返回 value。操作 0–10、21–29 调用 `darwin_net_imports` 声明的 libSystem 入口；操作 19 给出同一片段内按 C ABI 调用的关闭回调地址，操作 30 给出只执行 `ret` 的原生信号处理器地址。操作 29 的 `sigaction` 允许调用方传入新 disposition、接收旧 disposition 或恢复旧 disposition；信号过滤器由操作 9 的 `kevent` 管理。其他操作返回 EINVAL。网络调度、取消、套接字状态及信号订阅状态仍由 runtime 管理；DNS、托管调用、栈切换和目标注册不接入此片段。

`a64::fragments::darwin_process_call` 在原生栈上接收 x0 的操作号和 x1 的 `ProcessRequest` 地址，处理操作 0–4（非阻塞观察、终止、关闭、管道读写）。`darwin_process_worker` 首入口返回同一片段内的 C ABI worker 地址；worker 接收 x0 的 `ProcessRequest` 地址，从请求读取操作 0–3（spawn、pipe、open null、duplicate）。两者用 `darwin_process_imports` 列出的 libSystem 调用桩，并在请求中写回 value 与 error。创建进程时用 `SpawnPacket` 配置标准流和工作目录，写回 PID；文件动作初始化成功后总会销毁，管道配置失败时先保存 errno 再关闭两端。片段不负责 worker 调度、完成发布、托管栈切换或信号订阅。

`a64::fragments::darwin_resolver_symbol` 独立于套接字操作分派。它用 x0 接收 `ResolverSymbol` 操作号，通过 `ADRP/ADD` 重定位到 `darwin_resolver_imports` 声明的 libSystem 调用桩，并在 x0 返回可按 C ABI 调用的地址。无效操作号触发陷阱。片段不执行解析、不捕获 errno，也不接入 worker、托管调用、栈切换或目标注册。

`a64::fragments::darwin_vm(PlatformOp)` 为原生 C ABI 的独立范围片段工厂，契约与 `linux_vm` 相同：reserve 的 x0/x1 为 bytes/alignment，其他操作以 x0 接收 NativeExtent 的 base/+8 bytes，失败返回 -errno。全部经 `darwin_vm_imports` 声明的 libSystem 调用桩，int 结果经 `__error` 翻译 errno。reserve 与 linux_vm 同形：检查 bytes/align、二次幂和总长溢出，`mmap(0, bytes+align, PROT_NONE, MAP_ANON|MAP_PRIVATE, -1, 0)` 过量预留后按位对齐分别 `munmap` 首尾，任一 trim 失败释放仍拥有的范围并返回原错误。Commit/Unprotect 为 `mprotect(base, bytes, PROT_READ|PROT_WRITE)`，ProtectGuard 为 `mprotect(…, PROT_NONE)`，Release 为 `munmap`。Decommit 不复用 madvise：Darwin 的 `MADV_DONTNEED` 只标记页可回收、不保证再次访问读到零页，故改为 `mmap(base, bytes, PROT_NONE, MAP_ANON|MAP_PRIVATE|MAP_FIXED, -1, 0)` 以新映射覆盖区间，与 x86_64 Darwin 的 decommit 一致。`darwin_page_size()` 调用 `getpagesize` 并将 int 结果零扩展到 x0，不硬编码宿主页大小。

`examples/a64_darwin_vm_image.rs` 在默认测试之外构建带 LC_MAIN 的 arm64 Mach-O，经共享 rt0 进入，调用 `getpagesize` 打印真实页大小，预留一页、提交、写入、guard/unprotect、decommit 后重新提交验证零页、munmap，并检查不对齐 munmap 的 EINVAL。进程内脚本以 libSystem 替身覆盖各操作的原始参数与 errno 翻译、reserve 的 prefix/tail 清理和非法输入，不做真实 I/O。

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

lowering 的指令形式索引只包含固定 `FORMS`，查询键由编译器内置序列生成，采用已有
foldhash 固定种子状态，避免每次 emit 使用 SipHash。索引保留表中首个匹配 FormId，
不遍历哈希表生成代码；规范表序、指令字节、契约指纹和用户 asm 的验证规则保持不变。

编码契约（`ENCODER_SCHEMA = 2`、`ENCODER_REVISION = 2`、`LOWERING_REVISION = 4`、`ALLOCATION_REVISION = 2`、`METADATA_REVISION = 1`）把表、形状不变量与 lowering/分配规则版本一起冻结：`EncoderContract::verify` 校验每个 form 的形状、助记符、特性名与 baseline 归属，`fingerprint` 取域 `gugu-x64-encoder-v1` 下对规范化字节的哈希（域 `gugu-x64-encoder-catalog-v1` 单独标记目录），任何表改动都会改变指纹并进入 action key，分配规则 revision 与 lowering revision 一并进入 fragment key。`Rel8` 是独立操作数种类，覆盖 `jmp`/`jcc` 的 17 个短跳 form；`assemble` 按 form 声明的字段宽度（1 或 4 字节）回填局部标签，超出 rel8 的距离是编码错误。`SelectInstructions` 对封闭 LIR opcode 穷尽 lowering：函数级序列先按 rel32 编码，再按 code offset 只许长变短地收缩为 rel8；外部符号、冷边与 `call` 保持 rel32 reloc。内部符号文本为 `__gugu_<kind>_<64 lowercase hex>`（kind 为 `fn`、`static`、`vtable`、`glue`、`runtime`、`const` 或 `veneer`），C import/export 不加该前缀；符号 key 与 mangled 文本同源，不把已 mangled 的字符串再哈希一次。`LegalizeX86_64` 与函数级选指产出的序列连同约束、clobber、冷边、mangled 符号、**重定位引用的符号名集合**、热/冷块计数、encoded bytes 与[分配产物](#allocation-artifacts)组成 codegen fragment（`CODEGEN_SCHEMA = 5`，键域 `gugu-x64-fragment-key-v1`，世界指纹域 `gugu-x64-fragments-v1`，元数据指纹域 `gugu-x64-metadata-v1`）；fragment 校验会重算该集合，并对带 `__gugu_` 前缀的符号强制上述形状，供镜像规划做内部符号冲突检查。query key 还并入 scheduler/coroutine 契约指纹、`TypeUniverse` 指纹、`LOWERING_REVISION`、`ALLOCATION_REVISION` 与 `METADATA_REVISION`；逐站点记录指令、字节区间、重定位、约束、clobber 与点位范围，并在验证时重算站点序、字节区间、重定位范围、frame 一致性（`stats.frame_size_max == frame.frame_size`、点位段数与站点一致）与片段指纹。镜像计划与 CLI JSON 暴露 `x64-rel8-count`、`x64-hot-block-count`、`x64-cold-block-count`、`x64-entry-symbol`、`x64-frame-size-max`、`x64-spill-slot-count`、`x64-spill-bytes`、`x64-saved-gpr-count`、`x64-reload-count`、`x64-spill-store-count`、`x64-copy-move-count`、`x64-copy-cycle-count`、`x64-peak-live-gpr`、`x64-peak-live-xmm`、`x64-allocated-values`、`coroutine-stack-check-offset` 与 `scheduler-poll-flags-offset`；`x64-entry-symbol` 是 `MonoRoots` 里 `RootCategoryV1::Entry` 实例的那个片段，不能按片段顺序推断（片段按实例键升序，runtime helper 会排在入口前面）。


### A64 独立编码库

`gugu-backend::a64` 尚未接入任何目标或 CLI，不能据此选择
`aarch64` 目标。封闭表目前包含 307 个 ARMv8.0-A base 与 FP/AdvSIMD 标量形式；不包含
LSE 或 V128 整数向量指令。`FormId` 是固定表序下标，操作数采用汇编顺序；W/X 与
B/H/S/D/Q 是寄存器视图，SP 与 XZR 是不同寄存器种类，编码号 31 的解释由槽决定。
TBZ/TBNZ 的规范形式对位号 0–31 使用 W 视图，对 32–63 使用 X 视图。

每个 form 明确声明固定 `mask`/`match`（实现字段名 `fixed`）、最低特性和逐操作数字段
位置。共享 verifier 检查形状、立即数范围、字节对齐、扩展种类、写回及独占存储的寄存器
重叠约束。编码器生成一个小端 32 位字；逻辑立即数按元素宽度、旋转和连续 1 位段编码为
`N:immr:imms`，零、全 1 和不可表示的位模式被拒绝。解码器按表序首匹配，拒绝未知、
保留和不满足共享约束的字，不在指令表外猜测指令。逻辑立即数的等价位编码在重新编码时
规范化；从本库编码结果出发，`encode → decode → encode` 必须逐字相等。

符号操作数不立即求值：对应字段清零并返回目标名、加数与 `Branch26`、`Call26`、
`CondBr19`、`TestBr14`、`AdrpPage21`、`AddLo12`、按访问大小缩放的 `LdStLo12`
或 `Literal19` 重定位记录。地址基址不因重定位而清零。库不应用重定位、不布局标签，
分支立即数是相对当前指令的字节位移，ADRP 的立即数是页差乘 4096。
`AddLo12` 符号操作数仅适用于非置标志的 `add.W.imm` 与 `add.X.imm`；共享 verifier
拒绝 `sub`、`subs`、`adds` 立即数形式上的符号目标，不生成重定位。

`ENCODER_SCHEMA = 1`、`ENCODER_REVISION = 2` 冻结此契约。`contract::verify()` 校验
目录键唯一、固定值只占固定字段、字段不重叠且不越界，以及任意两行的 mask/match 不会
同时匹配一个字。域 `gugu-a64-encoder-catalog-v1` 覆盖有序目录、形状、特性与字段；
域 `gugu-a64-encoder-v1` 覆盖 schema、revision 与目录摘要。目录或规则变动必须提高
revision 并更新测试中的指纹钉值，不能静默沿用身份。

`a64/fixtures/llvm.txt` 逐行保存 LLVM 汇编文本与完整指令字，来自离线宿主
`clang -c -target arm64-apple-macos` 与 `otool -t`，并非由本编码器生成期望值。
测试明确引用该 fixture，通过 Rust 构造每个 form 的样本及边界样本，同时比较规范文本、
字节和解码回环；默认测试不启动 assembler 或任何子进程。

### A64 内部整数叶 lowering

`a64::codegen::compile_leaf(&Body)` 消费已验证的 LIR，再检查 arena/CFG、整数签名与
`PollFreeLeaf` 接口；不进入 `plan()` 或前端派发。不明确证明为 leaf，仍带 stack check、
poll、安全点或 managed/root 初始化义务的函数被拒绝，而不是删除这些操作。
输出为字节、最终标签偏移、frame 与 allocation 诊断，没有镜像或 GC map。

`compile_function(&Body)` 是另一个显式隔离入口：调用方预留入口栈容量，测试手建满足
约束的 LIR；它不要求 caller 的 leaf 标志，也不删除 StackCheck 或伪造 callee metadata。
仍带 StackCheck、poll、safepoint 或需要 call map 的 managed call 均以
`requires checked entry / stack maps (4c)` 拒绝。正常生产 caller 保留 checked entry，
所以仍被拒绝，直到 4c。`compile_leaf` 继续拒绝全部 Call/TailCall。
只接受 `CallTarget::Instance`、Managed、现有 `Call.poll_free_leaf = true` 且无
unwind/suspend/allocation/capture 的调用；runtime、foreign、indirect/vtable 均不支持。

这是私有整数叶 ABI，不是 AAPCS64/Darwin C ABI：参数用 `x0–x7`，其余标量用入口 SP
以上的 8 字节槽；结果用 `x0–x1`。隔离入口遵守现有 LIR sret：隐藏指针为首个参数，
多字返回由 LIR Store 写入其存储，寄存器结果为空，不把第三字放到 x2。
普通 `compile_leaf` 不接受 sret；正常 sret callee 没有 poll_free_leaf 证明，其调用仍须 4c。
保留 SP、`x16/x17`（rewrite/copy scratch）、`x18`（平台）、`x29`（FP）、`x30`（LR）；
`x27 = CoroutineHot*`、`x28 = LogicalProcessor*` 均不分配。分配池为 `x0–x15`、`x19–x26`，
用到的 `x19–x26` 保存恢复。非零 frame 保存 FP/LR；SP 16 字节对齐，无 red zone。
在上述两个隔离入口中，入口容量仍是调用方义务。frame 低地址为最大
outgoing 参数区（向上取整至 16 字节），后面为 FP/LR record、copy scratch、保存寄存器、
spill 与 local。x29 指向 record，所有 BL 强制建立 record，退出恢复 FP/LR 与 SP。
frame 不再限制为 32752 字节：大 SP delta 用 scratch 常量与 extended-register ADD/SUB，
大栈偏移先物化地址；frame/槽/偏移的算术溢出仍返回错误。

`machine` 虚拟操作数显式携带 read/write/read-write 角色，地址基址是 read。
`select` 与 `lower/{integer,memory}` 负责整数/原始指针、常量、访存与 CFG 选指。
比较物化为 0/1，窄值为零扩展位模式，有符号运算先扩展；carry/borrow 为两段进位/借位的或。
除法只接受静态非零除数，不能以 A64 零除结果代替语言 panic。FP/V128、managed/compressed
引用、异常、原子、runtime 与外部地址符号操作返回带 body/block/site 的错误。
显式终止的 Trap/Unreachable 用 BRK，TrapIf 不替代为 BRK。

`alloc` 仅归一化事件，共享 `regalloc` 完成 CFG liveness、加权 scan、slot 复用与并行 copy
调度，不做 rematerialization，没有第二套分配器。边拆为 copy block，支持同时赋值和环。
ABI 参数的寄存器/栈搬运与窄值规范化只在独立入口 stub 执行一次；stub 转入 LIR 入口块，
所有 CFG 边（含指向入口的回边）只指向 LIR 块，不重复读取调用方参数。
`frame/rewrite` 在 x16/x17 内完成 spill/reload、MOVZ/MOVN/MOVK 常量、SP 地址合法化；
小 frame 的 copy 环用 x16；大 frame 的环使用独立 frame scratch，避免地址合法化
覆盖 holder。slot-to-slot 搬运用 x17，不借用活跃分配寄存器。
调用以原子 use/def 事件送入共享分配器，`call = true` 且 mask 为 x0–x15；跨调用值
只分配 x19–x26 或 spill。先写 outgoing 栈参数，再用共享并行 copy 调度器放入 x0–x7；
返回后同时收集 x0/x1。TailCall 恢复当前保存寄存器/SP/LR 后 B 到符号，只复用足够大的
incoming 参数区；sret、local 栈逃逸、参数区增长和结果签名不匹配均拒绝。
`layout` 单调扩展越界 B.cond/CBZ/CBNZ/TBZ/TBNZ 为反条件跳过一条 B，再按最终 PC 回填；
B 仍越界则报错，不生成 veneer。现有 verifier/encoder 是最终边界；fragment 公开内部
`__gugu_fn_<instance hex>` 符号，以及最终字节 offset 的 Call26/尾调 Branch26 重定位。
其他未解决符号仍被拒绝，不在这里接入目标 linker。
测试进程内手建 LIR，选指到编码/解码回环，用有限的确定性解释器核对整数、CFG、内存、
spill 和保存寄存器。VM 在合成地址空间链接 fragment 并模拟 BL/LR/RET；大 frame 用稀疏
内存，距离边界/级联扩展使用合成大小，不分配巨大 padding。

未来站点已约定：poll clobber 所有 caller-saved 寄存器；morestack clobber `x9–x14`，
保留 `x0–x8/q0–q7`。叶与隔离调用入口不调用这些片段；runtime-site 入口开始消费此约束。

### A64 checked entry 与 poll（4c.1）

`compile_runtime(&Body, &RuntimeBindings)` 与现有选指、分配、frame/rewrite/layout 共用同一
路径，接受无根整数/原始指针函数和 StackCheck/SafepointPoll。绑定必须是不同且非空的
morestack_or_poll/safepoint_slow 符号，重定位仍为 Call26，由调用方同时提供独立片段。
`fragments::morestack_or_poll` 实现在 `fragments/coroutine.rs`，`entry.rs` 是 rt0。
入口 marker 必须是唯一的 LIR entry 首指令；非零 runtime frame 即使被叶分类移除 marker
也自动检查，和 x64/alloc/frame.rs 一致。全部检查都在 SP 修改、save、ABI 参数落位之前。

检查以 x11 = SP - frame.size 为候选，经 x9 acquire-load hot.stack_check；使用和
StackDescriptor::allows_frame 相同的有符号 >=，相等可用，下溢与 poison 都走慢路。
BL 前 x10 保存原 LR；morestack 保留 x0-x8/q0-q7，x9-x14 可破坏，但 x10 返回原 LR。
返回后先恢复 LR，再从当前 SP 和最新 hot 水位重试，不沿用换栈前 candidate。
此阶段 runtime 入口拒绝普通调用，因此不漏算 unchecked leaf callee 的额外 reserve。

poll 经 x16/x17 acquire-load 当前 processor flags；零直接跳过，非零 BL 到绑定的慢路。
poll 是共享分配器中的原子 call 点；保守 mask 整个分配池（含 x19-x26），所有跨点标量
落槽，不能假设普通寄存器跨慢路保留。runtime 恢复的 x27/x28 是当前 mutator 上下文，
不得用旧 processor 副本覆盖。poll 强制 FP/LR record，并保存全部 x19-x26 来维护当前函数
向外的 callee-save 义务；CFA 用 SP 而不依赖 poll 后的 x29。

fragment 的 runtime_sites 只登记最终返回 PC、来源和 CFA/root-free 诊断；label 放在 BL
下一条指令，布局扩展后才解析，lookup 必须精确匹配。MorestackEntry 尚无 frame，CFA=SP、
原 LR 在 x10；PollResume 的 CFA=SP+frame.size、原 LR 位于 SP+record+8。
架构中立的空根位图构造从 x64/metadata.rs 提取到 metadata.rs，x64 使用同一实现；
x64 source section 代码原样移入 metadata/source.rs，以保持修改文件小于 1000 行。

4c.1 不产可供 GC walker 消费的二进制 map/unwind section。带 managed/stack-interior
根、rooted local、sret、普通调用/Invoke/TailCall 与局部栈地址在 runtime 入口明确报
4c.2 错误；不靠伪造空 map 放行。4c.2 再完成 schema、root 强制落槽、协程切换与展开消费。
测试使用已独立测试的 PR24 片段契约替身，验证 retry/换栈、参数/LR、poll 全池破坏与
processor 迁移、精确 PC/CFA、空函数和大 frame；既有 VM 同时增加 LDAR acquire 读建模。
这些测试不执行 native 代码、GC 或真实协程调度，不登记目标，不接 plan/frontend。

### A64 stack-map codec 与 walker（4c.2a）

4c.2 再分为 codec/walker 基础与 lowering/协程集成两部分。前者在 runtime-abi 的原有
stackmap_codec 中增加 encode_for(architecture, ...)，SafepointLayout<R=u16> 保留 x64
默认输入，A64 用 u32；排序、去重、typed root 与 bitmap 编解码仍是同一实现。
x64 默认 encode 继续输出相同 v4 section，既有 RuntimeRawContract 不改 schema/指纹；
A64 v5 显式 tag 与寄存器编号见 stack-maps.md。默认 x64 路径不产生新的 header 字节。

解码表携带 architecture，并检查 A64 frame/PC 对齐、根范围、保留寄存器和强制落槽规则。
Rust reference walker 的 scan_frame 消费最终 section 表、精确返回 PC、frame words 和
入口 scratch；frame/寄存器都复用已有 scan_roots，不复制 handle/compression 或五类根算法。
初始化门控、实际 handle/compressed 引用和 MorestackEntry x0-x8 均由合成 VM snapshot 测试。
此部分不放宽 compile_runtime：LIR 根集/分配器 force-slot 接线、managed call map、序列化
unwind/CFA 和 Context switch/restore 仍属下一部分；不声称生产镜像 runtime 已支持 A64。

### A64 标量根、managed call 与 safepoint CFA（4c.2b.1）

4c.2b.2a 增加 rooted local：`StackAddr` 在 runtime 边界只允许有效 slot 和 Stack provenance；
slot root 表、逐根初始化事件与 lifetime 坐标先校验，再复用 LIR anchors、LiveSlots 和
InitSlots。frame 在 locals 后为 root 表的每一项预留一个 byte，checked prologue 通过后、
入口参数物化前清零一次。block 相对位置的 RootInit 按稳定事件顺序在对应指令前或末尾
terminator 前写 byte；回边不经过清零代码。byte store 和大偏移物化仅使用 x16/x17。
已静态初始化且 provenance 唯一的字使用 fixed bitmap；其余用 v5 TypedRoot 的 byte gate。
入口块存在回边时，A64 保守使用 byte gate，不让回边的初始化事实证明第一次进入已初始化。
这不改变共享 InitSlots/x64 的现行策略。init-byte 布局、事件推进和 local-root 放置从 x64
提取到共享 metadata.rs；x64 继续使用原来的静态证明和序列化顺序。
4c.2b.2b.1 接通内部 Context/copy 边界；4c.2b.2b.2 的显式数据绑定见下文，
不新增目标登记，也不把内部 API 当作可运行的生产协程调度器。

Context、MorestackMailbox、MorestackScratch 现在由 runtime-abi 定义，fragments 复用同一
布局。直接 managed 挂起调用必须带权威 Suspend 记录，输出 SuspendResume 图，仍通过共享
pointer_spill 落槽。隔离入口、runtime target 调用、捕获参数、aggregate/sret 与 Invoke 不放宽。
stack::a64::FrameMetadata::load 配对校验最终 v5 map/CFA，并对 code bias 做对齐与溢出检查；
Image::capture 只接受 stackless handoff 的完整已用区间、精确 user return PC 和已登记终止 PC。
Context.pc 是 switch fragment 的恢复位置，不能拿它冒充用户栈图 PC；Morestack 从 scratch
取得 return_pc、原 caller LR 和入口寄存器。复制逐帧消费 gate/StackInterior，修复结构性 FP
链和 Context SP/FP，复用 x64 relocation 算术；保存的普通 GPR、浮点位型和代码地址不猜测修复。
relocate_morestack 成功后才写回 scratch 根参数与 mailbox entry_sp/candidate，随后真实片段
才重载参数；失败不改源 bytes、Context、mailbox 或 scratch。恢复仍显式提供 x27/x28。
此边界要求 Context FP 与完整元数据帧链一致，底部 FP 为零；有未登记 runtime frame 的快照拒绝，
不猜测跳过 helper frame。当前不接生产 runtime 控制块、调度器或镜像加载入口。

poll-free managed leaf 的 map 豁免同时要求不进入 morestack/poll：runtime frame 布局将其
物理 frame 限制为 4096 字节，并禁止自动入口检查。caller 取直接 poll-free managed callee
的最大预算（固定 4096），入口 candidate=SP-frame.size-max_leaf_reserve；实际 SP 只减
frame.size。frame/CFA/根偏移不包括 reserve。超限或叶接口含 runtime 操作时报错。
不把 map-free 调用改为强制建图。此处沿用 x64 的 required_frame 容量记账形状，但 x64
现有 max_leaf_reserve 只统计 ForeignLeaf 的声明预算；它没有 managed leaf 的 4096 上限，
本次不改变 x64 行为。所有 runtime callee 必须经过 runtime 编译边界执行该预算契约，
compile_function 仍是由调用者自行保证容量的隔离边界。

compile_runtime 开始接受五类来源的 64 位标量根，以及直接 Instance managed call。
compile_leaf/compile_function 的隔离限制不放宽。非 poll-free 调用必须有指向该指令的
LIR CallReturn 记录；不补造 leaf proof 或逻辑根。入口参数的机器类型/provenance 必须与
signature 一致。aggregate signature/sret、Invoke 与外部调用仍明确拒绝；挂起调用使用对应的
SuspendResume 记录，镜像内 Gugu runtime decoder 仍未接通。

runtime 编译复用 lir::stackmap::anchors/LiveValues；从真实 operands 重建派生 use cache，
再取权威逻辑根。A64 regalloc adapter 把四类 managed 值标成 HeapPointer、StackInterior
标成 StackPointer，在 call/poll Point 设置 pointer_spill，由共享 scan/slots 决定落槽与复用。
五类细分仍来自逻辑根，不以 spill 槽的粗分类猜测。CallReturn 的实参根覆盖 source spill，
第九个及以后实参还覆盖传出栈区；callee 入口图登记 x0-x7，传入栈实参由 caller map 覆盖。
跨点活跃根必须覆盖选指后的 use 位置；刚由该调用定义、还没物化的 result 不进返回 PC 图。

BL 后立即放 return label，再做 result copy；branch relaxation 后解析最终 PC。
fragment.metadata.stackmaps 是 code_rva=0 的逐片段 v5 section，未注册为用户镜像。
metadata.unwind 与其逐 PC 配对，序列化入口 CFA=SP/LR=x10、建帧后 CFA=SP+frame.size/
LR=SP+record+8。Rust ABI decoder 校验两表 code range/frame/PC 完全一致，并从调用方
提供的 frame/register snapshot 恢复 caller SP/PC；不是任意 prologue PC 的平台 EH 表。
旧 runtime_sites 的空根诊断字段由真实序列化 maps 取代。

确定性 VM 验证 BL/RET、传出栈根、五类 bitmap、GC 替身改写实际 spill 后的 reload；
StackInterior map 可交给既有 copy_input 生成 fixup 槽表。尚未执行 A64 Context 保存恢复或
完整换栈修正，也未把 x64 CoroutineContext 当成 A64 context 复用。完整 4c.2b 超过约 2000 行，
先交付本部分，下一部分完成 gated locals、A64 coroutine/stack-copy 与生产 decoder 集成。

### A64 标量 FP 与 SIMD 传输（4d.1）

A64 使用共享 regalloc 的第二寄存器类；内部 PhysReg 32–63 表示 v0–v31，绝不进入
栈图的 xN 寄存器编号。v29–v31 留作 spill/parallel-copy scratch。F32/F64/V128 使用
16-byte、16-aligned spill 槽；callee-save 只保存 d8–d15 的低 64 位，V128 不分配到
这些跨调用仅部分保留的寄存器。调用与 poll 都固定 clobber 全部 caller-saved FP；
scalar 可跨点留在 d8–d15，V128 跨点必须落槽。morestack 入口仍保存完整 q0–q7。

内部调用独立分配 x0–x7 与 v0–v7 参数车道；溢出标量占 8-byte 栈槽，V128 占
16-byte 对齐的栈槽。返回支持两个整数与八个 FP/SIMD 车道。incoming、outgoing、
tail-area 检查和指针参数的栈图都复用同一 ABI placement，不按实参序号猜寄存器。
这不是外部 C ABI：foreign/system-stack 协议与 Darwin variadic 差异由 4d.3 接线。

标量算术、IEEE ordered/unordered 比较、FP select、常量、整数/FP 转换、resize 与
等宽 bitcast 使用表驱动 ARMv8.0 FP 形式。浮点转整数依赖已有 LIR 范围检查；不以
硬件饱和结果取代语言 panic。V128 此部分提供完整位型的 copy、内存、select、ABI
与 spill；向量算术以及其余未接通 LIR 操作继续给出显式 unsupported，后续补齐。
默认测试解释已编码字节，覆盖 FP bank、SIMD 高半部、混合参数、压力、entry/poll
和真实链接调用；不执行生成的 A64 机器码或 OS FFI。

### A64 ARMv8.0 原子 lowering（4d.2）

原子 load/store、exchange、fetch add/sub/and/or/xor、强 CAS 与 fence 复用既有
1/2/4/8-byte 编码，不要求 LSE。Relaxed load/store 用普通自然对齐访问，Acquire
load 用 LDAR，Release store 用 STLR；RMW 按成功序选择 LDXR/LDAXR 与
STXR/STLXR。CAS 失败序按并发规范验证，失败路径允许比所需更强的 acquire。
Acquire fence 用 DMB ISHLD，其余合法 fence 用 DMB ISH；SeqCst 原子操作在访问
前后保守插入 DMB ISH，包含 CAS 比较失败的退出路径。

选择阶段以一个固定寄存器事件块保留 x9–x15，实参在块前平行复制，结果在块后
复制；实际排他循环在寄存器分配后展开。循环中不能插入 spill、poll 或调用，避免
普通栈写清除 exclusive monitor 导致永久重试。STXR 失败重新读取并重新计算，
CAS 比较不等则 CLREX 后返回 actual/false，不写内存；只对排他存储失败重试。
字节 VM 用显式 monitor、确定性的失败注入和并发写入替身检查重新读取、窄宽度、
旧值返回、邻字节保护、顺序指令与有 spill 的寄存器别名情形；不执行 native 线程。

### A64 image/data world（内部）

私有 image/data foundation 由 `a64::codegen::build(BuildRequest)` 围绕同一 compile
pipeline 构造，不接入 TargetName 或 plan dispatch。`World` 持有 writer 共用的 Section、
Location、符号、代码重定位和数据指针重定位；ELF writer 已使用同一节容器，调用方可直接
消费该图。输入 body 与定义按稳定符号排序，常量按字节去重并提升到最大所需对齐，typed
局部 Data 绑定在选择前传入，不能在汇编后按字符串猜测局部下标。

普通 Global 分配 `8 + bytes` 的零初始化块；coverage 使用独立域的 32-bit 非原子计数器。
TLS 继续明确拒绝。其余 typed 符号复用 canonical mangler，并要求 compiled body 或显式
Definition；显式 blob 的 Pointer 保留 offset、target 与 addend。该阶段不从空定义制造
类型记录、vtable 或适配器别名，也不宣称已连接 frozen universe 的生产元数据生成。

所有 compiled body 仍走 runtime 选择、分配、frame、site 与 metadata 路径。world 将其放进
同一代码节，用真实函数偏移聚合 `a64_binding::Sections`，追加独立 maps/CFA 节；fragment
base 是代码节起点，不是 Mach-O image base。`World::macho_request` 固定全部节、导入、fixup 和符号形状，
指令重定位不就地回填——目标解析后转为请求携带的 `macho::Arm64Relocation` 记录，
由 `macho::link` 在最终布局上编码（`link_entry` 追加的启动节会移动部分节地址，
提前修补会过期），x86_64 请求必须保持该列表为空。
native data 的 SymbolAddr 多一次槽读取，blob 中的 native data 指针用 chained bind。
请求入口目前是所选 managed body；`a64::program::build` 把 world、`DarwinRuntime`/
`LinuxRuntime` 启动契约（见 `link_entry` 条目）与 `runtime_definitions` 的哨兵/平台
片段一并消费，输出完整可执行镜像字节。生产元数据/适配器生成、前端 arch 分派与
`plan` 的 A64 分支尚未接通，`program::build` 目前只由后端测试驱动。

`a64::runtime::Catalog` 复用 canonical `RuntimeGlueBindings` 的源码解析，不另建按函数名
猜测的 runtime 表。`build_runtime` 校验 body 仍与同一冻结世界一致；CallTarget::Runtime
在原调用点按 canonical effects、参数/结果 ABI 和 poll-free 证明解析到真实实例，LIR 本身
不改写，因此 select 的 default 标志、逻辑根和 Allocation 站点身份均保留。x64 仅开放
既有 pointer-provenance ABI 比较器的 crate 内可见性，算法和生成物不变。

非分配隐含调用复用既有 implied_glue 目录，检查 source prototype 后发普通 A64 call；
帧、调用破坏、spill 和返回点复用现有路径。最多八个整数参数，额外传出栈参数尚明确拒绝。
region publish/reset/promote/transfer 合成真实 plan/export 车道；无 map 的入口还必须拥有
source poll-free 叶证明。隐含调用保留 LR frame，并保守预留同一 4096-byte 叶调用预算。
冻结 LIR 的 scoped-view/no-safepoint/permit 标记不产指令，不改变验证过的证明。

`RuntimeMachineCall` 与 `PlatformCall` 不经 catalog：`machine_binding(op)` 把
`machine.name()` 或 `platform_<op>` 映射为 glue 名，lower 直接发
`Item::Call { target: mangle_runtime(name) }`；`GcAlloc`/`RegionAlloc` 按 x64
`slow_alloc` 同构的四车道慢调用（type_id、bytes、align、`compressed | code << 1`）
调 `gc_alloc_slow`/`gc_region_slow`/`gc_shared_alloc`，type_id 由调用方传入的冻结
`TypeUniverse` 按 descriptor 解析。所有 runtime glue 调用统一引用
`__gugu_runtime_*` 命名空间：`build_with_catalog` 为 catalog 每个 binding 把
`mangle_runtime(name)` 写成指向真实编译 body 的别名符号，机器片段则由
`a64::definitions::runtime_definitions(platform)` 以同名 Definition 产出，调用点不区分。
TLS 与普通 global 同形落 `__bss` 零初始化块（mangle 不消费 thread_local 标志），线程
所有者绑定仍是 runtime 层 coroutine_locals 的职责，world 不再整类拒绝。
`a64::program::build` 消费该 catalog：`runtime_definitions(platform)` 与四个启动保留区
（`__gugu_a64_payload`/`__gugu_a64_initial_slot`/`__gugu_a64_processor` 4KiB 对齐零初始化
页/`__gugu_a64_runtime_state`）和 `runtime_imports(platform)` 一起交给
`world::build_runtime`；非空的 `__gugu_a64_gc_types`/`__gugu_a64_gc_meta` 在装配时各自
独占一个只读节（与 `__gugu_maps`/`__gugu_cfa` 同法，Mach-O 名为 `__gugutyp`/`__ggmeta`），
使载荷的长度字段精确等于 blob 大小，不吞掉共享 `__const` 的后续常量。随后 Darwin 走
`world.macho_request` + `macho::link_entry(DarwinRuntime)`，Linux 走新加的
`world.elf_request`（经 world 符号表解析指令重定位目标、数据指针转 RELATIVE）+
`elf::link_entry_dynamic(LinuxRuntime)`，`Import::slot` 取 `linux_import_slot_symbol` 的
已布局槽。`CoroutineSwitch`、线程 worker 启动与前端对 `program::build` 的调用
仍留待后续 review unit；该驱动不是已完成的生产调度器。

`a64::fragments::runtime` 是哨兵与调度片段集：`context_restore`（单条 `b` 进
`context_switch + restore_offset`）、`current_slot`（x27 → x0）、`process_exit` 与
`report_write`（Linux `exit_group`/`write` syscall，Darwin 尾调 libSystem `exit`/`write`）、
`main_return` 与 `coroutine_enter`（mailbox 写 reason=2、return_value、processor、
candidate 后按 restore ABI 恢复系统上下文）、`gc_enter`（HOT/SP 切到 coordinator
系统栈，`gc_dispatch(request, entry_rsp)`，返回前恢复）、`gc_config`（adrp+add 返回
`gc_config_record` 数据地址）、`runtime_poll_defer`（裸 `ret`）与 `thread_id`
（Linux `gettid`，Darwin `pthread_threadid_np`）。`runtime_definitions` 统一生成这批
片段、`context_switch`/`morestack_or_poll`/`safepoint_slow`、六个已实现的
`platform_*` VM 原语、`foreign_native_worker`、平台 dispatch glue
（`fs_native_worker`/`net_call`/`resolver_symbol`/`process_call`/`process_worker`）和
`gc_config_record` 数据定义；Linux 侧另加 `linux_import_definitions` 的
`__gugu_linux_stub_*` C 函数桩与 `linux_import_slot_symbol` 八字节文件回填 GOT 槽。
`runtime_imports(platform)` 汇总各片段声明的原生导入：Darwin 全部经 LIB_SYSTEM
（含 exit/write/pthread_threadid_np，按名去重），Linux 为 resolver 与外调工作线程
共用的 libc 面（getaddrinfo/freeaddrinfo/gettid/__errno_location）。

`runtime_definitions` 另含两批 `native.rs`/`callback.rs` 机器片段：cage 与栈
（`cage_control` 返回 `__gugu_cage_control` 记录的地址；记录由
`CageControlRecord::boot_bytes(cage_bytes)` 从 `ProgramRequest.cage_bytes` 生成，
`stack_read_word`/`stack_copy_raw` 沿 coroutine 栈深度锚读写，`stack_copy_raw`
拷贝循环内联发射不依赖 memcpy glue）、回调（`callback_control`/`callback_queue`
携带 16-byte 对齐的 `stlxr` 队列协议，OPEN 登记成功在 x0 返回 `OK`，
`callback_file_worker` 经 `callback_poll` 消费）与启动/平台杂项（`startup`/`environment`、`fs_start`/`fs_poll`、
`monotonic`/`wall` 纳秒、`parallelism_probe`、`net_close`、`process_native_worker`、
Linux 的 `linux_foreign_start`/`linux_foreign_poll` 按
`BLOCKING_WORKER_STACK_BYTES = 65536` 以 `Operand::Wide { shift: 16 }`
拼装，超过 imm12 不加截断）。未绑定到任何 Definition 的
`__gugu_runtime_*` 重定位与指针目标由 world 构建期统一重写为
`__gugu_stub_exit`（定义即 `fragments::process_exit`），与 x64
`resolve_target` 的兜底语义一致；绑定名缺定义仍是链接期硬错误。
仍未提供片段的 glue 名（`signal_install`/`signal_handler`、panic glue 与源码侧
`gc_dispatch` body）不可达时不发射引用，可达路径引用按上述兜底处理，
不做占位 no-op。

### A64 外调参数包与内部 lowering（4d.3）

runtime-ABI 的 `A64ForeignCallPacket` 保留完整的 176-byte `ForeignCallPacket`
前缀：x0–x5 位于 16–56，v0–v7 的低 64 位仍在 64–120，x0/x1 与 d0/d1
结果仍在 128–152，errno 在 160。扩展的 x6/x7 在 176/184，x8/sret 在 192；
200 保存额外 HFA d2/d3 的 16-byte `FIXUP_OUTPUT` 目标指针。此指针与 sret
都由既有 raw-copy/fixup 事务形成，不指向 coroutine stack。native 栈参数从完整
208-byte header 后开始，长度向上取整到 16 字节，然后是 32-byte fixup 记录。
`A64Layout` checked 计算全部边界，拒绝溢出。

`runtime/foreign.gg` 的 `[128,168)` 回写窗口和错误状态位置不变；HFA 的额外结果
通过普通 output fixup 回写，不另造 admission、credit、pin、cleanup 或 copy-back
协议。shared prefix 的 reserved 字保持零。不登记新目标。

`foreign::classify` 按 canonical C 类型分别分类 Linux 与 Darwin，不从旧的 SysV/Win64
车道推断 HFA 或符号性。Darwin 不跳过奇数 GPR；固定窄标量栈参数紧凑排列，非 HFA
聚合的栈位置和占用尺寸仍向 8-byte 槽取整；匿名参数
默认提升后全部使用 8-byte 栈槽；Linux 的 16-byte 对齐参数从偶数 GPR 开始。
HFA 不能拆分到剩余 FP 寄存器与栈；大于 16-byte 的非 HFA 值通过 raw 副本间接传递。
canonical HFA 要求成员从偏移零连续排列且总大小等于成员数乘元素大小；含尾 padding
的过对齐聚合不是 HFA。producer 与 verifier 共用此检查。canonical 聚合另保留直接成员
最大对齐（数组取元素对象对齐），不计聚合自身的 repr(align) 调整。具体 GIR 保留
packed 标志，限制该聚合的成员对齐为 1；不能从可能又被 repr(align) 提高的对象对齐
猜测 packing。Linux AAPCS64
以此自然对齐（上限 16）分配参数寄存器和栈槽；Darwin 仍使用对象对齐。
原始大小、分组对齐和 copy-back 不变；具体类型 schema=6、BuildLir schema/body revision=26
失效旧描述缓存。

`machine::ForeignCall` 在分配后展开。先把全部 SSA 输入保存到独立 staging 区，
再构造 native 参数包，防止固定 scratch 覆盖尚未使用的输入。by-value、sret 与额外
HFA 结果使用原有 fixup；sret 的协程栈目标保存为 stack-high 相对深度，恢复后重算。
dispatcher 接收同 x64 一样的四个深度/长度/记录参数，普通与 dirty 符号必须显式绑定。
调用后的 PC 使用 kind=3、对应 dirty bit 的权威 ForeignBridge 图；参数根来自原始 spill，
不是仅在进入 dispatcher 前短暂存活的 packet scratch 副本。
SysV LIR 可省略只含尾 padding 的 eightbyte；staging 只复制实际输入车道，缺少的尾部
保持预清零，不得越过分组读取下一实参。返回也只恢复存在的 LIR 车道，不要求 padding 有结果值。
bridge 的调度协议由 CallKind 决定；C callee 的 `may_suspend=false` 不豁免 admission
或权威挂起图，也不能作为拒绝生产 LIR 外调的条件。

`fragments::foreign_native_worker` 是已获 admission 的 OS native 线程入口，依赖调用方
提供独立、16-byte 对齐的 system stack；不能直接从协程栈调用。它有界探测栈参数区，
加载 x0–x8/d0–d7，保存 x0/x1/d0–d3 和线程 errno，release 发布/清除线程 id，
不清除 thread handle、不发布已 join 或释放 credit。线程启动、join 确认和调度仍需平台绑定。
Linux C 引用经 `linux_import_stub_symbol` 的 GOT 桩符号，Darwin 函数使用 `import_stub_symbol`；Data 符号不加函数桩前缀。
字节 VM 使用 TLS 函数、admission/copy-back 与栈移动替身，绝不执行 OS FFI 或线程。
function assembler 同时保留 ADRP/ADD 地址重定位与 call 重定位；无链接的 `assemble`
入口仍拒绝任何未解析符号。

### A64 剩余显式拒绝清单（未登记目标）

已补入无调用 byte-region memcpy/memmove/memset、条件 BRK、实例及外部地址形成。
下列 LIR 仍不宣称完整支持：

- 孤立入口仍拒绝 `RuntimeCall` 与隐含 runtime 操作；私有 catalog 已按真实源码 body 绑定
  标量 RuntimeCall、handle/屏障与 region 生命周期，并消去冻结 LIR 的 scoped-view 证明标记。
  `RuntimeMachineCall`/`PlatformCall` 与 `GcAlloc`/`RegionAlloc` 已分别接入
  `__gugu_runtime_*` 命名空间和四车道慢调用 ABI；`CoroutineSwitch`、不具备
  no-safepoint 叶证明的 reserved 屏障与尚无片段/源码绑定的 glue 名仍拒绝（后者在
  链接期以未定义符号失败），不把未完成的平台接线伪装为可运行的 runtime。
- 孤立编译入口仍拒绝 global、adapter、descriptor、type table/name、vtable、data blob
  与 `CoverageCounter`；私有 image world 已绑定常量、普通全局与 coverage，并要求其余
  typed 符号有真实显式定义。TLS 与普通 global 同形落 `__bss`，线程所有者绑定仍属
  runtime 层；生产类型表生成与适配器构造尚未接入。
  Darwin image 的 extern data 地址读取导入槽，function 地址使用已有导入桩。
- `Vector` 的算术、shuffle、lane 与 reduction：通用 SIMD 尚无 lowering；现有生产
  LIR builder 不生成该 Op，V128 transport/select/load/store/bitcast 已支持。
- `ForeignLeaf`、可 unwind 或 tail 外调、foreign callback/export adapter：未绑定其
  system-stack/异常/回调路径；外调当前只接受普通和 dirty bridge，拒绝 coroutine
  stack 裸地址泄漏、GC 车道、过对齐/过大签名和超 native stack budget。
- `Invoke`/`ResumePanic` 已接入：invoke 发射调用、landing 记录进入 unwind 表，
  `ResumePanic` 走 panic glue；结构化跳转、条件跳转、switch、return、tail
  managed call、trap 和 unreachable 均已支持。
- 普通 managed 调用支持直接实例、绑定 runtime 目标、恰一个 Code-provenance
  参数的间接调用，与 `Vtable` 动态派发；aggregate/sret 按 caller 临时副本/结果
  缓冲地址处理。间接 callee 不占 ABI 槽，vtable 接收者照常作为实参传递；派发
  源同 x64 `dispatch_source`：`Metadata` lane 直接是 vtable（偏移 0），否则首个
  非 sret 的 `Ptr` 参数是指向胖对的指针，vtable 在其 `+8`（`dyn` 布局 `data@0`、
  `vtable@8`），槽地址为 vtable `+ slot*8`。目标入口在参数装填前物化到 x8：
  x16/x17 是并行拷贝组的破环与槽间暂存，不能承载跨拷贝的调用目标；间接 callee
  车道虽不在 `arguments` 中，也登记为 `Item::Call` 的 Read 事件，否则 regalloc
  会在调用点前回收其位置。间接/派发的 tail call、`captures_arguments`（runtime
  目标除外）与 `foreign_arguments` 仍拒绝。整数除余仍要求静态非零除数证明；
  窄宽度的非法 carry/wide、非法转换和超帧范围显式拒绝。

这些拒绝不隐藏为 no-op；特别是泛型 SIMD 与动态除零检查不是已实现的 PR 6/8 能力。

### A64 生产源与镜像数据绑定

`runtime/a64.gg` 随已有 runtime source table 提供，用现有 `#![cfg(arch = "aarch64")]`
按 descriptor 的规范架构名裁项；x64 不编译其中的声明，也不执行 A64 布局校验。
roots.gg 中供 A64 调用的三个 metadata bridge 也使用同一 arch cfg，避免进入 x64 runtime-owner 集合。
不引入目标名、frontend dispatch、plan 分支或 opt-in 开关。
A64Context、A64MorestackScratch、A64MorestackMailbox、A64ControlBlock 与 A64Snapshot
按 Rust 权威布局定义；GIR 派生时用冻结 HIR 与具体布局逐字段校验，不靠解析源码推测偏移。
a64_records 只在独立类型视图中校验，不加入 executable/contract roots。
它们不加入旧 CoroutineRuntimeContract.records，避免改变 x64 控制块与 raw fingerprint。

ControlBlock 把 Context、16-byte scratch 和 mailbox 放在不移动的 owner 存储；bind 只设置
hot、scratch、mutator_context 与 system_context 链接，不清空已有寄存器、entry SP 或 SIMD。
morestack snapshot 保留 Context.pc 的恢复位置，另取 scratch.return_pc 作为用户 metadata PC，
复制 x0–x8 并把 caller_lr 放在 x10，其他 snapshot GPR 清零。Gugu/Rust Snapshot 使用同一 C 布局。

`a64_binding::Sections` 聚合隔离 lowering 已生成的 metadata，并在 Rt0Payload 的已有字段中
绑定 map/CFA 地址；同一数据可被 Gugu bounded walker 和 Rust stack copier 消费。它不替代
尚不存在的 A64 platform writer、启动接线或 scheduler 的架构选择。默认 x64 collector 不改路由，
也不通过把 x64 CoroutineCold 当成 A64 record 来宣称生产调度已完成。测试解释生产 Gugu LIR，
不执行 native A64、真实 GC、真实线程或 OS 服务。解释器 fixture 仅把文件的架构谓词重定向到
宿主，函数体不变；另校验三个 x64 artifact 均已裁去 A64 声明。

### A64 运行时机器片段

独立片段通过同一编码表输出字节与片段内重定位；不进入现有 x64 运行时契约，也不登记目标。
片段边界保留 `x27`（协程槽）与 `x28`（processor）作为显式换入值，不使用平台寄存器
`x18`。换栈记录保存 SP、续点、其余 AAPCS64 非易失整数寄存器和 `d8–d15`。
入口冷路径经 `x28` 找到 mailbox，保存 `x0–x8`、`q0–q7`、冷路径返回 PC 及调用方
LR（由 prologue 放入 `x10`），以 `SP - candidate` 确定所需空间，不访问用户栈。
系统侧恢复该续点后，片段从 mailbox 关联的 scratch 重新装载参数和两条返回地址。
poll 慢路保留调用点的 LR 与 SP，优先尾跳 callback；仅在压力状态与入口均就绪且 committed
严格超过软上限时，将请求地址放入 `x0`，尾跳 GC 入口。

rt0 的 OS 入口负责加载修正、native 启动参数与退出；A64 胶合写入启动载荷、装载初始保留
寄存器并调用 runtime，随后恢复宿主保留寄存器并返回状态码。原子片段支持自然对齐的
1/2/4/8 字节对象；RMW 使用 ARMv8.0-A 独占读写及重试，不依赖 LSE，比较失败清除本地
独占监视器。各片段 ABI 仅供后续后端和运行时接线使用。

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
