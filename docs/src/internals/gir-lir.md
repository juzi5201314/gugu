# GIR 与 LIR

本章规定 Gugu 的两个中间表示：GIR 是完成类型检查后的、目标无关且显式控制流的语义 IR；LIR 是完成具体布局和 ABI lowering 后、面向目标机器的 SSA IR。两者是编译器内部契约，不是序列化给第三方工具的稳定格式。

完整管线固定为：

```text
AST -> HIR -> generic GIR -> monomorphic GIR -> LIR -> x86_64 machine code
```

不得绕过 GIR 直接从 HIR 生成机器码，也不得在 LIR 中重新执行 trait 选择、comptime 求值或语言级模式匹配。

## 权威边界

[值传递](../spec/passing.md)、[表达式](../spec/expressions.md)、[内存](../spec/memory.md)、[并发](../spec/concurrency.md)和[运行时](../spec/runtime.md)唯一规定 copy、defer、panic、GC可达性、等待与外部边界的可观察结果。HIR向 GIR提供已经按这些章节生成的语义计划；本章只固定计划的显式 CFG/operation表示和优化器不得破坏的不变量，不重新决定哪些出口执行哪些动作。

## 共同表示规则

GIR 和 LIR 分别以一个函数、闭包、初始化器或编译器生成 glue 为 body 单位。body 内的 block、local、value、statement、safepoint 和 source scope 使用从 0 开始的稠密 `u32` ID，并存入连续 `Vec` arena。边数无固定小上界，前驱/后继范围存入统一边表；不得为每条边单独分配对象。

所有 body 都带：

- `owner`：定义的稳定键与 session-local `DefId`；
- `signature`：规范化参数、返回类型和 effect；
- `source_scopes`：可恢复到 HIR span 的内联作用域树；
- `flags`：是否可 panic、可 suspend、可分配、含 unsafe 或属于 runtime glue；
- `revision`：IR schema 版本，当前 GIR 和 LIR 都是 1。

成功产物不允许错误类型、未解析定义、未求值 comptime 值或悬空 arena ID。每个改写 pass 必须在调试构建中运行局部 verifier；跨阶段边界必须运行完整 verifier。

## GIR

### 设计目标

GIR 对应 Rust MIR 的显式 place/CFG 作用和 Go SSA 之前的语义 lowering 边界，但保留 Gugu 自己的语义动作：值描述符复制、COW 封存、resource 租约、精确 GC 写屏障、协程 suspend 和 panic 清理。generic GIR 可以含类型/常量参数；monomorphic GIR 不可以。

GIR 不是 SSA。local 可以在不同控制流点多次赋值，初始化状态由数据流分析验证。这样模式绑定、`defer`、panic 清理和复合 place 写回不需要在前端提前构造 phi。

### body 与 local

```text
GirBody {
    owner,
    generic_params,
    locals: IndexVec<LocalId, GirLocal>,
    blocks: IndexVec<BlockId, GirBlock>,
    source_scopes,
    cleanup_regions,
    entry: BlockId,
}

GirLocal {
    ty: TyId,
    kind: Return | Argument | User | Temporary | SpillCandidate,
    mutable: bool,
    address_taken: bool,
    source_scope: SourceScopeId,
}
```

`LocalId(0)` 固定为返回槽，之后按参数声明顺序分配参数，再按 HIR 前序顺序分配用户 local 和临时值。compiler pass 新增的 local 按 pass 顺序追加，不能插入并重编号既有 local。ZST local 仍有 ID 以承载生命周期和诊断，但不要求分配机器存储。

### place、operand 与 rvalue

`Place` 是 `LocalId` 加投影序列。投影封闭为：

- `Deref`；
- `Field { index, field_ty }`；
- `TupleField { index, field_ty }`；
- `Index(LocalId)`；
- `ConstantIndex { offset, from_end }`；
- `Subslice { from, to }`；
- `Downcast(VariantId)`；
- `OpaqueCast(TyId)`。

投影序列存入 body 级连续 pool，`Place` 只保存 base 和 range。一个 place 中所有会执行用户代码或 panic 的下标表达式必须先求值到 local；投影本身不能隐藏调用。

`Operand` 固定为 `Copy(Place)`、`MoveInternal(Place)`、`Constant(ConstId)` 和 `Function(MonoCandidate)`。`MoveInternal` 只表示编译器已经证明源槽在该路径不再读取的存储转移，不改变语言的按值传递语义。

`Rvalue` 固定包含：

- `Use`、`UnaryOp`、`BinaryOp`、`CheckedOp`、`Compare`；
- `Aggregate`、`Repeat`、`Discriminant`、`Len`；
- `Ref`、`RawAddress`、`Cast`、`FunctionValue`、`DynErase`；
- `ValueCopy`、`CowSnapshot`；
- `AllocObject`、`AllocArray`、`StackSlotAddress`；
- `Intrinsic`。

整数操作在 GIR 中携带确定的类型与语言溢出语义；浮点操作携带严格 IEEE 模式。`CheckedOp` 仅用于除零、移位输入、下标、容量和内存大小等真实 panic 条件，不重新引入 debug/release 算术差异。

### statement

每个 `GirBlock` 是按顺序执行的 statement range 加一个 terminator。statement 固定为：

- `StorageLive(LocalId)`、`StorageDead(LocalId)`；
- `Assign(Place, Rvalue)`；
- `SetDiscriminant(Place, VariantId)`；
- `ValueAction { action, place, descriptor }`，其中 action 为 `Copy`、`Publish`、`Drop` 或 `Forget`；
- `ResourceAction { action, place, descriptor }`，其中 action 为 `AcquireLease`、`ReleaseLease`、`Transfer` 或 `Finalize`；
- `GcWrite { owner, destination, value }`；
- `Pin { place, token_local }`、`Unpin { token_local }`；
- `SafepointPoll(SafepointId)`、`StackCheck`；
- `Atomic`、`Volatile`；
- `CoverageCounter`、`Nop`。

语言级普通赋值在具体类型已知后必须展开成所需 `ValueAction`/`ResourceAction` 和最终 `Assign`；优化器只能在证明不改变观察结果时删除动作。向 GC 可达对象的引用字段写入必须成为 `GcWrite`，不能退化为裸 `Assign`。

`Nop` 只在 pass 保留 source location 时临时存在；离开 GIR 优化管线前必须删除。

### terminator

terminator 固定为：

- `Goto { target }`；
- `SwitchInt { value, targets, otherwise }`；
- `Call { callee, args, destination, normal, unwind, call_kind }`；
- `Return`；
- `Panic { payload, unwind }`；
- `ResumePanic`、`Abort`、`Unreachable`；
- `Suspend { reason, resume, cancelled, safepoint }`；
- `SelectCommit { cases, ready, suspend, safepoint }`。

`Call.unwind` 对可能 panic 的调用必须指向 cleanup block；证明 `nounwind` 的调用使用 `None`。外部 C 调用的 `call_kind` 明确为 `ForeignBridge` 或 `ForeignLeaf`，其 C ABI 转换不能由普通调用优化删除；只有 `ForeignBridge` 携带 runtime 交接。

`Suspend` 保存 stable resume/cancelled successor和该点活跃 local，不在 GIR重新决定取消值或协程寿命；`SelectCommit` 只引用 HIR已经固定的一次求值临时槽、case index和提交 operation。机器 context与等待记录由后端/调度器建立。

### cleanup 与控制流

HIR为每个控制流出口提供 `CleanupPlan { exit_kind, action_range, destination }`；action已按[表达式](../spec/expressions.md#defer)和[值传递](../spec/passing.md)排好顺序并保存注册点临时槽。GIR构造器不能按 `return`、panic或循环种类重新推导动作，只把相同 action suffix intern成共享 cleanup block并连接 normal/unwind edge。

cleanup block的输入先保存到独立 local；正常链以 `Goto/Return` 结束，unwind链以 `ResumePanic` 结束。构造后 verifier逐出口比较 GIR action ID序列与 HIR `CleanupPlan`，差异是内部错误。返回值、deferred call环境和 resource动作的求值时点都来自 plan，不由 CFG共享改变。

HIR同样提供保持源码臂优先级的 pattern matrix。GIR把它编译成判别值、长度和标量比较 decision DAG；共享测试只能读取已物化临时槽，leaf保存原 matrix row ID。verifier用 row ID检查守卫/臂优先级，不在本章另写一份模式语义。

### generic 与 monomorphic GIR

generic GIR 允许 `TyId` 和 `ConstId` 中引用 owner 的泛型参数，也允许 `Operand::Function` 保存尚未替换的 `MonoCandidate`。它必须已经固定 trait/impl 选择规则；选择中剩余的参数只作为规范化 substitution 的输入。

单态化为每个 [`MonoKey`](monomorphization-cache.md#单态化实例) 创建独立 body，并完成：

- 所有类型、常量和关联类型替换；
- 布局、字段偏移、enum 表示和调用签名确定；
- 静态分派调用变成具体实例，`dyn` 调用保留明确 vtable 槽；
- `ValueAction` 和 `ResourceAction` 绑定具体描述符/glue；
- 不可实例化、无限递归或仍含参数的 body 报编译错误。

## GIR 固定 pass 管线

monomorphic GIR 必须按以下顺序处理；pass 可以在没有匹配机会时为空操作，但不能交换会改变不变量的阶段：

1. `SubstituteAndNormalize`：完成 substitution、投影规范化和布局查询；
2. `ElaboratePatternsAndCleanup`：完成 decision DAG、cleanup 与确定初始化边；
3. `ElaborateValueAndResourceActions`：展开语义复制、COW、resource 和 pin 动作；
4. `LowerConcurrency`：把 `async`、channel、`select`、等待和取消变成 GIR/runtime 原语；
5. `Inline`：按下述固定成本模型内联直接调用；
6. `SimplifyCfg`：删除不可达 block、合并单前驱/单后继 block、折叠常量分支；
7. `SparseConditionalConstants`：传播常量与不可达边；
8. `CopyPropagationAndGvn`：传播无副作用 copy，并对纯 rvalue 做全局值编号；
9. `BoundsCheckElimination`：用支配关系、范围与循环归纳变量消除已证明检查；
10. `EscapeAndPlacement`：决定 stack、GC heap、arena 和闭包环境位置；
11. `CowAndResourceElision`：只删除被数据流证明不可观察的封存、租约和复制；
12. `InsertWriteBarriers`：为所有可能的 heap 引用写入生成屏障并删除已证明的新对象初始化屏障；
13. `InsertSafepointsAndStackChecks`：在规定位置插入轮询和函数栈检查；
14. `LowerLayoutAndAbi`：把聚合、枚举、调用和返回映射为具体字节布局；
15. `BuildLirSsa`：构造 LIR、block 参数和 memory SSA。

内联成本按单态化 GIR 计算：普通 statement 为 1，分支为 2，直接调用为 5，分配为 8，可能 suspend 的操作为 32。compiler intrinsic 和不产生独立机器 frame 的 glue 在启发式前强制展开。其余非递归 callee成本不超过 32时成为候选；在整个实例图只有一个直接调用点时上限为 128。递归 SCC、`ForeignBridge`、`ForeignLeaf`、naked、含 `Suspend` 或无法复制 cleanup 区域的 body从不成为候选；两种外调都只能作为调用边界 lowering，不能把 C 函数体内联进 GIR。

每个 caller的增长预算固定为 `max(floor(original_cost * 20 / 100), 256)`。只对 pass 开始时存在的调用点按 source scope、block ID、statement index和 callee `MonoKey` 排序遍历；候选展开后预计累计增长不超过预算就必须内联，否则必须保留。新暴露调用点留给其 callee 自身已经缓存的优化 body，不在本 caller重复开第二轮，保证管线终止且并行编译不改变结果。

循环 backedge、可能分配的调用、可能阻塞的 runtime 操作、`ForeignBridge`、显式 `yield` 和函数 prologue 是 safepoint 候选。`ForeignLeaf` 不因 C 调用本身产生 safepoint；`InsertSafepointsAndStackChecks` 对同一 block 中没有任何 intervening statement 的连续 poll只保留第一条；除此之外不得合并或后移。任何无调用的循环路径都必须至少保留一个可达轮询。

## LIR

### SSA 与 block 参数

LIR 中每个普通值只定义一次。控制流合流使用 block 参数，不使用单独的 phi 指令：每条前驱边必须按目标 block 参数顺序提供实参。block 参数 0 若存在，固定为 memory token；其余参数按来源 `LocalId`、字段偏移和定义 block 顺序排序。

```text
LirBody {
    owner,
    target,
    values: IndexVec<ValueId, ValueData>,
    blocks: IndexVec<LirBlockId, LirBlock>,
    stack_slots: IndexVec<StackSlotId, StackSlot>,
    safepoints: IndexVec<SafepointId, SafepointData>,
    source_scopes,
}
```

`ValueData` 保存一个定义位置、一个 `LirType` 和 GC provenance。使用链在构造后生成紧凑的 offset/count 表；优化 pass 更新定义/操作数后统一重建，不维护每个 value 的堆分配链表。

### 类型与 provenance

LIR 只允许以下机器值类型：

- `I8`、`I16`、`I32`、`I64`；
- `F32`、`F64`；
- `Ptr`；
- `Flags`，只在同一 block 内连接比较和条件分支；
- `Mem`，表示 memory SSA token；
- `Void`，只用于无普通结果的指令声明。

`i128`/`u128` 在进入 LIR 时拆为低、高两个 `I64`；聚合拆为按 ABI 分类的标量 tuple 或放入 stack slot。ZST 不产生普通 value。`Ptr` 的 provenance 固定为 `GcHeap`、`GcInterior`、`Stack`、`Raw`、`Code`、`Metadata` 或 `Foreign`；优化器不得把 `Raw`/整数推断成可追踪 GC 根。

### memory SSA

每个可能读写内存、分配、调用、原子、volatile、屏障或 safepoint 的操作都消费一个 `Mem` 并产生一个新的 `Mem`。纯算术、地址计算和已证明无读取的常量不消费 memory。合流 block 用 `Mem` block 参数合并各前驱 token。

memory token 只编码顺序，不占机器寄存器。每个普通 memory op 还携带 `AliasClass`：`Stack(StackSlotId)`、`FreshHeap(AllocId)`、`Global(StableDefKey)`、`ThreadLocal(StableDefKey)`、`Heap`、`Foreign`、`Atomic` 或 `Volatile`。不同 stack slot、不同尚未发布 fresh allocation、布局不重叠的不同 global以及不同 TLS定义互不 alias；pointer经未知 call/store/block-parameter merge/整数转换后降为 `Heap` 或 `Foreign`。只有上述封闭规则证明 class不相交时才重排普通 load/store；atomic、volatile、`ForeignCall`（包括 leaf 的外部内存效应）、safepoint 和 runtime barrier 是任何 class 都不可跨越的 effect fence。

### 指令集合

LIR 指令按封闭类别组织：

- 常量与地址：`IConst`、`FConst`、`SymbolAddr`、`StackAddr`、`PtrOffset`；
- 整数：加减乘、带进位加减、除余、按位、移位、扩展、截断和比较；
- 浮点：加减乘除、取负、比较、整数转换和位转换；
- 控制辅助：`Select`、`TrapIf`；
- 内存：`Load`、`Store`、`Memcpy`、`Memmove`、`Memset`；
- 并发：`AtomicLoad`、`AtomicStore`、`AtomicRmw`、`CompareExchange`、`Fence`；
- runtime：`GcAlloc`、`GcWriteBarrier`、`SafepointPoll`、`StackCheck`、`CoroutineSwitch`、`Park`、`Ready`；
- 调用：`Call`、`ForeignCall`；`ForeignCall` 的 mode 必须是 `ForeignBridge` 或 `ForeignLeaf`。
- 诊断插桩：`CoverageCounter`。

block terminator 固定为 `Jump`、`Branch`、`Switch`、`Invoke`、`Return`、`ResumePanic`、`TailCall`、`Trap` 和 `Unreachable`。可能 panic/unwind 的调用必须使用 `Invoke`，它同时提供 normal 和 unwind 边；确定不 unwind 的调用才使用普通 `Call`。`TailCall` 只允许在没有待执行 cleanup、调用/返回 ABI 完全相同、当前 frame 无活跃 stack root且全部参数能放入寄存器时形成；含 stack argument、sret 或 `ForeignBridge` 的调用不得 tail-call。`ForeignLeaf` 只有在普通 ABI 与 frame 条件全部满足时才可参与同一规则，不能因为 leaf 标记绕过 stack root 或 unwind 检查。


### LIR 固定 pass 管线

LIR 构造后按下列顺序运行：

1. `VerifySsaAndMemory`；
2. `CanonicalizeCfg`；
3. `SparseConditionalConstants`；
4. `AlgebraicSimplification`，只使用严格整数/浮点合法恒等式；
5. `GlobalValueNumbering`；
6. `DeadStoreAndDeadValueElimination`；
7. `LoopInvariantCodeMotion`，不得跨越 safepoint/effect fence；
8. `StrengthReduction`，保持整数环绕、除零和浮点语义；
9. `LowerRuntimeFastPaths`，展开 TLAB 分配、写屏障和轮询的快速路径；
10. `LowerTargetAbi`，固定 Gugu 内部 ABI 与 C ABI 参数位置；
11. `LegalizeX86_64`，保证每个操作都有目标指令序列；
12. `PrepareRegisterAllocation`，分裂关键边并生成并行 copy；
13. 后端指令选择、寄存器分配和 frame layout。

任何 pass 都不得删除一个仍可能触发调度或 GC 的 safepoint，不得把 GC provenance 降为 `Raw` 以逃避栈图，也不得把 panic 条件变成未定义行为。浮点优化不使用 reassociation、`NaN` 假设、flush-to-zero 或 fast-math。

## verifier

GIR verifier 至少检查：

- 每个 block 恰有一个 terminator，所有目标和 arena range 有效；
- place 投影类型连续，读取前确定初始化，`StorageLive`/`StorageDead` 配对；
- cleanup 边只沿作用域树向外，panic 与正常出口不混用；
- concrete body 不含泛型参数、未解析 call 或未知布局；
- 每个可能 suspend/allocate/block 的路径有合法 safepoint；`ForeignBridge` 必须有对应 bridge 交接记录，`ForeignLeaf` 必须有合法 leaf stack budget 与 pre-call `StackCheck`，不得伪造需要桥接的 effect；
- 每个 heap 引用写入经过 `GcWrite` 或被证明属于未发布新对象初始化。

LIR verifier 至少检查：

- 普通 value 单定义且定义支配使用，block 实参数量和类型完全匹配；
- memory token 在每条 effect 路径形成连续 SSA 链；
- `Flags` 不跨 block、不跨可能改写标志的指令；
- pointer provenance 与 load/store、stack map 和外部调用规则一致；
- panic/unwind 边、`ForeignBridge`/`ForeignLeaf` 的 mode、foreign runtime 交接和 safepoint ID 不缺失；
- 目标 legalization 后不存在 i128、聚合普通 value 或无编码操作。

release 编译器在进入代码生成前也必须运行完整 verifier。验证失败属于编译器内部错误并停止产出镜像，不能降级成保守机器码继续运行。

## IR dump

`-Zdump-gir` 和 `-Zdump-lir` 只属于编译器开发接口。dump 按稳定定义路径、block ID 和 value ID 排序，显式打印类型、provenance、memory token、normal/unwind 边和 source span。地址、hash 表桶序和线程编号不得出现。开发开关不进入语言规范，正式 CLI 未启用内部选项时不得接受这些参数。

## 参考实现资料

- [Rust 编译器开发指南：MIR](https://rustc-dev-guide.rust-lang.org/mir/index.html)
- [Rust 编译器开发指南：单态化](https://rustc-dev-guide.rust-lang.org/backend/monomorph.html)
- [Go 编译器 SSA 说明](https://go.dev/src/cmd/compile/internal/ssa/README)
