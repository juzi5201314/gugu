use super::{DefId, TypeId};
use serde::{Deserialize, Serialize};

/// 复合类型仅引用驻留后的子类型；不在每个表达式复制递归 Ty 树。
///
/// `TypeId` 是语言内置类型名（见 `docs/src/spec/declarations.md`），不随 lint 改名。
#[expect(
    clippy::enum_variant_names,
    reason = "TypeId 是语言内置类型名，不随 lint 改名"
)]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Type {
    Never,
    Unit,
    Bool,
    Char,
    String,
    TypeId,
    Range,
    Int {
        signed: bool,
        bits: u16,
    },
    Float(u16),
    Ref(TypeId),
    Ptr(TypeId),
    Slice(TypeId),
    Array(TypeId, u64),
    Tuple(Vec<TypeId>),
    Function {
        parameters: Vec<TypeId>,
        result: TypeId,
    },
    Callable {
        definition: DefId,
        arguments: Vec<TypeId>,
        signature: TypeId,
    },
    Named {
        definition: DefId,
        arguments: Vec<TypeId>,
    },
    Parameter {
        owner: DefId,
        index: u32,
    },
    Projection {
        self_ty: TypeId,
        interface: TraitRef,
        member: u32,
    },
    Opaque {
        definition: DefId,
        arguments: Vec<TypeId>,
    },
    Dyn(Vec<TraitRef>),
    Option(TypeId),
    Result(TypeId, TypeId),
    Chan(TypeId),
    Join(TypeId),
    ChanClosed,
    TrySendErr,
    TryRecvErr,
    Panic,
    Formatter,
    Hasher,
    MaybeUninit(TypeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TraitRef {
    pub definition: DefId,
    pub arguments: Vec<TypeId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub kind: ParameterKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ParameterKind {
    Type { pack: bool },
    Comptime(TypeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Obligation {
    Trait { ty: TypeId, interface: TraitRef },
    Callable { ty: TypeId, signature: TypeId },
    Equal { left: TypeId, right: TypeId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub definition: DefId,
    pub variants: Vec<Variant>,
    pub representation: Representation,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Representation {
    // C、packed、transparent 三个固定标志，其余位必须为零。
    pub flags: u8,
    pub align: u64,
    pub tag: Option<(bool, u16)>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<Field>,
    pub record: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: TypeId,
    pub public: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    pub definition: DefId,
    pub members: Vec<Member>,
    pub unsafety: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub definition: DefId,
    pub self_ty: TypeId,
    pub interface: Option<TraitRef>,
    pub members: Vec<Member>,
    pub negative: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub definition: Option<DefId>,
    pub kind: MemberKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MemberKind {
    Method {
        signature: TypeId,
        receiver: bool,
        default: bool,
        unsafety: bool,
    },
    Type(Option<TypeId>),
    Constant {
        ty: TypeId,
        value: Option<super::Literal>,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Opaque {
    pub definition: DefId,
    pub hidden: Option<TypeId>,
}
