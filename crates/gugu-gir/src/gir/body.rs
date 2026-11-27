//! generic GIR 的 body 表示：稠密 `u32` ID、连续 arena、封闭的 place/operand/rvalue/statement/terminator。
//!
//! 类型使用 HIR 模块类型表的 `TypeId`（generic GIR 允许引用 owner 的泛型参数）；
//! 常量驻留在 body 级常量池；投影驻留在 body 级投影池，`Place` 只保存 base 与 range。
use crate::hir::{self, DefId, Location, TypeId};
use crate::mono::instantiate::CallSite;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// GIR schema revision（见 gir-lir.md「共同表示规则」）。
pub const GIR_REVISION: u32 = 10;

macro_rules! ids {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        pub struct $name(pub u32);
        impl $name {
            pub fn index(self) -> usize {
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
pub struct BodyFlags;
impl BodyFlags {
    pub const PANIC: u32 = 1;
    pub const SUSPEND: u32 = 2;
    pub const ALLOCATE: u32 = 4;
    pub const UNSAFE: u32 = 8;
    pub const RUNTIME_GLUE: u32 = 16;
    pub const FOREIGN: u32 = 32;
    /// `#[track_caller]` owner：末尾三个 Argument local 是调用点隐藏位置实参。
    pub const TRACK_CALLER: u32 = 64;
    pub const KNOWN: u32 = 127;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BodyKind {
    Function,
    Closure,
    Async,
    StaticInit,
    GlobalAsm,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    pub parameters: Vec<TypeId>,
    pub result: TypeId,
    /// HIR `Effects` 位的并集。
    pub effects: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GirBody {
    pub owner: DefId,
    pub owner_key: [u8; 32],
    pub kind: BodyKind,
    pub signature: Signature,
    pub generic_params: u32,
    pub locals: Vec<GirLocal>,
    pub blocks: Vec<GirBlock>,
    pub statements: Vec<Statement>,
    pub predecessors: Vec<BlockId>,
    pub projections: Vec<Projection>,
    pub constants: Vec<Constant>,
    pub source_scopes: Vec<SourceScope>,
    pub cleanup_regions: Vec<CleanupRegion>,
    pub exit_records: Vec<ExitRecord>,
    pub safepoints: Vec<Safepoint>,
    pub no_safepoint_regions: Vec<NoSafepointReason>,
    pub select_cases: Vec<SelectCase>,
    /// 每个 HIR 表达式对应的结果 local；发散表达式为 `None`。
    pub expression_locals: Vec<Option<LocalId>>,
    /// 已选择 match 叶：`(进入叶的 block, HIR arm 行号)`。
    pub match_leaves: Vec<(BlockId, u32)>,
    /// 按值复制超过 64 字节的位结构体；lint 在 query 外按属性求值。
    pub large_copies: Vec<LargeCopySite>,
    pub flags: u32,
    pub revision: u32,
    pub entry: BlockId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LargeCopySite {
    pub location: hir::Location,
    pub size: u64,
    pub ty: TypeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LocalKind {
    Return,
    Argument,
    User,
    Temporary,
    SpillCandidate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GirLocal {
    pub element_initialization: bool,
    pub ty: TypeId,
    pub kind: LocalKind,
    pub mutable: bool,
    pub address_taken: bool,
    pub source_scope: ScopeId,
    /// 对应的 HIR 局部槽（用户 local）；临时值为 `None`。
    pub hir_local: Option<hir::LocalId>,
    /// 捕获或跨协程槽：`StorageDead` 推迟到函数出口。
    pub pinned_storage: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceScope {
    pub parent: Option<ScopeId>,
    pub location: Location,
    pub hir_scope: hir::ScopeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub location: Location,
    pub scope: ScopeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GirBlock {
    pub statements: Range<u32>,
    pub terminator: Terminator,
    pub source: SourceInfo,
    pub predecessors: Range<u32>,
    /// cleanup block 只能由 cleanup 边或其它 cleanup block 到达。
    pub cleanup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub kind: StatementKind,
    pub source: SourceInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub local: LocalId,
    pub projections: (u32, u32),
}

impl Place {
    pub fn local(local: LocalId) -> Self {
        Self {
            local,
            projections: (0, 0),
        }
    }
    pub fn range(self) -> Range<usize> {
        self.projections.0 as usize..self.projections.1 as usize
    }
    pub fn is_local(self) -> bool {
        self.projections.0 == self.projections.1
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Access {
    Normal,
    /// scoped borrowed view 的只读投影；不能形成写入 place。
    ScopedRead,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Projection {
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
pub struct MonoCandidate {
    pub definition: DefId,
    pub signature: TypeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Operand {
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
pub struct Constant {
    pub ty: TypeId,
    pub value: ConstValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConstValue {
    Unit,
    Bool(bool),
    Integer(u128),
    Float(u64),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    CString(Vec<u8>),
    Aggregate(Vec<ConstValue>),
    Type([u8; 32]),
    /// 模块级常量或关联常量项；单态化物化其规范值。
    Definition(DefId),
    Parameter(TypeId),
    /// 非法但类型正确的占位不存在：`Never` 只用于 `!` 类型槽的初始化标记。
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BinaryOp {
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
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// 真实 panic 条件；结果为 `bool`（true 表示检查通过）。`check` 是 HIR `RuntimeCheck` 编号。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CheckOpKind {
    Division { ty: TypeId },
    Shift { ty: TypeId },
    Bounds { slice: bool },
    Utf8Boundary,
    FloatToInt { signed: bool, bits: u16 },
    UnicodeScalar,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AggregateKind {
    Tuple,
    Array(TypeId),
    Adt {
        ty: TypeId,
        variant: u32,
    },
    /// union 构造只初始化一个字段；其余字节未初始化。
    Union {
        ty: TypeId,
        field: u32,
    },
    Range,
    Closure(DefId),
    Coroutine(DefId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CastKind {
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
pub enum Rvalue {
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
        count: hir::ArrayLength,
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
pub enum IntrinsicOp {
    PackFor,
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
    /// 非挂起的尝试发送/接收。
    ChanTrySend,
    ChanTryRecv,
    /// `static`/`local static` 存储引用；`Deref` 后形成 place。
    StaticRef(DefId),
    /// `static` 值槽的 initialized 字（块首 8 字节）；operands 为空，结果为整数值。
    ///
    /// 读点守卫据此判断值是否已发布：0 表示需要先调用初始化器，非 0 表示值槽已完整。
    StaticInitialized(DefId),
    /// 启动 `async` 协程；operands 为 callee/接收者/实参或协程环境。
    Spawn(SpawnTarget),
    /// 动态范围子切片：operands = [base, start, end]。
    Subslice {
        slice: bool,
    },
    /// 齐次变参尾：把 operands 物化为 `&[T]`，`types = [T]`，长度由 operands 数量决定。
    PackSlice,
    /// 内联汇编计划（HIR owner 的 assembly 编号）。
    Asm(u32),
    PtrRead,
    PtrWrite,
    ReadUnaligned,
    WriteUnaligned,
    UninitAsPtr,
    UninitWrite,
    /// `std.mem.LocalArena`/`SyncArena` 的 `new`/`with_capacity`：operands = [capacity]
    /// （`new` 折叠为 0 常量），结果是与句柄同形（单个 `&byte` 字段）的值；`sync`
    /// 标记是否走并发安全的共享分配路径。
    ArenaNew {
        sync: bool,
    },
    /// `alloc[T]`：operands = [receiver_ref, value]，types = [T]；LIR 负责
    /// resource 字段拒绝、区域写头与值落槽，结果 `&T`。
    ArenaAlloc {
        sync: bool,
    },
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
    /// string 与只读字节视图共用 backing；输入只能来自受限 runtime 原语。
    StringBytes,
    StringFromUtf8Unchecked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SpawnTarget {
    Body(DefId),
    Callee(Callee),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ValueActionKind {
    Copy,
    Publish,
    Drop,
    Forget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResourceActionKind {
    AcquireLease,
    ReleaseLease,
    Transfer,
    Finalize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ViewMode {
    ScopedRead,
    ScopedWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum NoSafepointReason {
    RuntimeLock,
    OwnershipPublish,
    RootPublish,
    /// 平台范围调用；它可能睡眠或撤销物理页，因此同样禁止进入无 safepoint 区域。
    PlatformCall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MemoryOrdering {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AtomicOp {
    Load,
    Store,
    Rmw,
    FetchOr,
    FetchAnd,
    CompareExchange,
    Fence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum VolatileOp {
    Load,
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StatementKind {
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
    ScopedViewBegin {
        source: Place,
        mode: ViewMode,
        token: LocalId,
    },
    ScopedViewEnd {
        token: LocalId,
    },
    /// 平台范围原语；实参与结果都已定型，region 与调用点由 verifier 检查。
    PlatformCall {
        op: crate::runtime::PlatformOp,
        operands: Vec<Operand>,
        destination: Option<Place>,
    },
    /// runtime 机器原语；实参与结果都已定型，glue 绑定由 ABI 校验与镜像写出阶段检查。
    RuntimeMachineCall {
        op: crate::runtime::RuntimeMachineOp,
        operands: Vec<Operand>,
        destination: Option<Place>,
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
pub enum Callee {
    /// 一等函数值。
    Value(Operand),
    /// HIR 已选择的静态派发（owner dispatch 编号）；具体实例在单态化绑定。
    Dispatch(u32),
    /// HIR 已选择的动态派发（owner dispatch 编号）；保留 vtable 槽。
    Dynamic(u32),
    /// 编译器内建操作（HIR builtin 调用目标）。
    Builtin(hir::Builtin),
    /// 静态初始化器：按定义绑定唯一实例，调用位点用 `CallSite::Initializer`/`StaticInit` 登记。
    ///
    /// 初始化器没有实参，也不捕获环境；守卫在值槽未发布时调用它，返回后位字已 release 置位。
    StaticInit(hir::DefId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CallKind {
    Managed,
    ForeignBridge,
    ForeignBridgeDirtyCpu,
    ForeignLeaf { stack: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SuspendReason {
    ChanSend { channel: Operand, value: Operand },
    ChanRecv { channel: Operand },
    JoinWait { join: Operand },
    Yield,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SelectOperation {
    Send { channel: Operand, value: Operand },
    Recv { channel: Operand },
    Wait { join: Operand },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectCase {
    pub operation: SelectOperation,
    /// 提交结果写入的槽；`Send` 为 `None`。
    pub destination: Option<Place>,
    /// HIR select arm 下标（保持源码臂优先级）。
    pub arm: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SafepointKind {
    Suspend,
    Select,
    Poll,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Safepoint {
    pub kind: SafepointKind,
    pub location: Location,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Terminator {
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
        location: hir::Location,
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
    /// 按固定顺序遍历全部后继（含 unwind/cancelled 边），不分配临时集合。
    #[inline]
    pub fn successors(&self) -> impl DoubleEndedIterator<Item = BlockId> + Clone + '_ {
        // 非 switch 终结符由类型定义保证至多三个直接后继；switch 借用原表，不限制分支数。
        let (direct, targets, otherwise) = match self {
            Self::Goto { target } => ([Some(*target), None, None], &[][..], None),
            Self::SwitchInt {
                targets, otherwise, ..
            } => ([None; 3], targets.as_slice(), Some(*otherwise)),
            Self::Call { normal, unwind, .. } => ([Some(*normal), *unwind, None], &[][..], None),
            Self::Panic { unwind, .. } => ([Some(*unwind), None, None], &[][..], None),
            Self::Return | Self::ResumePanic | Self::Abort | Self::Unreachable => {
                ([None; 3], &[][..], None)
            }
            Self::Suspend {
                resume, cancelled, ..
            } => ([Some(*resume), *cancelled, None], &[][..], None),
            Self::SelectCommit {
                ready,
                suspend,
                cancelled,
                ..
            } => ([Some(*ready), *suspend, *cancelled], &[][..], None),
        };
        direct
            .into_iter()
            .flatten()
            .chain(targets.iter().map(|(_, block)| *block))
            .chain(otherwise)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum CleanupChain {
    Normal,
    Unwind,
}

/// 一个已注册 action 的 lowering 区域：进入 `entry`，正常完成从 `exit` 继续。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CleanupRegion {
    pub action: hir::CleanupAction,
    pub chain: CleanupChain,
    pub entry: BlockId,
    pub exit: BlockId,
    /// 同一条出口链上的物化顺序；verifier 按此重建动作序列。
    pub order: u32,
}

/// 一个 HIR 出口与其 GIR cleanup 入口的对应；verifier 逐条重建动作序列并与 HIR 计划比较。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub plan: u32,
    pub chain: CleanupChain,
    pub entry: BlockId,
    /// 正常链完成后的落点；unwind 链为 `None`（以 `ResumePanic` 结束）。
    pub destination: Option<BlockId>,
}

impl GirBody {
    pub fn block_statements(&self, block: BlockId) -> &[Statement] {
        let range = &self.blocks[block.index()].statements;
        &self.statements[range.start as usize..range.end as usize]
    }
    pub fn projections_of(&self, place: Place) -> &[Projection] {
        &self.projections[place.range()]
    }
    pub fn predecessors_of(&self, block: BlockId) -> &[BlockId] {
        let range = &self.blocks[block.index()].predecessors;
        &self.predecessors[range.start as usize..range.end as usize]
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        crate::canonical::hash_derive_key("gugu-generic-gir-v1", self).expect("GIR schema 编码")
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockId, ConstId, LocalId, Operand, SafepointId, Terminator};

    #[test]
    fn switch_successors_preserve_order_duplicates_and_default() {
        let terminator = Terminator::SwitchInt {
            value: Operand::Constant(ConstId(0)),
            targets: vec![(1, BlockId(2)), (2, BlockId(1)), (3, BlockId(2))],
            otherwise: BlockId(0),
        };
        assert_eq!(
            terminator.successors().collect::<Vec<_>>(),
            [BlockId(2), BlockId(1), BlockId(2), BlockId(0)]
        );
    }

    #[test]
    fn select_keeps_cancellation_without_a_suspend_edge() {
        let terminator = Terminator::SelectCommit {
            cases: 0..0,
            index: LocalId(0),
            ready: BlockId(1),
            suspend: None,
            cancelled: Some(BlockId(2)),
            safepoint: SafepointId(0),
        };
        assert_eq!(
            terminator.successors().collect::<Vec<_>>(),
            [BlockId(1), BlockId(2)]
        );
    }
}
