//! 初始化依赖只消费名称解析后的边；线程/协程局部项不进入编译期 DAG。
use super::super::ast::{ItemId, ItemKind};
use super::model::{DefRef, Model};
use crate::{Diagnostic, DiagnosticCode};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum InitKind {
    Constant,
    Process,
    Coroutine,
    OsThread,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Initialization {
    pub(crate) definition: DefRef,
    pub(crate) kind: InitKind,
}

pub(super) fn plan(
    model: &Model<'_>,
    dependencies: &[Vec<Vec<DefRef>>],
) -> Result<Vec<Initialization>, Vec<Diagnostic>> {
    let mut kinds: Vec<Vec<Option<InitKind>>> = model
        .modules
        .iter()
        .map(|m| vec![None; m.arena.items.len()])
        .collect();
    let mut errors = Vec::new();
    for (module, parsed) in model.modules.iter().enumerate() {
        for (index, item) in parsed.arena.items.iter().enumerate() {
            if !parsed.configured.item_active(ItemId(index as u32)) {
                continue;
            }
            let coroutine = model.has_attribute(module, item.attributes, "coroutine_local");
            let thread = model.has_attribute(module, item.attributes, "os_thread_local");
            if (coroutine || thread)
                && (!matches!(item.kind, ItemKind::Static { .. }) || (coroutine && thread))
            {
                errors.push(Diagnostic::error(
                    DiagnosticCode::InvalidDeclaration,
                    "局部存储属性只能单独用于 static",
                    Some(item.span.clone()),
                ));
            }
            kinds[module][index] = match item.kind {
                ItemKind::Const { .. } => Some(InitKind::Constant),
                ItemKind::Static { .. } => Some(if coroutine {
                    InitKind::Coroutine
                } else if thread {
                    InitKind::OsThread
                } else {
                    InitKind::Process
                }),
                _ => None,
            };
        }
    }
    let mut result = Vec::new();
    // 模块/项编号是对应 arena 的稠密下标，状态不使用哈希表。
    let mut complete: Vec<Vec<bool>> = kinds.iter().map(|m| vec![false; m.len()]).collect();
    for (module, module_kinds) in kinds.iter().enumerate() {
        for (index, kind) in module_kinds.iter().enumerate() {
            let Some(kind) = kind else { continue };
            let definition = DefRef {
                module,
                item: ItemId(index as u32),
            };
            if matches!(kind, InitKind::Coroutine | InitKind::OsThread) {
                result.push(Initialization {
                    definition,
                    kind: *kind,
                });
            } else {
                let mut active = Vec::new();
                visit(
                    definition,
                    model,
                    dependencies,
                    &kinds,
                    &mut active,
                    &mut complete,
                    &mut result,
                    &mut errors,
                );
            }
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}

fn visit(
    def: DefRef,
    model: &Model<'_>,
    dependencies: &[Vec<Vec<DefRef>>],
    kinds: &[Vec<Option<InitKind>>],
    active: &mut Vec<DefRef>,
    complete: &mut [Vec<bool>],
    result: &mut Vec<Initialization>,
    errors: &mut Vec<Diagnostic>,
) {
    let index = def.item.0 as usize;
    debug_assert!(index < complete[def.module].len());
    let kind = kinds[def.module][index];
    if matches!(kind, Some(InitKind::Coroutine | InitKind::OsThread)) {
        errors.push(Diagnostic::error(
            DiagnosticCode::InvalidDeclaration,
            "编译期初始化不能读取运行时局部 static",
            Some(model.modules[def.module].arena.items[index].span.clone()),
        ));
        return;
    }
    if let Some(cycle) = active.iter().position(|&entry| entry == def) {
        // 函数自身递归合法；经过初始化器的环不合法。
        if active[cycle..]
            .iter()
            .any(|d| kinds[d.module][d.item.0 as usize].is_some())
        {
            errors.push(Diagnostic::error(
                DiagnosticCode::InvalidDeclaration,
                "const/static 初始化形成循环依赖",
                Some(model.modules[def.module].arena.items[index].span.clone()),
            ));
        }
        return;
    }
    if complete[def.module][index] {
        return;
    }
    active.push(def);
    for &dependency in &dependencies[def.module][index] {
        visit(
            dependency,
            model,
            dependencies,
            kinds,
            active,
            complete,
            result,
            errors,
        );
    }
    active.pop();
    // 函数的递归 SCC 可能从不同初始化根到达，不缓存未形成 DAG 的函数访问。
    if let Some(kind) = kind {
        complete[def.module][index] = true;
        result.push(Initialization {
            definition: def,
            kind,
        });
    }
}
