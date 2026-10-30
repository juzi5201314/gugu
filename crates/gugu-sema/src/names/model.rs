use crate::{ast::Visibility, source::Span};

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct ModuleId(pub(super) u32);

impl ModuleId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct DefId(pub(super) u32);

impl DefId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub enum Namespace {
    Module,
    Type,
    Value,
    Constructor,
    Field,
    External,
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub enum DefinitionKind {
    Function,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    TypeAlias,
    Const,
    Static,
    ExternBlock,
    GlobalAsm,
    Field,
    Variant,
    SourceMacro,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Definition {
    pub id: DefId,
    pub stable_key: [u8; 32],
    pub module: ModuleId,
    pub parent: Option<DefId>,
    pub module_binding: bool,
    pub name: Option<String>,
    pub kind: DefinitionKind,
    pub namespace: Option<Namespace>,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ResolvedTarget {
    Module(ModuleId),
    Def(DefId),
    External(String),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResolvedImport {
    pub module: ModuleId,
    pub alias: String,
    pub namespace: Namespace,
    pub target: ResolvedTarget,
    pub public: bool,
    pub span: Span,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct NameResolution {
    pub definitions: Vec<Definition>,
    pub imports: Vec<ResolvedImport>,
}

pub fn resolved_target_key(target: &ResolvedTarget) -> (u8, u32, &str) {
    match target {
        ResolvedTarget::Module(module) => (0, module.0, ""),
        ResolvedTarget::Def(definition) => (1, definition.0, ""),
        ResolvedTarget::External(path) => (2, 0, path),
    }
}

pub fn resolution_is_valid(resolution: &NameResolution) -> bool {
    let definitions = &resolution.definitions;
    definitions.iter().enumerate().all(|(index, definition)| {
        definition.id.index() == index
            && definition
                .parent
                .is_none_or(|parent| parent.index() < definitions.len())
            && definition.namespace == definition_namespace(definition.kind)
    }) && {
        // 编号按收集顺序分配（内建前缀 + 用户追加），稳定键只需唯一，不再要求有序。
        let mut keys: Vec<[u8; 32]> = definitions
            .iter()
            .map(|definition| definition.stable_key)
            .collect();
        keys.sort_unstable();
        keys.windows(2).all(|pair| pair[0] != pair[1])
    }
}

fn definition_namespace(kind: DefinitionKind) -> Option<Namespace> {
    match kind {
        DefinitionKind::Function | DefinitionKind::Const | DefinitionKind::Static => {
            Some(Namespace::Value)
        }
        DefinitionKind::Struct
        | DefinitionKind::Enum
        | DefinitionKind::Union
        | DefinitionKind::Trait
        | DefinitionKind::TypeAlias => Some(Namespace::Type),
        DefinitionKind::Field => Some(Namespace::Field),
        DefinitionKind::Variant => Some(Namespace::Constructor),
        DefinitionKind::Impl
        | DefinitionKind::ExternBlock
        | DefinitionKind::GlobalAsm
        | DefinitionKind::SourceMacro => None,
    }
}

pub(super) fn is_reserved_name(name: &str) -> bool {
    matches!(
        name,
        "Option"
            | "Result"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            | "Vec"
            | "Range"
            | "Join"
            | "ChanClosed"
            | "TrySendErr"
            | "TryRecvErr"
            | "Panic"
            | "panic"
            | "Print"
            | "Debug"
            | "Clone"
            | "Eq"
            | "Ord"
            | "Hash"
            | "StableHash"
            | "StableOrd"
            | "Default"
            | "Error"
            | "Iter"
            | "IntoIter"
            | "Index"
            | "Try"
            | "Fn"
            | "Any"
            | "TypeId"
            | "Read"
            | "Write"
            | "Seek"
            | "BufRead"
            | "OsString"
            | "ByteBuffer"
            | "HashMap"
            | "HashSet"
            | "Path"
            | "Duration"
            | "Formatter"
            | "Hasher"
    )
}
