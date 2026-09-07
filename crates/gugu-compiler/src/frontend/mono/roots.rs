//! `CollectMonoRoots`（query 12）：闭世界根的收集与缓存。
//!
//! 根类别按 [单态化与编译缓存](../../../docs/src/internals/monomorphization-cache.md)
//! §根与可达性：入口、导出/`used`、static/const 初始化器、global asm、
//! harness 测试项、lang item（当前 bootstrap 空源树为空集）、comptime 显式
//! 引用与 late comptime 依赖实例。`cfg` 删除项不在定义表内，天然不入根。

use super::instantiate::WalkEntry;
use super::keys::{MonoContext, MonoInterner, MonoKey};
use crate::Diagnostic;
use crate::frontend::hir;
use crate::frontend::semantics::{DefRef, ReflectionKind};
use crate::query::{QueryEngine, QueryKey, QueryKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `CollectMonoRoots` 结果 schema。
pub(crate) const ROOTS_SCHEMA: u32 = 1;

/// 一个根实例种子：key、工作条目与类别。
#[derive(Clone)]
pub(crate) struct RootSeed {
    pub key: MonoKey,
    pub entry: WalkEntry,
    pub category: RootCategoryV1,
}

/// 根类别；序数越小优先级越高，重复根保留更高优先级。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum RootCategoryV1 {
    Entry,
    Export,
    Used,
    StaticInit,
    GlobalAsm,
    Harness,
    LangItem,
    ComptimeReference,
    LateClosure,
}

/// 根集合：按 key 摘要字节序稳定输出。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MonoRootsV1 {
    pub schema: u32,
    /// 根 `MonoKey` 规范字节（按摘要排序）。
    pub keys: Vec<Vec<u8>>,
    /// 与 `keys` 一一对应的根类别。
    pub categories: Vec<RootCategoryV1>,
}

/// 收集闭世界根并缓存；返回根集合与驱动用的种子。
///
/// 载荷保存 (DefId, 类别)；根 `MonoKey` 由定义确定性重建，缓存命中后与
/// fresh 路径同构。
pub(crate) fn collect(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    queries: &QueryEngine,
) -> Result<(MonoRootsV1, Vec<RootSeed>), Vec<Diagnostic>> {
    let key = QueryKey::new(
        QueryKind::CollectMonoRoots,
        ROOTS_SCHEMA,
        context.module.input_fingerprint.to_vec(),
    );
    let mut fresh = None;
    let result = queries.compute(key, |_| {
        let seeds = compute_roots(context, interner)
            .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
        let payload: Vec<(u32, RootCategoryV1)> = seeds
            .iter()
            .map(|seed| (seed.entry.definition.0, seed.category))
            .collect();
        let bytes = serde_json::to_vec(&payload).expect("根集合序列化");
        fresh = Some(seeds);
        Ok((bytes, Vec::new()))
    });
    if let Some(seeds) = fresh {
        return Ok((roots_v1(&seeds), seeds));
    }
    let payload: Vec<(u32, RootCategoryV1)> = result
        .map_err(|error| vec![restore_error(error.to_string())])
        .and_then(|result| {
            serde_json::from_slice(result.payload())
                .map_err(|_| vec![restore_error("CollectMonoRoots 缓存 schema 不合法")])
        })?;
    let seeds = payload
        .into_iter()
        .map(|(definition, category)| {
            let entry = super::instantiate::root_entry(
                context,
                interner,
                hir::DefId(definition),
                BTreeMap::new(),
            )?;
            interner.intern_key(&entry.key)?;
            Ok(RootSeed {
                key: entry.key.clone(),
                entry,
                category,
            })
        })
        .collect::<Result<Vec<_>, Diagnostic>>()
        .map_err(|error| vec![error])?;
    Ok((roots_v1(&seeds), seeds))
}

fn roots_v1(seeds: &[RootSeed]) -> MonoRootsV1 {
    MonoRootsV1 {
        schema: ROOTS_SCHEMA,
        keys: seeds
            .iter()
            .map(|seed| seed.key.canonical_bytes())
            .collect(),
        categories: seeds.iter().map(|seed| seed.category).collect(),
    }
}

/// 计算当前模块的根种子集合（按 key 摘要排序）。
fn compute_roots(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
) -> Result<Vec<RootSeed>, Diagnostic> {
    let mut seeds: BTreeMap<[u8; 32], RootSeed> = BTreeMap::new();
    // 入口：可执行 `main`。
    if let Some(entry) = context.module.entry {
        push(context, interner, &mut seeds, entry, RootCategoryV1::Entry)?;
    }
    // 导出与 used：链接属性表。
    for linkage in &context.module.linkage {
        let category = if linkage.used {
            RootCategoryV1::Used
        } else if linkage.export_name.is_some() || is_pub_export(context, linkage.definition) {
            RootCategoryV1::Export
        } else {
            continue;
        };
        push(context, interner, &mut seeds, linkage.definition, category)?;
    }
    // static/const 初始化器：有 owner 的初始化条目。
    for initialization in &context.module.initialization {
        push(
            context,
            interner,
            &mut seeds,
            initialization.definition,
            RootCategoryV1::StaticInit,
        )?;
    }
    // global asm owner。
    for owner in &context.module.owners {
        if context.module.definitions[owner.definition.index()].kind
            == hir::DefinitionKind::GlobalAsm
        {
            push(
                context,
                interner,
                &mut seeds,
                owner.definition,
                RootCategoryV1::GlobalAsm,
            )?;
        }
    }
    // harness 测试/基准项：harness 域内带 `#[test]`/`#[bench]` 属性的函数。
    if context.harness {
        for (index, reference) in context.definition_ref.iter().enumerate() {
            let Some(reference) = reference else {
                continue;
            };
            if test_bench_marked(context, *reference) {
                push(
                    context,
                    interner,
                    &mut seeds,
                    hir::DefId(index as u32),
                    RootCategoryV1::Harness,
                )?;
            }
        }
    }
    // late comptime 依赖实例：使用 `type_id_count()`/`TypeAsInt` 的 owner
    // 是 late 闭包根，保证其静态 callee 进入闭世界（LateConstKey 由阶段 25 物化）。
    for body in &context.checked.bodies {
        if body.reflections.iter().any(|reflection| {
            matches!(
                reflection.kind,
                ReflectionKind::TypeIdCount | ReflectionKind::TypeAsInt
            )
        }) && let Some(definition) = context.definition_of(&body.definition)
        {
            push(
                context,
                interner,
                &mut seeds,
                definition,
                RootCategoryV1::LateClosure,
            )?;
        }
    }
    // lang item 根：runtime/std 源树当前为 bootstrap 空单元，登记表为空集。
    // comptime 显式引用的 metadata 类型根由实例遍历中的 `TypeId`/`TypeName`
    // 反射统一登记，不在此重复收集。
    Ok(seeds.into_values().collect())
}

/// 推入根种子；同 key 重复时保留类别优先级更高的记录。
fn push(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    seeds: &mut BTreeMap<[u8; 32], RootSeed>,
    definition: hir::DefId,
    category: RootCategoryV1,
) -> Result<(), Diagnostic> {
    let entry = super::instantiate::root_entry(context, interner, definition, BTreeMap::new())?;
    interner.intern_key(&entry.key)?;
    let digest = entry.key.digest();
    match seeds.get_mut(&digest) {
        Some(existing) => {
            if category < existing.category {
                existing.category = category;
            }
        }
        None => {
            seeds.insert(
                digest,
                RootSeed {
                    key: entry.key.clone(),
                    entry,
                    category,
                },
            );
        }
    }
    Ok(())
}

/// 判定 pub extern "C" 有体函数（C 导出）。
fn is_pub_export(context: &MonoContext<'_>, definition: hir::DefId) -> bool {
    let Some(reference) = context.definition_ref[definition.index()] else {
        return false;
    };
    let module = &context.model.modules[reference.module];
    let Some(item) = module.arena.items.get(reference.item.0 as usize) else {
        return false;
    };
    if item.visibility != crate::frontend::ast::Visibility::Pub {
        return false;
    }
    let crate::frontend::ast::ItemKind::Function(function) = item.kind else {
        return false;
    };
    let declaration = &module.arena.fns[function.0 as usize];
    declaration.extern_abi.is_some() && declaration.body != crate::frontend::ast::FnBody::None
}

/// 判定测试/基准标记属性。
fn test_bench_marked(context: &MonoContext<'_>, reference: DefRef) -> bool {
    let module = &context.model.modules[reference.module];
    let Some(item) = module.arena.items.get(reference.item.0 as usize) else {
        return false;
    };
    context
        .model
        .attributes(reference.module, item.attributes)
        .any(|(name, _)| name == "test" || name == "bench")
}

fn restore_error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(crate::DiagnosticCode::MonoDivergence, message.into(), None)
}
