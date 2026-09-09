use super::pass::{
    self, LIR_PASS_ORDER, LirPass, barriers, constants,
    policy::{OptimizationPolicyV1, PASS_PIPELINE_REVISION, POLL_BUDGET},
    poll,
    rewrite::Editor,
};
use super::{
    body::{self, Body, Op, Provenance, Terminator, Type, ValueId, ValueType, range},
    verify,
};
use crate::{Compilation, CompileRequest, Compiler, DiagnosticCode, TargetName};
use std::num::NonZeroU32;

const SSA: &str = include_str!("fixtures/ssa.gg");
const CONCRETE: &str = include_str!("fixtures/concrete.gg");
const EFFECTS: &str = include_str!("fixtures/effects.gg");
const POLL: &str = include_str!("fixtures/poll.gg");
const OPTIMIZE: &str = include_str!("fixtures/optimize.gg");

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
    // 带参数的叶函数不会被 GIR 内联；这里用带调用的非叶函数保留该记录。
    let compilation = compile("fn helper(value: int) int { value }\nfn main() { _ = helper(1) }");
    let mut body = named(&compilation, "main").clone();
    let stack_check = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::StackCheck))
        .expect("入口必须保留 StackCheck");
    body.instructions[stack_check].safepoint = None;
    rejected(&compilation, body);
    let mut body = named(&compilation, "main").clone();
    body.no_safepoint_regions
        .push(crate::frontend::gir::body::NoSafepointReason::RootPublish);
    body.instructions[stack_check].op = Op::NoSafepointBegin(0);
    body.instructions[stack_check].safepoint = None;
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

#[test]
fn lir_pass_order_is_fixed() {
    assert_eq!(
        LIR_PASS_ORDER,
        &[
            LirPass::VerifySsaAndMemory,
            LirPass::CanonicalizeCfg,
            LirPass::SparseConditionalConstants,
            LirPass::AlgebraicSimplification,
            LirPass::GlobalValueNumbering,
            LirPass::DeadStoreAndDeadValueElimination,
            LirPass::CanonicalizeLoops,
            LirPass::LoopInvariantCodeMotion,
            LirPass::StrengthReduction,
            LirPass::LoopVersioningAndUnswitching,
            LirPass::LoopVectorizationAndUnrolling,
            LirPass::LowerAllocationAndBarrierFastPaths,
            LirPass::LowerTargetAbi,
            LirPass::LegalizeX86_64,
            LirPass::ClassifyPollFreeLeafAndPlaceBudgetedPolls,
            LirPass::LowerPollFastPaths,
            LirPass::PrepareRegisterAllocation,
        ]
    );
    assert_eq!(PASS_PIPELINE_REVISION, 1);
    assert_eq!(POLL_BUDGET, 4096);
}

/// 用户可观察效果的多重集合；poll 与纯计算不属于可观察效果。
fn effect_signature(body: &Body) -> Vec<String> {
    body.instructions
        .iter()
        .filter_map(|instruction| match &instruction.op {
            Op::Load(access) if access.volatile => Some("volatile-load".to_owned()),
            Op::Store(access) if access.volatile => Some("volatile-store".to_owned()),
            Op::Atomic { op, .. } => Some(format!("atomic-{op:?}")),
            Op::ForeignCall(_) => Some("foreign-call".to_owned()),
            Op::Call(_) => Some("call".to_owned()),
            Op::TrapIf => Some("trap".to_owned()),
            Op::GcWriteBarrier { .. } | Op::GcWriteBarrierReserved { .. } => {
                Some("barrier".to_owned())
            }
            Op::InlineAsm(_) => Some("asm".to_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn pipeline_preserves_observable_effects() {
    let compilation = compile(EFFECTS);
    let body = named(&compilation, "main").clone();
    let before = effect_signature(&body);
    assert!(
        before.iter().any(|effect| effect == "volatile-store"),
        "fixture 必须包含可观察效果：{before:?}"
    );
    let mut bodies = vec![body];
    pass::optimize_world(
        &mut bodies,
        compilation.hir.as_ref().unwrap().module(),
        &crate::target::baseline_cost_profile(),
    )
    .expect("固定管线必须通过每个 pass 后的结构 verifier");
    assert_eq!(effect_signature(&bodies[0]), before);
    verify::verify(&bodies[0], compilation.hir.as_ref().unwrap().module())
        .expect("优化后的 LIR 必须通过结构 verifier");
}

#[test]
fn algebraic_simplification_preserves_wrapping_and_division() {
    let compilation = compile(
        "fn identity(x: int) int { let a = x + 0\n let b = a - 0\n return b }\nfn divide(y: int) int = y / 2 + y % 2\nfn main() { _ = identity(7)\n _ = divide(9) }",
    );
    let body = named(&compilation, "identity").clone();
    let expected = interpret(&body, &[7]);
    let mut editor = Editor::new(body);
    constants::algebraic_simplify(&mut editor).expect("代数化简");
    let simplified = editor.finish().expect("finish");
    assert_eq!(interpret(&simplified, &[7]), expected);

    let mut editor = Editor::new(named(&compilation, "divide").clone());
    constants::algebraic_simplify(&mut editor).expect("代数化简");
    let divide = editor.finish().expect("finish");
    assert!(
        divide
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::Integer(body::IntOp::DivSigned))),
        "有符号除法不得被消除"
    );
    assert!(
        divide
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::Integer(body::IntOp::RemSigned))),
        "取余不得被消除"
    );
}

#[test]
fn infinite_loop_gets_budgeted_poll() {
    let compilation = compile(POLL);
    let body = named(&compilation, "spin");
    assert!(
        body.instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::SafepointPoll { .. })),
        "无限循环必须被 poll 切断"
    );
    assert!(poll::body_clean_cycle(body).is_none());
    assert!(!body.poll_summary.has_poll_free_cycle);
}

#[test]
fn counted_loop_strip_mining_has_poll_free_inner_loop() {
    let compilation = compile(POLL);
    let body = named(&compilation, "chunked");
    let polls: Vec<_> = body
        .instructions
        .iter()
        .filter(|instruction| matches!(instruction.op, Op::SafepointPoll { .. }))
        .collect();
    assert_eq!(polls.len(), 1, "内层必须 poll-free，只有外层每次 poll");
    assert_eq!(
        polls[0].op,
        Op::SafepointPoll {
            interval: NonZeroU32::new(1).expect("interval 非零"),
        }
    );
    assert!(poll::body_clean_cycle(body).is_none());
    assert!(poll::body_poll_free_cost(body) <= POLL_BUDGET);
}

#[test]
fn small_counted_loop_stays_poll_free() {
    let compilation = compile(
        "fn small() int {\n let i = 0\n let total = 0\n while i < 3 {\n total = total + i\n i = i + 1\n }\n return total\n}\nfn main() { _ = small() }",
    );
    let body = named(&compilation, "small");
    assert!(
        !body
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::SafepointPoll { .. })),
        "总成本不超预算的计数循环不得插 poll"
    );
    assert_eq!(interpret(body, &[]), vec![3]);
}

#[test]
fn verifier_rejects_over_budget_poll_free_path() {
    let compilation = compile(POLL);
    let mut editor = Editor::new(named(&compilation, "spin").clone());
    let poll_site = editor
        .live_blocks()
        .into_iter()
        .find_map(|block| {
            (0..editor.instruction_count(block))
                .find(|index| {
                    matches!(
                        editor.instruction((block, *index)).op,
                        Op::SafepointPoll { .. }
                    )
                })
                .map(|index| (block, index))
        })
        .expect("无限循环必须被 poll 切断");
    editor.remove_instruction(poll_site).expect("删除 poll");
    let mut body = editor.finish().expect("finish");
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("去掉 poll 后的 poll-free 环必须被拒绝");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

#[test]
fn action_key_is_sensitive_to_optimization_policy() {
    let base = OptimizationPolicyV1::default();
    let mut changed = base;
    changed.vector_policy_revision = base.vector_policy_revision + 1;
    assert_ne!(base.canonical_bytes(), changed.canonical_bytes());
}

#[test]
fn optimize_fixture_preserves_results_after_pipeline() {
    let compilation = compile(OPTIMIZE);
    assert_eq!(interpret(named(&compilation, "choose"), &[1]), vec![7]);
    assert_eq!(interpret(named(&compilation, "choose"), &[0]), vec![9]);
    assert_eq!(interpret(named(&compilation, "countdown"), &[4]), vec![10]);
}

#[test]
fn slice_index_lowers_with_element_type() {
    // 回归：`v[i]` 的 place 类型必须先去引用再取元素，否则 LIR 会报分量不匹配。
    let compilation =
        compile("fn g(v: &[int], i: int) int = v[i]\nfn main() { let a = [0; 8]\n _ = g(&a, 1) }");
    let body = named(&compilation, "g");
    assert!(
        body.instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::Load(_))),
        "下标读取必须 lowering 成一次 Load"
    );
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
                Op::StackCheck
                | Op::SafepointPoll { .. }
                | Op::NoSafepointBegin(_)
                | Op::NoSafepointEnd(_) => continue,
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
