//! generic GIR 的 body 表示：稠密 `u32` ID、连续 arena、封闭的 place/operand/rvalue/statement/terminator。
//!
//! 类型使用 HIR 模块类型表的 `TypeId`（generic GIR 允许引用 owner 的泛型参数）；
//! 常量驻留在 body 级常量池；投影驻留在 body 级投影池，`Place` 只保存 base 与 range。
use crate::frontend::hir::{self, DefId, Location, TypeId};
use crate::frontend::mono::instantiate::CallSite;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// GIR schema revision（见 gir-lir.md「共同表示规则」）。
pub(crate) const GIR_REVISION: u32 = 2;

macro_rules! ids {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        pub(crate) struct $name(pub(crate) u32);
        impl $name {
            #[allow(dead_code)]
            pub(crate) fn index(self) -> usize {
                self.0 as usize
            }
        }
    )* };
}
ids!(
    BlockId,
    LocalId,
    ScopeId,
    ConstId,
    SafepointId,
    NoSafepointRegionId
);

/// body 标志位：`flags` 使用零分配位掩码。
pub(crate) struct BodyFlags;
impl BodyFlags {
    pub(crate) const PANIC: u32 = 1;
    pub(crate) const SUSPEND: u32 = 2;
    pub(crate) const ALLOCATE: u32 = 4;
    pub(crate) const UNSAFE: u32 = 8;
    pub(crate) const RUNTIME_GLUE: u32 = 16;
    pub(crate) const FOREIGN: u32 = 32;
    pub(crate) const KNOWN: u32 = 63;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum BodyKind {
    Function,
    Closure,
    Async,
    StaticInit,
    GlobalAsm,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Signature {
    pub(crate) parameters: Vec<TypeId>,
    pub(crate) result: TypeId,
    /// HIR `Effects` 位的并集。
    pub(crate) effects: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GirBody {
    pub(crate) owner: DefId,
    pub(crate) owner_key: [u8; 32],
    pub(crate) kind: BodyKind,
    pub(crate) signature: Signature,
    pub(crate) generic_params: u32,
    pub(crate) locals: Vec<GirLocal>,
    pub(crate) blocks: Vec<GirBlock>,
    pub(crate) statements: Vec<Statement>,
    pub(crate) predecessors: Vec<BlockId>,
    pub(crate) projections: Vec<Projection>,
    pub(crate) constants: Vec<Constant>,
    pub(crate) source_scopes: Vec<SourceScope>,
    pub(crate) cleanup_regions: Vec<CleanupRegion>,
    pub(crate) exit_records: Vec<ExitRecord>,
    pub(crate) safepoints: Vec<Safepoint>,
    pub(crate) no_safepoint_regions: Vec<NoSafepointReason>,
    pub(crate) select_cases: Vec<SelectCase>,
    /// 每个 HIR 表达式对应的结果 local；发散表达式为 `None`。
    pub(crate) expression_locals: Vec<Option<LocalId>>,
    /// 已选择 match 叶：`(进入叶的 block, HIR arm 行号)`。
    pub(crate) match_leaves: Vec<(BlockId, u32)>,
    pub(crate) flags: u32,
    pub(crate) revision: u32,
    pub(crate) entry: BlockId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum LocalKind {
    Return,
    Argument,
    User,
    Temporary,
    SpillCandidate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GirLocal {
    pub(crate) ty: TypeId,
    pub(crate) kind: LocalKind,
    pub(crate) mutable: bool,
    pub(crate) address_taken: bool,
    pub(crate) source_scope: ScopeId,
    /// 对应的 HIR 局部槽（用户 local）；临时值为 `None`。
    pub(crate) hir_local: Option<hir::LocalId>,
    /// 捕获或跨协程槽：`StorageDead` 推迟到函数出口。
    pub(crate) pinned_storage: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceScope {
    pub(crate) parent: Option<ScopeId>,
    pub(crate) location: Location,
    pub(crate) hir_scope: hir::ScopeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceInfo {
    pub(crate) location: Location,
    pub(crate) scope: ScopeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GirBlock {
    pub(crate) statements: Range<u32>,
    pub(crate) terminator: Terminator,
    pub(crate) source: SourceInfo,
    pub(crate) predecessors: Range<u32>,
    /// cleanup block 只能由 cleanup 边或其它 cleanup block 到达。
    pub(crate) cleanup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Statement {
    pub(crate) kind: StatementKind,
    pub(crate) source: SourceInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Place {
    pub(crate) local: LocalId,
    pub(crate) projections: (u32, u32),
}

impl Place {
    pub(crate) fn local(local: LocalId) -> Self {
        Self {
            local,
            projections: (0, 0),
        }
    }
    pub(crate) fn range(self) -> Range<usize> {
        self.projections.0 as usize..self.projections.1 as usize
    }
    pub(crate) fn is_local(self) -> bool {
        self.projections.0 == self.projections.1
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Access {
    Normal,
    /// scoped borrowed view 的只读投影；不能形成写入 place。
    ScopedRead,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Projection {
    Deref,
    Field {
        index: u32,
        field_ty: TypeId,
        access: Access,
    },
    TupleField {
        index: u32,
        field_ty: TypeId,
    },
    Index(LocalId),
    ConstantIndex {
        offset: u64,
        from_end: bool,
    },
    Subslice {
        from: u64,
        to: u64,
        from_end: bool,
    },
    Downcast(u32),
    OpaqueCast(TypeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MonoCandidate {
    pub(crate) definition: DefId,
    pub(crate) signature: TypeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Operand {
    Copy(Place),
    MoveInternal(Place),
    Constant(ConstId),
    /// 前端已封闭的 late 值：按 HIR 表达式编号在冻结类型集合的结果表中解析。
    LateConstRef {
        expression: u32,
    },
    Function(MonoCandidate),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Constant {
    pub(crate) ty: TypeId,
    pub(crate) value: ConstValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ConstValue {
    Unit,
    Bool(bool),
    Integer(u128),
    Float(u64),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    CString(Vec<u8>),
    /// 模块级常量或关联常量项；单态化物化其规范值。
    Definition(DefId),
    /// 非法但类型正确的占位不存在：`Never` 只用于 `!` 类型槽的初始化标记。
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// 真实 panic 条件；结果为 `bool`（true 表示检查通过）。`check` 是 HIR `RuntimeCheck` 编号。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CheckOpKind {
    Division { ty: TypeId },
    Shift { ty: TypeId },
    Bounds { slice: bool },
    Utf8Boundary,
    FloatToInt { signed: bool, bits: u16 },
    UnicodeScalar,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum AggregateKind {
    Tuple,
    Array(TypeId),
    Adt { ty: TypeId, variant: u32 },
    Range,
    Closure(DefId),
    Coroutine(DefId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CastKind {
    Scalar,
    Pointer,
    Transmute,
    NeverTo,
    ArrayToSlice,
    Opaque,
    Instantiate,
    /// 函数项擦除为函数值。
    FunctionErase,
    AssumeInit,
    MaybeUninit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Rvalue {
    Use(Operand),
    UnaryOp {
        op: UnaryOp,
        operand: Operand,
    },
    BinaryOp {
        op: BinaryOp,
        left: Operand,
        right: Operand,
    },
    CheckedOp {
        check: u32,
        kind: CheckOpKind,
        operands: Vec<Operand>,
    },
    Compare {
        op: CompareOp,
        left: Operand,
        right: Operand,
    },
    Aggregate {
        kind: AggregateKind,
        operands: Vec<Operand>,
    },
    Repeat {
        operand: Operand,
        count: u64,
    },
    Discriminant(Place),
    Len(Place),
    Ref(Place),
    RawAddress(Place),
    Cast {
        kind: CastKind,
        operand: Operand,
        ty: TypeId,
    },
    FunctionValue(MonoCandidate),
    DynErase {
        operand: Operand,
        ty: TypeId,
    },
    ValueCopy(Place),
    CowSnapshot(Place),
    AllocObject {
        ty: TypeId,
        operands: Vec<Operand>,
    },
    AllocArray {
        element: TypeId,
        length: Operand,
    },
    StackSlotAddress(LocalId),
    Intrinsic {
        op: IntrinsicOp,
        operands: Vec<Operand>,
        types: Vec<TypeId>,
    },
}

/// compiler 内部 intrinsic 闭集。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum IntrinsicOp {
    SizeOf,
    AlignOf,
    OffsetOf {
        field: u32,
    },
    TypeId,
    TypeName,
    Is,
    Downcast,
    DowncastCopy,
    /// 无缓冲/有缓冲 channel 创建。
    ChanNew,
    ChanClose,
    /// `static`/`local static` 存储引用；`Deref` 后形成 place。
    StaticRef(DefId),
    /// 启动 `async` 协程；operands 为 callee/接收者/实参或协程环境。
    Spawn(SpawnTarget),
    /// 动态范围子切片：operands = [base, start, end]。
    Subslice {
        slice: bool,
    },
    /// 字符串插值；`parts` 引用 HIR owner 的 string_parts 池。
    Format {
        parts: Range<u32>,
    },
    /// 内联汇编计划（HIR owner 的 assembly 编号）。
    Asm(u32),
    PtrRead,
    PtrWrite,
    ReadUnaligned,
    WriteUnaligned,
    UninitAsPtr,
    UninitWrite,
    /// 每帧 defer 链：压入一条记录，operands = [head, env...]，返回新头。
    DeferChainPush {
        action: u32,
    },
    /// 读取链头记录的 action 编号。
    DeferChainAction,
    /// 读取链头记录的环境聚合。
    DeferChainEnv {
        action: u32,
    },
    /// 弹出链头记录，返回下一个头。
    DeferChainPop,
    /// 空链句柄。
    DeferChainEmpty,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum SpawnTarget {
    Body(DefId),
    Callee(Callee),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ValueActionKind {
    Copy,
    Publish,
    Drop,
    Forget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ResourceActionKind {
    AcquireLease,
    ReleaseLease,
    Transfer,
    Finalize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ViewMode {
    ScopedRead,
    ScopedWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum NoSafepointReason {
    RuntimeLock,
    OwnershipPublish,
    RootPublish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum MemoryOrdering {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum AtomicOp {
    Load,
    Store,
    Rmw,
    CompareExchange,
    Fence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum VolatileOp {
    Load,
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum StatementKind {
    StorageLive(LocalId),
    StorageDead(LocalId),
    Assign(Place, Rvalue),
    SetDiscriminant {
        place: Place,
        variant: u32,
    },
    ValueAction {
        action: ValueActionKind,
        place: Place,
        descriptor: TypeId,
    },
    ResourceAction {
        action: ResourceActionKind,
        place: Place,
        descriptor: TypeId,
    },
    GcWrite {
        owner: Place,
        destination: Place,
        value: Operand,
    },
    Pin {
        place: Place,
        token: LocalId,
    },
    Unpin {
        token: LocalId,
    },
    ScopedViewBegin {
        source: Place,
        mode: ViewMode,
        token: LocalId,
    },
    ScopedViewEnd {
        token: LocalId,
    },
    SafepointPoll(SafepointId),
    StackCheck,
    NoSafepointBegin(NoSafepointRegionId),
    NoSafepointEnd(NoSafepointRegionId),
    Atomic {
        op: AtomicOp,
        ordering: MemoryOrdering,
        pointer: Option<Operand>,
        operands: Vec<Operand>,
        destination: Option<Place>,
    },
    Volatile {
        op: VolatileOp,
        pointer: Operand,
        value: Option<Operand>,
        destination: Option<Place>,
    },
    CoverageCounter(u32),
    Nop,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Callee {
    /// 一等函数值。
    Value(Operand),
    /// HIR 已选择的静态派发（owner dispatch 编号）；具体实例在单态化绑定。
    Dispatch(u32),
    /// HIR 已选择的动态派发（owner dispatch 编号）；保留 vtable 槽。
    Dynamic(u32),
    /// 编译器内建操作（HIR builtin 调用目标）。
    Builtin(hir::Builtin),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CallKind {
    Managed,
    ForeignBridge,
    ForeignBridgeDirtyCpu,
    ForeignLeaf { stack: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum SuspendReason {
    ChanSend { channel: Operand, value: Operand },
    ChanRecv { channel: Operand },
    JoinWait { join: Operand },
    Yield,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum SelectOperation {
    Send { channel: Operand, value: Operand },
    Recv { channel: Operand },
    Wait { join: Operand },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SelectCase {
    pub(crate) operation: SelectOperation,
    /// 提交结果写入的槽；`Send` 为 `None`。
    pub(crate) destination: Option<Place>,
    /// HIR select arm 下标（保持源码臂优先级）。
    pub(crate) arm: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum SafepointKind {
    Suspend,
    Select,
    Poll,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Safepoint {
    pub(crate) kind: SafepointKind,
    pub(crate) location: Location,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Terminator {
    Goto {
        target: BlockId,
    },
    SwitchInt {
        value: Operand,
        targets: Vec<(u128, BlockId)>,
        otherwise: BlockId,
    },
    Call {
        callee: Callee,
        args: Vec<Operand>,
        destination: Place,
        normal: BlockId,
        unwind: Option<BlockId>,
        call_kind: CallKind,
        site: CallSite,
    },
    Return,
    Panic {
        payload: Operand,
        unwind: BlockId,
    },
    ResumePanic,
    Abort,
    Unreachable,
    Suspend {
        reason: SuspendReason,
        destination: Option<Place>,
        resume: BlockId,
        cancelled: Option<BlockId>,
        safepoint: SafepointId,
    },
    SelectCommit {
        cases: Range<u32>,
        index: LocalId,
        ready: BlockId,
        suspend: Option<BlockId>,
        cancelled: Option<BlockId>,
        safepoint: SafepointId,
    },
}

impl Terminator {
    /// 按固定顺序列出全部后继（含 unwind/cancelled 边）。
    pub(crate) fn successors(&self) -> Vec<BlockId> {
        match self {
            Self::Goto { target } => vec![*target],
            Self::SwitchInt {
                targets, otherwise, ..
            } => {
                let mut out: Vec<_> = targets.iter().map(|(_, block)| *block).collect();
                out.push(*otherwise);
                out
            }
            Self::Call { normal, unwind, .. } => {
                let mut out = vec![*normal];
                out.extend(*unwind);
                out
            }
            Self::Panic { unwind, .. } => vec![*unwind],
            Self::Return | Self::ResumePanic | Self::Abort | Self::Unreachable => Vec::new(),
            Self::Suspend {
                resume, cancelled, ..
            } => {
                let mut out = vec![*resume];
                out.extend(*cancelled);
                out
            }
            Self::SelectCommit {
                ready,
                suspend,
                cancelled,
                ..
            } => {
                let mut out = vec![*ready];
                out.extend(*suspend);
                out.extend(*cancelled);
                out
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum CleanupChain {
    Normal,
    Unwind,
}

/// 一个已注册 action 的 lowering 区域：进入 `entry`，正常完成从 `exit` 继续。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CleanupRegion {
    pub(crate) action: hir::CleanupAction,
    pub(crate) chain: CleanupChain,
    pub(crate) entry: BlockId,
    pub(crate) exit: BlockId,
    /// 同一条出口链上的物化顺序；verifier 按此重建动作序列。
    pub(crate) order: u32,
}

/// 一个 HIR 出口与其 GIR cleanup 入口的对应；verifier 逐条重建动作序列并与 HIR 计划比较。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExitRecord {
    pub(crate) plan: u32,
    pub(crate) chain: CleanupChain,
    pub(crate) entry: BlockId,
    /// 正常链完成后的落点；unwind 链为 `None`（以 `ResumePanic` 结束）。
    pub(crate) destination: Option<BlockId>,
}

impl GirBody {
    pub(crate) fn block_statements(&self, block: BlockId) -> &[Statement] {
        let range = &self.blocks[block.index()].statements;
        &self.statements[range.start as usize..range.end as usize]
    }
    pub(crate) fn projections_of(&self, place: Place) -> &[Projection] {
        &self.projections[place.range()]
    }
    pub(crate) fn predecessors_of(&self, block: BlockId) -> &[BlockId] {
        let range = &self.blocks[block.index()].predecessors;
        &self.predecessors[range.start as usize..range.end as usize]
    }
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let bytes = serde_json::to_vec(self).expect("GIR schema 序列化");
        *blake3::Hasher::new_derive_key("gugu-generic-gir-v1")
            .update(&bytes)
            .finalize()
            .as_bytes()
    }
}
