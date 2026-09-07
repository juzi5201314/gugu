use super::{DefId, ExprId, LocalId, Location, PatternId, ScopeId, StmtId, TraitRef, TypeId};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub(crate) definition: DefId,
    pub(crate) parameters: Vec<PatternId>,
    pub(crate) body: ExprId,
    pub(crate) expressions: Vec<Expression>,
    pub(crate) expression_types: Vec<TypeId>,
    pub(crate) expression_inputs: Vec<TypeId>,
    pub(crate) statements: Vec<Statement>,
    pub(crate) patterns: Vec<Pattern>,
    pub(crate) locals: Vec<Local>,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) expression_ids: Vec<ExprId>,
    pub(crate) statement_ids: Vec<StmtId>,
    pub(crate) pattern_ids: Vec<PatternId>,
    pub(crate) scope_ids: Vec<ScopeId>,
    pub(crate) arms: Vec<MatchArm>,
    pub(crate) select_arms: Vec<SelectArm>,
    pub(crate) fields: Vec<FieldValue>,
    pub(crate) pattern_fields: Vec<PatternField>,
    pub(crate) string_parts: Vec<StringPart>,
    pub(crate) dispatches: Vec<Dispatch>,
    pub(crate) adjustments: Vec<Adjustment>,
    pub(crate) checks: Vec<RuntimeCheck>,
    pub(crate) captures: Vec<Capture>,
    pub(crate) variadic_calls: Vec<VariadicCall>,
    pub(crate) borrow_constraints: Vec<BorrowConstraint>,
    pub(crate) foreign_calls: Vec<ForeignCall>,
    pub(crate) cleanup: Vec<Cleanup>,
    pub(crate) assembly: Vec<Assembly>,
    pub(crate) input_fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Expression {
    pub(crate) location: Location,
    pub(crate) scope: ScopeId,
    pub(crate) kind: ExprKind,
    pub(crate) adjustments: Range<u32>,
    pub(crate) effects: Effects,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ExprKind {
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
pub(crate) enum Res {
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
pub(crate) enum Builtin {
    Panic,
    Some,
    None,
    Ok,
    Err,
    ChanSend,
    ChanRecv,
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
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Literal {
    Integer(u128),
    Float(u64),
    Bool(bool),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    CString(Vec<u8>),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CallTarget {
    Value(ExprId),
    Dispatch(u32),
    Builtin(Builtin),
    Constructor { ty: TypeId, variant: u32 },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Dispatch {
    pub(crate) function: Option<DefId>,
    pub(crate) implementation: Option<DefId>,
    pub(crate) interface: Option<TraitRef>,
    pub(crate) member: Option<u32>,
    pub(crate) dereferences: u32,
    pub(crate) borrow: bool,
    pub(crate) implicit_receiver: bool,
    pub(crate) self_ty: TypeId,
    pub(crate) signature: TypeId,
    pub(crate) dynamic: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Statement {
    pub(crate) location: Location,
    pub(crate) scope: ScopeId,
    pub(crate) kind: StatementKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum StatementKind {
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
pub(crate) struct Pattern {
    pub(crate) location: Location,
    pub(crate) ty: TypeId,
    pub(crate) kind: PatternKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PatternKind {
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
pub(crate) struct Local {
    pub(crate) name: String,
    pub(crate) ty: TypeId,
    pub(crate) location: Location,
    pub(crate) storage: u8,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Capture {
    pub(crate) local: LocalId,
    pub(crate) owner: DefId,
    pub(crate) source: LocalId,
    pub(crate) read_before_write: bool,
    pub(crate) written: bool,
    pub(crate) coroutine: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Scope {
    pub(crate) parent: Option<ScopeId>,
    pub(crate) kind: ScopeKind,
    pub(crate) location: Location,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ScopeKind {
    Function,
    Block,
    Loop,
    Try,
    Branch,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ExitTarget {
    Return,
    Break(ScopeId),
    Continue(ScopeId),
    Try(ScopeId),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cleanup {
    pub(crate) statement: StmtId,
    pub(crate) body: ExprId,
    pub(crate) scope: ScopeId,
    pub(crate) function_exit: bool,
    pub(crate) captures: Vec<LocalId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Adjustment {
    Dereference,
    ArrayToSlice(TypeId),
    NeverTo(TypeId),
    Erase(TypeId),
    Opaque(TypeId),
    Instantiate(TypeId),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Effects(pub(crate) u32);
impl Effects {
    pub(crate) const PANIC: u32 = 1;
    pub(crate) const ALLOCATE: u32 = 2;
    pub(crate) const SAFEPOINT: u32 = 4;
    pub(crate) const SUSPEND: u32 = 8;
    pub(crate) const READ: u32 = 16;
    pub(crate) const WRITE: u32 = 32;
    pub(crate) const FOREIGN: u32 = 64;
    pub(crate) const UNSAFE: u32 = 128;
    pub(crate) const KNOWN: u32 = 255;
    pub(crate) fn new(bits: u32) -> Self {
        debug_assert_eq!(bits & !Self::KNOWN, 0, "效果集合只有八个布尔标志");
        Self(bits)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct RuntimeCheck {
    pub(crate) expression: ExprId,
    pub(crate) kind: CheckKind,
    pub(crate) proof: Option<crate::frontend::semantics::analysis::ProofStatus>,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum CheckKind {
    Division {
        ty: TypeId,
        divisor: ExprId,
    },
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
pub(crate) struct VariadicCall {
    pub(crate) expression: ExprId,
    pub(crate) fixed_count: u32,
    pub(crate) element: TypeId,
    pub(crate) heterogeneous: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ForeignCall {
    pub(crate) expression: ExprId,
    pub(crate) effect: super::super::semantics::foreign::ForeignEffect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BorrowConstraint {
    pub(crate) expression: ExprId,
    pub(crate) base: TypeId,
    pub(crate) projection: Vec<super::super::semantics::borrow::Projection>,
    pub(crate) target: TypeId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MatchArm {
    pub(crate) pattern: PatternId,
    pub(crate) guard: Option<ExprId>,
    pub(crate) body: ExprId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum SelectArm {
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
pub(crate) struct FieldValue {
    pub(crate) field: u32,
    pub(crate) value: ExprId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PatternField {
    pub(crate) field: u32,
    pub(crate) pattern: PatternId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum StringPart {
    Text(String),
    Value {
        expression: ExprId,
        format: super::super::string::FormatSpec<FormatCount>,
        dispatch: Option<u32>,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum FormatCount {
    Fixed(u64),
    Value(ExprId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Assembly {
    pub(crate) template: String,
    pub(crate) context: super::super::semantics::assembly::AssemblyContext,
    pub(crate) operands: Vec<AssemblyOperand>,
    pub(crate) clobbers: u64,
    pub(crate) stack_reserve: Option<u64>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AssemblyOperand {
    pub(crate) register: super::super::semantics::assembly::Register,
    pub(crate) direction: super::super::semantics::assembly::Direction,
    pub(crate) value: ExprId,
}
