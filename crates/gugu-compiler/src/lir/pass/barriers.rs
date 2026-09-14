//! 分配与写屏障快路径：为 NoSafepointRegion 预留 barrier permit。
//!
//! permit 的额度是 compile-time 证明：`max_shades` 覆盖 region 内每条 hybrid barrier 的两个
//! shade slot，`max_card_marks` 覆盖 region 内可能触及的**distinct 写入地址**上界。同一
//! `ValueId` 必然解析到同一地址、同一 512 字节 card，因此可在本地 dedup 表里合并为一个
//! card-mark slot；不同地址各自计一个 slot（保守上界）。region 内不得再检查容量或连接
//! refill edge，额度不足只能在 region 外走 mandatory statepoint。
use super::rewrite::{Editor, InstRef};
use crate::Diagnostic;
use crate::lir::body::{Op, ValueId};

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    validate_allocations(editor)?;
    let mut changed = false;
    while let Some((region, begin, end)) = next_unreserved(editor)? {
        let mut barriers = Vec::new();
        for index in begin.1..end.1 {
            if matches!(
                editor.instruction((begin.0, index)).op,
                Op::GcWriteBarrier { .. }
            ) {
                barriers.push((begin.0, index));
            }
        }
        let max_shades = u32::try_from(barriers.len())
            .expect("屏障数量适配 u32")
            .saturating_mul(2);
        let max_card_marks = card_mark_quota(editor, &barriers)?;
        let permit = editor.add_barrier_permit(region, max_shades, max_card_marks);
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

/// 计算 card-mark 额度：region 内全部屏障的**distinct 写入地址**数量。
///
/// 屏障的首操作数是实际写入位置的地址；同一 `ValueId` 在 region 内不可能被重定义，
/// 因此相同 `ValueId` 必然落在同一 arena 的同一 card 上，可以共用一个 slot。不同
/// `ValueId` 可能碰巧落在同一 card，但那只会让真实消耗小于额度，不破坏闭包。
pub(crate) fn card_mark_quota(
    editor: &Editor,
    barriers: &[(crate::lir::body::BlockId, usize)],
) -> Result<u32, Diagnostic> {
    // 地址集合用有序 `Vec` + 二分查找：region 内屏障数量很小，省掉哈希表分配，
    // 与 verifier 的复算保持同一个表示。
    let mut addresses: Vec<ValueId> = Vec::new();
    for &(block, index) in barriers {
        let instruction = editor.instruction((block, index));
        let Op::GcWriteBarrier { .. } = instruction.op else {
            continue;
        };
        let pointer = editor.operand((block, index), 0);
        if let Err(position) = addresses.binary_search(&pointer) {
            addresses.insert(position, pointer);
        }
    }
    u32::try_from(addresses.len()).map_err(|_| crate::lir::invalid("card-mark 额度超出 u32"))
}

/// 每次插入都会移动同 block 的指令，必须重新配对并定位下一段裸屏障。
fn next_unreserved(editor: &Editor) -> Result<Option<(u32, InstRef, InstRef)>, Diagnostic> {
    for block in editor.live_blocks() {
        let mut open = Vec::new();
        for index in 0..editor.instruction_count(block) {
            match &editor.instruction((block, index)).op {
                Op::NoSafepointBegin(region) => open.push((*region, index)),
                Op::NoSafepointEnd(region) => {
                    let Some((opened, begin)) = open.pop() else {
                        return Err(crate::lir::invalid("NoSafepointRegion end 没有匹配 begin"));
                    };
                    if opened != *region {
                        return Err(crate::lir::invalid("NoSafepointRegion 没有正确嵌套"));
                    }
                    if (begin..index).any(|at| {
                        matches!(
                            editor.instruction((block, at)).op,
                            Op::GcWriteBarrier { .. }
                        )
                    }) {
                        return Ok(Some((opened, (block, begin), (block, index))));
                    }
                }
                _ => {}
            }
        }
        if !open.is_empty() {
            return Err(crate::lir::invalid(
                "barrier reserve 不支持跨 block 的 NoSafepointRegion",
            ));
        }
    }
    Ok(None)
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
