//! HIR 只保存已选择的语义操作与 owner-local 索引，不持有 token、AST 或可变名称环境。
use serde::{Deserialize, Serialize};

mod body;
mod types;
mod verify;
pub(crate) use body::*;
pub(crate) use types::*;

macro_rules! ids {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        pub(crate) struct $name(pub(crate) u32);
        impl $name {
            pub(crate) fn index(self) -> usize { self.0 as usize }
        }
    )* };
}
ids!(DefId, TypeId, ExprId, StmtId, PatternId, ScopeId, LocalId);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Location {
    pub(crate) source: u32,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) expansion: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Source {
    pub(crate) path: String,
    pub(crate) hash: [u8; 32],
    pub(crate) length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Expansion {
    pub(crate) parent: u32,
    pub(crate) call: Location,
    pub(crate) definition: Location,
    pub(crate) source: u32,
    pub(crate) slot: crate::source::SourceSlot,
    pub(crate) hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Definition {
    pub(crate) key: [u8; 32],
    pub(crate) name: String,
    pub(crate) parent: Option<DefId>,
    pub(crate) location: Option<Location>,
    pub(crate) kind: DefinitionKind,
    pub(crate) signature: Option<TypeId>,
    pub(crate) parameters: Vec<Parameter>,
    pub(crate) obligations: Vec<Obligation>,
    pub(crate) public: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum DefinitionKind {
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
pub(crate) struct Module {
    pub(crate) sources: Vec<Source>,
    pub(crate) expansions: Vec<Expansion>,
    pub(crate) definitions: Vec<Definition>,
    pub(crate) types: Vec<Type>,
    pub(crate) aggregates: Vec<Aggregate>,
    pub(crate) interfaces: Vec<Interface>,
    pub(crate) implementations: Vec<Implementation>,
    pub(crate) opaques: Vec<Opaque>,
    pub(crate) owners: Vec<Owner>,
    pub(crate) initialization: Vec<Initialization>,
    pub(crate) linkage: Vec<Linkage>,
    pub(crate) entry: Option<DefId>,
    pub(crate) input_fingerprint: [u8; 32],
}

/// 只有完整 verifier 可以创建；反序列化得到的 Module 必须重新验证，不直接恢复此凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Validated {
    module: std::sync::Arc<Module>,
    fingerprint: [u8; 32],
}
impl Validated {
    pub(in crate::frontend) fn freeze(
        module: Module,
    ) -> Result<(Self, Vec<u8>), crate::Diagnostic> {
        module.verify()?;
        let bytes = serde_json::to_vec(&module).expect("HIR schema 序列化");
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
    pub(crate) fn module(&self) -> &Module {
        &self.module
    }
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Initialization {
    pub(crate) definition: DefId,
    pub(crate) domain: StorageDomain,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum StorageDomain {
    Constant,
    Process,
    Coroutine,
    OsThread,
    Lazy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Linkage {
    pub(crate) definition: DefId,
    pub(crate) export_name: Option<String>,
    pub(crate) import_name: Option<String>,
    pub(crate) section: Option<String>,
    pub(crate) used: bool,
    pub(crate) foreign: Option<super::semantics::foreign::ForeignEffect>,
    pub(crate) naked: bool,
}
