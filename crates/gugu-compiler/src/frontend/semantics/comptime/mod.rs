//! EarlyConst 执行域：早期常量表、ConstId interner 与 `EvaluateEarlyComptime` query。
//!
//! 该 query 在名称解析之后、类型检查之前运行，eager 求值全部顶层 `const`/`static`
//! 初始化器并登记 registry 摘要；类型检查与 HIR lowering 通过本表消费已求值常量，
//! 表中缺失的位置回退到带 fuel 的惰性求值路径。

pub(crate) mod eval;
pub(crate) mod registry;

use std::collections::BTreeMap;

use super::model::{Model, Ty};
use super::query::{restore_errors, store_errors};
use crate::frontend::ast::{ExprId, ExprKind, ItemId, ItemKind, PatKind, TyId};
use crate::frontend::cfg::CfgContext;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};
use crate::{Diagnostic, DiagnosticCode, SourceMap};

use super::comptime::eval::ConstantValue;

/// EarlyConst 结果 schema 版本。
pub(crate) const EARLY_SCHEMA_VERSION: u32 = 1;

/// 表内条目的规范身份；`expr` 为 `u32::MAX` 表示项级初始化器。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct ConstKey {
    pub(crate) module: u32,
    pub(crate) item: u32,
    pub(crate) expr: u32,
}

impl ConstKey {
    fn item(module: usize, item: u32) -> Self {
        Self {
            module: module as u32,
            item,
            expr: u32::MAX,
        }
    }

    fn expression(module: usize, item: u32, expr: u32) -> Self {
        Self {
            module: module as u32,
            item,
            expr,
        }
    }
}

/// 一条已求值并 interner 归档的早期常量。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EarlyConstant {
    pub(crate) key: ConstKey,
    pub(crate) id: u32,
    pub(crate) value: ConstantValue,
}

/// EarlyConst query 的确定性结果。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EarlyConstTable {
    pub(crate) registry_revision: u32,
    pub(crate) registry_summary: [u8; 32],
    pub(crate) constants: Vec<EarlyConstant>,
}

impl Default for EarlyConstTable {
    fn default() -> Self {
        Self {
            registry_revision: registry::REGISTRY_REVISION,
            registry_summary: registry::summary(),
            constants: Vec::new(),
        }
    }
}

impl EarlyConstTable {
    /// 返回 registry 的 revision 与规范摘要，供 action key 与缓存输入使用。
    pub(crate) fn registry_identity(&self) -> (u32, [u8; 32]) {
        (self.registry_revision, self.registry_summary)
    }

    /// 按表达式位置查询已求值常量。
    pub(crate) fn expression_value(&self, module: usize, expr: u32) -> Option<&ConstantValue> {
        let key = ConstKey::expression(module, u32::MAX, expr);
        self.constants
            .iter()
            .find(|entry| entry.key.module == key.module && entry.key.expr == key.expr)
            .map(|entry| &entry.value)
    }

    /// 校验表的规范顺序与 registry 身份。
    pub(crate) fn verify(&self) -> Result<(), Diagnostic> {
        if self.registry_revision != registry::REGISTRY_REVISION
            || self.registry_summary != registry::summary()
        {
            return Err(Diagnostic::error(
                DiagnosticCode::InvalidType,
                "早期常量表 registry 身份不合法",
                None,
            ));
        }
        let mut previous: Option<ConstKey> = None;
        for entry in &self.constants {
            if previous.is_some_and(|previous| previous >= entry.key) {
                return Err(Diagnostic::error(
                    DiagnosticCode::InvalidType,
                    "早期常量表条目顺序不合法",
                    None,
                ));
            }
            previous = Some(entry.key);
        }
        Ok(())
    }
}

/// 确定性 ConstId interner。
#[derive(Debug, Default)]
pub(crate) struct ConstInterner {
    values: Vec<ConstantValue>,
    indices: BTreeMap<ConstantValue, u32>,
}

/// interner 分配的早期常量身份。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ConstId(u32);

impl ConstId {
    /// 返回稠密编号。
    pub(crate) const fn index(self) -> u32 {
        self.0
    }
}

impl ConstInterner {
    /// interner 归档一个值；相同值得到相同 `ConstId`。
    pub(crate) fn intern(&mut self, value: ConstantValue) -> ConstId {
        let next = self.values.len() as u32;
        let id = *self.indices.entry(value.clone()).or_insert(next);
        if id == next {
            self.values.push(value);
        }
        ConstId(id)
    }
}

/// 求值 EarlyConst 域并登记结果；失败诊断经 query 状态机缓存并重绑定源码表。
pub(super) fn evaluate(
    model: &Model<'_>,
    sources: &SourceMap,
    cfg: &CfgContext,
    queries: &QueryEngine,
) -> Result<(EarlyConstTable, DependencyFingerprint), Vec<Diagnostic>> {
    let mut hash = blake3::Hasher::new_derive_key("gugu-early-comptime-input-v1");
    let configuration = format!("{cfg:?}");
    hash.update(configuration.as_bytes());
    for source in sources.snapshots() {
        hash.update(&(source.logical_path().len() as u64).to_le_bytes());
        hash.update(source.logical_path().as_bytes());
        hash.update(&source.content_hash());
    }
    hash.update(&model.name_fingerprint());
    hash.update(&registry::summary());
    let input_fingerprint = *hash.finalize().as_bytes();
    let key = QueryKey::new(
        QueryKind::EvaluateEarlyComptime,
        EARLY_SCHEMA_VERSION,
        input_fingerprint,
    );
    let result = queries.compute(key.clone(), |context| {
        for source in sources.snapshots() {
            context.record_dependency(
                QueryKey::new(QueryKind::SourceSnapshot, 1, source.logical_path()),
                source.content_hash(),
            );
        }
        context.record_dependency(
            QueryKey::new(QueryKind::Configure, 1, b"cfg"),
            *blake3::hash(configuration.as_bytes()).as_bytes(),
        );
        context.record_dependency(
            QueryKey::new(QueryKind::ResolveImports, 1, b"names"),
            model.name_fingerprint(),
        );
        match collect(model) {
            Ok(table) => Ok((
                serde_json::to_vec(&table).expect("早期常量表 schema 序列化"),
                Vec::new(),
            )),
            Err(errors) => Err(store_errors(&errors)),
        }
    });
    match result {
        Ok(result) => {
            let table: EarlyConstTable =
                serde_json::from_slice(result.payload()).map_err(|_| {
                    vec![Diagnostic::error(
                        DiagnosticCode::InvalidType,
                        "早期常量 query 缓存 schema 不合法",
                        None,
                    )]
                })?;
            table.verify().map_err(|error| vec![error])?;
            Ok((table, DependencyFingerprint::new(key, result.fingerprint())))
        }
        Err(error) => Err(restore_errors(error, sources)),
    }
}

/// eager 求值全部顶层 const/static 初始化器。
///
/// 能力、预算与 panic 违规必须在此失败；evaluator 尚不支持的构造不在此处报错，
/// 留给需要该值的使用点沿惰性路径给出精确诊断。表达式级条目（数组长度、repeat
/// 计数、范围模式端点）只缓存成功结果，失败沿惰性路径诊断，避免重复报告。
fn collect(model: &Model<'_>) -> Result<EarlyConstTable, Vec<Diagnostic>> {
    let mut entries: Vec<(ConstKey, ConstantValue)> = Vec::new();
    let mut errors = Vec::new();
    for (module, parsed) in model.modules.iter().enumerate() {
        for (index, item) in parsed.arena.items.iter().enumerate() {
            if !parsed.configured.item_active(ItemId(index as u32)) {
                continue;
            }
            match item.kind {
                ItemKind::Const { ty, value } => {
                    evaluate_item(
                        model,
                        module,
                        index as u32,
                        ty,
                        value,
                        &mut entries,
                        &mut errors,
                    );
                }
                ItemKind::Static { ty, value } => {
                    evaluate_item(
                        model,
                        module,
                        index as u32,
                        Some(ty),
                        Some(value),
                        &mut entries,
                        &mut errors,
                    );
                }
                _ => {}
            }
        }
        for kind in parsed.arena.exprs.iter().map(|expr| expr.kind) {
            if let ExprKind::Repeat { count, .. } = kind {
                cache_expression(model, module, count.0, &mut entries);
            }
        }
        for pat in parsed.arena.pats.iter() {
            if let PatKind::Range { start, end } = pat.kind {
                cache_expression(model, module, start.0, &mut entries);
                cache_expression(model, module, end.0, &mut entries);
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut interner = ConstInterner::default();
    let constants = entries
        .into_iter()
        .map(|(key, value)| EarlyConstant {
            key,
            id: interner.intern(value.clone()).index(),
            value,
        })
        .collect();
    Ok(EarlyConstTable {
        registry_revision: registry::REGISTRY_REVISION,
        registry_summary: registry::summary(),
        constants,
    })
}

fn evaluate_item(
    model: &Model<'_>,
    module: usize,
    item: u32,
    ty: Option<TyId>,
    value: Option<ExprId>,
    entries: &mut Vec<(ConstKey, ConstantValue)>,
    errors: &mut Vec<Diagnostic>,
) {
    let Some(value) = value else { return };
    let declared = match ty {
        Some(ty) => model.form(module, ty),
        None => model.constant_type(module, value),
    };
    match declared.and_then(|declared| model.eval_early_const(module, value, &declared)) {
        Ok(value) => entries.push((ConstKey::item(module, item), value)),
        Err(error) => match error.code() {
            DiagnosticCode::ComptimeCapability
            | DiagnosticCode::ComptimeBudget
            | DiagnosticCode::ComptimePanic => errors.push(error),
            _ => {}
        },
    }
}

fn cache_expression(
    model: &Model<'_>,
    module: usize,
    expr: u32,
    entries: &mut Vec<(ConstKey, ConstantValue)>,
) {
    if let Ok(value) = model.eval_early_const(module, ExprId(expr), &Ty::int()) {
        entries.push((ConstKey::expression(module, u32::MAX, expr), value));
    }
}
