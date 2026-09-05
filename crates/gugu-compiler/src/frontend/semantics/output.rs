//! 检查器与后续 lowering 的版本化交接对象，不保留未收敛类型。
use super::super::ast::{ExprId, ItemId};
use super::{
    initialization::Initialization,
    model::{DefRef, Model, Ty},
};
use crate::{Diagnostic, DiagnosticCode};

pub(crate) const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckedSemantics {
    pub(crate) bodies: Vec<CheckedBody>,
    pub(crate) initialization: Vec<Initialization>,
    pub(crate) input_fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckedBody {
    pub(crate) definition: DefRef,
    pub(crate) expressions: Vec<(ExprId, Ty)>,
    pub(crate) slots: Vec<Ty>,
    pub(crate) local_statics: Vec<LocalStatic>,
    pub(crate) cleanup: Vec<CleanupRegistration>,
    pub(crate) runtime_checks: Vec<RuntimeCheck>,
    pub(crate) patterns: Vec<PatternPlan>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PatternPlan {
    pub(crate) pattern: super::super::ast::PatId,
    pub(crate) ty: Ty,
    pub(crate) bound_slots: std::ops::Range<usize>,
    pub(crate) irrefutable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LocalStatic {
    pub(crate) statement: super::super::ast::StmtId,
    pub(crate) initializer: ExprId,
    pub(crate) ty: Ty,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CleanupRegistration {
    pub(crate) statement: super::super::ast::StmtId,
    pub(crate) body: ExprId,
    pub(crate) function_exit: bool,
    pub(crate) captures: Vec<usize>,
}

/// 每个检查关联一次已类型化的操作；lowering 不重新求值操作数。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RuntimeCheck {
    pub(crate) expression: ExprId,
    pub(crate) kind: CheckKind,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CheckKind {
    /// 除数非零；有符号溢出对按语言规则产生环绕商或零余数。
    IntegerDivision {
        ty: Ty,
    },
    /// 拒绝负移位量，其余移位量按左操作数位宽取模。
    Shift {
        ty: Ty,
    },
    /// 单下标检查 0 <= i < len；切片检查 0 <= start <= end <= len。
    Bounds {
        slice: bool,
    },
    Utf8Boundary,
    FloatToInt {
        signed: bool,
        bits: u16,
    },
    UnicodeScalar,
}

impl CheckedSemantics {
    pub(crate) fn verify(&self, model: &Model<'_>) -> Result<(), Diagnostic> {
        for body in &self.bodies {
            let def = body.definition;
            let Some(module) = model.modules.get(def.module) else {
                return Err(invalid());
            };
            if !module.configured.item_active(def.item) {
                return Err(invalid());
            }
            let mut previous = None;
            for (id, ty) in &body.expressions {
                if usize::try_from(id.0).expect("u32 arena 下标") >= module.arena.exprs.len()
                    || previous.is_some_and(|p| p >= id.0)
                    || !formed(ty, model)
                {
                    return Err(invalid());
                }
                previous = Some(id.0);
            }
            if body.slots.iter().any(|ty| !formed(ty, model)) {
                return Err(invalid());
            }
            for pattern in &body.patterns {
                if pattern.pattern.0 as usize >= module.arena.pats.len()
                    || !formed(&pattern.ty, model)
                    || pattern.bound_slots.start > pattern.bound_slots.end
                    || pattern.bound_slots.end > body.slots.len()
                {
                    return Err(invalid());
                }
            }
            for init in &body.local_statics {
                let Some(statement) = module.arena.stmts.get(init.statement.0 as usize) else {
                    return Err(invalid());
                };
                if !matches!(statement.kind, super::super::ast::StmtKind::Static { value, .. } if value == init.initializer)
                    || !formed(&init.ty, model)
                {
                    return Err(invalid());
                }
            }
            for cleanup in &body.cleanup {
                if !matches!(module.arena.stmts.get(cleanup.statement.0 as usize).map(|s| s.kind), Some(super::super::ast::StmtKind::Defer { body: expr, ret }) if expr == cleanup.body && ret == cleanup.function_exit)
                    || cleanup
                        .captures
                        .iter()
                        .any(|&slot| slot >= body.slots.len())
                {
                    return Err(invalid());
                }
            }
            for check in &body.runtime_checks {
                if !body
                    .expressions
                    .iter()
                    .any(|(id, _)| *id == check.expression)
                {
                    return Err(invalid());
                }
                if let CheckKind::IntegerDivision { ty } | CheckKind::Shift { ty } = &check.kind {
                    if !matches!(
                        ty,
                        Ty::Int {
                            bits: 8 | 16 | 32 | 64 | 128,
                            ..
                        }
                    ) {
                        return Err(invalid());
                    }
                }
            }
        }
        for init in &self.initialization {
            let Some(module) = model.modules.get(init.definition.module) else {
                return Err(invalid());
            };
            if !module
                .configured
                .item_active(ItemId(init.definition.item.0))
            {
                return Err(invalid());
            }
            if !matches!(
                (
                    &module.arena.items[usize::try_from(init.definition.item.0).expect("项编号")]
                        .kind,
                    init.kind
                ),
                (
                    super::super::ast::ItemKind::Const { .. },
                    super::initialization::InitKind::Constant
                ) | (
                    super::super::ast::ItemKind::Static { .. },
                    super::initialization::InitKind::Process
                        | super::initialization::InitKind::Coroutine
                        | super::initialization::InitKind::OsThread
                )
            ) {
                return Err(invalid());
            }
        }
        Ok(())
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        // Debug 仅承载此私有、版本化 schema，不包含地址、Span 表身份或宿主路径。
        let mut hash = blake3::Hasher::new_derive_key("gugu-checked-semantics-v1");
        hash.update(&SCHEMA_VERSION.to_le_bytes());
        serde_json::to_writer(&mut hash, self).expect("语义 schema 序列化");
        *hash.finalize().as_bytes()
    }
}

fn invalid() -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::InvalidType,
        "类型检查交接对象未通过 verifier",
        None,
    )
}

pub(super) fn formed(ty: &Ty, model: &Model<'_>) -> bool {
    match ty {
        Ty::Error | Ty::Var(_) => false,
        Ty::Named(index, args) => {
            model
                .nominal
                .get(*index)
                .is_some_and(|n| n.params.len() == args.len())
                && args.iter().all(|t| formed(t, model))
        }
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t) => formed(t, model),
        Ty::Tuple(ts) => ts.iter().all(|t| formed(t, model)),
        Ty::Function(ts, ret) => ts.iter().all(|t| formed(t, model)) && formed(ret, model),
        Ty::Result(t, e) => formed(t, model) && formed(e, model),
        _ => true,
    }
}
