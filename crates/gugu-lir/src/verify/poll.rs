//! poll 预算与摘要的一致性断言。
use super::{Graph, invalid};
use crate::Diagnostic;
use crate::body::{Body, Op, range};
use crate::pass::policy::POLL_BUDGET;
use crate::pass::poll;
use crate::target::TargetOracle;

pub(super) fn verify(
    body: &Body,
    _graph: &Graph,
    target: &dyn TargetOracle,
) -> Result<(), Diagnostic> {
    let entry_stack_check = range(&body.blocks[body.entry.index()].instructions)
        .any(|index| matches!(body.instructions[index].op, Op::StackCheck));
    if body.poll_summary.entry_stack_check != entry_stack_check {
        return Err(invalid("poll 摘要的入口 StackCheck 状态与实际不符"));
    }
    if body.poll_summary.poll_free_cost > POLL_BUDGET {
        return Err(invalid("poll 摘要记录了超过预算的 poll-free 路径"));
    }
    let (cost, cycle) = poll::body_flow(body, target);
    if cost > POLL_BUDGET {
        return Err(invalid("存在超过 poll 预算的 poll-free 路径"));
    }
    // body_flow 已排除经过证明的有界回边；余下环必须直接拒绝。
    if cycle.is_some() {
        return Err(invalid("poll-free 环没有 poll 切断且无法证明有界"));
    }
    Ok(())
}
