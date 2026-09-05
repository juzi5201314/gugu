use crate::{frontend::ast::Visibility, source::Span};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ModuleId(pub(super) u32);

impl ModuleId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DefId(pub(super) u32);

impl DefId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Namespace {
    Module,
    Type,
    Value,
    Constructor,
    Field,
    External,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum DefinitionKind {
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

#[derive(Clone, Debug)]
pub(crate) struct Definition {
    pub(crate) id: DefId,
    pub(crate) stable_key: [u8; 32],
    pub(crate) module: ModuleId,
    pub(crate) parent: Option<DefId>,
    pub(crate) module_binding: bool,
    pub(crate) name: Option<String>,
    pub(crate) kind: DefinitionKind,
    pub(crate) namespace: Option<Namespace>,
    pub(crate) visibility: Visibility,
    pub(crate) span: Span,
}

#[derive(Clone, Debug)]
pub(crate) enum ResolvedTarget {
    Module(ModuleId),
    Def(DefId),
    External,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedImport {
    pub(crate) module: ModuleId,
    pub(crate) alias: String,
    pub(crate) namespace: Namespace,
    pub(crate) target: ResolvedTarget,
    pub(crate) public: bool,
    pub(crate) span: Span,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NameResolution {
    pub(crate) definitions: Vec<Definition>,
    pub(crate) imports: Vec<ResolvedImport>,
}

pub(super) fn resolved_target_key(target: &ResolvedTarget) -> (u8, u32) {
    match target {
        ResolvedTarget::Module(module) => (0, module.0),
        ResolvedTarget::Def(definition) => (1, definition.0),
        ResolvedTarget::External => (2, 0),
    }
}

pub(super) fn resolution_is_valid(resolution: &NameResolution) -> bool {
    let definitions = &resolution.definitions;
    definitions.iter().enumerate().all(|(index, definition)| {
        definition.id.index() == index
            && definition
                .parent
                .is_none_or(|parent| parent.index() < definitions.len())
            && definition.namespace == definition_namespace(definition.kind)
    }) && definitions
        .windows(2)
        .all(|pair| pair[0].stable_key < pair[1].stable_key)
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
            | "HashMap"
            | "HashSet"
            | "Path"
            | "Duration"
    )
}
