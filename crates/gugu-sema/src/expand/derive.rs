//! 闭集 derive 生成普通 impl，继续复用名称解析、trait 检查与函数 lowering。
use super::*;
use crate::ast::{Field, GenericParam, GenericParamKind, Item, StructBody, VariantKind};
use crate::token::TokenKind;

pub(crate) fn traits<'a>(module: &'a ParsedModule, item: &Item) -> Vec<&'a str> {
    item.attributes
        .as_slice(&module.arena.attrs)
        .iter()
        .flat_map(|attribute| {
            let AttrKind::Outer {
                token_open,
                token_close,
            } = attribute.kind
            else {
                return Vec::new();
            };
            let tokens = &module.tokens.tokens[token_open as usize + 1..token_close as usize];
            if tokens
                .first()
                .and_then(|token| token.symbol)
                .map(|symbol| module.tokens.intern.get_str(symbol))
                != Some("derive")
            {
                return Vec::new();
            }
            tokens
                .iter()
                .skip(2)
                .filter(|token| token.kind == TokenKind::Ident)
                .filter_map(|token| token.symbol)
                .map(|symbol| module.tokens.intern.get_str(symbol))
                .filter(|name| matches!(*name, "Eq" | "Clone" | "Print"))
                .collect()
        })
        .collect()
}

pub(crate) fn generics(item: &Item) -> AstRange<GenericParam> {
    match item.kind {
        ItemKind::Struct { generics, .. } | ItemKind::Enum { generics, .. } => generics,
        _ => AstRange::empty(),
    }
}

pub(crate) struct Shape<'a> {
    pub name: &'a str,
    pub fields: Vec<&'a Field>,
    pub record: bool,
    pub unit: bool,
}

pub(crate) fn shapes<'a>(module: &'a ParsedModule, item: &'a Item) -> Vec<Shape<'a>> {
    let name = |symbol| module.tokens.intern.get_str(symbol);
    let fields = |range: AstRange<Field>| {
        range
            .as_slice(&module.arena.fields)
            .iter()
            .enumerate()
            .filter(|(index, _)| module.configured.field_active(range.start as usize + index))
            .map(|(_, field)| field)
            .collect()
    };
    match &item.kind {
        ItemKind::Struct { body, .. } => vec![Shape {
            name: name(item.name.expect("结构体有名称")),
            fields: match body {
                StructBody::Record(range) => fields(*range),
                StructBody::Newtype(field) => vec![field],
            },
            record: matches!(body, StructBody::Record(_)),
            unit: false,
        }],
        ItemKind::Enum { variants, .. } => variants
            .as_slice(&module.arena.variants)
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                module
                    .configured
                    .variant_active(variants.start as usize + index)
            })
            .map(|(_, variant)| Shape {
                name: name(variant.name),
                fields: match variant.kind {
                    VariantKind::Unit => Vec::new(),
                    VariantKind::Tuple(range) | VariantKind::Struct(range) => fields(range),
                },
                record: matches!(variant.kind, VariantKind::Struct(_)),
                unit: matches!(variant.kind, VariantKind::Unit),
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn run(
    module: &mut ParsedModule,
    sources: &mut SourceMap,
    cfg: &CfgContext,
    token_files: &mut TokenFileTable,
    scanned: &mut usize,
) -> Result<(), Vec<Diagnostic>> {
    let end = module.arena.items.len();
    debug_assert!(*scanned <= end, "AST 声明只追加");
    for index in *scanned..end {
        let item_id = ItemId(index as u32);
        if !module.configured.item_active(item_id) {
            continue;
        }
        let item = &module.arena.items[index];
        let interfaces = traits(module, item);
        if interfaces.is_empty() {
            continue;
        }
        if !matches!(item.kind, ItemKind::Struct { .. } | ItemKind::Enum { .. }) {
            return Err(vec![Diagnostic::error(
                DiagnosticCode::InvalidType,
                "derive 只能用于结构体、newtype 或枚举",
                Some(item.span.clone()),
            )]);
        }
        let text = interfaces
            .iter()
            .map(|interface| implementation(module, item, sources, interface))
            .collect::<Vec<_>>()
            .join("\n");
        let origin = item.span.clone();
        append(module, sources, cfg, token_files, item_id, &origin, &text)?;
    }
    *scanned = end;
    Ok(())
}

fn implementation(
    module: &ParsedModule,
    item: &Item,
    sources: &SourceMap,
    interface: &str,
) -> String {
    let name = module
        .tokens
        .intern
        .get_str(item.name.expect("derive 仅用于名义类型"));
    let parameters = generics(item).as_slice(&module.arena.generic_params);
    let arguments: Vec<_> = parameters
        .iter()
        .map(|parameter| match parameter.kind {
            GenericParamKind::Type { name, .. } | GenericParamKind::Comptime { name, .. } => {
                module.tokens.intern.get_str(name)
            }
        })
        .collect();
    let declarations: Vec<_> = parameters
        .iter()
        .map(|parameter| {
            let source = sources
                .snapshot(parameter.span.file())
                .expect("泛型声明快照");
            source.content()[parameter.span.start() as usize..parameter.span.end() as usize]
                .to_owned()
        })
        .collect();
    let generic = if declarations.is_empty() {
        String::new()
    } else {
        format!("[{}]", declarations.join(", "))
    };
    let ty = if arguments.is_empty() {
        name.to_owned()
    } else {
        format!("{name}[{}]", arguments.join(", "))
    };
    let signature = match interface {
        "Eq" => "fn eq(self: &Self, other: &Self) bool",
        "Clone" => "fn clone(self: &Self) Self",
        "Print" => "fn print(self: &Self, out: &Formatter)",
        _ => unreachable!("闭集派生接口"),
    };
    let shapes = shapes(module, item);
    let body = if matches!(item.kind, ItemKind::Enum { .. }) {
        enum_body(module, name, &ty, &shapes, interface)
    } else {
        shape_body(module, &shapes[0], &ty, interface, "self.", "other.")
    };
    format!("impl{generic} {interface} for {ty} {{ {signature} {{ {body} }} }}")
}

fn enum_body(
    module: &ParsedModule,
    name: &str,
    ty: &str,
    shapes: &[Shape<'_>],
    interface: &str,
) -> String {
    let arms: Vec<_> = shapes
        .iter()
        .map(|shape| {
            let path = format!("{name}::{}", shape.name);
            let left = construct(module, shape, &path, "left", false);
            let constructor = format!("{ty}::{}", shape.name);
            let body = shape_body(module, shape, &constructor, interface, "left", "right");
            let body = if interface == "Eq" {
                let right = construct(module, shape, &path, "right", false);
                format!("match *other {{ {right} => {{ {body} }}, _ => false }}")
            } else {
                body
            };
            format!("{left} => {{ {body} }}")
        })
        .collect();
    format!("match *self {{ {} }}", arms.join(", "))
}

fn shape_body(
    module: &ParsedModule,
    shape: &Shape<'_>,
    path: &str,
    interface: &str,
    left: &str,
    right: &str,
) -> String {
    match interface {
        "Eq" => {
            let comparisons: Vec<_> = shape
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| {
                    format!(
                        "{} == {}",
                        access(module, field, index, left),
                        access(module, field, index, right)
                    )
                })
                .collect();
            if comparisons.is_empty() {
                "true".to_owned()
            } else {
                comparisons.join(" && ")
            }
        }
        "Clone" => construct(module, shape, path, left, true),
        "Print" => {
            let mut body = format!(
                "out.write_str(\"{}{}\");",
                shape.name,
                if shape.unit {
                    ""
                } else if shape.record {
                    " { "
                } else {
                    "("
                }
            );
            for (index, field) in shape.fields.iter().enumerate() {
                if index != 0 {
                    body.push_str("out.write_str(\", \");");
                }
                if shape.record {
                    body.push_str(&format!(
                        "out.write_str(\"{}: \");",
                        module.tokens.intern.get_str(field.name.expect("具名字段"))
                    ));
                }
                body.push_str(&format!(
                    "Print::print(&{}, out);",
                    access(module, field, index, left)
                ));
            }
            if !shape.unit {
                body.push_str(if shape.record {
                    if shape.fields.is_empty() {
                        "out.write_str(\"}\");"
                    } else {
                        "out.write_str(\" }\");"
                    }
                } else {
                    "out.write_str(\")\");"
                });
            }
            body
        }
        _ => unreachable!("闭集派生接口"),
    }
}

fn access(module: &ParsedModule, field: &Field, index: usize, prefix: &str) -> String {
    if prefix.ends_with('.') {
        format!(
            "{prefix}{}",
            field
                .name
                .map(|symbol| module.tokens.intern.get_str(symbol).to_owned())
                .unwrap_or_else(|| index.to_string())
        )
    } else {
        format!("{prefix}{index}")
    }
}

fn construct(
    module: &ParsedModule,
    shape: &Shape<'_>,
    path: &str,
    prefix: &str,
    clone: bool,
) -> String {
    if shape.unit {
        return path.to_owned();
    }
    let fields: Vec<_> = shape
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let value = access(module, field, index, prefix);
            let value = if clone {
                format!("Clone::clone(&{value})")
            } else {
                value
            };
            if shape.record {
                format!(
                    "{}: {value}",
                    module.tokens.intern.get_str(field.name.expect("具名字段"))
                )
            } else {
                value
            }
        })
        .collect();
    if shape.record {
        format!("{path} {{ {} }}", fields.join(", "))
    } else {
        format!("{path}({})", fields.join(", "))
    }
}

fn append(
    module: &mut ParsedModule,
    sources: &mut SourceMap,
    cfg: &CfgContext,
    token_files: &mut TokenFileTable,
    item: ItemId,
    origin: &Span,
    text: &str,
) -> Result<(), Vec<Diagnostic>> {
    let path = format!(
        "{}::derive{}",
        sources
            .snapshot(origin.file())
            .expect("派生声明快照")
            .logical_path(),
        item.0
    );
    let snapshot =
        SourceSnapshot::from_str(std::path::Path::new(&path), text).map_err(|error| {
            vec![Diagnostic::error(
                source_error_code(&error),
                error.to_string(),
                Some(origin.clone()),
            )]
        })?;
    let file = sources.push_snapshot(snapshot).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::InvalidSourcePath,
            error.to_string(),
            Some(origin.clone()),
        )]
    })?;
    let expansion = sources
        .register_expansion(ExpansionInput {
            parent: origin.expansion(),
            macro_call: origin.clone(),
            macro_definition: origin.clone(),
            generated_source: file,
            fragment_kind: SourceSlot::Item,
            round: 0,
            fragment_order: 0,
        })
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::InvalidType,
                error.to_string(),
                Some(origin.clone()),
            )]
        })?;
    let pre = module.arena.lens();
    let (fragment, arena, errors) = parse_fragment(
        text,
        sources,
        file,
        expansion,
        FragmentKind::Items,
        std::mem::take(&mut module.arena),
        &mut module.tokens,
    );
    module.arena = arena;
    if !errors.is_empty() {
        return Err(errors);
    }
    token_files.push_fragment(module.tokens.tokens.len(), file, DEFAULT_DEPTH_LIMIT);
    let FragmentAst::Items {
        mut items,
        inner_attributes,
    } = fragment
    else {
        unreachable!("派生 impl 是声明片段")
    };
    let configured = cfg::configure_fragment(
        sources.snapshot(file).expect("派生快照"),
        sources,
        file,
        &module.arena,
        &module.tokens,
        cfg,
        expansion,
        CfgRoots::Items {
            inner_attributes,
            items: &items,
        },
    )?;
    module.configured.merge_fragment(&configured, pre);
    module
        .derived_impls
        .extend(items.iter().map(|&implementation| (implementation, item)));
    // 保留原声明，只在其后插入生成 impl；共用宏展开的列表范围修正。
    items.insert(0, item);
    splice_ast(
        module,
        MacroPosition::Item(item),
        FragmentAst::Items {
            items,
            inner_attributes,
        },
    )
}
