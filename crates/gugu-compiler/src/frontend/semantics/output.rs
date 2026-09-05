//! 检查器与后续 lowering 的版本化交接对象，不保留未收敛类型。
use super::super::ast::{ExprId, ItemId};
use super::{
    initialization::Initialization,
    model::{DefRef, Model, Ty},
};
use crate::{Diagnostic, DiagnosticCode};

pub(crate) const SCHEMA_VERSION: u32 = 3;

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
    pub(crate) captures: Vec<CapturePlan>,
    pub(crate) slot_storage: Vec<u8>,
    pub(crate) variadic_calls: Vec<VariadicCall>,
    pub(crate) dispatches: Vec<Dispatch>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Dispatch {
    pub(crate) expression: ExprId,
    pub(crate) callable: Option<super::model::CallableId>,
    pub(crate) implementation: Option<DefRef>,
    pub(crate) interface: Option<super::traits::TraitRef>,
    pub(crate) member: Option<u32>,
    pub(crate) self_ty: Ty,
    pub(crate) signature: Ty,
    pub(crate) dereferences: u32,
    pub(crate) borrow: bool,
    pub(crate) implicit_receiver: bool,
}

/// 槽存储标志有三个独立布尔量，固定编码在一个字节中。
pub(crate) const ADDRESS_TAKEN: u8 = 1;
pub(crate) const CAPTURED: u8 = 2;
pub(crate) const CROSS_COROUTINE: u8 = 4;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CapturePlan {
    pub(crate) expression: ExprId,
    pub(crate) function: Option<super::model::CallableId>,
    pub(crate) signature: Ty,
    pub(crate) captures: Vec<CapturedSlot>,
    pub(crate) coroutine: bool,
    pub(crate) dependencies: Vec<super::model::DefRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CapturedSlot {
    pub(crate) slot: usize,
    pub(crate) read_before_write: bool,
    pub(crate) written: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct VariadicCall {
    pub(crate) callee: ExprId,
    pub(crate) arguments: Vec<ExprId>,
    pub(crate) fixed_count: usize,
    pub(crate) element: Ty,
    pub(crate) heterogeneous: bool,
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
            if body.slot_storage.len() != body.slots.len()
                || body
                    .slot_storage
                    .iter()
                    .any(|flags| flags & !(ADDRESS_TAKEN | CAPTURED | CROSS_COROUTINE) != 0)
            {
                return Err(invalid());
            }
            for dispatch in &body.dispatches {
                if dispatch.expression.0 as usize >= module.arena.exprs.len()
                    || !formed(&dispatch.self_ty, model)
                    || !formed(&dispatch.signature, model)
                    || !matches!(dispatch.signature, Ty::Function(..))
                {
                    return Err(invalid());
                }
                if let Some(id) = dispatch.callable {
                    if !model
                        .modules
                        .get(id.module)
                        .is_some_and(|module| (id.function as usize) < module.arena.fns.len())
                        || model.function_definition(id).is_none()
                    {
                        return Err(invalid());
                    }
                }
                if let Some(definition) = dispatch.implementation {
                    if !model
                        .modules
                        .get(definition.module)
                        .and_then(|module| module.arena.items.get(definition.item.0 as usize))
                        .is_some_and(|item| {
                            matches!(
                                item.kind,
                                super::super::ast::ItemKind::Impl {
                                    negative: false,
                                    ..
                                }
                            )
                        })
                    {
                        return Err(invalid());
                    }
                }
                if let Some(interface) = &dispatch.interface {
                    let Some(definition) = model.traits.interfaces.get(interface.id) else {
                        return Err(invalid());
                    };
                    if definition.parameters.len() != interface.arguments.len()
                        || !interface.arguments.iter().all(|ty| formed(ty, model))
                        || dispatch
                            .member
                            .is_none_or(|member| member as usize >= definition.members.len())
                    {
                        return Err(invalid());
                    }
                }
            }
            for plan in &body.captures {
                if !formed(&plan.signature, model) || !matches!(plan.signature, Ty::Function(..)) {
                    return Err(invalid());
                }
                let Some(expression) = module.arena.exprs.get(plan.expression.0 as usize) else {
                    return Err(invalid());
                };
                match (plan.function, &expression.kind) {
                    (Some(id), super::super::ast::ExprKind::Closure(function))
                        if id.module == def.module
                            && id.function == function.0
                            && !plan.coroutine => {}
                    (None, super::super::ast::ExprKind::Async(_)) if plan.coroutine => {}
                    _ => return Err(invalid()),
                }
                for dependency in &plan.dependencies {
                    if !model.modules.get(dependency.module).is_some_and(|module| {
                        (dependency.item.0 as usize) < module.arena.items.len()
                            && module.configured.item_active(dependency.item)
                    }) {
                        return Err(invalid());
                    }
                }
                let mut previous = None;
                for capture in &plan.captures {
                    if capture.slot >= body.slots.len()
                        || previous.is_some_and(|slot| slot >= capture.slot)
                        || body.slot_storage[capture.slot] & CAPTURED == 0
                        || plan.coroutine && body.slot_storage[capture.slot] & CROSS_COROUTINE == 0
                    {
                        return Err(invalid());
                    }
                    previous = Some(capture.slot);
                }
            }
            for call in &body.variadic_calls {
                if call.fixed_count > call.arguments.len()
                    || !formed(&call.element, model)
                    || !body.expressions.iter().any(|(id, _)| *id == call.callee)
                    || call
                        .arguments
                        .iter()
                        .any(|arg| !body.expressions.iter().any(|(id, _)| id == arg))
                {
                    return Err(invalid());
                }
                if call.heterogeneous
                    && !matches!(&call.element, Ty::Tuple(elements) if elements.len() == call.arguments.len() - call.fixed_count)
                {
                    return Err(invalid());
                }
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
        Ty::Projection(base, interface, name) => {
            formed(base, model)
                && interface.arguments.iter().all(|ty| formed(ty, model))
                && model
                    .traits
                    .interfaces
                    .get(interface.id)
                    .is_some_and(|definition| {
                        definition.parameters.len() == interface.arguments.len()
                            && definition.members.get(name).is_some_and(|member| {
                                matches!(member.kind, super::traits::MemberKind::Type(_))
                            })
                    })
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
        Ty::Callable(id, arguments, signature) => {
            model
                .modules
                .get(id.module)
                .is_some_and(|module| (id.function as usize) < module.arena.fns.len())
                && arguments.iter().all(|ty| formed(ty, model))
                && matches!(**signature, Ty::Function(..))
                && formed(signature, model)
        }
        Ty::Result(t, e) => formed(t, model) && formed(e, model),
        _ => true,
    }
}
