use super::{DefId, ExprId, LocalId, Location, PatternId, ScopeId, StmtId, TraitRef, TypeId};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub definition: DefId,
    pub parameters: Vec<PatternId>,
    pub body: ExprId,
    pub expressions: Vec<Expression>,
    pub expression_types: Vec<TypeId>,
    pub expression_inputs: Vec<TypeId>,
    pub statements: Vec<Statement>,
    pub patterns: Vec<Pattern>,
    pub locals: Vec<Local>,
    pub scopes: Vec<Scope>,
    pub expression_ids: Vec<ExprId>,
    pub statement_ids: Vec<StmtId>,
    pub pattern_ids: Vec<PatternId>,
    pub scope_ids: Vec<ScopeId>,
    pub arms: Vec<MatchArm>,
    pub select_arms: Vec<SelectArm>,
    pub fields: Vec<FieldValue>,
    pub pattern_fields: Vec<PatternField>,
    pub string_parts: Vec<StringPart>,
    pub dispatches: Vec<Dispatch>,
    pub adjustments: Vec<Adjustment>,
    pub checks: Vec<RuntimeCheck>,
    pub captures: Vec<Capture>,
    pub variadic_calls: Vec<VariadicCall>,
    pub borrow_constraints: Vec<BorrowConstraint>,
    pub foreign_calls: Vec<ForeignCall>,
    pub cleanup: Vec<Cleanup>,
    pub cleanup_plans: Vec<CleanupPlan>,
    pub cleanup_actions: Vec<CleanupAction>,
    /// 函数体正常完成后的 `Return` 计划（覆盖全部已注册 action）。
    pub return_plan: u32,
    pub assembly: Vec<Assembly>,
    pub input_fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Expression {
    pub location: Location,
    pub scope: ScopeId,
    /// 子表达式求值后、执行当前操作时已注册的清理动作。
    pub unwind_plan: u32,
    pub kind: ExprKind,
    pub adjustments: Range<u32>,
    pub effects: Effects,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExprKind {
    Resolved(Res),
    Literal(Literal),
    Tuple(Range<u32>),
    Array(Range<u32>),
    Repeat {
        value: ExprId,
        count: u64,
    },
    Construct {
        variant: u32,
        fields: Range<u32>,
    },
    Block {
        statements: Range<u32>,
        tail: Option<ExprId>,
        /// 该块作用域正常结束时执行的 `BlockEnd` 计划；没有块 defer 时为 `None`。
        end_plan: Option<u32>,
    },
    If {
        condition: ExprId,
        then_value: ExprId,
        else_value: Option<ExprId>,
    },
    Match {
        value: ExprId,
        arms: Range<u32>,
    },
    Loop {
        body: ExprId,
    },
    While {
        condition: ExprId,
        body: ExprId,
    },
    For {
        pattern: PatternId,
        value: ExprId,
        body: ExprId,
        into_iter: Option<u32>,
        next: Option<u32>,
    },
    Try {
        body: ExprId,
        from_value: Option<u32>,
    },
    TryExit {
        value: ExprId,
        branch: Option<u32>,
        from_error: Option<u32>,
        target: ExitTarget,
        cleanup: Range<u32>,
        plan: u32,
    },
    Select {
        arms: Range<u32>,
    },
    Closure {
        definition: DefId,
    },
    Spawn {
        definition: DefId,
    },
    SpawnCall {
        target: CallTarget,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
    },
    Call {
        target: CallTarget,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
    },
    Intrinsic {
        operation: Builtin,
        arguments: Range<u32>,
        types: Vec<TypeId>,
        field: Option<u32>,
    },
    Field {
        base: ExprId,
        index: u32,
    },
    Index {
        base: ExprId,
        index: ExprId,
        read: Option<u32>,
        write: Option<u32>,
    },
    Slice {
        base: ExprId,
        start: Option<ExprId>,
        end: Option<ExprId>,
    },
    Unary {
        operation: super::super::ast::UnOp,
        value: ExprId,
    },
    Binary {
        operation: super::super::ast::BinOp,
        left: ExprId,
        right: ExprId,
        dispatch: Option<u32>,
    },
    Range {
        start: ExprId,
        end: ExprId,
    },
    Comptime {
        value: ExprId,
    },
    Assembly(u32),
    Exit {
        target: ExitTarget,
        value: Option<ExprId>,
        cleanup: Range<u32>,
        plan: u32,
    },
    String {
        parts: Range<u32>,
    },
    LetCondition {
        pattern: PatternId,
        value: ExprId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Res {
    Def(DefId),
    Local(LocalId),
    Primitive(TypeId),
    Builtin(Builtin),
    Associated {
        definition: DefId,
        self_ty: TypeId,
        interface: Option<TraitRef>,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Builtin {
    Panic,
    Some,
    None,
    Ok,
    Err,
    ChanSend,
    ChanRecv,
    ChanTrySend,
    ChanTryRecv,
    ChanClose,
    JoinWait,
    SizeOf,
    AlignOf,
    OffsetOf,
    TypeId,
    TypeIdCount,
    Chan,
    Is,
    Downcast,
    DowncastCopy,
    TypeName,
    TypeAsInt,
    Len,
    Memory(super::super::semantics::model::MemoryIntrinsic),
    Runtime(super::super::semantics::model::RuntimeIntrinsic),
    RuntimeMachine(super::super::semantics::model::RuntimeMachineIntrinsic),
    Platform(super::super::semantics::model::PlatformIntrinsic),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Integer(u128),
    Float(u64),
    Bool(bool),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    CString(Vec<u8>),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CallTarget {
    Value(ExprId),
    Dispatch(u32),
    Builtin(Builtin),
    Constructor { ty: TypeId, variant: u32 },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Dispatch {
    pub function: Option<DefId>,
    pub implementation: Option<DefId>,
    pub interface: Option<TraitRef>,
    pub member: Option<u32>,
    /// 接口成员规范名；内建 trait 方法没有函数定义，低层按名字展开。
    pub member_name: Option<String>,
    pub dereferences: u32,
    pub borrow: bool,
    pub implicit_receiver: bool,
    pub self_ty: TypeId,
    pub signature: TypeId,
    pub dynamic: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub location: Location,
    pub scope: ScopeId,
    pub unwind_plan: u32,
    pub kind: StatementKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StatementKind {
    Let {
        pattern: PatternId,
        value: Option<ExprId>,
        otherwise: Option<ExprId>,
    },
    Static {
        local: LocalId,
        definition: DefId,
    },
    Assign {
        place: ExprId,
        value: ExprId,
        operation: super::super::ast::AssignOp,
        dispatch: Option<u32>,
    },
    Defer(u32),
    Yield,
    Expression(ExprId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    pub location: Location,
    pub ty: TypeId,
    pub kind: PatternKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PatternKind {
    Wildcard,
    Bind(LocalId),
    Literal(Literal),
    Ref(PatternId),
    Range {
        start: Literal,
        end: Literal,
    },
    Tuple(Range<u32>),
    Array {
        prefix: Range<u32>,
        rest: Option<LocalId>,
        has_rest: bool,
        suffix: Range<u32>,
    },
    Construct {
        variant: u32,
        fields: Range<u32>,
    },
    Or(Range<u32>),
    At {
        local: LocalId,
        pattern: PatternId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Local {
    pub name: String,
    pub ty: TypeId,
    pub location: Location,
    pub storage: u8,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub local: LocalId,
    pub owner: DefId,
    pub source: LocalId,
    pub read_before_write: bool,
    pub written: bool,
    pub coroutine: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind,
    pub location: Location,
    /// 进入该作用域、尚未注册任何新 action 时的 `Unwind` 计划。
    pub unwind_plan: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScopeKind {
    Function,
    Block,
    Loop,
    Try,
    Branch,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExitTarget {
    Return,
    Break(ScopeId),
    Continue(ScopeId),
    Try(ScopeId),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cleanup {
    pub statement: StmtId,
    pub body: ExprId,
    pub scope: ScopeId,
    pub function_exit: bool,
    pub captures: Vec<LocalId>,
    /// 注册点到函数作用域之间的控制结构决定的注册表示。
    pub registration: Registration,
    /// 该 action 注册完成后立即生效的 `Unwind` 计划。
    pub unwind_plan: u32,
}

/// 函数出口 action 的注册表示；块 action 总是 `Static`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Registration {
    /// 注册点与目标作用域之间只有 Block 作用域：到达出口时必然已注册。
    Static,
    /// 注册点位于分支或 try 之下：出口按运行时注册标志决定是否执行。
    Flag,
    /// 注册点位于循环之下：每轮注册一次，出口按每帧 defer 链 LIFO 消费。
    Chain,
}

/// 一个控制流出口的完整清理计划：动作已按规范排序，GIR 只能 intern 与连接。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub exit: ExitKind,
    pub actions: Range<u32>,
    /// 计划执行完毕后控制到达的作用域；离开函数（`Return`/`Unwind`）为 `None`。
    pub destination: Option<ScopeId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ExitKind {
    Return,
    Break(ScopeId),
    Continue(ScopeId),
    Try(ScopeId),
    /// 块作用域正常结束。
    BlockEnd(ScopeId),
    /// 从该作用域内的程序点开始 panic 展开。
    Unwind(ScopeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum CleanupAction {
    /// 执行一个已注册 action；`Flag` 注册在运行时按标志守卫。
    Action { cleanup: u32, guard: Registration },
    /// 按 LIFO 消费 defer 链上比 `until` 站点注册更晚的记录；`None` 消费到链底。
    DrainChain { until: Option<u32> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Adjustment {
    Dereference,
    ArrayToSlice(TypeId),
    NeverTo(TypeId),
    Erase(TypeId),
    Opaque(TypeId),
    Instantiate(TypeId),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Effects(pub u32);
impl Effects {
    pub const PANIC: u32 = 1;
    pub const ALLOCATE: u32 = 2;
    pub const SAFEPOINT: u32 = 4;
    pub const SUSPEND: u32 = 8;
    pub const READ: u32 = 16;
    pub const WRITE: u32 = 32;
    pub const FOREIGN: u32 = 64;
    pub const UNSAFE: u32 = 128;
    pub const KNOWN: u32 = 255;
    pub fn new(bits: u32) -> Self {
        debug_assert_eq!(bits & !Self::KNOWN, 0, "效果集合只有八个布尔标志");
        Self(bits)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct RuntimeCheck {
    pub expression: ExprId,
    pub kind: CheckKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum CheckKind {
    Division {
        ty: TypeId,
        divisor: ExprId,
    },
    /// `ty` 是移位量类型，用于判定有符号性。
    Shift {
        ty: TypeId,
        amount: ExprId,
    },
    Bounds {
        slice: bool,
    },
    Utf8Boundary,
    FloatToInt {
        signed: bool,
        bits: u16,
        value: ExprId,
    },
    UnicodeScalar {
        value: ExprId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VariadicCall {
    pub expression: ExprId,
    pub fixed_count: u32,
    pub element: TypeId,
    pub heterogeneous: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForeignCall {
    pub expression: ExprId,
    pub effect: super::super::semantics::foreign::ForeignEffect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BorrowConstraint {
    pub expression: ExprId,
    pub base: TypeId,
    pub projection: Vec<super::super::semantics::borrow::Projection>,
    pub target: TypeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MatchArm {
    pub pattern: PatternId,
    pub guard: Option<ExprId>,
    pub body: ExprId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SelectArm {
    Send {
        channel: ExprId,
        value: ExprId,
        body: ExprId,
    },
    Recv {
        channel: ExprId,
        pattern: PatternId,
        body: ExprId,
    },
    Wait {
        join: ExprId,
        pattern: PatternId,
        body: ExprId,
    },
    Default {
        body: ExprId,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FieldValue {
    pub field: u32,
    pub value: ExprId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PatternField {
    pub field: u32,
    pub pattern: PatternId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StringPart {
    Text(String),
    Value {
        expression: ExprId,
        format: super::super::string::FormatSpec<FormatCount>,
        dispatch: Option<u32>,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FormatCount {
    Fixed(u64),
    Value(ExprId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Assembly {
    pub template: String,
    pub context: super::super::semantics::assembly::AssemblyContext,
    pub operands: Vec<AssemblyOperand>,
    pub clobbers: u64,
    pub stack_reserve: Option<u64>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssemblyOperand {
    pub register: super::super::semantics::assembly::Register,
    pub direction: super::super::semantics::assembly::Direction,
    pub value: ExprId,
}
