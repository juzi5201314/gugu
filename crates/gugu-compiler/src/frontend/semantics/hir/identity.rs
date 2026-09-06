//! 先登记全部身份再形成类型；稠密 AST 编号仅用于本次转换，稳定键沿定义归属传播。
use super::super::{
    model::{CallableId, DefRef, Model},
    output::CheckedSemantics,
};
use crate::frontend::{ast, hir, names};
use crate::{Diagnostic, DiagnosticCode, SourceMap, Span};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) enum Origin {
    Named(usize),
    Closure(CallableId),
    Async {
        module: usize,
        expression: ast::ExprId,
    },
    LocalStatic {
        module: usize,
        statement: ast::StmtId,
    },
    Opaque(u32),
    BuiltinTrait(usize),
}

pub(super) struct Identities {
    pub(super) origins: Vec<Origin>,
    pub(super) named_items: Vec<Option<DefRef>>,
    pub(super) items: Vec<Vec<Option<hir::DefId>>>,
    pub(super) functions: Vec<Vec<Option<hir::DefId>>>,
    pub(super) asynchronous: Vec<Vec<Option<hir::DefId>>>,
    pub(super) local_statics: Vec<Vec<Option<hir::DefId>>>,
    pub(super) opaques: Vec<hir::DefId>,
    pub(super) interfaces: Vec<hir::DefId>,
}
impl Identities {
    pub(super) fn item(&self, id: DefRef) -> hir::DefId {
        self.items[id.module][id.item.0 as usize].expect("active 声明已分配 HIR 身份")
    }
    pub(super) fn function(&self, id: CallableId) -> hir::DefId {
        self.functions[id.module][id.function as usize].expect("已检查函数具有 HIR 身份")
    }
}

struct Candidate {
    origin: Origin,
    module: usize,
    span: Option<Span>,
    name: String,
    kind: hir::DefinitionKind,
    key: [u8; 32],
    parent: Option<usize>,
    public: bool,
}

pub(super) fn collect(
    model: &Model<'_>,
    names: &names::NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
) -> Result<(Vec<hir::Definition>, Identities), Diagnostic> {
    let mut candidates: Vec<_> = names
        .definitions
        .iter()
        .enumerate()
        .map(|(index, definition)| Candidate {
            origin: Origin::Named(index),
            module: definition.module.index(),
            span: Some(definition.span.clone()),
            name: definition.name.clone().unwrap_or_default(),
            kind: definition_kind(definition.kind),
            key: definition.stable_key,
            parent: definition.parent.map(|parent| parent.index()),
            public: definition.visibility == ast::Visibility::Pub,
        })
        .collect();
    let mut functions = model
        .modules
        .iter()
        .map(|module| vec![None; module.arena.fns.len()])
        .collect::<Vec<_>>();
    let mut items = model
        .modules
        .iter()
        .map(|module| vec![None; module.arena.items.len()])
        .collect::<Vec<_>>();
    let mut named_items = vec![None; names.definitions.len()];
    let mut item_by_span = BTreeMap::new();
    for (module, parsed) in model.modules.iter().enumerate() {
        for (index, item) in parsed.arena.items.iter().enumerate() {
            if parsed.configured.item_active(ast::ItemId(index as u32)) {
                item_by_span.insert((module, item.span.start(), item.span.end()), index);
            }
        }
    }
    for (index, candidate) in candidates.iter().enumerate() {
        if matches!(
            candidate.kind,
            hir::DefinitionKind::Field | hir::DefinitionKind::Variant
        ) {
            continue;
        }
        let span = candidate.span.as_ref().expect("源码声明有位置");
        if let Some(&item) = item_by_span.get(&(candidate.module, span.start(), span.end())) {
            items[candidate.module][item] = Some(index);
            named_items[index] = Some(DefRef {
                module: candidate.module,
                item: ast::ItemId(item as u32),
            });
            if let ast::ItemKind::Function(function) =
                model.modules[candidate.module].arena.items[item].kind
            {
                functions[candidate.module][function.0 as usize] = Some(index);
            }
        }
    }
    let mut asynchronous = model
        .modules
        .iter()
        .map(|module| vec![None; module.arena.exprs.len()])
        .collect::<Vec<_>>();
    let mut local_statics = model
        .modules
        .iter()
        .map(|module| vec![None; module.arena.stmts.len()])
        .collect::<Vec<_>>();
    for body in &checked.bodies {
        let module = body.definition.module;
        for capture in &body.captures {
            let (origin, span, kind, destination) = if let Some(function) = capture.function {
                (
                    Origin::Closure(function),
                    &model.modules[function.module].arena.fns[function.function as usize].span,
                    hir::DefinitionKind::Closure,
                    &mut functions[function.module][function.function as usize],
                )
            } else {
                (
                    Origin::Async {
                        module,
                        expression: capture.expression,
                    },
                    &model.modules[module].arena.exprs[capture.expression.0 as usize].span,
                    hir::DefinitionKind::Async,
                    &mut asynchronous[module][capture.expression.0 as usize],
                )
            };
            if destination.is_some() {
                continue;
            }
            *destination = Some(candidates.len());
            candidates.push(synthetic(origin, module, span.clone(), kind));
        }
        for local in &body.local_statics {
            let destination = &mut local_statics[module][local.statement.0 as usize];
            if destination.is_some() {
                continue;
            }
            *destination = Some(candidates.len());
            candidates.push(synthetic(
                Origin::LocalStatic {
                    module,
                    statement: local.statement,
                },
                module,
                model.modules[module].arena.stmts[local.statement.0 as usize]
                    .span
                    .clone(),
                hir::DefinitionKind::LocalStatic,
            ));
        }
    }
    assign_lexical_parents(&mut candidates)?;
    let mut opaque_candidates = Vec::new();
    for (index, opaque) in model.opaques.definitions.iter().enumerate() {
        let parent = match opaque.origin {
            super::super::opaque::Origin::Parameter(function)
            | super::super::opaque::Origin::Return(function) => {
                functions[function.module][function.function as usize]
            }
            super::super::opaque::Origin::Alias(definition) => {
                items[definition.module][definition.item.0 as usize]
            }
        }
        .ok_or_else(|| invalid("隐藏类型没有声明 owner"))?;
        let mut candidate = synthetic(
            Origin::Opaque(index as u32),
            opaque.module,
            model.opaque_span(index as u32).clone(),
            hir::DefinitionKind::Opaque,
        );
        candidate.parent = Some(parent);
        candidate.key = synthetic_key(&candidate, &candidates[parent]);
        opaque_candidates.push(candidates.len());
        candidates.push(candidate);
    }
    let mut interface_candidates = Vec::new();
    for (index, interface) in model.traits.interfaces.iter().enumerate() {
        if let Some(definition) = interface.definition {
            interface_candidates.push(
                items[definition.module][definition.item.0 as usize].expect("接口声明已登记"),
            );
        } else {
            let key = *blake3::Hasher::new_derive_key("gugu-hir-builtin-trait-v1")
                .update(interface.name.as_bytes())
                .finalize()
                .as_bytes();
            interface_candidates.push(candidates.len());
            candidates.push(Candidate {
                origin: Origin::BuiltinTrait(index),
                module: 0,
                span: None,
                name: interface.name.clone(),
                kind: hir::DefinitionKind::Trait,
                key,
                parent: None,
                public: true,
            });
        }
    }
    if candidates.len() >= u32::MAX as usize {
        return Err(invalid("HIR 定义数量超过 u32 上界"));
    }
    let mut order: Vec<_> = (0..candidates.len()).collect();
    order.sort_unstable_by_key(|&index| candidates[index].key);
    if order
        .windows(2)
        .any(|pair| candidates[pair[0]].key == candidates[pair[1]].key)
    {
        return Err(invalid("HIR 稳定定义键冲突"));
    }
    let mut remap = vec![hir::DefId(0); candidates.len()];
    for (index, &candidate) in order.iter().enumerate() {
        debug_assert!(index < u32::MAX as usize);
        remap[candidate] = hir::DefId(index as u32);
    }
    let origins = order
        .iter()
        .map(|&index| candidates[index].origin)
        .collect();
    let definitions = order
        .into_iter()
        .map(|index| {
            let candidate = &mut candidates[index];
            Ok(hir::Definition {
                key: candidate.key,
                name: std::mem::take(&mut candidate.name),
                parent: candidate.parent.map(|parent| remap[parent]),
                location: candidate
                    .span
                    .as_ref()
                    .map(|span| location(sources, span))
                    .transpose()?,
                kind: candidate.kind,
                signature: None,
                parameters: Vec::new(),
                obligations: Vec::new(),
                public: candidate.public,
            })
        })
        .collect::<Result<Vec<_>, Diagnostic>>()?;
    let remap_table = |table: Vec<Vec<Option<usize>>>| {
        table
            .into_iter()
            .map(|module| {
                module
                    .into_iter()
                    .map(|id| id.map(|id| remap[id]))
                    .collect()
            })
            .collect()
    };
    Ok((
        definitions,
        Identities {
            origins,
            named_items,
            items: remap_table(items),
            functions: remap_table(functions),
            asynchronous: remap_table(asynchronous),
            local_statics: remap_table(local_statics),
            opaques: opaque_candidates
                .into_iter()
                .map(|index| remap[index])
                .collect(),
            interfaces: interface_candidates
                .into_iter()
                .map(|index| remap[index])
                .collect(),
        },
    ))
}

fn synthetic(origin: Origin, module: usize, span: Span, kind: hir::DefinitionKind) -> Candidate {
    Candidate {
        origin,
        module,
        span: Some(span),
        name: String::new(),
        kind,
        key: [0; 32],
        parent: None,
        public: false,
    }
}

fn assign_lexical_parents(candidates: &mut [Candidate]) -> Result<(), Diagnostic> {
    let mut order: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            matches!(
                candidate.kind,
                hir::DefinitionKind::Function
                    | hir::DefinitionKind::Closure
                    | hir::DefinitionKind::Async
                    | hir::DefinitionKind::Constant
                    | hir::DefinitionKind::Static
                    | hir::DefinitionKind::LocalStatic
            )
            .then_some(index)
        })
        .collect();
    order.sort_unstable_by_key(|&index| {
        let candidate = &candidates[index];
        let span = candidate.span.as_ref().expect("body owner 有源码位置");
        (
            candidate.module,
            span.start(),
            std::cmp::Reverse(span.end()),
        )
    });
    let mut stack: Vec<usize> = Vec::new();
    for index in order {
        let current = &candidates[index];
        let span = current.span.as_ref().expect("源码 body");
        while let Some(&parent) = stack.last() {
            let enclosing = &candidates[parent];
            let enclosing_span = enclosing.span.as_ref().expect("源码 body");
            if current.module == enclosing.module
                && span.start() >= enclosing_span.start()
                && span.end() <= enclosing_span.end()
            {
                break;
            }
            stack.pop();
        }
        if !matches!(current.origin, Origin::Named(_)) {
            let parent = stack
                .last()
                .copied()
                .ok_or_else(|| invalid("匿名 body 没有外围声明"))?;
            candidates[index].parent = Some(parent);
            candidates[index].key = synthetic_key(&candidates[index], &candidates[parent]);
        }
        stack.push(index);
    }
    Ok(())
}

fn synthetic_key(candidate: &Candidate, parent: &Candidate) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-hir-nested-definition-v1");
    hash.update(&parent.key);
    hash.update(&[candidate.kind as u8]);
    let span = candidate.span.as_ref().expect("匿名声明有来源");
    hash.update(&span.start().to_le_bytes());
    hash.update(&span.end().to_le_bytes());
    *hash.finalize().as_bytes()
}

pub(super) fn location(sources: &SourceMap, span: &Span) -> Result<hir::Location, Diagnostic> {
    if span.table() != sources.table() {
        return Err(invalid("HIR 来源不能混用源码表"));
    }
    let file = sources
        .file_id(span.path().to_str().expect("规范 UTF-8 路径"))
        .ok_or_else(|| invalid("HIR 来源不在当前源码快照中"))?;
    Ok(hir::Location {
        source: file.as_u32(),
        start: span.start(),
        end: span.end(),
        expansion: span.expansion().as_u32(),
    })
}

fn definition_kind(kind: names::DefinitionKind) -> hir::DefinitionKind {
    match kind {
        names::DefinitionKind::Function => hir::DefinitionKind::Function,
        names::DefinitionKind::Struct => hir::DefinitionKind::Struct,
        names::DefinitionKind::Enum => hir::DefinitionKind::Enum,
        names::DefinitionKind::Union => hir::DefinitionKind::Union,
        names::DefinitionKind::Trait => hir::DefinitionKind::Trait,
        names::DefinitionKind::Impl => hir::DefinitionKind::Impl,
        names::DefinitionKind::TypeAlias => hir::DefinitionKind::TypeAlias,
        names::DefinitionKind::Const => hir::DefinitionKind::Constant,
        names::DefinitionKind::Static => hir::DefinitionKind::Static,
        names::DefinitionKind::ExternBlock => hir::DefinitionKind::ExternBlock,
        names::DefinitionKind::GlobalAsm => hir::DefinitionKind::GlobalAsm,
        names::DefinitionKind::Field => hir::DefinitionKind::Field,
        names::DefinitionKind::Variant => hir::DefinitionKind::Variant,
        names::DefinitionKind::SourceMacro => unreachable!("源码宏不能进入已检查语义"),
    }
}
fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
