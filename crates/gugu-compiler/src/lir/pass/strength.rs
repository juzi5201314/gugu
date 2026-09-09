//! 强度削减：把 2 的幂乘除余改写为移位与掩码。
//!
//! 只处理无符号除法/取余与整数乘法；有符号除法、除零检查与浮点运算不变换。
use super::constants::constant_bits;
use super::rewrite::{Editor, InstRef};
use crate::Diagnostic;
use crate::lir::body::{IntOp, Op, Type, ValueId};
use std::collections::BTreeMap;

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    let mut constants: BTreeMap<(u64, Type), ValueId> = BTreeMap::new();
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            let at: InstRef = (block, index);
            let instruction = editor.instruction(at).clone();
            if instruction.removed || instruction.results.len() != 1 {
                continue;
            }
            let ty = editor.kind(instruction.results[0]).ty;
            if !ty.integer() {
                continue;
            }
            let [left, right] = instruction.arguments.as_slice() else {
                continue;
            };
            let Some(divisor) = constant_bits(editor, *right) else {
                continue;
            };
            if divisor == 0 || !divisor.is_power_of_two() {
                continue;
            }
            let shift = divisor.trailing_zeros() as u64;
            let replacement = match instruction.op {
                Op::Integer(IntOp::Mul) => Some(IntOp::Shl),
                Op::Integer(IntOp::DivUnsigned) => Some(IntOp::ShrUnsigned),
                Op::Integer(IntOp::RemUnsigned) => None,
                _ => continue,
            };
            match replacement {
                Some(operation) => {
                    editor.set_op(at, Op::Integer(operation));
                    let constant = materialize(editor, &mut constants, shift, ty);
                    editor.set_operand(at, 1, constant);
                }
                None => {
                    // 无符号取余 2^k 等价于按位与 (2^k - 1)。
                    editor.set_op(at, Op::Integer(IntOp::And));
                    let constant = materialize(editor, &mut constants, divisor - 1, ty);
                    editor.set_operand(at, 1, constant);
                }
            }
            let _ = left;
            changed = true;
        }
    }
    Ok(changed)
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
