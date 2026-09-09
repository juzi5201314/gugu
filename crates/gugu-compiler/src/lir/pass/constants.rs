//! 稀疏常量传播与代数化简。
//!
//! 只在整数环绕与 IEEE 语义成立的前提下折叠：不重结合浮点、不假设 NaN、
//! 不消除除零检查，也不把 `x * 0` 之外的浮点运算改写为常量。
use super::rewrite::{Editor, Term};
use crate::Diagnostic;
use crate::lir::body::{Condition, Conversion, IntOp, Op, Type, ValueId, ValueType};
use std::collections::BTreeMap;

/// 每个 value 的已知常量；索引即 value 编号。
pub(crate) type Known = Vec<Option<(u64, ValueType)>>;

fn width(ty: Type) -> Option<u32> {
    ty.bytes()
        .map(|bytes| u32::try_from(bytes * 8).expect("位宽适配 u32"))
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

fn sext(value: u64, bits: u32) -> i64 {
    if bits >= 64 {
        value as i64
    } else {
        let shift = 64 - bits;
        ((value << shift) as i64) >> shift
    }
}

/// 整数运算的常量折叠；除零与越界移位返回 `None`。
fn fold_integer(op: IntOp, left: u64, right: u64, ty: Type) -> Option<u64> {
    let bits = width(ty)?;
    let modulus = mask(bits);
    let left = left & modulus;
    let right = right & modulus;
    Some(match op {
        IntOp::Add => left.wrapping_add(right) & modulus,
        IntOp::Sub => left.wrapping_sub(right) & modulus,
        IntOp::Mul => left.wrapping_mul(right) & modulus,
        IntOp::DivSigned => {
            let divisor = sext(right, bits);
            if divisor == 0 {
                return None;
            }
            (sext(left, bits).checked_div(divisor)? as u64) & modulus
        }
        IntOp::DivUnsigned => {
            if right == 0 {
                return None;
            }
            (left / right) & modulus
        }
        IntOp::RemSigned => {
            let divisor = sext(right, bits);
            if divisor == 0 {
                return None;
            }
            (sext(left, bits).checked_rem(divisor)? as u64) & modulus
        }
        IntOp::RemUnsigned => {
            if right == 0 {
                return None;
            }
            (left % right) & modulus
        }
        IntOp::And => left & right,
        IntOp::Or => left | right,
        IntOp::Xor => left ^ right,
        IntOp::Shl => {
            if right >= u64::from(bits) {
                return None;
            }
            (left << right) & modulus
        }
        IntOp::ShrSigned => {
            if right >= u64::from(bits) {
                return None;
            }
            ((sext(left, bits) >> right) as u64) & modulus
        }
        IntOp::ShrUnsigned => {
            if right >= u64::from(bits) {
                return None;
            }
            (left >> right) & modulus
        }
        IntOp::Neg => 0u64.wrapping_sub(left) & modulus,
        IntOp::Not => (!left) & modulus,
        IntOp::AddCarry | IntOp::SubBorrow | IntOp::MulWide => return None,
    })
}

fn fold_compare(
    condition: Condition,
    signed: bool,
    left: u64,
    right: u64,
    ty: Type,
) -> Option<u64> {
    let bits = width(ty)?;
    let modulus = mask(bits);
    let result = if ty.integer() && signed {
        let left = sext(left, bits);
        let right = sext(right, bits);
        match condition {
            Condition::Eq => left == right,
            Condition::Ne => left != right,
            Condition::Lt => left < right,
            Condition::Le => left <= right,
            Condition::Gt => left > right,
            Condition::Ge => left >= right,
        }
    } else {
        let left = left & modulus;
        let right = right & modulus;
        match condition {
            Condition::Eq => left == right,
            Condition::Ne => left != right,
            Condition::Lt => left < right,
            Condition::Le => left <= right,
            Condition::Gt => left > right,
            Condition::Ge => left >= right,
        }
    };
    Some(u64::from(result))
}

fn fold_convert(conversion: Conversion, value: u64, from: Type, to: Type) -> Option<u64> {
    let from_bits = width(from)?;
    let to_bits = width(to)?;
    Some(match conversion {
        Conversion::SignExtend => {
            if from_bits > to_bits {
                return None;
            }
            (sext(value, from_bits) as u64) & mask(to_bits)
        }
        Conversion::ZeroExtend => value & mask(from_bits) & mask(to_bits),
        Conversion::Truncate => value & mask(to_bits),
        Conversion::Bitcast => {
            if from_bits != to_bits {
                return None;
            }
            value
        }
        Conversion::PointerToInt | Conversion::IntToPointer | Conversion::PointerCast => {
            if from_bits != to_bits {
                return None;
            }
            value
        }
        Conversion::IntToFloat { .. }
        | Conversion::FloatToInt { .. }
        | Conversion::FloatResize
        | Conversion::RawToReference => return None,
    })
}

/// 对一条纯指令做常量折叠；`arguments` 必须全部是已知常量。
fn fold_pure(op: &Op, arguments: &[(u64, ValueType)], result: Type) -> Option<u64> {
    match op {
        Op::IConst(value) => Some(*value),
        Op::Integer(operation) => match arguments {
            [(value, _)] => fold_integer(*operation, *value, 0, result),
            [(left, _), (right, _)] => fold_integer(*operation, *left, *right, result),
            _ => None,
        },
        Op::Compare { condition, signed } => match arguments {
            [(left, left_ty), (right, _)] if result == Type::I8 => {
                fold_compare(*condition, *signed, *left, *right, left_ty.ty)
            }
            _ => None,
        },
        Op::Convert(conversion) => match arguments {
            [(value, value_ty)] if result.integer() => {
                fold_convert(*conversion, *value, value_ty.ty, result)
            }
            _ => None,
        },
        _ => None,
    }
}

fn lookup(known: &Known, value: ValueId) -> Option<(u64, ValueType)> {
    known.get(value.index()).copied().flatten()
}

/// 用已知常量把条件分支/switch 折叠为无条件跳转。
pub(crate) fn fold_terminators(editor: &mut Editor, known: &Known) -> bool {
    let mut changed = false;
    for block in editor.live_blocks() {
        match editor.terminator(block).clone() {
            Term::Branch { condition, yes, no } => {
                if let Some((value, _)) = lookup(known, condition) {
                    let (keep, drop) = if value != 0 { (yes, no) } else { (no, yes) };
                    editor.set_terminator(block, Term::Jump(keep));
                    editor.remove_edge(drop);
                    changed = true;
                }
            }
            Term::Switch {
                value,
                cases,
                otherwise,
            } => {
                if let Some((constant, _)) = lookup(known, value) {
                    let target = cases
                        .iter()
                        .find(|(case, _)| *case == constant)
                        .map(|(_, edge)| *edge)
                        .unwrap_or(otherwise);
                    for (_, edge) in &cases {
                        if *edge != target {
                            editor.remove_edge(*edge);
                        }
                    }
                    if otherwise != target {
                        editor.remove_edge(otherwise);
                    }
                    editor.set_terminator(block, Term::Jump(target));
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// 稀疏条件常量传播：`IConst`、块参数与纯指令的可折叠结果。
pub(crate) fn sparse_conditional(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let known = propagate(editor);
    let mut changed = fold_terminators(editor, &known);
    changed |= super::cfg::remove_unreachable(editor)?;
    Ok(changed)
}

fn propagate(editor: &Editor) -> Known {
    let mut known: Known = vec![None; editor.values.len()];
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            let instruction = editor.instruction((block, index));
            if instruction.removed {
                continue;
            }
            if let Op::IConst(value) = &instruction.op
                && let Some(result) = instruction.results.first()
            {
                known[result.index()] = Some((*value, editor.kind(*result)));
            }
        }
    }
    loop {
        let mut changed = false;
        for block in editor.live_blocks() {
            let params = editor.block(block).expect("活跃 block").params.clone();
            for (index, param) in params.iter().enumerate().skip(1) {
                if known[param.value.index()].is_some() {
                    continue;
                }
                let mut candidate = None;
                let mut consistent = false;
                for edge in editor.predecessors(block) {
                    let edge = editor.edge(edge).expect("活跃边");
                    match known.get(edge.arguments[index].index()).copied().flatten() {
                        Some(value) => match candidate {
                            None => {
                                candidate = Some(value);
                                consistent = true;
                            }
                            Some(existing) if existing == value => {}
                            Some(_) => {
                                consistent = false;
                                break;
                            }
                        },
                        None => {
                            consistent = false;
                            break;
                        }
                    }
                }
                if consistent && let Some(value) = candidate {
                    known[param.value.index()] = Some(value);
                    changed = true;
                }
            }
        }
        for block in editor.live_blocks() {
            for index in 0..editor.instruction_count(block) {
                let instruction = editor.instruction((block, index));
                if instruction.removed
                    || instruction.op.has_memory()
                    || instruction.op.safepoint_kind().is_some()
                    || instruction.results.len() != 1
                {
                    continue;
                }
                let result = instruction.results[0];
                if known[result.index()].is_some() {
                    continue;
                }
                let mut arguments = Vec::with_capacity(instruction.arguments.len());
                let mut complete = true;
                for argument in &instruction.arguments {
                    match lookup(&known, *argument) {
                        Some(value) => arguments.push(value),
                        None => {
                            complete = false;
                            break;
                        }
                    }
                }
                if !complete {
                    continue;
                }
                if let Some(value) = fold_pure(&instruction.op, &arguments, editor.kind(result).ty)
                {
                    known[result.index()] = Some((value, editor.kind(result)));
                    changed = true;
                }
            }
        }
        if !changed {
            return known;
        }
    }
}

/// 代数化简：常量折叠与严格恒等式。
pub(crate) fn algebraic_simplify(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    let mut constants: BTreeMap<(u64, Type), ValueId> = BTreeMap::new();
    for block in editor.live_blocks() {
        let mut index = 0;
        while index < editor.instruction_count(block) {
            let at = (block, index);
            index += 1;
            let instruction = editor.instruction(at).clone();
            if instruction.removed
                || instruction.op.has_memory()
                || instruction.op.safepoint_kind().is_some()
                || instruction.results.len() != 1
                || matches!(instruction.op, Op::IConst(_) | Op::FConst(_))
            {
                continue;
            }
            let result = instruction.results[0];
            let ty = editor.kind(result).ty;
            let mut arguments = Vec::with_capacity(instruction.arguments.len());
            let mut complete = true;
            for argument in &instruction.arguments {
                match constant_argument(editor, *argument) {
                    Some(value) => arguments.push(value),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            if complete
                && ty.integer()
                && let Some(value) = fold_pure(&instruction.op, &arguments, ty)
            {
                let materialized = materialize(editor, &mut constants, value, ty);
                editor.replace_value(result, materialized);
                editor.remove_instruction((block, index - 1))?;
                changed = true;
                continue;
            }
            if let Some(replacement) = simplify(editor, &instruction, ty) {
                let value = match replacement {
                    Replacement::Value(value) => value,
                    Replacement::Const(value) => materialize(editor, &mut constants, value, ty),
                };
                editor.replace_value(result, value);
                editor.remove_instruction((block, index - 1))?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

enum Replacement {
    Value(ValueId),
    Const(u64),
}

fn constant_argument(editor: &Editor, value: ValueId) -> Option<(u64, ValueType)> {
    let at = editor.defining_instruction(value)?;
    match &editor.instruction(at).op {
        Op::IConst(constant) => Some((*constant, editor.kind(value))),
        _ => None,
    }
}

pub(crate) fn constant_bits(editor: &Editor, value: ValueId) -> Option<u64> {
    constant_argument(editor, value).map(|(bits, _)| bits)
}

fn materialize(
    editor: &mut Editor,
    cache: &mut BTreeMap<(u64, Type), ValueId>,
    value: u64,
    ty: Type,
) -> ValueId {
    if let Some(existing) = cache.get(&(value, ty)) {
        return *existing;
    }
    let created = editor.constant(value, ty);
    cache.insert((value, ty), created);
    created
}

fn simplify(editor: &Editor, instruction: &super::rewrite::MInst, ty: Type) -> Option<Replacement> {
    let arguments = &instruction.arguments;
    let integer = ty.integer();
    match &instruction.op {
        Op::Integer(operation) => match (operation, arguments.as_slice()) {
            (IntOp::Add, [left, right]) => {
                if constant_bits(editor, *left) == Some(0) {
                    Some(Replacement::Value(*right))
                } else if constant_bits(editor, *right) == Some(0) {
                    Some(Replacement::Value(*left))
                } else {
                    None
                }
            }
            (IntOp::Sub, [left, right]) if integer => {
                if constant_bits(editor, *right) == Some(0) {
                    Some(Replacement::Value(*left))
                } else if left == right {
                    Some(Replacement::Const(0))
                } else {
                    None
                }
            }
            (IntOp::Mul, [left, right]) if integer => {
                if constant_bits(editor, *left) == Some(1) {
                    Some(Replacement::Value(*right))
                } else if constant_bits(editor, *right) == Some(1) {
                    Some(Replacement::Value(*left))
                } else if constant_bits(editor, *left) == Some(0)
                    || constant_bits(editor, *right) == Some(0)
                {
                    Some(Replacement::Const(0))
                } else {
                    None
                }
            }
            (IntOp::And, [left, right]) if integer => {
                let bits = mask(width(ty)?);
                if left == right {
                    Some(Replacement::Value(*left))
                } else if constant_bits(editor, *left) == Some(0)
                    || constant_bits(editor, *right) == Some(0)
                {
                    Some(Replacement::Const(0))
                } else if constant_bits(editor, *left) == Some(bits) {
                    Some(Replacement::Value(*right))
                } else if constant_bits(editor, *right) == Some(bits) {
                    Some(Replacement::Value(*left))
                } else {
                    None
                }
            }
            (IntOp::Or, [left, right]) if integer => {
                let bits = mask(width(ty)?);
                if left == right {
                    Some(Replacement::Value(*left))
                } else if constant_bits(editor, *left) == Some(0) {
                    Some(Replacement::Value(*right))
                } else if constant_bits(editor, *right) == Some(0) {
                    Some(Replacement::Value(*left))
                } else if constant_bits(editor, *left) == Some(bits)
                    || constant_bits(editor, *right) == Some(bits)
                {
                    Some(Replacement::Const(bits))
                } else {
                    None
                }
            }
            (IntOp::Xor, [left, right]) if integer => {
                if left == right {
                    Some(Replacement::Const(0))
                } else if constant_bits(editor, *left) == Some(0) {
                    Some(Replacement::Value(*right))
                } else if constant_bits(editor, *right) == Some(0) {
                    Some(Replacement::Value(*left))
                } else {
                    None
                }
            }
            (IntOp::Shl | IntOp::ShrSigned | IntOp::ShrUnsigned, [left, right]) => {
                if constant_bits(editor, *right) == Some(0) {
                    Some(Replacement::Value(*left))
                } else {
                    None
                }
            }
            (IntOp::Neg, [operand]) => double_operation(editor, *operand, IntOp::Neg),
            (IntOp::Not, [operand]) => double_operation(editor, *operand, IntOp::Not),
            _ => None,
        },
        Op::Compare { condition, signed } if ty == Type::I8 => {
            let [left, right] = arguments.as_slice() else {
                return None;
            };
            let operand_ty = editor.kind(*left).ty;
            if left != right || !operand_ty.integer() {
                return None;
            }
            let value = match condition {
                Condition::Eq | Condition::Le | Condition::Ge => 1,
                Condition::Ne | Condition::Lt | Condition::Gt => 0,
            };
            let _ = signed;
            Some(Replacement::Const(value))
        }
        Op::Select => {
            let [condition, yes, no] = arguments.as_slice() else {
                return None;
            };
            match constant_bits(editor, *condition) {
                Some(0) => Some(Replacement::Value(*no)),
                Some(_) => Some(Replacement::Value(*yes)),
                None => None,
            }
        }
        Op::Convert(Conversion::Truncate) => {
            let [operand] = arguments.as_slice() else {
                return None;
            };
            let at = editor.defining_instruction(*operand)?;
            let inner = editor.instruction(at);
            match (&inner.op, inner.arguments.as_slice()) {
                (Op::Convert(Conversion::SignExtend | Conversion::ZeroExtend), [source])
                    if editor.kind(*source).ty == ty =>
                {
                    Some(Replacement::Value(*source))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn double_operation(editor: &Editor, operand: ValueId, operation: IntOp) -> Option<Replacement> {
    let at = editor.defining_instruction(operand)?;
    let inner = editor.instruction(at);
    match (&inner.op, inner.arguments.as_slice()) {
        (Op::Integer(inner), [source]) if *inner == operation => Some(Replacement::Value(*source)),
        _ => None,
    }
}
