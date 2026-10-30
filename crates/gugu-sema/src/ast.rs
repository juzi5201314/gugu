//! 稠密 AST arena：节点身份是文件内 `u32` 下标，不使用指针。
//!
//! 可变长度子项是同一 arena 内的连续范围。主要访问是按文件全量遍历和稠密点查，
//! 元素数有 `u32` 上界。

use std::marker::PhantomData;

use crate::source::{SourceFileId, Span};

use super::intern::Symbol;
use super::token::checked_u32;

/// 一个源文件内的语法节点身份。`local` 是 arena 分配序，不是指针。
/// 前缀 unary/paren 在相同起点时父节点先于子节点；中缀与后缀 wrap 在子节点之后分配父节点。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AstNodeId {
    pub file: SourceFileId,
    pub local: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ItemId(pub u32);
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExprId(pub u32);
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StmtId(pub u32);
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PatId(pub u32);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TyId(pub u32);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathId(pub u32);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FnId(pub u32);

/// 同一 arena 向量上的半开范围。
#[derive(Debug, Eq, PartialEq)]
pub struct AstRange<T> {
    pub start: u32,
    pub len: u32,
    pub _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for AstRange<T> {}
impl<T> Clone for AstRange<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> AstRange<T> {
    pub const fn empty() -> Self {
        Self {
            start: 0,
            len: 0,
            _marker: PhantomData,
        }
    }

    pub fn from_indices(start: usize, len: usize) -> Self {
        Self {
            start: checked_u32(start),
            len: checked_u32(len),
            _marker: PhantomData,
        }
    }

    pub fn as_slice<'a>(&self, data: &'a [T]) -> &'a [T] {
        let start = self.start as usize;
        &data[start..start + self.len as usize]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Visibility {
    Private,
    Pub,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttrKind {
    Outer { token_open: u32, token_close: u32 },
    Inner { token_open: u32, token_close: u32 },
    Doc { start: u32, end: u32 },
    InnerDoc { start: u32, end: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attribute {
    pub id: AstNodeId,
    pub span: Span,
    pub kind: AttrKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSegment {
    pub name: Symbol,
    pub span: Span,
    pub colon: bool,
    pub args: AstRange<GenericArg>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Path {
    pub id: AstNodeId,
    pub span: Span,
    pub segments: AstRange<PathSegment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenericArg {
    Type(TyId),
    Expr(ExprId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GenericParamKind {
    Type {
        name: Symbol,
        name_span: Span,
        bounds: AstRange<Bound>,
        pack: bool,
    },
    Comptime {
        name: Symbol,
        name_span: Span,
        ty: TyId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericParam {
    pub id: AstNodeId,
    pub span: Span,
    pub kind: GenericParamKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundKind {
    Path(PathId),
    Fn {
        params: AstRange<TyId>,
        ret: Option<TyId>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bound {
    pub id: AstNodeId,
    pub span: Span,
    pub kind: BoundKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Param {
    pub attributes: AstRange<Attribute>,
    pub id: AstNodeId,
    pub span: Span,
    pub comptime: bool,
    pub variadic: bool,
    pub variadic_name: Option<Symbol>,
    pub variadic_name_span: Option<Span>,
    pub pat: Option<PatId>,
    pub ty: Option<TyId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FnBody {
    Block(ExprId),
    Eq(ExprId),
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FnDecl {
    pub id: AstNodeId,
    pub span: Span,
    pub unsafety: bool,
    pub name: Option<Symbol>,
    pub name_span: Option<Span>,
    pub generics: AstRange<GenericParam>,
    pub params: AstRange<Param>,
    pub return_ty: Option<TyId>,
    pub body: FnBody,
    pub extern_abi: Option<Symbol>,
    pub extern_import: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    pub id: AstNodeId,
    pub span: Span,
    pub attributes: AstRange<Attribute>,
    pub visibility: Visibility,
    pub name: Option<Symbol>,
    pub name_span: Option<Span>,
    pub ty: TyId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VariantKind {
    Unit,
    Tuple(AstRange<Field>),
    Struct(AstRange<Field>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Variant {
    pub id: AstNodeId,
    pub span: Span,
    pub attributes: AstRange<Attribute>,
    pub name: Symbol,
    pub name_span: Span,
    pub kind: VariantKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructBody {
    Newtype(Field),
    Record(AstRange<Field>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UseTreeKind {
    Path {
        path: PathId,
        alias: Option<Symbol>,
    },
    Brace {
        path: PathId,
        items: AstRange<UseItem>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UseItem {
    pub attributes: AstRange<Attribute>,
    pub name: Symbol,
    pub alias: Option<Symbol>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ItemKind {
    Use(UseTreeKind),
    Function(FnId),
    Struct {
        generics: AstRange<GenericParam>,
        body: StructBody,
    },
    Enum {
        generics: AstRange<GenericParam>,
        variants: AstRange<Variant>,
    },
    Union {
        generics: AstRange<GenericParam>,
        fields: AstRange<Field>,
    },
    TypeAlias {
        generics: AstRange<GenericParam>,
        ty: Option<TyId>,
    },
    Const {
        ty: Option<TyId>,
        value: Option<ExprId>,
    },
    Static {
        ty: TyId,
        value: ExprId,
    },
    Trait {
        unsafety: bool,
        generics: AstRange<GenericParam>,
        items: AstRange<ItemId>,
    },
    Impl {
        negative: bool,
        unsafety: bool,
        generics: AstRange<GenericParam>,
        self_ty: TyId,
        trait_ty: Option<TyId>,
        items: AstRange<ItemId>,
    },
    ExternBlock {
        abi: Symbol,
        items: AstRange<ItemId>,
    },
    GlobalAsm {
        template: ExprId,
    },
    SourceMacro {
        body: ExprId,
    },
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    pub id: AstNodeId,
    pub span: Span,
    pub attributes: AstRange<Attribute>,
    pub visibility: Visibility,
    pub name: Option<Symbol>,
    pub name_span: Option<Span>,
    pub kind: ItemKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UnOp {
    Not,
    Neg,
    BitNot,
    Ref,
    Deref,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BinOp {
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
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AssignOp {
    Assign,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LitKind {
    Int { radix: u8, limbs: AstRange<u32> },
    Float { digits: Symbol, exp10: i32 },
    Bool(bool),
    Char { value: char, text: Symbol },
    ByteChar { value: u8, text: Symbol },
    String { text: Symbol },
    ByteString { text: Symbol },
    CString { text: Symbol },
    RawString { text: Symbol },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexKind {
    Expr(ExprId),
    Range {
        start: Option<ExprId>,
        end: Option<ExprId>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldExpr {
    pub attributes: AstRange<Attribute>,
    pub name: Symbol,
    pub span: Span,
    pub value: Option<ExprId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchArm {
    pub attributes: AstRange<Attribute>,
    pub id: AstNodeId,
    pub span: Span,
    pub pat: PatId,
    pub guard: Option<ExprId>,
    pub body: ExprId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectArmKind {
    Send {
        chan: ExprId,
        payload: ExprId,
        body: ExprId,
    },
    Recv {
        pat: PatId,
        chan: ExprId,
        body: ExprId,
    },
    Wait {
        pat: PatId,
        join: ExprId,
        body: ExprId,
    },
    Default {
        body: ExprId,
    },
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectArm {
    pub attributes: AstRange<Attribute>,
    pub id: AstNodeId,
    pub span: Span,
    pub kind: SelectArmKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsmOperandKind {
    In { reg: Symbol, expr: ExprId },
    Out { reg: Symbol, place: ExprId },
    Lateout { reg: Symbol, place: ExprId },
    Clobber { regs: AstRange<Symbol> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsmOperand {
    pub span: Span,
    pub kind: AsmOperandKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FStringPart {
    Text {
        text: Symbol,
        span: Span,
    },
    Interp {
        span: Span,
        expr: ExprId,
        spec: Option<Symbol>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntrinsicKind {
    SizeOf,
    AlignOf,
    TypeId,
    OffsetOf,
    TypeIdCount,
    Chan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExprKind {
    Path(PathId),
    Literal(LitKind),
    Paren(ExprId),
    /// 指针/引用类型调用头；同形的值解引用由类型检查区分。
    TypeCallee(TyId),
    Tuple(AstRange<ExprId>),
    Array(AstRange<ExprId>),
    Repeat {
        elem: ExprId,
        count: ExprId,
    },
    Struct {
        path: PathId,
        fields: AstRange<FieldExpr>,
    },
    Block {
        stmts: AstRange<StmtId>,
        tail: Option<ExprId>,
    },
    If {
        cond: ExprId,
        then_block: ExprId,
        else_branch: Option<ExprId>,
    },
    Match {
        scrutinee: ExprId,
        arms: AstRange<MatchArm>,
    },
    Loop(ExprId),
    While {
        cond: ExprId,
        body: ExprId,
    },
    For {
        pat: PatId,
        iter: ExprId,
        body: ExprId,
    },
    Try(ExprId),
    Select {
        arms: AstRange<SelectArm>,
    },
    Async(ExprId),
    Closure(FnId),
    Call {
        callee: ExprId,
        type_args: AstRange<GenericArg>,
        args: AstRange<ExprId>,
    },
    TypeApp {
        base: ExprId,
        args: AstRange<GenericArg>,
    },
    Field {
        base: ExprId,
        name: Symbol,
    },
    TupleField {
        base: ExprId,
        index: u32,
    },
    Index {
        base: ExprId,
        index: IndexKind,
    },
    TryOp(ExprId),
    Unary {
        op: UnOp,
        expr: ExprId,
    },
    Binary {
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    Range {
        start: ExprId,
        end: ExprId,
    },
    Unsafe(ExprId),
    Comptime(ExprId),
    SourceMacro {
        body: ExprId,
    },
    Intrinsic {
        kind: IntrinsicKind,
        tys: AstRange<GenericArg>,
        args: AstRange<ExprId>,
        field: Option<Symbol>,
    },
    Asm {
        template: ExprId,
        operands: AstRange<AsmOperand>,
    },
    Return(Option<ExprId>),
    Break(Option<ExprId>),
    Continue,
    FString {
        parts: AstRange<FStringPart>,
    },
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Expr {
    pub id: AstNodeId,
    pub span: Span,
    pub attributes: AstRange<Attribute>,
    pub kind: ExprKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StmtKind {
    Static {
        name: Symbol,
        ty: TyId,
        value: ExprId,
    },
    Let {
        pat: PatId,
        ty: Option<TyId>,
        init: Option<ExprId>,
        else_block: Option<ExprId>,
    },
    Assign {
        op: AssignOp,
        place: ExprId,
        value: ExprId,
    },
    Defer {
        ret: bool,
        body: ExprId,
    },
    Yield,
    Expr {
        expr: ExprId,
        discarded: bool,
    },
    SourceMacro {
        body: ExprId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stmt {
    pub attributes: AstRange<Attribute>,
    pub id: AstNodeId,
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatKind {
    Wildcard,
    Ident(Symbol),
    Literal(LitKind),
    NegativeLiteral(LitKind),
    Ref(PatId),
    Range {
        start: ExprId,
        end: ExprId,
    },
    Tuple(AstRange<PatId>),
    Array {
        prefix: AstRange<PatId>,
        rest: Option<RestPat>,
        suffix: AstRange<PatId>,
    },
    Struct {
        path: PathId,
        fields: AstRange<FieldPat>,
        rest: bool,
    },
    Constructor {
        path: PathId,
        fields: AstRange<PatId>,
    },
    Or(AstRange<PatId>),
    At {
        name: Symbol,
        name_span: Span,
        pat: PatId,
    },
    SourceMacro {
        body: ExprId,
    },
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestPat {
    pub name: Option<Symbol>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldPat {
    pub name: Symbol,
    pub span: Span,
    pub pat: Option<PatId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pat {
    pub id: AstNodeId,
    pub span: Span,
    pub kind: PatKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TyKind {
    Never,
    Infer,
    Path(PathId),
    Tuple(AstRange<TyId>),
    Array {
        elem: TyId,
        len: ExprId,
    },
    Slice(TyId),
    Ref(TyId),
    Ptr(TyId),
    Fn {
        params: AstRange<TyId>,
        ret: Option<TyId>,
    },
    Dyn(AstRange<PathId>),
    Impl(AstRange<Bound>),
    Chan(AstRange<GenericArg>),
    SourceMacro {
        body: ExprId,
    },
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ty {
    pub id: AstNodeId,
    pub span: Span,
    pub kind: TyKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstFile {
    pub source: SourceFileId,
    pub inner_attributes: AstRange<Attribute>,
    pub items: AstRange<ItemId>,
    pub eof_span: Span,
}

/// 一个源文件的全部 AST 表。子项用连续范围，点查用稠密下标。
#[derive(Clone, Debug, Default)]
pub struct AstArena {
    pub items: Vec<Item>,
    pub exprs: Vec<Expr>,
    pub stmts: Vec<Stmt>,
    pub pats: Vec<Pat>,
    pub tys: Vec<Ty>,
    pub paths: Vec<Path>,
    pub fns: Vec<FnDecl>,
    pub attrs: Vec<Attribute>,
    pub segments: Vec<PathSegment>,
    pub generic_args: Vec<GenericArg>,
    pub generic_params: Vec<GenericParam>,
    pub bounds: Vec<Bound>,
    pub params: Vec<Param>,
    pub fields: Vec<Field>,
    pub variants: Vec<Variant>,
    pub use_items: Vec<UseItem>,
    pub item_ids: Vec<ItemId>,
    pub expr_ids: Vec<ExprId>,
    pub stmt_ids: Vec<StmtId>,
    pub pat_ids: Vec<PatId>,
    pub ty_ids: Vec<TyId>,
    pub path_ids: Vec<PathId>,
    pub field_exprs: Vec<FieldExpr>,
    pub field_pats: Vec<FieldPat>,
    pub match_arms: Vec<MatchArm>,
    pub select_arms: Vec<SelectArm>,
    pub asm_operands: Vec<AsmOperand>,
    pub fstring_parts: Vec<FStringPart>,
    pub symbols: Vec<Symbol>,
    pub int_limbs: Vec<u32>,
    pub next_node: u32,
}

impl AstArena {
    pub fn with_token_hint(token_len: usize) -> Self {
        let n = token_len.max(8);
        Self {
            items: Vec::with_capacity(n / 8 + 1),
            exprs: Vec::with_capacity(n / 2 + 1),
            stmts: Vec::with_capacity(n / 4 + 1),
            pats: Vec::with_capacity(n / 8 + 1),
            tys: Vec::with_capacity(n / 8 + 1),
            paths: Vec::with_capacity(n / 8 + 1),
            fns: Vec::with_capacity(n / 16 + 1),
            attrs: Vec::with_capacity(4),
            segments: Vec::with_capacity(n / 8 + 1),
            generic_args: Vec::with_capacity(8),
            generic_params: Vec::with_capacity(8),
            bounds: Vec::with_capacity(8),
            params: Vec::with_capacity(n / 16 + 1),
            fields: Vec::with_capacity(8),
            variants: Vec::with_capacity(8),
            use_items: Vec::with_capacity(8),
            item_ids: Vec::with_capacity(n / 8 + 1),
            expr_ids: Vec::with_capacity(n / 2 + 1),
            stmt_ids: Vec::with_capacity(n / 4 + 1),
            pat_ids: Vec::with_capacity(n / 8 + 1),
            ty_ids: Vec::with_capacity(n / 8 + 1),
            path_ids: Vec::with_capacity(4),
            field_exprs: Vec::with_capacity(8),
            field_pats: Vec::with_capacity(8),
            match_arms: Vec::with_capacity(8),
            select_arms: Vec::with_capacity(4),
            asm_operands: Vec::with_capacity(4),
            fstring_parts: Vec::with_capacity(4),
            symbols: Vec::with_capacity(4),
            int_limbs: Vec::with_capacity(4),
            next_node: 0,
        }
    }

    pub fn alloc_node_or_error(&mut self, file: SourceFileId) -> Option<AstNodeId> {
        if self.next_node == u32::MAX {
            return None;
        }
        let id = AstNodeId {
            file,
            local: self.next_node,
        };
        self.next_node += 1;
        Some(id)
    }

    pub fn alloc_node(&mut self, file: SourceFileId) -> AstNodeId {
        self.alloc_node_or_error(file)
            .expect("AST 节点数达到 u32 上界")
    }

    pub fn try_push_item(&mut self, item: Item) -> Option<ItemId> {
        let index = self.items.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.items.push(item);
        Some(ItemId(index as u32))
    }

    pub fn try_push_expr(&mut self, expr: Expr) -> Option<ExprId> {
        let index = self.exprs.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.exprs.push(expr);
        Some(ExprId(index as u32))
    }

    pub fn push_expr(&mut self, expr: Expr) -> ExprId {
        self.try_push_expr(expr).expect("AST expr 表达到 u32 上界")
    }

    pub fn try_push_stmt(&mut self, stmt: Stmt) -> Option<StmtId> {
        let index = self.stmts.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.stmts.push(stmt);
        Some(StmtId(index as u32))
    }

    pub fn try_push_pat(&mut self, pat: Pat) -> Option<PatId> {
        let index = self.pats.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.pats.push(pat);
        Some(PatId(index as u32))
    }

    pub fn push_pat(&mut self, pat: Pat) -> PatId {
        self.try_push_pat(pat).expect("AST pat 表达到 u32 上界")
    }

    pub fn try_push_ty(&mut self, ty: Ty) -> Option<TyId> {
        let index = self.tys.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.tys.push(ty);
        Some(TyId(index as u32))
    }

    pub fn push_ty(&mut self, ty: Ty) -> TyId {
        self.try_push_ty(ty).expect("AST ty 表达到 u32 上界")
    }

    pub fn try_push_path(&mut self, path: Path) -> Option<PathId> {
        let index = self.paths.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.paths.push(path);
        Some(PathId(index as u32))
    }

    pub fn push_path(&mut self, path: Path) -> PathId {
        self.try_push_path(path).expect("AST path 表达到 u32 上界")
    }

    pub fn try_push_fn(&mut self, decl: FnDecl) -> Option<FnId> {
        let index = self.fns.len();
        if index >= u32::MAX as usize {
            return None;
        }
        self.fns.push(decl);
        Some(FnId(index as u32))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ArenaLens {
    pub items: usize,
    pub exprs: usize,
    pub stmts: usize,
    pub pats: usize,
    pub tys: usize,
    pub paths: usize,
    pub fns: usize,
    pub attrs: usize,
    pub segments: usize,
    pub generic_args: usize,
    pub generic_params: usize,
    pub bounds: usize,
    pub params: usize,
    pub fields: usize,
    pub variants: usize,
    pub use_items: usize,
    pub item_ids: usize,
    pub expr_ids: usize,
    pub stmt_ids: usize,
    pub pat_ids: usize,
    pub ty_ids: usize,
    pub path_ids: usize,
    pub field_exprs: usize,
    pub field_pats: usize,
    pub match_arms: usize,
    pub select_arms: usize,
    pub asm_operands: usize,
    pub fstring_parts: usize,
    pub symbols: usize,
    pub int_limbs: usize,
    pub next_node: u32,
}

impl AstArena {
    pub fn lens(&self) -> ArenaLens {
        ArenaLens {
            items: self.items.len(),
            exprs: self.exprs.len(),
            stmts: self.stmts.len(),
            pats: self.pats.len(),
            tys: self.tys.len(),
            paths: self.paths.len(),
            fns: self.fns.len(),
            attrs: self.attrs.len(),
            segments: self.segments.len(),
            generic_args: self.generic_args.len(),
            generic_params: self.generic_params.len(),
            bounds: self.bounds.len(),
            params: self.params.len(),
            fields: self.fields.len(),
            variants: self.variants.len(),
            use_items: self.use_items.len(),
            item_ids: self.item_ids.len(),
            expr_ids: self.expr_ids.len(),
            stmt_ids: self.stmt_ids.len(),
            pat_ids: self.pat_ids.len(),
            ty_ids: self.ty_ids.len(),
            path_ids: self.path_ids.len(),
            field_exprs: self.field_exprs.len(),
            field_pats: self.field_pats.len(),
            match_arms: self.match_arms.len(),
            select_arms: self.select_arms.len(),
            asm_operands: self.asm_operands.len(),
            fstring_parts: self.fstring_parts.len(),
            symbols: self.symbols.len(),
            int_limbs: self.int_limbs.len(),
            next_node: self.next_node,
        }
    }

    pub fn truncate(&mut self, lens: ArenaLens) {
        self.items.truncate(lens.items);
        self.exprs.truncate(lens.exprs);
        self.stmts.truncate(lens.stmts);
        self.pats.truncate(lens.pats);
        self.tys.truncate(lens.tys);
        self.paths.truncate(lens.paths);
        self.fns.truncate(lens.fns);
        self.attrs.truncate(lens.attrs);
        self.segments.truncate(lens.segments);
        self.generic_args.truncate(lens.generic_args);
        self.generic_params.truncate(lens.generic_params);
        self.bounds.truncate(lens.bounds);
        self.params.truncate(lens.params);
        self.fields.truncate(lens.fields);
        self.variants.truncate(lens.variants);
        self.use_items.truncate(lens.use_items);
        self.item_ids.truncate(lens.item_ids);
        self.expr_ids.truncate(lens.expr_ids);
        self.stmt_ids.truncate(lens.stmt_ids);
        self.pat_ids.truncate(lens.pat_ids);
        self.ty_ids.truncate(lens.ty_ids);
        self.path_ids.truncate(lens.path_ids);
        self.field_exprs.truncate(lens.field_exprs);
        self.field_pats.truncate(lens.field_pats);
        self.match_arms.truncate(lens.match_arms);
        self.select_arms.truncate(lens.select_arms);
        self.asm_operands.truncate(lens.asm_operands);
        self.fstring_parts.truncate(lens.fstring_parts);
        self.symbols.truncate(lens.symbols);
        self.int_limbs.truncate(lens.int_limbs);
        self.next_node = lens.next_node;
    }
}

pub fn try_extend_range<T>(
    vec: &mut Vec<T>,
    items: impl IntoIterator<Item = T>,
) -> Option<AstRange<T>> {
    let start = vec.len();
    if start > u32::MAX as usize {
        return None;
    }
    vec.extend(items);
    let len = vec.len().checked_sub(start)?;
    if len > u32::MAX as usize {
        return None;
    }
    Some(AstRange::from_indices(start, len))
}
