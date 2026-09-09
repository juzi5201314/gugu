//! 分配与写屏障快路径：为 NoSafepointRegion 预留 barrier permit。
use super::rewrite::{Editor, InstRef};
use crate::Diagnostic;
use crate::lir::body::Op;

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    validate_allocations(editor)?;
    let mut changed = false;
    for (region, begin, end) in regions(editor) {
        let mut barriers = Vec::new();
        let mut reserved = false;
        for index in begin.1..end.1 {
            match &editor.instruction((begin.0, index)).op {
                Op::GcWriteBarrier { .. } => barriers.push((begin.0, index)),
                Op::GcWriteBarrierReserved { .. } => reserved = true,
                _ => {}
            }
        }
        if barriers.is_empty() || reserved {
            continue;
        }
        let max_shades = u32::try_from(barriers.len())
            .expect("屏障数量适配 u32")
            .saturating_mul(2);
        let permit = editor.add_barrier_permit(region, max_shades);
        let input = editor
            .instruction(begin)
            .memory
            .ok_or_else(|| crate::lir::invalid("NoSafepointBegin 缺少 Mem"))?
            .input;
        let (_, output) = editor.insert_memory(begin, Op::BarrierReserve(permit), &[], &[], input);
        editor.set_memory_input((begin.0, begin.1 + 1), output);
        for (block, index) in barriers {
            let shifted = if block == begin.0 && index >= begin.1 {
                index + 1
            } else {
                index
            };
            let at: InstRef = (block, shifted);
            let Op::GcWriteBarrier { store } = editor.instruction(at).op else {
                continue;
            };
            editor.set_op(at, Op::GcWriteBarrierReserved { store, permit });
        }
        changed = true;
    }
    Ok(changed)
}

/// 收集单 block 内配对的 region（region verifier 保证 begin/end 唯一）。
fn regions(editor: &Editor) -> Vec<(u32, InstRef, InstRef)> {
    let mut regions = Vec::new();
    for block in editor.live_blocks() {
        let mut open: Option<(u32, usize)> = None;
        for index in 0..editor.instruction_count(block) {
            match &editor.instruction((block, index)).op {
                Op::NoSafepointBegin(region) => open = Some((*region, index)),
                Op::NoSafepointEnd(region) => {
                    if let Some((opened, begin)) = open
                        && opened == *region
                    {
                        regions.push((opened, (block, begin), (block, index)));
                        open = None;
                    }
                }
                _ => {}
            }
        }
    }
    regions
}

/// 分配点合法性：`GcAlloc` 的 placement 与 align 由 operations verifier 覆盖；
/// 这里显式再查一次，作为 pass 边界。
pub(crate) fn validate_allocations(editor: &Editor) -> Result<(), Diagnostic> {
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            if let Op::GcAlloc { align, .. } | Op::RegionAlloc { align, .. } =
                editor.instruction((block, index)).op
                && (align == 0 || !align.is_power_of_two())
            {
                return Err(crate::lir::invalid("分配点对齐非法"));
            }
        }
    }
    Ok(())
}
