//! 死值删除与保守的栈存储删除。
use super::rewrite::Editor;
use crate::Diagnostic;
use crate::lir::body::{AliasClass, InstId, Op};

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    loop {
        let mut local = false;
        for block in editor.live_blocks() {
            for index in 0..editor.instruction_count(block) {
                let instruction = editor.instruction((block, index));
                if instruction.removed
                    || instruction.op.has_memory()
                    || instruction.op.safepoint_kind().is_some()
                    || instruction.results.is_empty()
                {
                    continue;
                }
                if instruction
                    .results
                    .iter()
                    .any(|value| editor.is_used(*value))
                {
                    continue;
                }
                editor.remove_instruction((block, index))?;
                local = true;
            }
        }
        changed |= local;
        if !local {
            break;
        }
    }
    changed |= dead_stores(editor)?;
    Ok(changed)
}

/// 删除被同地址、同宽度的后续栈存储完全覆盖的存储。
///
/// 只有指针操作数与宽度都相同才证明覆盖；中间出现同 alias 读取、调用、
/// 原子、volatile 或 safepoint 时放弃。堆、foreign 与 volatile 存储永不删除。
fn dead_stores(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut barriers: Vec<InstId> = Vec::new();
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            match &editor.instruction((block, index)).op {
                Op::GcWriteBarrier { store } | Op::GcWriteBarrierReserved { store, .. } => {
                    barriers.push(*store);
                }
                _ => {}
            }
        }
    }
    let mut changed = false;
    for block in editor.live_blocks() {
        let count = editor.instruction_count(block);
        let mut index = 0;
        while index < count {
            let instruction = editor.instruction((block, index)).clone();
            if instruction.removed {
                index += 1;
                continue;
            }
            let Op::Store(access) = &instruction.op else {
                index += 1;
                continue;
            };
            if access.volatile || !matches!(access.alias, AliasClass::Stack(_)) {
                index += 1;
                continue;
            }
            if instruction
                .origin
                .is_some_and(|origin| barriers.contains(&origin))
            {
                index += 1;
                continue;
            }
            let pointer = instruction.arguments[0];
            let size = editor.kind(instruction.arguments[1]).ty.bytes();
            let mut overwritten = false;
            let mut safe = true;
            for next in index + 1..count {
                let other = editor.instruction((block, next)).clone();
                if other.removed {
                    continue;
                }
                match &other.op {
                    Op::Store(other_access)
                        if !other_access.volatile
                            && other_access.alias == access.alias
                            && other.arguments[0] == pointer
                            && editor.kind(other.arguments[1]).ty.bytes() == size =>
                    {
                        overwritten = true;
                        break;
                    }
                    Op::Load(load) if load.alias == access.alias => {
                        safe = false;
                        break;
                    }
                    Op::Memcpy { .. }
                    | Op::Memmove { .. }
                    | Op::Memset { .. }
                    | Op::Atomic { .. } => {
                        safe = false;
                        break;
                    }
                    op if op.safepoint_kind().is_some() || op.fence() => {
                        safe = false;
                        break;
                    }
                    _ => {}
                }
            }
            if overwritten && safe {
                editor.remove_instruction((block, index))?;
                changed = true;
            }
            index += 1;
        }
    }
    Ok(changed)
}
