use super::{
    World,
    body::{Definition, Terminator, range},
};
use std::fmt::Write;

pub(super) fn world(world: &World) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "LIR v{} target={} fingerprint={} optimization-revision={} poll-budget={}",
        world.schema,
        world.target,
        blake3::Hash::from(world.fingerprint).to_hex(),
        crate::lir::pass::policy::PASS_PIPELINE_REVISION,
        crate::lir::pass::policy::POLL_BUDGET
    )
    .expect("写入 String");
    for body in &world.bodies {
        writeln!(
            output,
            "\nfn {} instance={} {{",
            body.name,
            blake3::Hash::from(body.instance).to_hex()
        )
        .expect("写入 String");
        writeln!(output, "  signature {:?}", body.signature).expect("写入 String");
        writeln!(
            output,
            "  poll-summary entry_stack_check={} poll_free_cost={} has_poll_free_cycle={}",
            body.poll_summary.entry_stack_check,
            body.poll_summary.poll_free_cost,
            body.poll_summary.has_poll_free_cycle
        )
        .expect("写入 String");
        for (index, slot) in body.stack_slots.iter().enumerate() {
            writeln!(
                output,
                "  slot{index} bytes={} align={} roots={:?}",
                slot.bytes, slot.align, slot.roots
            )
            .expect("写入 String");
        }
        for (index, block) in body.blocks.iter().enumerate() {
            write!(output, "  bb{index}(").expect("写入 String");
            for (index, parameter) in body.parameters[range(&block.parameters)].iter().enumerate() {
                if index != 0 {
                    output.push_str(", ");
                }
                write!(
                    output,
                    "v{}:{:?}",
                    parameter.value.0,
                    body.values[parameter.value.index()].kind
                )
                .expect("写入 String");
            }
            writeln!(
                output,
                ") scope={} cleanup={}:",
                block.source.scope.0, block.cleanup
            )
            .expect("写入 String");
            for index in range(&block.instructions) {
                let instruction = &body.instructions[index];
                write!(output, "    i{index} ").expect("写入 String");
                for result in instruction.results.clone() {
                    write!(output, "v{result} ").expect("写入 String");
                }
                write!(
                    output,
                    "= {:?} {:?}",
                    instruction.op,
                    body.args(&instruction.arguments)
                )
                .expect("写入 String");
                if let Some(memory) = instruction.memory {
                    write!(output, " [m{} -> m{}]", memory.input.0, memory.output.0)
                        .expect("写入 String");
                }
                if let Some(safepoint) = instruction.safepoint {
                    write!(output, " safepoint{}", safepoint.0).expect("写入 String");
                }
                if instruction.op.fence() {
                    output.push_str(" fence");
                }
                write!(
                    output,
                    " poll-cost={}",
                    instruction.op.poll_cost_with(&body.assembly)
                )
                .expect("写入 String");
                writeln!(
                    output,
                    " scope={} source={}:{}-{}",
                    instruction.source.scope.0,
                    instruction.source.location.source,
                    instruction.source.location.start,
                    instruction.source.location.end
                )
                .expect("写入 String");
            }
            match &block.terminator {
                Terminator::Invoke {
                    call,
                    arguments,
                    results,
                    memory,
                    normal,
                    unwind,
                    ..
                } => writeln!(
                    output,
                    "    invoke {:?} {:?} -> {:?} [m{} -> m{}] normal=e{} unwind=e{}",
                    call.target,
                    body.args(arguments),
                    results,
                    memory.input.0,
                    memory.output.0,
                    normal.0,
                    unwind.0
                )
                .expect("写入 String"),
                other => writeln!(output, "    {other:?}").expect("写入 String"),
            }
        }
        for (index, edge) in body.edges.iter().enumerate() {
            writeln!(
                output,
                "  e{index}: bb{} -> bb{} {:?} unwind={}",
                edge.from.0,
                edge.to.0,
                body.args(&edge.arguments),
                edge.unwind
            )
            .expect("写入 String");
        }
        for (index, value) in body.values.iter().enumerate() {
            if value.kind.provenance.is_some() {
                writeln!(
                    output,
                    "  v{index} {:?} origin={:?} uses={}",
                    value.kind,
                    value.origin,
                    value.uses.end - value.uses.start
                )
                .expect("写入 String");
            }
            debug_assert!(matches!(
                value.definition,
                Definition::Parameter { .. }
                    | Definition::Instruction { .. }
                    | Definition::Invoke { .. }
            ));
        }
        output.push_str("}\n");
    }
    output
}
