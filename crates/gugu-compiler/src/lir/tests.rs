use super::pass::{
    self, LIR_PASS_ORDER, LirPass, constants,
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
const PUBLISH: &str = include_str!("fixtures/publish.gg");
const STACKMAP: &str = include_str!("fixtures/stackmap.gg");
/// 资源样例：ResourceCell 的构造、按值转移与结束时释放。
const RESOURCE: &str = "struct ResourceCell { id: uint }\nfn main() {\n let a = ResourceCell { id: 1 }\n let b = a\n _ = b\n}";

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

/// 断言非法 LIR 在 runtime raw 平面契约上被拒绝。
fn rejected_raw(compilation: &Compilation, mut body: Body) {
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("非法 publish 区域必须在后端前失败");
    assert_eq!(
        error.code(),
        DiagnosticCode::RuntimeRawInvariant,
        "{}",
        error.message()
    );
}

fn publish_body(compilation: &Compilation) -> Body {
    compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .world
        .bodies
        .iter()
        .find(|body| !body.no_safepoint_regions.is_empty())
        .expect("publish 闭包必须保留 NoSafepointRegion")
        .clone()
}

/// 返回 region 内第一条 `Store` 的指令下标。
fn region_store(body: &Body, region: u32) -> usize {
    let mut inside = false;
    for (index, instruction) in body.instructions.iter().enumerate() {
        match instruction.op {
            Op::NoSafepointBegin(open) if open == region => inside = true,
            Op::NoSafepointEnd(close) if close == region => inside = false,
            Op::Store(_) if inside => return index,
            _ => {}
        }
    }
    panic!("publish 区域必须包含 Store");
}

/// 返回资源样例中承载资源调用的 body。
fn resource_body(compilation: &Compilation) -> Body {
    compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .world
        .bodies
        .iter()
        .find(|body| {
            super::verify::resource_isolation::resource_descriptors(std::slice::from_ref(*body))
                .is_ok_and(|descriptors| !descriptors.is_empty())
        })
        .expect("资源样例必须保留 ResourceAcquire/Release 调用")
        .clone()
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
fn publish_region_keeps_raw_plane_contract() {
    let compilation = compile(PUBLISH);
    let body = publish_body(&compilation);
    assert!(
        body.no_safepoint_regions.iter().any(|reason| matches!(
            reason,
            crate::frontend::gir::body::NoSafepointReason::OwnershipPublish
                | crate::frontend::gir::body::NoSafepointReason::RootPublish
        )),
        "publish 区域必须保留 reason"
    );
    let store = region_store(&body, 0);
    assert!(matches!(body.instructions[store].op, Op::Store(_)));
}

#[test]
fn publish_region_rejects_managed_interior_value() {
    use crate::lir::body::Provenance;

    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let store = region_store(&body, 0);
    let value = body.args(&body.instructions[store].arguments)[1];
    assert_eq!(
        body.values[value.index()].kind.provenance,
        Some(Provenance::GcHeap),
        "fixture 的 publish 区域写入的是句柄身份"
    );
    body.values[value.index()].kind.provenance = Some(Provenance::GcInterior);
    rejected_raw(&compilation, body);
}

#[test]
fn publish_region_rejects_bulk_memory_operation() {
    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let store = region_store(&body, 0);
    let arguments = body.instructions[store].arguments.clone();
    let byte = body
        .instructions
        .iter()
        .find_map(|instruction| match instruction.op {
            Op::IConst(_) if !instruction.results.is_empty() => {
                Some(ValueId(instruction.results.start))
            }
            _ => None,
        })
        .expect("fixture 必须含整数常量");
    body.operands[usize::try_from(arguments.start).expect("操作数起点")] = byte;
    body.instructions[store].op = Op::Memset { bytes: 8 };
    rejected_raw(&compilation, body);
}

#[test]
fn resource_descriptor_cannot_be_region_allocated() {
    let compilation = compile(RESOURCE);
    let mut body = resource_body(&compilation);
    let descriptors =
        super::verify::resource_isolation::resource_descriptors(std::slice::from_ref(&body))
            .expect("资源调用必须登记类型描述符");
    let descriptor = *descriptors.iter().next().expect("资源描述符集合非空");
    // 常量指令既不是资源调用也不是描述符定义，替换它不会破坏描述符反查。
    let index = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::IConst(_)))
        .expect("资源样例必须保留整数常量");
    body.instructions[index].op = Op::RegionAlloc {
        region: 0,
        descriptor,
        align: 8,
    };
    let error = super::verify::resource_isolation::verify(std::slice::from_ref(&body))
        .expect_err("资源描述符不能进入 RegionAlloc");
    assert_eq!(
        error.code(),
        DiagnosticCode::ResourceInvariant,
        "{}",
        error.message()
    );
}

#[test]
fn resource_descriptors_are_derived_from_bodies() {
    let compilation = compile(RESOURCE);
    let body = resource_body(&compilation);
    let bodies = std::slice::from_ref(&body);
    let first = super::verify::resource_isolation::resource_descriptors(bodies)
        .expect("资源调用必须登记类型描述符");
    let second = super::verify::resource_isolation::resource_descriptors(bodies)
        .expect("资源调用必须登记类型描述符");
    assert_eq!(first, second, "同一 body 的描述符扫描必须确定");
    assert!(!first.is_empty(), "资源样例必须产生资源描述符");
    assert!(
        super::verify::resource_isolation::verify(bodies).is_ok(),
        "未进入 region 的资源样例必须通过隔离闸门"
    );
}

#[test]
fn barrier_reserve_materializes_permits() {
    use crate::frontend::gir::body::NoSafepointReason::{OwnershipPublish, RootPublish};
    use crate::runtime::barrier_schema::EDGE_DELTAS_PER_WRITE;

    let compilation = compile(PUBLISH);
    let body = compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .world
        .bodies
        .iter()
        .find(|body| !body.no_safepoint_regions.is_empty())
        .expect("publish 闭包必须保留 NoSafepointRegion");
    assert_eq!(body.no_safepoint_regions, [OwnershipPublish, RootPublish]);
    assert_eq!(body.barrier_permits.len(), 2);
    for (index, permit) in body.barrier_permits.iter().enumerate() {
        assert_eq!(permit.region, body::id(index));
        // 每个 publish region 只包住一条句柄 Assign：一个 store、一个写入地址。
        assert_eq!(permit.max_shades, 2);
        assert_eq!(permit.max_card_marks, 1);
        // shade 额度同时是 edge scratch 的容量证明：每条写入最多贡献
        // `EDGE_DELTAS_PER_WRITE` 条边变更，两者上界同源，因此不需要第二份额度字段。
        assert_eq!(
            EDGE_DELTAS_PER_WRITE,
            crate::runtime::barrier_schema::SHADE_SLOTS_PER_WRITE
        );
        assert!(permit.max_shades >= EDGE_DELTAS_PER_WRITE);
    }
    let mut reserves = [0; 2];
    let mut barriers = [0; 2];
    for instruction in &body.instructions {
        match instruction.op {
            Op::BarrierReserve(permit) => reserves[permit.index()] += 1,
            Op::GcWriteBarrierReserved { permit, .. } => barriers[permit.index()] += 1,
            Op::GcWriteBarrier { .. } => panic!("region 内屏障必须已预留"),
            _ => {}
        }
    }
    assert_eq!(reserves, [1, 1]);
    assert_eq!(barriers, [1, 1]);
    assert!(body.blocks.iter().any(|block| {
        body.instructions[range(&block.instructions)]
            .iter()
            .filter(|instruction| matches!(instruction.op, Op::NoSafepointBegin(_)))
            .count()
            == 2
    }));
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
fn stackmap_world_covers_call_poll_suspend_and_select() {
    let compilation = compile(STACKMAP);
    let world = super::stackmap::derive(
        &compilation.lir.as_ref().expect("已生成 LIR").world.bodies,
        compilation.hir.as_ref().unwrap().module(),
    )
    .expect("栈图推导必须通过 verifier");
    // 函数按实例键排序；安全点按（函数序、站点序、种类、身份）确定性排列。
    assert!(
        world
            .functions
            .windows(2)
            .all(|pair| pair[0].instance < pair[1].instance),
        "函数必须按实例键排序"
    );
    let kinds: Vec<u8> = world.safepoints.iter().map(|point| point.kind).collect();
    assert!(
        kinds.contains(&super::stackmap::KIND_CALL_RETURN),
        "普通调用必须有 CallReturn 记录：{kinds:?}"
    );
    assert!(
        kinds.contains(&super::stackmap::KIND_POLL_RESUME),
        "循环 poll 必须有 PollResume 记录：{kinds:?}"
    );
    assert!(
        kinds.contains(&super::stackmap::KIND_SUSPEND_RESUME),
        "挂起与无 default select 必须有 SuspendResume 记录：{kinds:?}"
    );
    assert!(
        kinds.contains(&super::stackmap::KIND_MORESTACK_ENTRY),
        "入口检查必须有 MorestackEntry 记录：{kinds:?}"
    );
    // kind 分类计数与安全点总数一致；去重 map 不超过安全点数。
    let demand = compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .stackmap_demand(compilation.hir.as_ref().unwrap().module());
    assert_eq!(
        demand.call_return
            + demand.poll_resume
            + demand.suspend_resume
            + demand.foreign_bridge
            + demand.morestack_entry,
        demand.safepoints,
        "kind 分类必须求和为安全点总数"
    );
    assert!(
        demand.maps <= demand.safepoints && demand.maps > 0,
        "去重 map 必须非空且不超过安全点数"
    );
    assert!(
        demand.functions_with_landing <= demand.functions,
        "落地函数不得超过函数总数"
    );
    // `MorestackEntry` 只含 ABI 参数根。
    for point in world
        .safepoints
        .iter()
        .filter(|point| point.kind == super::stackmap::KIND_MORESTACK_ENTRY)
    {
        for root in point
            .roots
            .direct
            .iter()
            .chain(&point.roots.interior)
            .chain(&point.roots.handle)
            .chain(&point.roots.compressed)
            .chain(&point.roots.stack)
        {
            assert!(
                matches!(root, super::stackmap::LogicalRoot::Argument { .. }),
                "MorestackEntry 只允许 ABI 参数根"
            );
        }
    }
    // 冷热编译的栈图需求一致。
    let compiler = Compiler::new();
    let request = || CompileRequest::single_file("main.gg", STACKMAP, TargetName::X86_64Linux);
    let cold = compiler.compile(request());
    let warm = compiler.compile(request());
    assert!(cold.is_success() && warm.is_success());
    assert_eq!(
        cold.image_plan().expect("image-plan").stackmap_demand(),
        warm.image_plan().expect("image-plan").stackmap_demand()
    );
}

#[test]
fn stackmap_rejects_duplicate_root_across_kinds() {
    let compilation = compile(STACKMAP);
    let mut world = super::stackmap::derive(
        &compilation.lir.as_ref().expect("已生成 LIR").world.bodies,
        compilation.hir.as_ref().unwrap().module(),
    )
    .expect("栈图推导必须通过 verifier");
    let point = world
        .safepoints
        .iter_mut()
        .find(|point| !point.roots.direct.is_empty() || !point.roots.stack.is_empty())
        .expect("fixture 必须含非空根集合");
    let duplicate = point
        .roots
        .direct
        .first()
        .or(point.roots.stack.first())
        .expect("根集合非空")
        .clone();
    // 同一身份进入两类位图：verifier 必须拒绝。
    if !point.roots.direct.is_empty() {
        point.roots.stack.push(duplicate);
    } else {
        point.roots.direct.push(duplicate);
    }
    world.fingerprint = world.fingerprint_of();
    let error = super::stackmap::verify_world(&world).expect_err("重复根必须被拒绝");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
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

#[test]
fn permit_quota_beyond_buffer_capacity_is_rejected() {
    use crate::runtime::barrier_schema::CARD_MARK_BUFFER_ENTRIES;

    // permit 是 compile-time 容量证明：`max_card_marks` 超过 processor 的
    // `CardMarkBuffer` 容量时，region 内必然需要补容量，而补容量只能发生在 region 外。
    // verifier 必须在后端前拒绝这种 permit。
    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let permit = body
        .barrier_permits
        .iter_mut()
        .next()
        .expect("publish region 必须带 permit");
    permit.max_card_marks = CARD_MARK_BUFFER_ENTRIES + 1;
    // 必须断言具体分支：额度一致性检查也会以同一错误码拒绝，只断言错误码无法分辨。
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("超额 permit 必须在后端前失败");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        error.message().contains("超过 CardMarkBuffer 容量"),
        "必须由容量检查拒绝，实际为：{}",
        error.message()
    );
}

#[test]
fn permit_quota_at_buffer_capacity_passes_the_capacity_check() {
    use crate::runtime::barrier_schema::CARD_MARK_BUFFER_ENTRIES;

    // 边界是 `>`：额度恰好等于容量时容量检查不触发，随后的失败必须来自额度一致性检查。
    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let permit = body
        .barrier_permits
        .iter_mut()
        .next()
        .expect("publish region 必须带 permit");
    permit.max_card_marks = CARD_MARK_BUFFER_ENTRIES;
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("额度与静态复算不一致仍必须失败");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        !error.message().contains("超过 CardMarkBuffer 容量"),
        "恰好等于容量不得触发容量检查，实际为：{}",
        error.message()
    );
    assert!(
        error.message().contains("静态消费上界不一致"),
        "应落到额度一致性检查，实际为：{}",
        error.message()
    );
}

/// TurnRegion：带环境的闭包在 turn 结束时整区发布并重置。
#[test]
fn turn_region_ops_close_at_suspend_boundaries() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let body = named(&compilation, "main");
    let allocs: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionAlloc { region, .. } => Some(region),
            _ => None,
        })
        .collect();
    let publishes: Vec<(u32, u8)> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionPublish { region, export } => Some((region, export)),
            _ => None,
        })
        .collect();
    let resets: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionReset { region } => Some(region),
            _ => None,
        })
        .collect();
    let transfers: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionTransfer { region } => Some(region),
            _ => None,
        })
        .collect();
    assert_eq!(allocs.len(), 1, "闭包环境是唯一的 region 分配点");
    assert_eq!(
        publishes.len(),
        resets.len(),
        "每个出口恰好发布一次并重置一次"
    );
    assert!(publishes.iter().any(|(_, export)| *export == 0));
    assert!(transfers.is_empty(), "普通 turn 结束不产生转移");
    verify::verify_structure(body, compilation.hir.as_ref().unwrap().module())
        .expect("region 生命周期必须自洽");
}

/// channel send 在 sender 之后不再使用该闭包时整区移交，而不是复制或保留。
#[test]
fn channel_send_transfers_region_when_sender_is_dead() {
    let compilation = compile(
        "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n }",
    );
    let body = named(&compilation, "main");
    let transfers: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionTransfer { region } => Some(region),
            _ => None,
        })
        .collect();
    assert_eq!(transfers.len(), 1, "移交语义必须落在一个 RegionTransfer 上");
    let resets = body
        .instructions
        .iter()
        .filter(|instruction| matches!(instruction.op, Op::RegionReset { .. }))
        .count();
    assert_eq!(resets, 0, "移交后 sender 不得重置同一个 region");
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.turn_region_transfer_sites(), 1);
    assert!(plan.turn_region_sites() >= 1);
    verify::verify_structure(body, compilation.hir.as_ref().unwrap().module())
        .expect("移交路径必须自洽");
}

/// sender 在 send 之后仍使用闭包时不得选择 region，退回 SharedHeap。
#[test]
fn channel_send_with_live_sender_keeps_stable_storage() {
    let compilation = compile(
        "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n _ = closure()\n }",
    );
    let body = named(&compilation, "main");
    assert!(
        body.instructions
            .iter()
            .all(|instruction| !matches!(instruction.op, Op::RegionAlloc { .. })),
        "sender 仍在使用时必须落在 stable storage"
    );
    assert!(body.instructions.iter().any(|instruction| matches!(
        instruction.op,
        Op::GcAlloc {
            placement: crate::frontend::gir::placement::PlacementKind::SharedHeap,
            ..
        }
    )));
    assert_eq!(
        compilation
            .image_plan()
            .expect("镜像计划")
            .turn_region_sites(),
        0
    );
}

/// 未闭合 export summary 的 region 不允许 reset。
#[test]
fn reset_is_rejected_when_export_summary_is_open() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    for instruction in &mut body.instructions {
        if let Op::RegionPublish { export, .. } = &mut instruction.op {
            *export = 1;
        }
    }
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("未闭合 summary 不能重置");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        error.message().contains("export summary 闭合"),
        "{}",
        error.message()
    );
}

/// 发布之后缺少结束动作的 region 必须被拒绝。
#[test]
fn region_without_end_action_is_rejected() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    body.instructions.retain(|instruction| {
        !matches!(
            instruction.op,
            Op::RegionReset { .. } | Op::RegionTransfer { .. }
        )
    });
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("发布后必须有结束动作");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

/// region 移交只能发生在 channel send 边界上。
#[test]
fn region_transfer_requires_channel_send_boundary() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    for instruction in &mut body.instructions {
        // 换成同样不需要 safepoint 的结束动作：生命周期自洽，但普通出口上出现移交必须被
        // channel 闸门拒绝。
        if let Op::RegionReset { region } = instruction.op {
            instruction.op = Op::RegionTransfer { region };
        }
    }
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("普通出口不得移交 region");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        error.message().contains("channel send"),
        "{}",
        error.message()
    );
}

/// region 指令不引用任何值：参数数量由 verifier 强制。
#[test]
fn region_lifecycle_ops_carry_no_values() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    for instruction in &mut body.instructions {
        if matches!(instruction.op, Op::RegionPublish { .. }) {
            instruction.arguments = 0..1;
        }
    }
    super::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module())
        .expect_err("region 指令不得带参数");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}
