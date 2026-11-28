//! HIR 只保存已选择的语义操作与 owner-local 索引，不持有 token、AST 或可变名称环境。
use gugu_base::shared_table::SharedTable;
use serde::{Deserialize, Serialize};

mod body;
mod types;
mod verify;
pub use body::*;
pub use types::*;

macro_rules! ids {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        pub struct $name(pub u32);
        impl $name {
            pub fn index(self) -> usize { self.0 as usize }
        }
    )* };
}
ids!(DefId, TypeId, ExprId, StmtId, PatternId, ScopeId, LocalId);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub source: u32,
    pub start: u32,
    pub end: u32,
    pub expansion: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    pub hash: [u8; 32],
    pub length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Expansion {
    pub parent: u32,
    pub call: Location,
    pub definition: Location,
    pub source: u32,
    pub slot: crate::source::SourceSlot,
    pub hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub key: [u8; 32],
    /// 定义所属的会话模块编号；定义表按模块连续排列，分块安装据此切段。
    pub module: u32,
    pub name: String,
    pub parent: Option<DefId>,
    pub location: Option<Location>,
    pub kind: DefinitionKind,
    pub signature: Option<TypeId>,
    pub parameters: Vec<Parameter>,
    pub obligations: Vec<Obligation>,
    pub public: bool,
    /// `#[track_caller]`：`caller()` 在该函数体内读到的是其调用者的位置；
    /// 由 callsite 追加的隐藏形参携带。
    pub track_caller: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DefinitionKind {
    Function,
    Closure,
    Async,
    Constant,
    Static,
    LocalStatic,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    TypeAlias,
    Opaque,
    ExternBlock,
    GlobalAsm,
    Field,
    Variant,
    Builtin,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Module {
    pub sources: Vec<Source>,
    pub expansions: Vec<Expansion>,
    pub definitions: SharedTable<Definition>,
    pub types: SharedTable<Type>,
    pub aggregates: SharedTable<Aggregate>,
    pub interfaces: SharedTable<Interface>,
    pub implementations: SharedTable<Implementation>,
    pub opaques: SharedTable<Opaque>,
    pub owners: Vec<Owner>,
    pub initialization: SharedTable<Initialization>,
    pub linkage: SharedTable<Linkage>,
    pub entry: Option<DefId>,
    pub input_fingerprint: [u8; 32],
}

impl Module {
    /// 把物化 HIR 的不可变表固化为共享前缀，之后只允许会话侧追加。
    pub fn share(&mut self) {
        self.types.share();
        self.definitions.share();
        self.aggregates.share();
        self.interfaces.share();
        self.implementations.share();
        self.opaques.share();
        self.initialization.share();
        self.linkage.share();
        for owner in &mut self.owners {
            owner.share();
        }
    }
}

/// 只有完整 verifier 可以创建；反序列化得到的 Module 必须重新验证，不直接恢复此凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Validated {
    module: std::sync::Arc<Module>,
    fingerprint: [u8; 32],
}
impl Validated {
    pub fn freeze(module: Module) -> Result<(Self, Vec<u8>), crate::Diagnostic> {
        module.verify()?;
        let bytes = crate::canonical::to_vec(&module).expect("HIR schema 序列化");
        let fingerprint = *blake3::Hasher::new_derive_key("gugu-validated-hir-v2")
            .update(&bytes)
            .finalize()
            .as_bytes();
        Ok((
            Self {
                module: std::sync::Arc::new(module),
                fingerprint,
            },
            bytes,
        ))
    }
    pub fn module(&self) -> &Module {
        &self.module
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Initialization {
    pub definition: DefId,
    pub domain: StorageDomain,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StorageDomain {
    Constant,
    Process,
    Coroutine,
    OsThread,
    Lazy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Linkage {
    pub definition: DefId,
    pub export_name: Option<String>,
    pub import_name: Option<String>,
    pub section: Option<String>,
    pub used: bool,
    pub root_kind: super::semantics::linkage::RootLinkageKind,
    pub foreign: Option<super::semantics::foreign::ForeignEffect>,
    pub naked: bool,
}
