use super::super::body::{
    AggregateKind, Callee, CheckOpKind, ConstId, ConstValue, Constant, GirBody, IntrinsicOp,
    Operand, Projection, Rvalue, SpawnTarget, StatementKind, Terminator,
};
use super::{
    ConcreteCall, Diagnostic, InstanceSummaryV1, MonoContext, MonoWorldV1, id, index, invalid,
    layout,
};
use crate::frontend::semantics::{Ty, comptime::eval::ConstantValue};

pub(super) fn types(
    body: &mut GirBody,
    calls: &[ConcreteCall],
    layouts: &mut layout::Builder<'_, '_>,
) -> Result<(), Diagnostic> {
    let mut overrides = vec![None; body.locals.len()];
    for statement in &body.statements {
        if let StatementKind::Assign(
            place,
            Rvalue::Intrinsic {
                op: IntrinsicOp::StaticRef(_),
                ..
            },
        ) = &statement.kind
        {
            overrides[place.local.index()] =
                Some(layouts.address_type(body.locals[place.local.index()].ty)?);
        }
    }
    for block in &body.blocks {
        if let Terminator::Call {
            destination, site, ..
        } = &block.terminator
            && destination.is_local()
            && let Some(call) = calls.iter().find(|call| call.site == *site)
        {
            overrides[destination.local.index()] = Some(crate::frontend::hir::TypeId(call.result));
        }
    }
    body.signature.result = layouts.source(body.signature.result)?;
    for ty in &mut body.signature.parameters {
        *ty = layouts.source(*ty)?;
    }
    for (local, overridden) in body.locals.iter_mut().zip(overrides) {
        local.ty = match overridden {
            Some(ty) => ty,
            None => layouts.source(local.ty)?,
        };
    }
    for constant in &mut body.constants {
        constant.ty = layouts.source(constant.ty)?;
    }
    for copy in &mut body.large_copies {
        copy.ty = layouts.source(copy.ty)?;
    }
    for projection in &mut body.projections {
        match projection {
            Projection::Field { field_ty, .. }
            | Projection::TupleField { field_ty, .. }
            | Projection::OpaqueCast(field_ty) => *field_ty = layouts.source(*field_ty)?,
            _ => {}
        }
    }
    for statement in &mut body.statements {
        match &mut statement.kind {
            StatementKind::Assign(_, rvalue) => rvalue_types(rvalue, layouts)?,
            StatementKind::ValueAction { descriptor, .. }
            | StatementKind::ResourceAction { descriptor, .. } => {
                *descriptor = layouts.source(*descriptor)?;
            }
            StatementKind::GcWrite { value, .. } => operand_type(value, layouts)?,
            StatementKind::Atomic {
                pointer, operands, ..
            } => {
                if let Some(pointer) = pointer {
                    operand_type(pointer, layouts)?;
                }
                for operand in operands {
                    operand_type(operand, layouts)?;
                }
            }
            StatementKind::Volatile { pointer, value, .. } => {
                operand_type(pointer, layouts)?;
                if let Some(value) = value {
                    operand_type(value, layouts)?;
                }
            }
            _ => {}
        }
    }
    for block in &mut body.blocks {
        match &mut block.terminator {
            Terminator::Call { callee, args, .. } => {
                callee_type(callee, layouts)?;
                for operand in args {
                    operand_type(operand, layouts)?;
                }
            }
            Terminator::SwitchInt { value, .. } | Terminator::Panic { payload: value, .. } => {
                operand_type(value, layouts)?
            }
            _ => {}
        }
    }
    Ok(())
}

fn rvalue_types(
    value: &mut Rvalue,
    layouts: &mut layout::Builder<'_, '_>,
) -> Result<(), Diagnostic> {
    match value {
        Rvalue::Use(value)
        | Rvalue::UnaryOp { operand: value, .. }
        | Rvalue::Repeat { operand: value, .. } => operand_type(value, layouts)?,
        Rvalue::BinaryOp { left, right, .. } | Rvalue::Compare { left, right, .. } => {
            operand_type(left, layouts)?;
            operand_type(right, layouts)?;
        }
        Rvalue::CheckedOp { kind, operands, .. } => {
            if let CheckOpKind::Division { ty } | CheckOpKind::Shift { ty } = kind {
                *ty = layouts.source(*ty)?;
            }
            for operand in operands {
                operand_type(operand, layouts)?;
            }
        }
        Rvalue::Aggregate { kind, operands } => {
            if let AggregateKind::Array(ty)
            | AggregateKind::Adt { ty, .. }
            | AggregateKind::Union { ty, .. } = kind
            {
                *ty = layouts.source(*ty)?;
            }
            for operand in operands {
                operand_type(operand, layouts)?;
            }
        }
        Rvalue::Cast { operand, ty, .. } | Rvalue::DynErase { operand, ty } => {
            *ty = layouts.source(*ty)?;
            operand_type(operand, layouts)?;
        }
        Rvalue::AllocObject { ty, operands } => {
            *ty = layouts.source(*ty)?;
            for operand in operands {
                operand_type(operand, layouts)?;
            }
        }
        Rvalue::AllocArray { element, length } => {
            *element = layouts.source(*element)?;
            operand_type(length, layouts)?;
        }
        Rvalue::FunctionValue(candidate) => {
            candidate.signature = layouts.source(candidate.signature)?
        }
        Rvalue::Intrinsic {
            op,
            operands,
            types,
        } => {
            for ty in types {
                *ty = layouts.source(*ty)?;
            }
            for operand in operands {
                operand_type(operand, layouts)?;
            }
            if let IntrinsicOp::Spawn(SpawnTarget::Callee(callee)) = op {
                callee_type(callee, layouts)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn operand_type(
    operand: &mut Operand,
    layouts: &mut layout::Builder<'_, '_>,
) -> Result<(), Diagnostic> {
    if let Operand::Function(candidate) = operand {
        candidate.signature = layouts.source(candidate.signature)?;
    }
    Ok(())
}

fn callee_type(
    callee: &mut Callee,
    layouts: &mut layout::Builder<'_, '_>,
) -> Result<(), Diagnostic> {
    if let Callee::Value(operand) = callee {
        operand_type(operand, layouts)?;
    }
    Ok(())
}

pub(super) fn constants(
    context: &MonoContext<'_>,
    world: &MonoWorldV1,
    instance: &InstanceSummaryV1,
    body: &mut GirBody,
) -> Result<(), Diagnostic> {
    for constant in &mut body.constants {
        let ConstValue::Definition(definition) = constant.value else {
            continue;
        };
        let source = context.definition_ref[definition.index()]
            .ok_or_else(|| invalid("常量缺少已解析定义"))?;
        if let Some(value) = context
            .checked
            .early_constants
            .constants
            .iter()
            .find(|entry| {
                index(entry.key.module) == source.module
                    && entry.key.item == source.item.0
                    && entry.key.expr == u32::MAX
            })
            .map(|entry| &entry.value)
        {
            let ty = context.type_at(constant.ty, &instance.substitutions)?;
            constant.value = early_value(context, value, &ty)?;
        } else {
            let target = instance
                .functions
                .iter()
                .find(|function| function.definition == definition.0)
                .map(|function| function.instance)
                .or_else(|| {
                    world
                        .instances
                        .iter()
                        .find(|instance| instance.definition == definition.0)
                        .map(|instance| crate::frontend::mono::digest_of(&instance.mono_key))
                })
                .ok_or_else(|| invalid("常量定义没有闭合实例"))?;
            let owner = context
                .module
                .owners
                .iter()
                .find(|owner| owner.definition == definition)
                .ok_or_else(|| invalid("late 常量没有冻结 owner"))?;
            let result = world
                .late
                .results
                .iter()
                .find(|result| {
                    result.key.instance == target && result.key.expression == owner.body.0
                })
                .ok_or_else(|| invalid("常量定义缺少 EarlyConst/LateConst 已求值结果"))?;
            constant.value = late_value(&result.value);
        }
    }
    Ok(())
}

fn early_value(
    context: &MonoContext<'_>,
    value: &ConstantValue,
    ty: &Ty,
) -> Result<ConstValue, Diagnostic> {
    Ok(match value {
        ConstantValue::Unit => ConstValue::Unit,
        ConstantValue::Int(value) => {
            let mut value = u128::from_le_bytes(value.to_le_bytes());
            if let Ty::Int { bits, .. } = ty
                && *bits < 128
            {
                value &= (1u128 << bits) - 1;
            }
            ConstValue::Integer(value)
        }
        ConstantValue::Bool(value) => ConstValue::Bool(*value),
        ConstantValue::Float(value) => ConstValue::Float(*value),
        ConstantValue::String(value) => ConstValue::String(value.clone()),
        ConstantValue::Type(ty) => ConstValue::Type(crate::frontend::mono::keys::hash_domain(
            "gugu-mono-v1",
            &context.encode_type(ty)?,
        )),
        ConstantValue::Array(values) | ConstantValue::Tuple(values) => {
            let types = match ty {
                Ty::Array(element, _) => vec![(**element).clone(); values.len()],
                Ty::Tuple(types) => types.clone(),
                _ => return Err(invalid("早期聚合常量的已检查类型不匹配")),
            };
            let values = values
                .iter()
                .zip(&types)
                .map(|(value, ty)| early_value(context, value, ty))
                .collect::<Result<_, _>>()?;
            ConstValue::Aggregate(values)
        }
        ConstantValue::Struct(values) => {
            let variants = context
                .model
                .variants(ty)
                .ok_or_else(|| invalid("结构体常量缺少已检查字段"))?;
            let fields = variants
                .first()
                .ok_or_else(|| invalid("结构体常量没有字段布局"))?;
            ConstValue::Aggregate(
                fields
                    .fields
                    .iter()
                    .map(|field| {
                        let value = values
                            .get(&field.name)
                            .ok_or_else(|| invalid("结构体常量缺少字段值"))?;
                        early_value(context, value, &field.ty)
                    })
                    .collect::<Result<_, _>>()?,
            )
        }
        ConstantValue::ParsedSource(_)
        | ConstantValue::ResultOk(_)
        | ConstantValue::ResultErr(_) => {
            return Err(invalid("源码展开域的值不能进入运行时 GIR"));
        }
    })
}

pub(super) fn late(
    mono: &MonoWorldV1,
    instance: &InstanceSummaryV1,
    body: &mut GirBody,
) -> Result<(), Diagnostic> {
    let digest = crate::frontend::mono::digest_of(&instance.mono_key);
    for statement in &mut body.statements {
        let StatementKind::Assign(place, rvalue) = &mut statement.kind else {
            continue;
        };
        let Rvalue::Use(Operand::LateConstRef { expression }) = rvalue else {
            continue;
        };
        let result = mono
            .late
            .results
            .iter()
            .find(|result| result.key.instance == digest && result.key.expression == *expression)
            .ok_or_else(|| invalid("具体 GIR 的 late 引用没有冻结结果"))?;
        let constant = ConstId(id(body.constants.len()));
        body.constants.push(Constant {
            ty: body.locals[place.local.index()].ty,
            value: late_value(&result.value),
        });
        *rvalue = Rvalue::Use(Operand::Constant(constant));
    }
    Ok(())
}

fn late_value(value: &crate::frontend::late::Value) -> ConstValue {
    use crate::frontend::late::Value;
    match value {
        Value::Unit => ConstValue::Unit,
        Value::Bool(value) => ConstValue::Bool(*value),
        Value::Char(value) => ConstValue::Char(*value),
        Value::Integer(value) => ConstValue::Integer(*value),
        Value::Float(value) => ConstValue::Float(*value),
        Value::Type(value) => ConstValue::Type(*value),
        Value::String(value) => ConstValue::String(value.clone()),
        Value::Aggregate(values) => ConstValue::Aggregate(values.iter().map(late_value).collect()),
        Value::Range(start, end) => ConstValue::Aggregate(vec![
            ConstValue::Integer(u128::from_le_bytes(start.to_le_bytes())),
            ConstValue::Integer(u128::from_le_bytes(end.to_le_bytes())),
        ]),
    }
}
