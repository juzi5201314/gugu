use super::body::{BlockId, Body, EdgeId, InstId, Terminator, Use, UseSite, ValueId, id};
use std::ops::Range;

pub(crate) fn visit(body: &Body, mut visit: impl FnMut(ValueId, Use)) {
    for (index, instruction) in body.instructions.iter().enumerate() {
        let site = UseSite::Instruction(InstId(id(index)));
        for (index, value) in body.args(&instruction.arguments).iter().enumerate() {
            visit(
                *value,
                Use {
                    site,
                    operand: id(index),
                },
            );
        }
        if let Some(memory) = instruction.memory {
            visit(
                memory.input,
                Use {
                    site,
                    operand: u32::MAX,
                },
            );
        }
    }
    for (index, block) in body.blocks.iter().enumerate() {
        let site = UseSite::Terminator(BlockId(id(index)));
        let (values, memory) = match &block.terminator {
            Terminator::Branch { condition, .. } => {
                visit(*condition, Use { site, operand: 0 });
                (None, None)
            }
            Terminator::Switch { value, .. } => {
                visit(*value, Use { site, operand: 0 });
                (None, None)
            }
            Terminator::Invoke {
                arguments, memory, ..
            } => (Some(arguments), Some(memory.input)),
            Terminator::Return { values, memory } => (Some(values), Some(*memory)),
            Terminator::TailCall {
                arguments, memory, ..
            } => (Some(arguments), Some(*memory)),
            Terminator::ResumePanic { memory }
            | Terminator::Trap { memory }
            | Terminator::Unreachable { memory } => (None, Some(*memory)),
            Terminator::Jump(_) => (None, None),
        };
        if let Some(values) = values {
            for (index, value) in body.args(values).iter().enumerate() {
                visit(
                    *value,
                    Use {
                        site,
                        operand: id(index),
                    },
                );
            }
        }
        if let Some(memory) = memory {
            visit(
                memory,
                Use {
                    site,
                    operand: u32::MAX,
                },
            );
        }
    }
    for (index, edge) in body.edges.iter().enumerate() {
        let site = UseSite::Edge(EdgeId(id(index)));
        for (index, value) in body.args(&edge.arguments).iter().enumerate() {
            visit(
                *value,
                Use {
                    site,
                    operand: id(index),
                },
            );
        }
    }
}

pub(crate) fn calculate(body: &Body) -> (Vec<Range<u32>>, Vec<Use>) {
    let mut counts = vec![0u32; body.values.len()];
    visit(body, |value, _| counts[value.index()] += 1);
    let mut total = 0;
    let ranges: Vec<_> = counts
        .into_iter()
        .map(|count| {
            let start = total;
            total += count;
            start..total
        })
        .collect();
    let mut next: Vec<_> = ranges.iter().map(|range| range.start).collect();
    let mut uses = vec![
        Use {
            site: UseSite::Terminator(body.entry),
            operand: 0
        };
        usize::try_from(total).expect("use 数量适配宿主")
    ];
    visit(body, |value, use_site| {
        let index = usize::try_from(next[value.index()]).expect("use 偏移适配宿主");
        uses[index] = use_site;
        next[value.index()] += 1;
    });
    debug_assert!(
        next.iter()
            .zip(&ranges)
            .all(|(next, range)| *next == range.end)
    );
    (ranges, uses)
}

pub(crate) fn rebuild(body: &mut Body) {
    let (ranges, uses) = calculate(body);
    for (value, range) in body.values.iter_mut().zip(ranges) {
        value.uses = range;
    }
    body.uses = uses;
}
