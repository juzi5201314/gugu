use super::{DefId, TypeId};
use serde::{Deserialize, Serialize};

/// 复合类型仅引用驻留后的子类型；不在每个表达式复制递归 Ty 树。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum Type {
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
    MaybeUninit(TypeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct TraitRef {
    pub(crate) definition: DefId,
    pub(crate) arguments: Vec<TypeId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Parameter {
    pub(crate) name: String,
    pub(crate) kind: ParameterKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ParameterKind {
    Type { pack: bool },
    Comptime(TypeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Obligation {
    Trait { ty: TypeId, interface: TraitRef },
    Callable { ty: TypeId, signature: TypeId },
    Equal { left: TypeId, right: TypeId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Aggregate {
    pub(crate) definition: DefId,
    pub(crate) variants: Vec<Variant>,
    pub(crate) representation: Representation,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Representation {
    // C、packed、transparent 三个固定标志，其余位必须为零。
    pub(crate) flags: u8,
    pub(crate) align: u64,
    pub(crate) tag: Option<(bool, u16)>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Variant {
    pub(crate) name: String,
    pub(crate) fields: Vec<Field>,
    pub(crate) record: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) ty: TypeId,
    pub(crate) public: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Interface {
    pub(crate) definition: DefId,
    pub(crate) members: Vec<Member>,
    pub(crate) unsafety: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Implementation {
    pub(crate) definition: DefId,
    pub(crate) self_ty: TypeId,
    pub(crate) interface: Option<TraitRef>,
    pub(crate) members: Vec<Member>,
    pub(crate) negative: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Member {
    pub(crate) name: String,
    pub(crate) definition: Option<DefId>,
    pub(crate) kind: MemberKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum MemberKind {
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
pub(crate) struct Opaque {
    pub(crate) definition: DefId,
    pub(crate) hidden: Option<TypeId>,
}
