use super::{
    Value,
    graph::Program,
    universe::{Shape, invalid},
};
use crate::frontend::{
    ast::{AssignOp, BinOp, UnOp},
    hir::{self, Builtin, CallTarget, ExprId, ExprKind, PatternKind, StatementKind},
};
use crate::{Diagnostic, DiagnosticCode};

pub(super) struct Evaluator<'p, 'a> {
    pub program: &'p Program<'a>,
    fuel: u64,
    heap: u64,
    depth: u32,
}

struct Frame {
    instance: usize,
    locals: Vec<Option<Value>>,
    exit: Option<(hir::ExitTarget, Value)>,
}

impl<'p, 'a> Evaluator<'p, 'a> {
    pub fn new(program: &'p Program<'a>) -> Self {
        Self {
            program,
            fuel: 1_000_000,
            heap: 0,
            depth: 0,
        }
    }

    pub fn evaluate(&mut self, instance: usize, expr: ExprId) -> Result<Value, Diagnostic> {
        let mut frame = self.frame(instance)?;
        self.value(&mut frame, expr)
    }

    fn frame(&self, instance: usize) -> Result<Frame, Diagnostic> {
        Ok(Frame {
            instance,
            locals: vec![None; self.program.owner(instance)?.locals.len()],
            exit: None,
        })
    }

    fn allocate(&mut self, count: usize) -> Result<(), Diagnostic> {
        self.heap = self
            .heap
            .checked_add((count as u64).saturating_mul(std::mem::size_of::<Value>() as u64))
            .ok_or_else(budget)?;
        if self.heap > 4 * 1024 * 1024 {
            return Err(budget());
        }
        Ok(())
    }

    fn value(&mut self, frame: &mut Frame, id: ExprId) -> Result<Value, Diagnostic> {
        self.fuel = self.fuel.checked_sub(1).ok_or_else(budget)?;
        self.depth += 1;
        if self.depth > 128 {
            return Err(budget());
        }
        let result = self.expression(frame, id);
        self.depth -= 1;
        let owner = self.program.owner(frame.instance)?;
        result.and_then(|value| {
            self.normalize(frame.instance, owner.expression_types[id.index()], value)
        })
    }

    fn expression(&mut self, frame: &mut Frame, id: ExprId) -> Result<Value, Diagnostic> {
        let owner = self.program.owner(frame.instance)?;
        let values = |range: &std::ops::Range<u32>| {
            &owner.expression_ids[range.start as usize..range.end as usize]
        };
        match &owner.expressions[id.index()].kind {
            ExprKind::Literal(literal) => literal_value(literal),
            ExprKind::Resolved(hir::Res::Local(local)) => frame.locals[local.index()]
                .clone()
                .ok_or_else(|| invalid("late comptime 不能读取运行时局部值")),
            ExprKind::Resolved(
                hir::Res::Def(def)
                | hir::Res::Associated {
                    definition: def, ..
                },
            ) => {
                let instance = self
                    .program
                    .constant(*def)
                    .ok_or_else(|| invalid("late 常量依赖没有进入闭世界"))?;
                let mut nested = self.frame(instance)?;
                self.value(&mut nested, self.program.owner(instance)?.body)
            }
            ExprKind::Comptime { value } => self.value(frame, *value),
            ExprKind::Tuple(range) | ExprKind::Array(range) => {
                self.allocate(values(range).len())?;
                Ok(Value::Aggregate(
                    values(range)
                        .iter()
                        .map(|id| self.value(frame, *id))
                        .collect::<Result<_, _>>()?,
                ))
            }
            ExprKind::Repeat { value, count } => {
                self.allocate(usize::try_from(*count).map_err(|_| budget())?)?;
                Ok(Value::Aggregate(vec![
                    self.value(frame, *value)?;
                    *count as usize
                ]))
            }
            ExprKind::Construct { fields, .. } => {
                let fields = &owner.fields[fields.start as usize..fields.end as usize];
                self.allocate(fields.len())?;
                let mut result = vec![Value::Unit; fields.len()];
                for field in fields {
                    result[field.field as usize] = self.value(frame, field.value)?;
                }
                Ok(Value::Aggregate(result))
            }
            ExprKind::Field { base, index } => {
                let Value::Aggregate(fields) = self.value(frame, *base)? else {
                    return Err(invalid("late 字段访问要求聚合值"));
                };
                fields
                    .into_iter()
                    .nth(*index as usize)
                    .ok_or_else(|| invalid("late 字段越界"))
            }
            ExprKind::Index { base, index, .. } => {
                let Value::Aggregate(fields) = self.value(frame, *base)? else {
                    return Err(invalid("late 下标要求数组或元组"));
                };
                let Value::Integer(index) = self.value(frame, *index)? else {
                    return Err(invalid("late 下标要求整数"));
                };
                fields
                    .into_iter()
                    .nth(usize::try_from(index).map_err(|_| invalid("late 下标越界"))?)
                    .ok_or_else(|| invalid("late 下标越界"))
            }
            ExprKind::Block { statements, tail } => {
                for statement in
                    &owner.statement_ids[statements.start as usize..statements.end as usize]
                {
                    self.statement(frame, *statement)?;
                    if frame.exit.is_some() {
                        return Ok(Value::Unit);
                    }
                }
                tail.map_or(Ok(Value::Unit), |e| self.value(frame, e))
            }
            ExprKind::If {
                condition,
                then_value,
                else_value,
            } => {
                if self.condition(frame, *condition)? {
                    self.value(frame, *then_value)
                } else {
                    else_value.map_or(Ok(Value::Unit), |e| self.value(frame, e))
                }
            }
            ExprKind::Binary {
                operation,
                left,
                right,
                dispatch,
            } => {
                if dispatch.is_some() {
                    return self.call(frame, id, None, &[*left, *right]);
                }
                let a = self.value(frame, *left)?;
                if matches!(
                    (&a, operation),
                    (Value::Bool(false), BinOp::And) | (Value::Bool(true), BinOp::Or)
                ) {
                    return Ok(a);
                }
                let b = self.value(frame, *right)?;
                self.binary(
                    frame.instance,
                    owner.expression_types[left.index()],
                    *operation,
                    a,
                    b,
                )
            }
            ExprKind::Unary { operation, value } => match (operation, self.value(frame, *value)?) {
                (UnOp::Not, Value::Bool(v)) => Ok(Value::Bool(!v)),
                (UnOp::Neg, Value::Integer(v)) => Ok(Value::Integer(v.wrapping_neg())),
                (UnOp::BitNot, Value::Integer(v)) => Ok(Value::Integer(!v)),
                (UnOp::Neg, Value::Float(v)) => Ok(Value::Float((-f64::from_bits(v)).to_bits())),
                _ => Err(invalid("late 不允许指针或引用操作")),
            },
            ExprKind::Intrinsic {
                operation,
                arguments,
                types,
                field,
            } => self.intrinsic(frame, *operation, values(arguments), types, *field),
            ExprKind::Call {
                target,
                receiver,
                arguments,
            } => match target {
                CallTarget::Builtin(builtin) => {
                    self.intrinsic(frame, *builtin, values(arguments), &[], None)
                }
                CallTarget::Constructor { ty, .. } => {
                    let args = values(arguments);
                    if args.len() == 1
                        && matches!(
                            self.program.module.types[ty.index()],
                            hir::Type::Int { .. } | hir::Type::Float(_) | hir::Type::Char
                        )
                    {
                        let value = self.value(frame, args[0])?;
                        self.convert(frame.instance, *ty, value)
                    } else {
                        Ok(Value::Aggregate(
                            args.iter()
                                .map(|id| self.value(frame, *id))
                                .collect::<Result<_, _>>()?,
                        ))
                    }
                }
                _ => self.call(frame, id, *receiver, values(arguments)),
            },
            ExprKind::Range { start, end } => {
                let (Value::Integer(start), Value::Integer(end)) =
                    (self.value(frame, *start)?, self.value(frame, *end)?)
                else {
                    return Err(invalid("late 范围要求整数"));
                };
                Ok(Value::Range(start as i64 as i128, end as i64 as i128))
            }
            ExprKind::For {
                pattern,
                value,
                body,
                ..
            } => {
                let Value::Range(start, end) = self.value(frame, *value)? else {
                    return Err(invalid("late for 要求固定整数范围"));
                };
                for n in start..end {
                    self.bind(frame, *pattern, &Value::Integer(n as u128))?;
                    self.value(frame, *body)?;
                    if loop_exit(frame) {
                        break;
                    }
                }
                Ok(Value::Unit)
            }
            ExprKind::While { condition, body } => {
                while self.condition(frame, *condition)? {
                    self.value(frame, *body)?;
                    if loop_exit(frame) {
                        break;
                    }
                }
                Ok(Value::Unit)
            }
            ExprKind::Loop { body } => {
                loop {
                    self.value(frame, *body)?;
                    if loop_exit(frame) {
                        break;
                    }
                }
                Ok(Value::Unit)
            }
            ExprKind::Exit { target, value, .. } => {
                let value = value.map_or(Ok(Value::Unit), |id| self.value(frame, id))?;
                frame.exit = Some((*target, value));
                Ok(Value::Unit)
            }
            ExprKind::Match { value, arms } => {
                let value = self.value(frame, *value)?;
                for arm in &owner.arms[arms.start as usize..arms.end as usize] {
                    if self.bind(frame, arm.pattern, &value)?
                        && arm.guard.map_or(Ok(true), |e| self.condition(frame, e))?
                    {
                        return self.value(frame, arm.body);
                    }
                }
                Err(invalid("late match 没有匹配分支"))
            }
            _ => Err(invalid("late 闭包包含不能编译期执行的操作")),
        }
    }

    fn statement(&mut self, frame: &mut Frame, id: hir::StmtId) -> Result<(), Diagnostic> {
        let owner = self.program.owner(frame.instance)?;
        match &owner.statements[id.index()].kind {
            StatementKind::Let {
                pattern,
                value: Some(value),
                otherwise,
            } => {
                let value = self.value(frame, *value)?;
                if !self.bind(frame, *pattern, &value)? {
                    if let Some(e) = otherwise {
                        self.value(frame, *e)?;
                    } else {
                        return Err(invalid("late 绑定模式不匹配"));
                    }
                }
            }
            StatementKind::Let { value: None, .. } => {}
            StatementKind::Expression(id) => {
                self.value(frame, *id)?;
            }
            StatementKind::Assign {
                place,
                value,
                operation,
                ..
            } => {
                let value = self.value(frame, *value)?;
                let value = if *operation == AssignOp::Assign {
                    value
                } else {
                    let old = self.value(frame, *place)?;
                    self.binary(
                        frame.instance,
                        owner.expression_types[place.index()],
                        assign_op(*operation),
                        old,
                        value,
                    )?
                };
                self.assign(frame, *place, value)?;
            }
            _ => return Err(invalid("late 不允许 static、调度或清理注册")),
        }
        Ok(())
    }

    fn assign(&mut self, frame: &mut Frame, place: ExprId, value: Value) -> Result<(), Diagnostic> {
        let owner = self.program.owner(frame.instance)?;
        match &owner.expressions[place.index()].kind {
            ExprKind::Resolved(hir::Res::Local(local)) => {
                frame.locals[local.index()] =
                    Some(self.normalize(frame.instance, owner.locals[local.index()].ty, value)?);
            }
            ExprKind::Index { base, index, .. } => {
                let Value::Integer(index) = self.value(frame, *index)? else {
                    return Err(invalid("late 赋值下标不是整数"));
                };
                let Value::Aggregate(mut fields) = self.value(frame, *base)? else {
                    return Err(invalid("late 赋值要求固定聚合"));
                };
                *fields
                    .get_mut(usize::try_from(index).map_err(|_| invalid("late 赋值下标越界"))?)
                    .ok_or_else(|| invalid("late 赋值下标越界"))? = value;
                self.assign(frame, *base, Value::Aggregate(fields))?;
            }
            ExprKind::Field { base, index } => {
                let Value::Aggregate(mut fields) = self.value(frame, *base)? else {
                    return Err(invalid("late 字段赋值要求聚合"));
                };
                *fields
                    .get_mut(*index as usize)
                    .ok_or_else(|| invalid("late 字段越界"))? = value;
                self.assign(frame, *base, Value::Aggregate(fields))?;
            }
            _ => return Err(invalid("late 只能修改本次求值的局部槽")),
        }
        Ok(())
    }

    fn bind(
        &self,
        frame: &mut Frame,
        id: hir::PatternId,
        value: &Value,
    ) -> Result<bool, Diagnostic> {
        let owner = self.program.owner(frame.instance)?;
        match &owner.patterns[id.index()].kind {
            PatternKind::Wildcard => Ok(true),
            PatternKind::Bind(local) => {
                frame.locals[local.index()] = Some(value.clone());
                Ok(true)
            }
            PatternKind::Literal(literal) => Ok(literal_value(literal)? == *value),
            PatternKind::Range { start, end } => {
                let (Value::Integer(start), Value::Integer(end), Value::Integer(value)) =
                    (literal_value(start)?, literal_value(end)?, value)
                else {
                    return Err(invalid("late 范围模式不是整数"));
                };
                Ok(start <= *value && *value <= end)
            }
            PatternKind::Tuple(range) => {
                let Value::Aggregate(values) = value else {
                    return Ok(false);
                };
                let patterns = &owner.pattern_ids[range.start as usize..range.end as usize];
                if values.len() != patterns.len() {
                    return Ok(false);
                }
                for (pattern, value) in patterns.iter().zip(values) {
                    if !self.bind(frame, *pattern, value)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            PatternKind::Construct { fields, .. } => {
                let Value::Aggregate(values) = value else {
                    return Ok(false);
                };
                for field in &owner.pattern_fields[fields.start as usize..fields.end as usize] {
                    if !self.bind(frame, field.pattern, &values[field.field as usize])? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            PatternKind::At { local, pattern } => {
                frame.locals[local.index()] = Some(value.clone());
                self.bind(frame, *pattern, value)
            }
            PatternKind::Or(range) => {
                for id in &owner.pattern_ids[range.start as usize..range.end as usize] {
                    if self.bind(frame, *id, value)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => Err(invalid("late 模式不是固定标量聚合")),
        }
    }

    fn condition(&mut self, frame: &mut Frame, expr: ExprId) -> Result<bool, Diagnostic> {
        match self.value(frame, expr)? {
            Value::Bool(value) => Ok(value),
            _ => Err(invalid("late 条件必须是 bool")),
        }
    }

    fn call(
        &mut self,
        frame: &mut Frame,
        expression: ExprId,
        receiver: Option<ExprId>,
        arguments: &[ExprId],
    ) -> Result<Value, Diagnostic> {
        let instance = self
            .program
            .callee(frame.instance, expression)
            .ok_or_else(|| invalid("late 调用没有唯一静态实例"))?;
        let owner = self.program.owner(instance)?;
        if !owner.captures.is_empty() {
            return Err(invalid("late 调用不能捕获运行时环境"));
        }
        let mut args = Vec::with_capacity(arguments.len() + usize::from(receiver.is_some()));
        if let Some(receiver) = receiver {
            args.push(self.value(frame, receiver)?);
        }
        for argument in arguments {
            args.push(self.value(frame, *argument)?);
        }
        let mut nested = self.frame(instance)?;
        if owner.parameters.len() != args.len() {
            return Err(invalid("late 调用实参数量不匹配"));
        }
        for (param, arg) in owner.parameters.iter().zip(&args) {
            self.bind(&mut nested, *param, arg)?;
        }
        let value = self.value(&mut nested, owner.body)?;
        match nested.exit {
            Some((hir::ExitTarget::Return, value)) => Ok(value),
            None => Ok(value),
            _ => Err(invalid("late 函数具有未闭合控制出口")),
        }
    }

    fn intrinsic(
        &mut self,
        frame: &mut Frame,
        builtin: Builtin,
        arguments: &[ExprId],
        types: &[hir::TypeId],
        _field: Option<u32>,
    ) -> Result<Value, Diagnostic> {
        let universe = &self.program.world.universe;
        let first = |this: &mut Self, frame: &mut Frame| {
            arguments
                .first()
                .ok_or_else(|| invalid("late intrinsic 缺少实参"))
                .and_then(|id| this.value(frame, *id))
        };
        match builtin {
            Builtin::TypeIdCount => Ok(Value::Integer(universe.records.len() as u128)),
            Builtin::TypeId => {
                let key = self.program.ty(
                    frame.instance,
                    *types
                        .first()
                        .ok_or_else(|| invalid("type_id 缺少具体类型"))?,
                )?;
                universe.record(&key)?;
                Ok(Value::Type(key))
            }
            Builtin::TypeAsInt | Builtin::TypeName => {
                let Value::Type(key) = first(self, frame)? else {
                    return Err(invalid("TypeId 方法需要符号化类型身份"));
                };
                if builtin == Builtin::TypeAsInt {
                    Ok(Value::Integer(
                        universe
                            .type_id(&key)
                            .ok_or_else(|| invalid("TypeId 不在冻结集合"))?
                            as u128,
                    ))
                } else {
                    Ok(Value::String(universe.record(&key)?.name.clone()))
                }
            }
            Builtin::SizeOf | Builtin::AlignOf => {
                let key = self.program.ty(
                    frame.instance,
                    *types.first().ok_or_else(|| invalid("布局查询缺少类型"))?,
                )?;
                let (size, align) = universe
                    .record(&key)?
                    .layout
                    .ok_or_else(|| invalid("unsized 类型没有固定大小"))?;
                Ok(Value::Integer(if builtin == Builtin::SizeOf {
                    size
                } else {
                    align
                } as u128))
            }
            Builtin::Len => match first(self, frame)? {
                Value::Aggregate(values) => Ok(Value::Integer(values.len() as u128)),
                _ => Err(invalid("late len 需要固定数组")),
            },
            Builtin::Panic => Err(Diagnostic::error(
                DiagnosticCode::ComptimePanic,
                "late comptime 执行了 panic",
                None,
            )),
            _ => Err(Diagnostic::error(
                DiagnosticCode::ComptimeCapability,
                "该能力不允许在 LateConst 执行域执行",
                None,
            )),
        }
    }

    fn normalize(
        &self,
        instance: usize,
        ty: hir::TypeId,
        value: Value,
    ) -> Result<Value, Diagnostic> {
        let value = match (&self.program.module.types[ty.index()], value) {
            (hir::Type::Int { bits, .. }, Value::Integer(value)) => {
                Value::Integer(mask(value, *bits))
            }
            (hir::Type::Float(32), Value::Float(bits)) => {
                Value::Float(f64::from(f64::from_bits(bits) as f32).to_bits())
            }
            (_, value) => value,
        };
        let _ = instance;
        Ok(value)
    }

    fn convert(&self, instance: usize, ty: hir::TypeId, value: Value) -> Result<Value, Diagnostic> {
        let value = match (&self.program.module.types[ty.index()], value) {
            (hir::Type::Float(_), Value::Integer(value)) => Value::Float((value as f64).to_bits()),
            (hir::Type::Int { .. }, Value::Float(bits)) => {
                Value::Integer(f64::from_bits(bits) as u128)
            }
            (hir::Type::Char, Value::Integer(value)) => Value::Char(
                u32::try_from(value)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| invalid("late char 转换不是 Unicode scalar"))?,
            ),
            (hir::Type::Int { .. }, Value::Char(value)) => Value::Integer(value as u128),
            (_, value) => value,
        };
        self.normalize(instance, ty, value)
    }

    fn binary(
        &self,
        instance: usize,
        ty: hir::TypeId,
        op: BinOp,
        a: Value,
        b: Value,
    ) -> Result<Value, Diagnostic> {
        let (a, b) = match (a, b) {
            (Value::Type(a), Value::Type(b)) => (
                Value::Integer(
                    self.program
                        .world
                        .universe
                        .type_id(&a)
                        .ok_or_else(|| invalid("TypeId 越界"))? as u128,
                ),
                Value::Integer(
                    self.program
                        .world
                        .universe
                        .type_id(&b)
                        .ok_or_else(|| invalid("TypeId 越界"))? as u128,
                ),
            ),
            pair => pair,
        };
        let (signed, bits) = match self
            .program
            .ty(instance, ty)
            .ok()
            .and_then(|key| self.program.world.universe.record(&key).ok())
        {
            Some(record) => match record.shape {
                Shape::Int { signed, bits } => (signed, bits),
                _ => (false, 128),
            },
            None => (false, 128),
        };
        let comparison = |ordering: Option<std::cmp::Ordering>| -> Result<Value, Diagnostic> {
            use std::cmp::Ordering::*;
            Ok(Value::Bool(match op {
                BinOp::Eq => ordering == Some(Equal),
                BinOp::Ne => ordering != Some(Equal),
                BinOp::Lt => ordering == Some(Less),
                BinOp::Le => matches!(ordering, Some(Less | Equal)),
                BinOp::Gt => ordering == Some(Greater),
                BinOp::Ge => matches!(ordering, Some(Greater | Equal)),
                _ => return Err(invalid("无效 late 比较操作")),
            }))
        };
        match (a, b) {
            (Value::Integer(a), Value::Integer(b)) => {
                let sign = |v| ((v << (128 - bits)) as i128) >> (128 - bits);
                let result = match op {
                    BinOp::Add => a.wrapping_add(b),
                    BinOp::Sub => a.wrapping_sub(b),
                    BinOp::Mul => a.wrapping_mul(b),
                    BinOp::Div | BinOp::Rem if b == 0 => return Err(invalid("late 整数除零")),
                    BinOp::Div if signed => sign(a).wrapping_div(sign(b)) as u128,
                    BinOp::Rem if signed => sign(a).wrapping_rem(sign(b)) as u128,
                    BinOp::Div => a / b,
                    BinOp::Rem => a % b,
                    BinOp::BitAnd => a & b,
                    BinOp::BitOr => a | b,
                    BinOp::BitXor => a ^ b,
                    BinOp::Shl | BinOp::Shr if b >= bits as u128 => {
                        return Err(invalid("late 移位量越界"));
                    }
                    BinOp::Shl => a << b,
                    BinOp::Shr if signed => (sign(a) >> b) as u128,
                    BinOp::Shr => a >> b,
                    _ => {
                        return comparison(Some(if signed {
                            sign(a).cmp(&sign(b))
                        } else {
                            a.cmp(&b)
                        }));
                    }
                };
                Ok(Value::Integer(mask(result, bits)))
            }
            (Value::Float(a), Value::Float(b)) => {
                let (a, b) = (f64::from_bits(a), f64::from_bits(b));
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Rem => a % b,
                    _ => return comparison(a.partial_cmp(&b)),
                };
                Ok(Value::Float(v.to_bits()))
            }
            (Value::Bool(a), Value::Bool(b)) => match op {
                BinOp::And => Ok(Value::Bool(a && b)),
                BinOp::Or => Ok(Value::Bool(a || b)),
                _ => comparison(Some(a.cmp(&b))),
            },
            (Value::Char(a), Value::Char(b)) => comparison(Some(a.cmp(&b))),
            (Value::String(a), Value::String(b)) => comparison(Some(a.cmp(&b))),
            (Value::Aggregate(a), Value::Aggregate(b)) if matches!(op, BinOp::Eq | BinOp::Ne) => {
                Ok(Value::Bool((a == b) == (op == BinOp::Eq)))
            }
            _ => Err(invalid("late 二元运算类型不匹配")),
        }
    }
}

fn mask(value: u128, bits: u16) -> u128 {
    if bits == 128 {
        value
    } else {
        value & ((1u128 << bits) - 1)
    }
}
fn budget() -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ComptimeBudget,
        "late comptime 超过 fuel、heap 或深度预算",
        None,
    )
}
fn literal_value(literal: &hir::Literal) -> Result<Value, Diagnostic> {
    Ok(match literal {
        hir::Literal::Integer(v) => Value::Integer(*v),
        hir::Literal::Float(v) => Value::Float(*v),
        hir::Literal::Bool(v) => Value::Bool(*v),
        hir::Literal::Char(v) => Value::Char(*v),
        hir::Literal::String(v) => Value::String(v.clone()),
        _ => return Err(invalid("late 字节或 C 字符串不可发布")),
    })
}
fn loop_exit(frame: &mut Frame) -> bool {
    match frame.exit {
        Some((hir::ExitTarget::Break(_), _)) => {
            frame.exit = None;
            true
        }
        Some((hir::ExitTarget::Continue(_), _)) => {
            frame.exit = None;
            false
        }
        Some(_) => true,
        None => false,
    }
}
fn assign_op(op: AssignOp) -> BinOp {
    match op {
        AssignOp::Add => BinOp::Add,
        AssignOp::Sub => BinOp::Sub,
        AssignOp::Mul => BinOp::Mul,
        AssignOp::Div => BinOp::Div,
        AssignOp::Rem => BinOp::Rem,
        AssignOp::BitAnd => BinOp::BitAnd,
        AssignOp::BitOr => BinOp::BitOr,
        AssignOp::BitXor => BinOp::BitXor,
        AssignOp::Shl => BinOp::Shl,
        AssignOp::Shr => BinOp::Shr,
        AssignOp::Assign => unreachable!(),
    }
}
