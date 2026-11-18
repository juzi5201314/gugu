//! 公共 API 文档模型与 `missing_docs` lint。
//!
//! `DocWorld` 是 `gugu doc` 的结构化输出：模块、公共项、成员、签名与文档文本，
//! 源位置保留文件 ID 与行号，供渲染层生成源位置链接。`missing_docs` lint
//! 按 `format-style` 规范对缺少有意义 `///` 文档的对外可见项发出诊断。

use crate::ParsedModule;
use crate::ast::{
    AstRange, AttrKind, Attribute, Field, Item, ItemId, ItemKind, StructBody, Visibility,
};
use crate::attr::{self, LintLevel};
use crate::diagnostics::{Diagnostic, DiagnosticCode, Severity};
use crate::source::SourceMap;
use crate::source::Span;

/// 一个 package 的公共 API 文档表；模块按模块路径稳定排序。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DocWorld {
    pub modules: Vec<DocModule>,
}

/// 一个模块的文档页。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocModule {
    /// 模块路径（如 `std.text`）。
    pub path: String,
    /// `//!` 模块级文档文本。
    pub docs: String,
    /// 该模块的源码逻辑路径（用于源位置链接）。
    pub source_path: String,
    /// 公共项列表，保持源码顺序。
    pub items: Vec<DocItem>,
}

/// 一个对外可见 item 的文档条目。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocItem {
    /// item 名（impl 块等无名项不出现）。
    pub name: String,
    /// item 类别：`function`、`struct`、`enum`、`union`、`trait`、`type`、`const`、`static`。
    pub kind: &'static str,
    /// 声明签名文本（不含函数体与初始值）。
    pub signature: String,
    /// `///` 文档文本。
    pub docs: String,
    /// 源码逻辑路径。
    pub source_path: String,
    /// 1-based 行号，源位置链接的锚点。
    pub line: u32,
    /// 公共成员：字段、变体、trait 方法。
    pub members: Vec<DocItem>,
}

/// 由 `///` 行抽取的文档文本；无文档行时返回 `None`（空白行不算「有意义」）。
pub fn doc_text(
    source_map: &SourceMap,
    attributes: AstRange<Attribute>,
    arena_attrs: &[Attribute],
) -> Option<String> {
    let mut text = String::new();
    for attribute in attributes.as_slice(arena_attrs) {
        if !matches!(
            attribute.kind,
            AttrKind::Doc { .. } | AttrKind::InnerDoc { .. }
        ) {
            continue;
        }
        // 宏生成的项带生成快照 span：按 span.file 取源，禁止用宿主源切片。
        let Some(source) = source_map
            .snapshot(attribute.span.file())
            .map(|snapshot| snapshot.content())
        else {
            continue;
        };
        text.push_str(doc_line(
            &source[attribute.span.start() as usize..attribute.span.end() as usize],
        ));
        text.push('\n');
    }
    let text = text.trim_end().to_owned();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// `///`/`//!` 注释行去掉前缀后的正文。
fn doc_line(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    trimmed
        .strip_prefix("///")
        .or_else(|| trimmed.strip_prefix("//!"))
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        .unwrap_or(trimmed)
}

/// 声明签名：item 头部文本到第一个顶层 `{`、`=`、`;`、`,` 为止，空白折叠。
fn signature_text(source_map: &SourceMap, span: &Span) -> String {
    let Some(source) = source_map
        .snapshot(span.file())
        .map(|snapshot| snapshot.content())
    else {
        return String::new();
    };
    let text = &source[span.start() as usize..span.end() as usize];
    let mut depth = 0_u32;
    let mut cut = text.len();
    for (offset, ch) in text.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '{' | '=' | ';' | ',' if depth == 0 => {
                cut = offset;
                break;
            }
            _ => {}
        }
    }
    let mut signature = String::new();
    let mut pending_space = false;
    for ch in text[..cut].trim().chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !signature.is_empty() {
            signature.push(' ');
        }
        pending_space = false;
        signature.push(ch);
    }
    signature
}

fn is_generated(module: &ParsedModule) -> bool {
    module.builtin || module.path.starts_with("__gugu_")
}

/// 收集一个 package 的公共 API 文档表；内建与生成模块不进入文档。
pub fn world(modules: &[ParsedModule], source_map: &SourceMap) -> DocWorld {
    let mut result = DocWorld::default();
    for module in modules {
        if is_generated(module) {
            continue;
        }
        let Some(snapshot) = source_map.snapshot(module.file.source) else {
            continue;
        };
        let source_path = snapshot.logical_path().to_owned();
        let docs = doc_text(
            source_map,
            module.file.inner_attributes,
            &module.arena.attrs,
        )
        .unwrap_or_default();
        let mut items = Vec::new();
        for item_id in module.file.items.as_slice(&module.arena.item_ids) {
            if !module.configured.item_active(*item_id) {
                continue;
            }
            let item = &module.arena.items[item_id.0 as usize];
            if let Some(doc_item) = item_doc(module, source_map, &source_path, item) {
                items.push(doc_item);
            }
        }
        result.modules.push(DocModule {
            path: module.path.clone(),
            docs,
            source_path,
            items,
        });
    }
    result
        .modules
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
}

fn item_doc(
    module: &ParsedModule,
    source_map: &SourceMap,
    source_path: &str,
    item: &Item,
) -> Option<DocItem> {
    if item.visibility != Visibility::Pub {
        return None;
    }
    let (kind, members) = match &item.kind {
        ItemKind::Function(_) => ("function", Vec::new()),
        ItemKind::Struct { body, .. } => (
            "struct",
            field_members(module, source_map, source_path, body),
        ),
        ItemKind::Enum { variants, .. } => (
            "enum",
            variants
                .as_slice(&module.arena.variants)
                .iter()
                .map(|variant| DocItem {
                    name: module.tokens.intern.get_str(variant.name).to_owned(),
                    kind: "variant",
                    signature: signature_text(source_map, &variant.span),
                    docs: doc_text(source_map, variant.attributes, &module.arena.attrs)
                        .unwrap_or_default(),
                    source_path: source_path.to_owned(),
                    line: variant.span.line(),
                    members: Vec::new(),
                })
                .collect(),
        ),
        ItemKind::Union { fields, .. } => (
            "union",
            fields
                .as_slice(&module.arena.fields)
                .iter()
                .filter(|field| field.visibility == Visibility::Pub)
                .filter_map(|field| field_doc(module, source_map, source_path, field))
                .collect(),
        ),
        ItemKind::TypeAlias { .. } => ("type", Vec::new()),
        ItemKind::Const { .. } => ("const", Vec::new()),
        ItemKind::Static { .. } => ("static", Vec::new()),
        ItemKind::Trait { items, .. } => (
            "trait",
            items
                .as_slice(&module.arena.item_ids)
                .iter()
                .filter(|id| module.configured.item_active(**id))
                .map(|id| &module.arena.items[id.0 as usize])
                .filter_map(|member| trait_member_doc(module, source_map, source_path, member))
                .collect(),
        ),
        ItemKind::Impl { items, .. } => (
            "impl",
            items
                .as_slice(&module.arena.item_ids)
                .iter()
                .filter(|id| module.configured.item_active(**id))
                .map(|id| &module.arena.items[id.0 as usize])
                .filter(|member| member.visibility == Visibility::Pub)
                .filter_map(|member| {
                    member.name.map(|name| DocItem {
                        name: module.tokens.intern.get_str(name).to_owned(),
                        kind: "function",
                        signature: signature_text(source_map, &member.span),
                        docs: doc_text(source_map, member.attributes, &module.arena.attrs)
                            .unwrap_or_default(),
                        source_path: source_path.to_owned(),
                        line: member.span.line(),
                        members: Vec::new(),
                    })
                })
                .collect(),
        ),
        _ => return None,
    };
    let name = item.name?;
    Some(DocItem {
        name: module.tokens.intern.get_str(name).to_owned(),
        kind,
        signature: signature_text(source_map, &item.span),
        docs: doc_text(source_map, item.attributes, &module.arena.attrs).unwrap_or_default(),
        source_path: source_path.to_owned(),
        line: item.span.line(),
        members,
    })
}

fn field_doc(
    module: &ParsedModule,
    source_map: &SourceMap,
    source_path: &str,
    field: &crate::ast::Field,
) -> Option<DocItem> {
    Some(DocItem {
        name: module.tokens.intern.get_str(field.name?).to_owned(),
        kind: "field",
        signature: signature_text(source_map, &field.span),
        docs: doc_text(source_map, field.attributes, &module.arena.attrs).unwrap_or_default(),
        source_path: source_path.to_owned(),
        line: field.span.line(),
        members: Vec::new(),
    })
}

fn field_members(
    module: &ParsedModule,
    source_map: &SourceMap,
    source_path: &str,
    body: &StructBody,
) -> Vec<DocItem> {
    let fields: &[crate::ast::Field] = match body {
        StructBody::Newtype(field) => std::slice::from_ref(field),
        StructBody::Record(fields) => fields.as_slice(&module.arena.fields),
    };
    fields
        .iter()
        .filter(|field| field.visibility == Visibility::Pub)
        .filter_map(|field| field_doc(module, source_map, source_path, field))
        .collect()
}

fn trait_member_doc(
    module: &ParsedModule,
    source_map: &SourceMap,
    source_path: &str,
    member: &Item,
) -> Option<DocItem> {
    let kind = match &member.kind {
        ItemKind::Function(_) => "method",
        ItemKind::Const { .. } => "const",
        ItemKind::TypeAlias { .. } => "type",
        _ => return None,
    };
    Some(DocItem {
        name: module.tokens.intern.get_str(member.name?).to_owned(),
        kind,
        signature: signature_text(source_map, &member.span),
        docs: doc_text(source_map, member.attributes, &module.arena.attrs).unwrap_or_default(),
        source_path: source_path.to_owned(),
        line: member.span.line(),
        members: Vec::new(),
    })
}

/// `missing_docs`：对外可见的公共项缺少有意义的 `///` 文档时按级别发出诊断。
///
/// 级别按 `allow`/`warn`/`deny`/`forbid` 属性的词法嵌套作用域解析；内建与生成
/// 模块豁免——生成模块已在头部声明 `#![allow(missing_docs)]`，这里仍以路径前缀
/// 兜底，避免任何生成路径回归为诊断。
pub fn missing_docs_lints(modules: &[ParsedModule], source_map: &SourceMap) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for module in modules {
        if is_generated(module) {
            continue;
        }
        let Some(snapshot) = source_map.snapshot(module.file.source) else {
            continue;
        };
        let source = snapshot.content();
        lint_module_docs(module, source_map, source, &mut diagnostics);
        for item_id in module.file.items.as_slice(&module.arena.item_ids) {
            if !module.configured.item_active(*item_id) {
                continue;
            }
            lint_item(module, source_map, source, *item_id, &mut diagnostics);
        }
    }
    diagnostics
}

fn lint_module_docs(
    module: &ParsedModule,
    source_map: &SourceMap,
    source: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if doc_text(
        source_map,
        module.file.inner_attributes,
        &module.arena.attrs,
    )
    .is_some()
    {
        return;
    }
    let Some(first) = module.file.items.as_slice(&module.arena.item_ids).first() else {
        return;
    };
    let span = module.arena.items[first.0 as usize].span.clone();
    emit_doc_lint(
        module,
        source,
        &span,
        &format!("公共模块 `{}` 缺少 `//!` 模块文档", module.path),
        diagnostics,
    );
}

fn lint_item(
    module: &ParsedModule,
    source_map: &SourceMap,
    source: &str,
    item_id: ItemId,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let item = &module.arena.items[item_id.0 as usize];
    if item.visibility != Visibility::Pub {
        return;
    }
    let kind: &str = match &item.kind {
        ItemKind::Function(_) => "函数",
        ItemKind::Struct { body, .. } => {
            lint_fields(module, source_map, source, body, diagnostics);
            "类型"
        }
        ItemKind::Enum { .. } => "类型",
        ItemKind::Union { fields, .. } => {
            lint_field_range(module, source_map, source, *fields, diagnostics);
            "类型"
        }
        ItemKind::TypeAlias { .. } => "类型",
        ItemKind::Const { .. } => "常量",
        ItemKind::Static { .. } => "常量",
        ItemKind::Trait { items, .. } => {
            for member_id in items.as_slice(&module.arena.item_ids) {
                if !module.configured.item_active(*member_id) {
                    continue;
                }
                let member = &module.arena.items[member_id.0 as usize];
                if !matches!(member.kind, ItemKind::Function(_)) {
                    continue;
                }
                if doc_text(source_map, member.attributes, &module.arena.attrs).is_some() {
                    continue;
                }
                let name = member
                    .name
                    .map(|symbol| module.tokens.intern.get_str(symbol).to_owned())
                    .unwrap_or_default();
                let span = member
                    .name_span
                    .clone()
                    .unwrap_or_else(|| member.span.clone());
                emit_doc_lint(
                    module,
                    source,
                    &span,
                    &format!("公共 trait 方法 `{name}` 缺少 `///` 文档"),
                    diagnostics,
                );
            }
            "trait"
        }
        ItemKind::Impl { items, .. } => {
            for member_id in items.as_slice(&module.arena.item_ids) {
                if !module.configured.item_active(*member_id) {
                    continue;
                }
                lint_item(module, source_map, source, *member_id, diagnostics);
            }
            "impl"
        }
        _ => return,
    };
    if kind == "impl" {
        return;
    }
    if doc_text(source_map, item.attributes, &module.arena.attrs).is_some() {
        return;
    }
    let name = item
        .name
        .map(|symbol| module.tokens.intern.get_str(symbol).to_owned());
    let Some(name) = name else { return };
    let span = item.name_span.clone().unwrap_or_else(|| item.span.clone());
    emit_doc_lint(
        module,
        source,
        &span,
        &format!("公共{kind} `{name}` 缺少 `///` 文档"),
        diagnostics,
    );
}

fn lint_fields(
    module: &ParsedModule,
    source_map: &SourceMap,
    source: &str,
    body: &StructBody,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let fields: &[Field] = match body {
        StructBody::Newtype(field) => std::slice::from_ref(field),
        StructBody::Record(fields) => fields.as_slice(&module.arena.fields),
    };
    for field in fields {
        lint_field(module, source_map, source, field, diagnostics);
    }
}

fn lint_field_range(
    module: &ParsedModule,
    source_map: &SourceMap,
    source: &str,
    fields: AstRange<Field>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for field in fields.as_slice(&module.arena.fields) {
        lint_field(module, source_map, source, field, diagnostics);
    }
}

fn lint_field(
    module: &ParsedModule,
    source_map: &SourceMap,
    source: &str,
    field: &Field,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if field.visibility != Visibility::Pub {
        return;
    }
    if doc_text(source_map, field.attributes, &module.arena.attrs).is_some() {
        return;
    }
    let Some(name) = field.name else { return };
    let name = module.tokens.intern.get_str(name).to_owned();
    let span = field
        .name_span
        .clone()
        .unwrap_or_else(|| field.span.clone());
    emit_doc_lint(
        module,
        source,
        &span,
        &format!("公共字段 `{name}` 缺少 `///` 文档"),
        diagnostics,
    );
}

fn emit_doc_lint(
    module: &ParsedModule,
    source: &str,
    span: &Span,
    message: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let level = lint_level_at(module, source, span.start());
    let Some(severity) = (match level {
        LintLevel::Allow => None,
        LintLevel::Warn => Some(Severity::Warning),
        LintLevel::Deny | LintLevel::Forbid => Some(Severity::Error),
    }) else {
        return;
    };
    diagnostics.push(Diagnostic::new(
        severity,
        DiagnosticCode::MissingDocs,
        message.to_owned(),
        Some(span.clone()),
        0,
    ));
}

fn lint_level_at(module: &ParsedModule, source: &str, offset: u32) -> LintLevel {
    attr::lint_level(
        source,
        module.file.inner_attributes,
        &module.arena,
        &module.tokens,
        &module.configured,
        offset,
        attr::MISSING_DOCS,
    )
}
