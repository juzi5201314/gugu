use crate::{ast::Visibility, source::Span};
use gugu_base::SharedTable;

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
    /// 规范记录的位置；当前会话位置通过 `NameResolution::definition_span` 获取。
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
    /// 规范记录的位置；当前会话位置通过 `NameResolution::import_span` 获取。
    pub span: Span,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct NameResolution {
    pub definitions: SharedTable<Definition>,
    pub imports: SharedTable<ResolvedImport>,
    /// 只覆盖内建定义前缀的会话位置；追加定义直接使用记录内的 Span。
    /// 会话位置不进规范编码，物化格式仍是两张连续记录表。
    #[serde(skip)]
    pub(crate) definition_spans: SharedTable<Span>,
    /// 只覆盖内建导入前缀，长度不得超过导入记录表。
    #[serde(skip)]
    pub(crate) import_spans: SharedTable<Span>,
}

impl NameResolution {
    /// 共享内建记录与已重绑的位置前缀；用户记录的位置随追加记录保存。
    /// 两张位置表都与对应记录表的 `[0, len)` 一一对应，不包含用户追加区。
    pub(crate) fn with_session_spans(
        definitions: SharedTable<Definition>,
        imports: SharedTable<ResolvedImport>,
        definition_spans: SharedTable<Span>,
        import_spans: SharedTable<Span>,
    ) -> Self {
        debug_assert!(definition_spans.len() <= definitions.len());
        debug_assert!(import_spans.len() <= imports.len());
        Self {
            definitions,
            imports,
            definition_spans,
            import_spans,
        }
    }

    /// 按全局定义编号取会话位置：内建前缀读重绑表，追加区读定义记录。
    pub fn definition_span(&self, index: usize) -> &Span {
        debug_assert!(self.definition_spans.len() <= self.definitions.len());
        if index < self.definition_spans.len() {
            &self.definition_spans[index]
        } else {
            &self.definitions[index].span
        }
    }

    /// 按全局导入下标取会话位置：内建前缀读重绑表，追加区读导入记录。
    pub fn import_span(&self, index: usize) -> &Span {
        debug_assert!(self.import_spans.len() <= self.imports.len());
        if index < self.import_spans.len() {
            &self.import_spans[index]
        } else {
            &self.imports[index].span
        }
    }
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
    } && resolution.definition_spans.len() <= definitions.len()
        && resolution.import_spans.len() <= resolution.imports.len()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot, SourceTableId};

    fn prefix() -> NameResolution {
        let span = Span::detached(std::path::Path::new("builtin.gg"), 0, 1);
        let mut names = NameResolution::default();
        names.definitions.push(Definition {
            id: DefId(0),
            stable_key: [0; 32],
            module: ModuleId(0),
            parent: None,
            module_binding: true,
            name: Some("builtin".into()),
            kind: DefinitionKind::Function,
            namespace: Some(Namespace::Value),
            visibility: Visibility::Pub,
            span: span.clone(),
        });
        names.imports.push(ResolvedImport {
            module: ModuleId(0),
            alias: "builtin".into(),
            namespace: Namespace::Value,
            target: ResolvedTarget::Def(DefId(0)),
            public: true,
            span,
        });
        names.definitions.share();
        names.imports.share();
        names
    }

    fn sources(user: &str) -> SourceMap {
        SourceMap::new(vec![
            SourceSnapshot::from_str("builtin.gg", "abcd").unwrap(),
            SourceSnapshot::from_str("user.gg", user).unwrap(),
        ])
        .unwrap()
    }

    fn bind(prefix: &NameResolution, sources: &SourceMap) -> NameResolution {
        let span = sources
            .span(SourceFileId::new(0), 0, 1, ExpansionId::ROOT)
            .unwrap();
        let mut spans = SharedTable::from(vec![span]);
        spans.share();
        NameResolution::with_session_spans(
            prefix.definitions.clone(),
            prefix.imports.clone(),
            spans.clone(),
            spans,
        )
    }

    #[test]
    fn shared_records_keep_session_and_appended_spans_separate() {
        let prefix = prefix();
        let sources = sources("user");
        let other_sources = self::sources("other");
        let mut names = bind(&prefix, &sources);
        let other = bind(&prefix, &other_sources);
        assert!(std::ptr::eq(&names.definitions[0], &other.definitions[0]));
        assert!(std::ptr::eq(&names.imports[0], &other.imports[0]));
        assert_ne!(
            names.definition_span(0).table(),
            other.definition_span(0).table()
        );
        assert_eq!(names.import_span(0).table(), sources.table());
        assert_eq!(other.import_span(0).table(), other_sources.table());

        let user_span = sources
            .span(SourceFileId::new(1), 0, 1, ExpansionId::ROOT)
            .unwrap();
        let mut definition = prefix.definitions[0].clone();
        definition.id = DefId(1);
        definition.stable_key = [1; 32];
        definition.span = user_span.clone();
        names.definitions.push(definition);
        let mut import = prefix.imports[0].clone();
        import.span = user_span.clone();
        names.imports.push(import);
        assert!(resolution_is_valid(&names));
        assert_eq!(names.definition_span(1), &user_span);
        assert_eq!(names.import_span(1), &user_span);
        assert!(std::ptr::eq(
            names.definition_span(1),
            &names.definitions[1].span
        ));
        assert!(std::ptr::eq(names.import_span(1), &names.imports[1].span));
        assert_eq!(names.definition_spans.len(), 1);
        assert_eq!(names.import_spans.len(), 1);
        assert_eq!(prefix.definitions[0].span.table(), SourceTableId::UNBOUND);
    }

    #[test]
    fn session_spans_do_not_change_contiguous_record_encoding() {
        #[derive(serde::Serialize)]
        struct EncodedNames<'a> {
            definitions: Vec<&'a Definition>,
            imports: Vec<&'a ResolvedImport>,
        }

        let names = bind(&prefix(), &sources("user"));
        let encoded = crate::canonical::to_vec(&names).unwrap();
        let records = EncodedNames {
            definitions: names.definitions.iter().collect(),
            imports: names.imports.iter().collect(),
        };
        assert_eq!(encoded, crate::canonical::to_vec(&records).unwrap());
        let decoded: NameResolution = crate::canonical::from_slice(&encoded).unwrap();
        assert!(decoded.definition_spans.is_empty());
        assert!(decoded.import_spans.is_empty());
        assert_eq!(decoded.definition_span(0), &names.definitions[0].span);
        assert_eq!(decoded.import_span(0), &names.imports[0].span);
    }
}
