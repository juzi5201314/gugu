use super::{
    body::{self, Body, Op, Provenance, Terminator, Type, ValueId, ValueType, range},
    verify,
};
use crate::{Compilation, CompileRequest, Compiler, DiagnosticCode, TargetName};

const SSA: &str = include_str!("fixtures/ssa.gg");
const CONCRETE: &str = include_str!("fixtures/concrete.gg");
const EFFECTS: &str = include_str!("fixtures/effects.gg");

fn compile(source: &str) -> Compilation {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
}
fn named<'a>(compilation: &'a Compilation, name: &str) -> &'a Body {
    let module = compilation.hir.as_ref().unwrap().module();
    let key = module
        .definitions
        .iter()
        .find(|definition| definition.name == name)
        .unwrap()
        .key;
    compilation
        .lir
        .as_ref()
        .unwrap()
        .world
        .bodies
        .iter()
        .find(|body| body.owner == key)
        .unwrap()
}
fn rejected(compilation: &Compilation, mut body: Body) {
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("非法 LIR 必须在后端前失败");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

#[test]
fn block_parameters_preserve_branch_and_loop_values() {
    let compilation = compile(SSA);
    let body = named(&compilation, "choose");
    assert_eq!(interpret(body, &[1, 10, 4]), vec![17]);
    assert_eq!(interpret(body, &[0, 10, 0]), vec![12]);
    assert_eq!(interpret(body, &[0, 10, 3]), vec![15]);
}

#[test]
fn concrete_instances_preserve_scalar_and_wide_returns() {
    let compilation = compile(CONCRETE);
    let module = compilation.hir.as_ref().unwrap().module();
    let identity = module
        .definitions
        .iter()
        .find(|definition| definition.name == "identity")
        .unwrap()
        .key;
    let bodies: Vec<_> = compilation
        .lir
        .as_ref()
        .unwrap()
        .world
        .bodies
        .iter()
        .filter(|body| body.owner == identity)
        .collect();
    let narrow = bodies
        .iter()
        .find(|body| body.signature.parameters == [ValueType::scalar(Type::I64)])
        .unwrap();
    let wide = bodies
        .iter()
        .find(|body| body.signature.parameters == [ValueType::scalar(Type::I64); 2])
        .unwrap();
    assert_eq!(interpret(narrow, &[17]), vec![17]);
    assert_eq!(interpret(wide, &[7, 9]), vec![7, 9]);
    assert!(bodies.iter().any(|body| body.signature.sret.is_some()));
}

#[test]
fn memory_chain_cannot_skip_a_volatile_effect() {
    let compilation = compile(EFFECTS);
    let mut body = named(&compilation, "main").clone();
    let block = body
        .blocks
        .iter()
        .find(|block| {
            body.instructions[range(&block.instructions)]
                .iter()
                .any(|instruction| matches!(&instruction.op, Op::Store(access) if access.volatile))
        })
        .unwrap();
    let entry = body.parameters[usize::try_from(block.parameters.start).unwrap()].value;
    let store = body.instructions[range(&block.instructions)]
        .iter()
        .position(|instruction| matches!(&instruction.op, Op::Store(access) if access.volatile))
        .unwrap()
        + usize::try_from(block.instructions.start).unwrap();
    assert_ne!(body.instructions[store].memory.unwrap().input, entry);
    body.instructions[store].memory.as_mut().unwrap().input = entry;
    rejected(&compilation, body);
}

#[test]
fn branch_values_cannot_be_used_on_the_other_edge() {
    let compilation = compile(SSA);
    let mut body = named(&compilation, "choose").clone();
    let additions: Vec<_> = body
        .instructions
        .iter()
        .enumerate()
        .filter(|(_, instruction)| matches!(instruction.op, Op::Integer(body::IntOp::Add)))
        .map(|(index, instruction)| (index, ValueId(instruction.results.start)))
        .collect();
    let left = additions[0].1;
    let right = additions[1].1;
    let operand = body
        .edges
        .iter()
        .flat_map(|edge| range(&edge.arguments))
        .find(|&index| body.operands[index] == left)
        .unwrap();
    body.operands[operand] = right;
    rejected(&compilation, body);
}

#[test]
fn integer_pointer_cast_cannot_forge_a_managed_root() {
    let compilation = compile("fn main() { let value = 1\n let pointer = &value\n _ = *pointer }");
    let mut body = named(&compilation, "main").clone();
    let instruction = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::PtrOffset | Op::StackAddr(_)))
        .unwrap();
    let result = ValueId(body.instructions[instruction].results.start);
    body.values[result.index()].kind.provenance = Some(Provenance::Code);
    rejected(&compilation, body);
}

#[test]
fn atomic_load_rejects_release_order() {
    let compilation = compile(EFFECTS);
    let mut body = named(&compilation, "main").clone();
    let load = body
        .instructions
        .iter_mut()
        .find(|instruction| matches!(instruction.op, Op::Load(_)))
        .unwrap();
    load.op = Op::Atomic {
        op: body::AtomicOp::Load,
        ordering: crate::frontend::gir::body::MemoryOrdering::Release,
        failure: None,
        align: 8,
    };
    rejected(&compilation, body);
}

#[test]
fn managed_store_requires_its_hybrid_barrier() {
    let compilation = compile(
        "fn main() { let value = 1\n let closure = fn() int { return value }\n _ = closure() }",
    );
    let mut body = named(&compilation, "main").clone();
    let barrier = body
        .instructions
        .iter_mut()
        .find(|instruction| matches!(instruction.op, Op::GcWriteBarrier { .. }))
        .unwrap();
    barrier.op = Op::StackCheck;
    barrier.arguments.end = barrier.arguments.start;
    let safepoint = barrier.safepoint.unwrap();
    body.safepoints[safepoint.index()].kind = body::SafepointKind::StackCheck;
    rejected(&compilation, body);
}

#[test]
fn missing_safepoint_and_unclosed_region_are_rejected() {
    let compilation = compile("fn main() {}");
    let mut body = named(&compilation, "main").clone();
    body.instructions[0].safepoint = None;
    rejected(&compilation, body);
    let mut body = named(&compilation, "main").clone();
    body.no_safepoint_regions
        .push(crate::frontend::gir::body::NoSafepointReason::RootPublish);
    body.instructions[0].op = Op::NoSafepointBegin(0);
    body.instructions[0].safepoint = None;
    body.safepoints.clear();
    rejected(&compilation, body);
}

#[test]
fn cached_lir_matches_fresh_output_and_action_identity() {
    let compiler = Compiler::new();
    let request = || CompileRequest::single_file("main.gg", SSA, TargetName::X86_64Linux);
    let cold = compiler.compile(request());
    let warm = compiler.compile(request());
    assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
    assert!(warm.is_success(), "{:?}", warm.diagnostics().items());
    assert_eq!(cold.dump_lir(), warm.dump_lir());
    assert_eq!(cold.action_key(), warm.action_key());
    let changed = compiler.compile(CompileRequest::single_file(
        "main.gg",
        SSA.replace("value + 2", "value + 3"),
        TargetName::X86_64Linux,
    ));
    assert!(changed.is_success(), "{:?}", changed.diagnostics().items());
    assert_ne!(cold.lir_fingerprint(), changed.lir_fingerprint());
}

/// 仅解释 fixture 中的整数 SSA 子集，检验循环回边与合流的可观察结果。
fn interpret(body: &Body, arguments: &[u64]) -> Vec<u64> {
    let mut values = vec![0u64; body.values.len()];
    let mut block = body.entry;
    for (parameter, value) in body.params(block).iter().skip(1).zip(arguments) {
        values[parameter.value.index()] = *value;
    }
    for _ in 0..256 {
        for instruction in &body.instructions[range(&body.blocks[block.index()].instructions)] {
            let args: Vec<_> = body
                .args(&instruction.arguments)
                .iter()
                .map(|value| values[value.index()])
                .collect();
            let value = match instruction.op {
                Op::StackCheck => continue,
                Op::IConst(value) => value,
                Op::Integer(body::IntOp::Add) => args[0].wrapping_add(args[1]),
                Op::Integer(body::IntOp::Sub) => args[0].wrapping_sub(args[1]),
                Op::Compare { condition, signed } => {
                    let ordering = if signed {
                        i64::from_le_bytes(args[0].to_le_bytes())
                            .cmp(&i64::from_le_bytes(args[1].to_le_bytes()))
                    } else {
                        args[0].cmp(&args[1])
                    };
                    u64::from(match condition {
                        body::Condition::Eq => ordering.is_eq(),
                        body::Condition::Ne => !ordering.is_eq(),
                        body::Condition::Lt => ordering.is_lt(),
                        body::Condition::Le => !ordering.is_gt(),
                        body::Condition::Gt => ordering.is_gt(),
                        body::Condition::Ge => !ordering.is_lt(),
                    })
                }
                ref op => panic!("fixture 解释器遇到未登记操作：{op:?}"),
            };
            values[usize::try_from(instruction.results.start).unwrap()] = value;
        }
        let edge = match &body.blocks[block.index()].terminator {
            Terminator::Jump(edge) => *edge,
            Terminator::Branch { condition, yes, no } => {
                if values[condition.index()] != 0 {
                    *yes
                } else {
                    *no
                }
            }
            Terminator::Switch {
                value,
                cases,
                otherwise,
            } => body.switch_cases[range(cases)]
                .iter()
                .find(|(case, _)| *case == values[value.index()])
                .map_or(*otherwise, |(_, edge)| *edge),
            Terminator::Return { values: output, .. } => {
                return body
                    .args(output)
                    .iter()
                    .map(|value| values[value.index()])
                    .collect();
            }
            terminator => panic!("fixture 解释器遇到未登记终结符：{terminator:?}"),
        };
        let edge = &body.edges[edge.index()];
        let incoming: Vec<_> = body
            .args(&edge.arguments)
            .iter()
            .map(|value| values[value.index()])
            .collect();
        for (parameter, value) in body.params(edge.to).iter().zip(incoming) {
            values[parameter.value.index()] = value;
        }
        block = edge.to;
    }
    panic!("固定有界 fixture 的 LIR 没有终止")
}
