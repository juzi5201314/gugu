use super::{ArrayLength, Representation};
use crate::ast::ItemId;
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize)]
pub enum Ty {
    Error,
    Var(u32),
    Never,
    Unit,
    Bool,
    Int {
        signed: bool,
        bits: u16,
    },
    Float(u16),
    Char,
    String,
    Ref(Box<Ty>),
    Ptr(Box<Ty>),
    Slice(Box<Ty>),
    Array(Box<Ty>, ArrayLength),
    Const(ArrayLength),
    Tuple(Vec<Ty>),
    Function(Vec<Ty>, Box<Ty>),
    Callable(CallableId, Vec<Ty>, Box<Ty>),
    Named(usize, Vec<Ty>),
    Param(String),
    Projection(Box<Ty>, crate::semantics::traits::TraitRef, String),
    Opaque(u32, Vec<Ty>),
    Dyn(Vec<crate::semantics::traits::TraitRef>),
    TypeId,
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    /// 格式 trait 的写入目标。载荷方法随 `std.fmt` 提供。
    Formatter,
    Range,
    Chan(Box<Ty>),
    Join(Box<Ty>),
    ChanClosed,
    TrySendErr,
    TryRecvErr,
    Panic,
    MaybeUninit(Box<Ty>),
}

/// 函数项和闭包的身份来自模块内稠密 FnDecl 编号，不使用地址。
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct CallableId {
    pub module: usize,
    pub function: u32,
}
impl Ty {
    pub fn int() -> Self {
        Self::Int {
            signed: true,
            bits: 64,
        }
    }
    pub fn strip_refs(&self) -> &Self {
        match self {
            Self::Ref(t) => t.strip_refs(),
            _ => self,
        }
    }
    pub fn signature(&self) -> Option<(&[Ty], &Ty)> {
        match self {
            Self::Function(params, ret) => Some((params, ret)),
            Self::Callable(_, _, signature) => signature.signature(),
            _ => None,
        }
    }
    pub fn primitive(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => Self::Bool,
            "char" => Self::Char,
            "string" => Self::String,
            "TypeId" => Self::TypeId,
            "int" | "i64" | "isize" => Self::int(),
            "uint" | "u64" | "usize" => Self::Int {
                signed: false,
                bits: 64,
            },
            "byte" | "u8" => Self::Int {
                signed: false,
                bits: 8,
            },
            "float" | "f64" => Self::Float(64),
            "f32" => Self::Float(32),
            "i8" => Self::Int {
                signed: true,
                bits: 8,
            },
            "i16" => Self::Int {
                signed: true,
                bits: 16,
            },
            "i32" => Self::Int {
                signed: true,
                bits: 32,
            },
            "i128" => Self::Int {
                signed: true,
                bits: 128,
            },
            "u16" => Self::Int {
                signed: false,
                bits: 16,
            },
            "u32" => Self::Int {
                signed: false,
                bits: 32,
            },
            "u128" => Self::Int {
                signed: false,
                bits: 128,
            },
            "Formatter" => Self::Formatter,
            "Range" => Self::Range,
            "ChanClosed" => Self::ChanClosed,
            "TrySendErr" => Self::TrySendErr,
            "TryRecvErr" => Self::TryRecvErr,
            "Panic" => Self::Panic,
            _ => return None,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DefRef {
    pub module: usize,
    pub item: ItemId,
}
#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub name: String,
    pub ty: Ty,
    pub public: bool,
}
#[derive(Clone, Debug)]
pub struct Constructor {
    pub index: usize,
    pub name: String,
    pub fields: Vec<FieldInfo>,
    pub record: bool,
}
#[derive(Clone, Debug)]
pub struct Nominal {
    pub definition: DefRef,
    pub name: String,
    pub params: Vec<String>,
    pub variants: Vec<Constructor>,
    pub is_enum: bool,
    pub repr: Representation,
}
