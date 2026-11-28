//! 先登记全部身份再形成类型；稠密 AST 编号仅用于本次转换，稳定键沿定义归属传播。
use super::super::{
    model::{CallableId, DefRef, Model},
    output::CheckedSemantics,
};
use crate::{Diagnostic, DiagnosticCode, SourceMap, Span};
use crate::{ast, hir, names};
use gugu_base::shared_table::SharedTable;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Origin {
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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Identities {
    pub(super) origins: Vec<Origin>,
    pub(super) named_items: Vec<Option<DefRef>>,
    pub items: Vec<Vec<Option<hir::DefId>>>,
    pub functions: Vec<Vec<Option<hir::DefId>>>,
    pub asynchronous: Vec<Vec<Option<hir::DefId>>>,
    pub local_statics: Vec<Vec<Option<hir::DefId>>>,
    pub opaques: Vec<hir::DefId>,
    pub interfaces: Vec<hir::DefId>,
}
impl Identities {
    /// 各 `HIR DefId` 的来源。编号前缀要求它按「物化内建块 + 会话追加」定序。
    pub fn origins(&self) -> &[Origin] {
        &self.origins
    }
    pub fn item(&self, id: DefRef) -> hir::DefId {
        self.items[id.module][id.item.0 as usize].unwrap_or_else(|| {
            panic!(
                "active 声明已分配 HIR 身份：module {} item {}",
                id.module, id.item.0
            )
        })
    }
    pub fn function(&self, id: CallableId) -> hir::DefId {
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
    track_caller: bool,
}

pub(super) fn collect(
    model: &Model<'_>,
    names: &names::NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
    prefix: Option<&crate::sema_prefix::SemaPrefix>,
) -> Result<(SharedTable<hir::Definition>, Identities, usize), Diagnostic> {
    let mut candidates: Vec<_> = names
        .definitions
        .iter()
        .enumerate()
        .map(|(index, definition)| {
            let span = names.definition_span(index);
            Candidate {
                origin: Origin::Named(index),
                module: definition.module.index(),
                track_caller: model.modules[definition.module.index()]
                    .arena
                    .items
                    .iter()
                    .find(|item| &item.span == span)
                    .is_some_and(|item| {
                        model.has_attribute(
                            definition.module.index(),
                            item.attributes,
                            "track_caller",
                        )
                    }),
                span: Some(span.clone()),
                name: definition.name.clone().unwrap_or_default(),
                kind: definition_kind(definition.kind),
                key: definition.stable_key,
                parent: definition.parent.map(|parent| parent.index()),
                public: definition.visibility == ast::Visibility::Pub,
            }
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
            continue;
        }
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
            track_caller: false,
        });
    }
    if candidates.len() >= u32::MAX as usize {
        return Err(invalid("HIR 定义数量超过 u32 上界"));
    }
    // 编号 = 「物化覆盖的内建候选在前、未覆盖的内建候选与用户模块在后」：块内保持登记顺序，
    // 跨块引用只走稳定键。判定只看「内建源里未被物化的探针源」——与模块表定序用的是同一条
    // 规则，构建期派生（没有探针源）与用户编译因此得到同一顺序；按「是否被物化表覆盖」判定
    // 会让两条路径给出不同顺序。
    let materialized_modules: Vec<bool> = model
        .modules
        .iter()
        .map(|module| {
            module.builtin
                && sources
                    .snapshot(module.file.source)
                    .is_none_or(|snapshot| !crate::is_probe_builtin_source(snapshot.logical_path()))
        })
        .collect();
    // 内建块的规模先算出来：稳定排序把「BuiltinTrait 或物化模块」的候选放在最前，前 `builtin`
    // 位就是物化表覆盖的编号前缀。
    let builtin = candidates
        .iter()
        .filter(|candidate| {
            matches!(candidate.origin, Origin::BuiltinTrait(_))
                || materialized_modules[candidate.module]
        })
        .count();
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    // 排序键：物化块优先，其次按模块编号分组，组内保持候选登记顺序。定义表因此按模块连续排列
    // （分块安装的前提），而 `Module::verify` 会校验这条单调性。
    order.sort_by_key(|&index| {
        (
            !matches!(candidates[index].origin, Origin::BuiltinTrait(_))
                && !materialized_modules[candidates[index].module],
            candidates[index].module,
            index,
        )
    });
    let mut by_key = order.clone();
    by_key.sort_unstable_by_key(|&index| candidates[index].key);
    if by_key
        .windows(2)
        .any(|pair| candidates[pair[0]].key == candidates[pair[1]].key)
    {
        return Err(invalid("HIR 稳定定义键冲突"));
    }
    let mut remap = vec![0_usize; candidates.len()];
    for (position, &index) in order.iter().enumerate() {
        remap[index] = position;
    }
    let mut origins = order
        .iter()
        .map(|&index| candidates[index].origin)
        .collect::<Vec<_>>();
    let mut definitions = if let Some(installed) = prefix {
        // 物化片段的定义整段接管：稳定键与签名/参数/义务都按构建期派生原样沿用，用户侧才走
        // 下面的 lowering。片段的顺序与本次现算逐项相同，装载前已在 origins 上核对过。
        installed.hir().module().definitions.clone()
    } else {
        SharedTable::default()
    };
    let installed_definitions = definitions.len();
    for index in order {
        if remap[index] < installed_definitions {
            continue;
        }
        let candidate = &mut candidates[index];
        definitions.push(hir::Definition {
            key: candidate.key,
            module: u32::try_from(candidate.module).map_err(|_| invalid("模块编号超出 u32"))?,
            name: std::mem::take(&mut candidate.name),
            parent: candidate.parent.map(|parent| hir_id(remap[parent])),
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
            track_caller: candidate.track_caller,
        });
    }
    let remap_table = |table: Vec<Vec<Option<usize>>>| {
        table
            .into_iter()
            .map(|module| {
                module
                    .into_iter()
                    .map(|id| id.map(|id| hir_id(remap[id])))
                    .collect()
            })
            .collect::<Vec<Vec<Option<hir::DefId>>>>()
    };
    let mut items = remap_table(items);
    let mut functions = remap_table(functions);
    let mut asynchronous = remap_table(asynchronous);
    let mut local_statics = remap_table(local_statics);
    let mut opaques = opaque_candidates
        .into_iter()
        .map(|index| hir_id(remap[index]))
        .collect::<Vec<_>>();
    let mut interfaces = interface_candidates
        .into_iter()
        .map(|index| hir_id(remap[index]))
        .collect::<Vec<_>>();
    if let Some(prefix) = prefix {
        // 物化覆盖的模块整体取自前缀；装载前先核对现算结果与前缀同量同序——不符说明物化表与本次
        // 源码树不是同一份，必须报错而不是拼出半个世界。
        let installed = prefix.identities();
        let installed_definitions = prefix.hir_definition_count() as usize;
        if installed_definitions != builtin {
            return Err(invalid(&format!(
                "内建块边界与物化前缀不符：现算 {builtin} 定义 vs 前缀 {installed_definitions}"
            )));
        }
        if origins.len() < installed_definitions
            || opaques.len() < installed.opaques.len()
            || interfaces.len() < installed.interfaces.len()
            || named_items.len() < installed.named_items.len()
        {
            return Err(invalid("内建块现算的身份少于物化前缀"));
        }
        if origins[..installed_definitions] != installed.origins[..]
            || opaques[..installed.opaques.len()] != installed.opaques[..]
            || interfaces[..installed.interfaces.len()] != installed.interfaces[..]
        {
            let fresh_named = origins[..installed_definitions]
                .iter()
                .filter(|origin| matches!(origin, Origin::Named(_)))
                .count();
            let installed_named = installed
                .origins
                .iter()
                .filter(|origin| matches!(origin, Origin::Named(_)))
                .count();
            return Err(invalid(&format!(
                "内建块现算的身份与物化前缀不一致：Named {fresh_named} vs {installed_named}"
            )));
        }
        // 核对通过后整段换上物化前缀：块内顺序已逐项相同，换的是跨会话不可沿用的会话局部身份。
        origins.splice(..installed_definitions, installed.origins.iter().copied());
        opaques.splice(..installed.opaques.len(), installed.opaques.iter().copied());
        interfaces.splice(
            ..installed.interfaces.len(),
            installed.interfaces.iter().copied(),
        );
        named_items[..installed.named_items.len()].copy_from_slice(&installed.named_items);
        for module in 0..installed.items.len() {
            items[module].clone_from_slice(&installed.items[module]);
            functions[module].clone_from_slice(&installed.functions[module]);
            asynchronous[module].clone_from_slice(&installed.asynchronous[module]);
            local_statics[module].clone_from_slice(&installed.local_statics[module]);
        }
    }
    Ok((
        definitions,
        Identities {
            origins,
            named_items,
            items,
            functions,
            asynchronous,
            local_statics,
            opaques,
            interfaces,
        },
        builtin,
    ))
}

/// 候选下标即稠密 `HIR DefId`：编号只由登记顺序决定，物化覆盖的内建块占据前缀。
fn hir_id(index: usize) -> hir::DefId {
    debug_assert!(index < u32::MAX as usize, "HIR 定义数量超过 u32 上界");
    hir::DefId(index as u32)
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
        track_caller: false,
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
