//! 检查器与后续 lowering 的版本化交接对象，不保留未收敛类型。
use super::super::ast::{ExprId, ItemId};
use super::model::RuntimeIntrinsic;
use super::{
    initialization::Initialization,
    model::{DefRef, Model, Ty},
};
use crate::{Diagnostic, DiagnosticCode};

pub const SCHEMA_VERSION: u32 = 17;

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CheckedSemantics {
    pub early_constants: super::comptime::EarlyConstTable,
    pub bodies: Vec<CheckedBody>,
    pub initialization: Vec<Initialization>,
    pub input_fingerprint: [u8; 32],
    pub hidden_types: Vec<Option<Ty>>,
    pub foreign_definitions: Vec<super::foreign::ForeignDefinition>,
    pub linkage: Vec<super::linkage::Linkage>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CheckedBody {
    pub definition: DefRef,
    pub expressions: Vec<(ExprId, Ty)>,
    pub slots: Vec<Ty>,
    pub local_statics: Vec<LocalStatic>,
    pub cleanup: Vec<CleanupRegistration>,
    pub runtime_checks: Vec<RuntimeCheck>,
    pub patterns: Vec<PatternPlan>,
    pub captures: Vec<CapturePlan>,
    pub slot_storage: Vec<u8>,
    pub slot_origins: Vec<SlotOrigin>,
    pub variadic_calls: Vec<VariadicCall>,
    pub dispatches: Vec<Dispatch>,
    pub adjustments: Vec<TypeAdjustment>,
    pub reflections: Vec<Reflection>,
    pub formatting: Vec<FormattingPart>,
    pub memory_operations: Vec<MemoryOperation>,
    pub runtime_operations: Vec<RuntimeOperation>,
    pub platform_operations: Vec<PlatformOperation>,
    pub runtime_machine_operations: Vec<RuntimeMachineOperation>,
    pub foreign_calls: Vec<super::foreign::ForeignCall>,
    pub assembly: Vec<super::assembly::AssemblyPlan>,
    pub borrow_checks: Vec<super::borrow::BorrowCheck>,
    pub must_use: Vec<MustUseSite>,
    pub source_literals: Vec<SourceLiteral>,
    pub embed_literals: Vec<EmbedLiteral>,
}

/// 丢掉必须使用的值的源码范围。诊断在 lint 级别解析之后才发射。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MustUseSite {
    pub start: u32,
    pub end: u32,
    pub expansion: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SlotOrigin {
    pub name: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryOperation {
    pub expression: ExprId,
    pub kind: super::model::MemoryIntrinsic,
    pub value: Ty,
    pub result: Ty,
    pub arguments: Vec<ExprId>,
}

/// `std.src.*` 查询的字段身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SourceField {
    File,
    Line,
    Column,
    /// `std.src.caller()`：HIR 按 owner 是否 `#[track_caller]` 决定折叠为
    /// 调用点常量还是读取调用方追加的隐藏位置形参。
    Caller,
}

/// 一次 `std.src.file`/`line`/`column` 调用折叠；位置在 checker 取定，
/// 所有下游只消费这份快照，宏展开点的 span 不进入其它常量。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceLiteral {
    pub expression: ExprId,
    pub kind: SourceField,
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// 一次 `std.mem.embed_file` 调用折叠；字节内容在 checker 的早期求值中
/// 取定，HIR 落成 `Literal::Bytes`，文件路径与内容都是编译输入。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EmbedLiteral {
    pub expression: ExprId,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeOperation {
    pub expression: ExprId,
    pub kind: RuntimeIntrinsic,
    pub ty: Ty,
    pub arguments: Vec<ExprId>,
}

/// 一次平台范围原语调用；参数与返回类型已由 checker 定型。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlatformOperation {
    pub expression: ExprId,
    pub kind: super::model::PlatformIntrinsic,
    pub ty: Ty,
    pub arguments: Vec<ExprId>,
}

/// 一次 runtime 机器原语调用；参数与返回类型已由 checker 定型。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeMachineOperation {
    pub expression: ExprId,
    pub kind: super::model::RuntimeMachineIntrinsic,
    pub ty: Ty,
    pub arguments: Vec<ExprId>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Dispatch {
    pub expression: ExprId,
    pub callable: Option<super::model::CallableId>,
    pub implementation: Option<DefRef>,
    pub interface: Option<super::traits::TraitRef>,
    pub member: Option<u32>,
    pub self_ty: Ty,
    pub signature: Ty,
    pub dereferences: u32,
    pub borrow: bool,
    pub implicit_receiver: bool,
    pub dynamic: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TypeAdjustment {
    pub expression: ExprId,
    pub source: Ty,
    pub target: Ty,
    pub kind: AdjustmentKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AdjustmentKind {
    Erase,
    Opaque,
    ArrayToSlice,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FormattingPart {
    pub part: u32,
    pub expression: ExprId,
    pub spec: super::super::string::FormatSpec<FormattingCount>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FormattingCount {
    Fixed(u64),
    Slot(usize),
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reflection {
    pub expression: ExprId,
    pub kind: ReflectionKind,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReflectionKind {
    Is(Ty),
    Downcast(Ty),
    DowncastCopy(Ty),
    TypeId(Ty),
    TypeIdCount,
    TypeName,
    TypeAsInt,
}

/// 槽存储标志有三个独立布尔量，固定编码在一个字节中。
pub const ADDRESS_TAKEN: u8 = 1;
pub const CAPTURED: u8 = 2;
pub const CROSS_COROUTINE: u8 = 4;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapturePlan {
    pub expression: ExprId,
    pub function: Option<super::model::CallableId>,
    pub signature: Ty,
    pub captures: Vec<CapturedSlot>,
    pub coroutine: bool,
    pub dependencies: Vec<super::model::DefRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapturedSlot {
    pub slot: usize,
    pub read_before_write: bool,
    pub written: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VariadicCall {
    pub callee: ExprId,
    pub arguments: Vec<ExprId>,
    pub fixed_count: usize,
    pub element: Ty,
    pub heterogeneous: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PatternPlan {
    pub pattern: super::super::ast::PatId,
    pub ty: Ty,
    pub bound_slots: std::ops::Range<usize>,
    pub irrefutable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LocalStatic {
    pub statement: super::super::ast::StmtId,
    pub initializer: ExprId,
    pub ty: Ty,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CleanupRegistration {
    pub statement: super::super::ast::StmtId,
    pub body: ExprId,
    pub function_exit: bool,
    pub captures: Vec<usize>,
}

/// 每个检查关联一次已类型化的操作；lowering 不重新求值操作数。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCheck {
    pub expression: ExprId,
    pub kind: CheckKind,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CheckKind {
    /// 除数非零；有符号溢出对按语言规则产生环绕商或零余数。
    IntegerDivision {
        ty: Ty,
        divisor: ExprId,
    },
    /// 拒绝负移位量，其余移位量按左操作数位宽取模。
    ///
    /// `ty` 是移位量类型，用于判定有符号性；左操作数位宽不参与检查。
    Shift {
        ty: Ty,
        amount: ExprId,
    },
    /// 单下标检查 0 <= i < len；切片检查 0 <= start <= end <= len。
    Bounds {
        slice: bool,
    },
    Utf8Boundary,
    FloatToInt {
        signed: bool,
        bits: u16,
        value: ExprId,
    },
    UnicodeScalar {
        value: ExprId,
    },
}

impl CheckedSemantics {
    pub fn verify(&self, model: &Model<'_>) -> Result<(), Diagnostic> {
        if self.foreign_definitions != model.foreign.definitions
            || self.linkage != model.foreign.linkage
        {
            return Err(invalid());
        }
        if self.hidden_types.len() != model.opaques.definitions.len() {
            return Err(invalid());
        }
        for (id, hidden) in self.hidden_types.iter().enumerate() {
            if model.opaque_requires_definition(id as u32) != hidden.is_some() {
                return Err(invalid());
            }
            if let Some(ty) = hidden
                && (!formed(ty, model)
                    || super::opaque::has_unbound(ty, &model.opaque_context(id as u32)))
            {
                return Err(invalid());
            }
        }
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
                || body.slot_origins.len() != body.slots.len()
                || body.slot_origins.iter().any(|origin| {
                    origin.start > origin.end || origin.end > module.file.eof_span.end()
                })
                || body
                    .slot_storage
                    .iter()
                    .any(|flags| flags & !(ADDRESS_TAKEN | CAPTURED | CROSS_COROUTINE) != 0)
            {
                return Err(invalid());
            }
            for check in &body.borrow_checks {
                if check.expression.0 as usize >= module.arena.exprs.len()
                    || !formed(&check.base, model)
                    || !formed(&check.target, model)
                {
                    return Err(invalid());
                }
            }
            for operation in &body.memory_operations {
                if operation.expression.0 as usize >= module.arena.exprs.len()
                    || !formed(&operation.value, model)
                    || !formed(&operation.result, model)
                    || operation
                        .arguments
                        .iter()
                        .any(|id| id.0 as usize >= module.arena.exprs.len())
                {
                    return Err(invalid());
                }
            }
            for operation in &body.runtime_operations {
                if usize::try_from(operation.expression.0).expect("表达式下标")
                    >= module.arena.exprs.len()
                    || !formed(&operation.ty, model)
                    || operation.arguments.len() != 2
                    || operation.arguments.iter().any(|id| {
                        usize::try_from(id.0).expect("表达式下标") >= module.arena.exprs.len()
                    })
                {
                    return Err(invalid());
                }
            }
            for operation in &body.platform_operations {
                if usize::try_from(operation.expression.0).expect("表达式下标")
                    >= module.arena.exprs.len()
                    || !formed(&operation.ty, model)
                    || operation.arguments.len() != operation.kind.value_arguments()
                    || operation.arguments.iter().any(|id| {
                        usize::try_from(id.0).expect("表达式下标") >= module.arena.exprs.len()
                    })
                {
                    return Err(invalid());
                }
            }
            for operation in &body.runtime_machine_operations {
                if usize::try_from(operation.expression.0).expect("表达式下标")
                    >= module.arena.exprs.len()
                    || !formed(&operation.ty, model)
                    || operation.arguments.len() != operation.kind.value_arguments()
                    || operation.arguments.iter().any(|id| {
                        usize::try_from(id.0).expect("表达式下标") >= module.arena.exprs.len()
                    })
                {
                    return Err(invalid());
                }
            }
            for erasure in &body.adjustments {
                if erasure.expression.0 as usize >= module.arena.exprs.len()
                    || !formed(&erasure.source, model)
                    || !formed(&erasure.target, model)
                    || erasure.source == erasure.target
                    || match erasure.kind {
                        AdjustmentKind::Erase => {
                            !matches!(erasure.target, Ty::Dyn(_) | Ty::Function(..))
                        }
                        AdjustmentKind::Opaque => !matches!(erasure.target, Ty::Opaque(..)),
                        AdjustmentKind::ArrayToSlice => !matches!(
                            (erasure.source.strip_refs(), erasure.target.strip_refs()),
                            (Ty::Array(..), Ty::Slice(_))
                        ),
                    }
                {
                    return Err(invalid());
                }
            }
            for formatting in &body.formatting {
                if !matches!(module.arena.fstring_parts.get(formatting.part as usize), Some(super::super::ast::FStringPart::Interp { expr, .. }) if *expr == formatting.expression)
                    || formatting
                        .spec
                        .width
                        .iter()
                        .chain(&formatting.spec.precision)
                        .any(|count| match count {
                            FormattingCount::Fixed(value) => *value > i64::MAX as u64,
                            FormattingCount::Slot(slot) => {
                                body.slots.get(*slot)
                                    != Some(&Ty::Int {
                                        signed: true,
                                        bits: 64,
                                    })
                            }
                        })
                {
                    return Err(invalid());
                }
            }
            for reflection in &body.reflections {
                if reflection.expression.0 as usize >= module.arena.exprs.len() {
                    return Err(invalid());
                }
                if let ReflectionKind::Is(ty)
                | ReflectionKind::Downcast(ty)
                | ReflectionKind::DowncastCopy(ty)
                | ReflectionKind::TypeId(ty) = &reflection.kind
                    && (!formed(ty, model) || matches!(ty, Ty::Never | Ty::MaybeUninit(_)))
                {
                    return Err(invalid());
                }
            }
            for dispatch in &body.dispatches {
                if dispatch.expression.0 as usize >= module.arena.exprs.len()
                    || !formed(&dispatch.self_ty, model)
                    || !formed(&dispatch.signature, model)
                    || !matches!(dispatch.signature, Ty::Function(..))
                {
                    return Err(invalid());
                }
                if dispatch.dynamic
                    && (dispatch.callable.is_some()
                        || dispatch.implementation.is_some()
                        || !matches!(&dispatch.self_ty, Ty::Dyn(interfaces) if dispatch.interface.as_ref().is_some_and(|interface| interfaces.contains(interface))))
                {
                    return Err(invalid());
                }
                if let Some(id) = dispatch.callable
                    && (!model
                        .modules
                        .get(id.module)
                        .is_some_and(|module| (id.function as usize) < module.arena.fns.len())
                        || model.function_definition(id).is_none())
                {
                    return Err(invalid());
                }
                if let Some(definition) = dispatch.implementation
                    && !model
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
                if let CheckKind::IntegerDivision { ty, .. } | CheckKind::Shift { ty, .. } =
                    &check.kind
                    && !matches!(
                        ty,
                        Ty::Int {
                            bits: 8 | 16 | 32 | 64 | 128,
                            ..
                        }
                    )
                {
                    return Err(invalid());
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
        Ty::Opaque(id, arguments) => {
            model.opaques.definitions.get(*id as usize).is_some()
                && model.opaque_context(*id).len() == arguments.len()
                && arguments.iter().all(|ty| formed(ty, model))
        }
        Ty::Dyn(interfaces) => {
            !interfaces.is_empty()
                && interfaces.iter().all(|interface| {
                    model
                        .traits
                        .interfaces
                        .get(interface.id)
                        .is_some_and(|definition| {
                            definition.parameters.len() == interface.arguments.len()
                        })
                        && interface.arguments.iter().all(|ty| formed(ty, model))
                        && model.object_safe(interface.id).is_ok()
                })
        }
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => formed(t, model),
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
