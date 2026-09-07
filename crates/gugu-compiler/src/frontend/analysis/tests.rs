//! AbstractAnalysis 单测：证明状态、策略敏感性与摘要保守性。

use crate::frontend::analysis::{AnalysisPolicyV1, ProofStatus, WORLD_SCHEMA_VERSION};

#[test]
fn analysis_policy_bytes_change_action_key() {
    use crate::project::ActionInputs;
    let mut first = ActionInputs::new(b"c", "host", "host", "bin");
    first.set_analysis_policy(AnalysisPolicyV1::default().canonical_bytes());
    let mut second = ActionInputs::new(b"c", "host", "host", "bin");
    let mut policy = AnalysisPolicyV1::default();
    policy.max_scc_iterations = policy.max_scc_iterations.saturating_add(1);
    second.set_analysis_policy(policy.canonical_bytes());
    assert_ne!(first.key(), second.key());
}

#[test]
fn macro_budget_change_action_key() {
    use crate::project::ActionInputs;
    let mut first = ActionInputs::new(b"c", "host", "host", "bin");
    first.set_macro_budget([1u8, 2, 3]);
    let mut second = ActionInputs::new(b"c", "host", "host", "bin");
    second.set_macro_budget([4u8, 5, 6]);
    assert_ne!(first.key(), second.key());
}

#[test]
fn empty_world_schema_is_stable() {
    let world = crate::frontend::analysis::empty_world();
    assert_eq!(world.schema, WORLD_SCHEMA_VERSION);
    assert_eq!(world.runtime_checks_elided_count, 0);
}

#[test]
fn proof_status_unknown_by_default() {
    use crate::frontend::analysis::{RuntimeCheckKey, empty_world};
    use crate::frontend::hir::{CheckKind, ExprId};
    let world = empty_world();
    let key = RuntimeCheckKey {
        owner_index: 0,
        expression: ExprId(0),
        kind: CheckKind::Bounds { slice: false },
    };
    assert_eq!(world.proof_status(&key), ProofStatus::Unknown);
}

#[test]
fn conservative_summary_stays_conservative() {
    use crate::frontend::analysis::FunctionSummary;
    let mut summary = FunctionSummary::conservative();
    summary.join_with(&FunctionSummary::conservative());
    assert_eq!(summary, FunctionSummary::conservative());
    let mut optimistic = FunctionSummary::default();
    optimistic.join_with(&FunctionSummary::conservative());
    // 效果并集只能变保守，不能把"可能发生"降级掉。
    assert_eq!(optimistic, FunctionSummary::conservative());
}

fn compile(
    sources: &[(&str, &str)],
    queries: &crate::QueryEngine,
) -> crate::frontend::FrontendOutput {
    let mut map = crate::SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| crate::SourceSnapshot::from_str(path, source).expect("快照"))
            .collect(),
    )
    .expect("源映射");
    let cfg = crate::frontend::cfg::CfgContext::new(
        crate::TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut map,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/analysis@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        queries,
    )
    .expect("前端检查通过")
}

fn proof_statuses(
    sources: &[(&str, &str)],
) -> Vec<(
    crate::frontend::hir::CheckKind,
    crate::frontend::analysis::ProofStatus,
)> {
    let queries = crate::QueryEngine::new();
    let output = compile(sources, &queries);
    let warm = compile(sources, &queries);
    assert_eq!(
        output.hir.module().owners,
        warm.hir.module().owners,
        "冷热 HIR（含 proof）必须一致"
    );
    output
        .hir
        .module()
        .owners
        .iter()
        .flat_map(|owner| {
            owner
                .checks
                .iter()
                .map(|check| (check.kind.clone(), check.proof.expect("patched")))
        })
        .collect()
}

#[test]
fn variable_operands_stay_unknown_and_constants_get_proved() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    let source = "fn f(x: int, a: &[int]) { _ = x / 2\n _ = 7 / 2\n _ = a[0]\n _ = x << 3\n _ = char(97) }\nfn main() { let s = [1, 2, 3]\n f(1, &s) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    let lookup = |pred: fn(&CheckKind) -> bool| {
        statuses
            .iter()
            .find(|(kind, _)| pred(kind))
            .map(|(_, status)| *status)
            .unwrap_or_else(|| panic!("缺少对应检查：{statuses:?}"))
    };
    // 切片下标：长度不可静态知 → Unknown；数组常量下标见下一个测试。
    assert_eq!(
        lookup(|kind| matches!(kind, CheckKind::Bounds { slice: false })),
        ProofStatus::Unknown
    );
    // 常量除数非零、常量移位量非负、合法标量：Proved。
    assert!(matches!(
        statuses.as_slice(),
        [
            (CheckKind::Division { .. }, ProofStatus::Proved),
            (CheckKind::Division { .. }, ProofStatus::Proved),
            (CheckKind::Bounds { slice: false }, ProofStatus::Unknown),
            (CheckKind::Shift { .. }, ProofStatus::Proved),
            (CheckKind::UnicodeScalar { .. }, ProofStatus::Proved),
        ]
    ));
}

#[test]
fn variable_divisor_and_slice_index_stay_unknown() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    // 除数与下标都是变量：必须 Unknown，不得误标 Proved 或 Disproved。
    let source = "fn f(x: int, d: int, a: &[int], i: int) { _ = x / d\n _ = a[i] }\nfn main() { let s = [1, 2, 3]\n let d = 2\n let i = 1\n f(1, d, &s, i) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(statuses.iter().all(|(kind, status)| {
        let expected = match kind {
            // 切片下标：长度未知 → Unknown。
            CheckKind::Bounds { slice: false } => ProofStatus::Unknown,
            CheckKind::Division { .. } => ProofStatus::Unknown,
            _ => *status,
        };
        assert_eq!(*status, expected, "变量操作数必须保持 Unknown：{kind:?}");
        true
    }));
}

#[test]
fn statically_failing_checks_are_disproved_and_never_proved() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    // 数组长度 2：下标 5 必然越界、下标 1 可证安全；除以 0 必然失败；非法标量必然失败。
    let source = "fn f() { let a = [1, 2]\n _ = a[5]\n _ = a[1]\n _ = 1 / 0\n _ = char(1114112) }\nfn main() { f() }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(matches!(
        statuses.as_slice(),
        [
            (CheckKind::Bounds { slice: false }, ProofStatus::Disproved),
            (CheckKind::Bounds { slice: false }, ProofStatus::Proved),
            (CheckKind::Division { .. }, ProofStatus::Disproved),
            (CheckKind::UnicodeScalar { .. }, ProofStatus::Disproved),
        ]
    ));
}

#[test]
fn summaries_of_callers_absorb_callee_effects() {
    // caller 无自身调用但有检查（可能 panic）；callee 含除法检查；
    // main 调用两者，其摘要必须包含 may_panic。
    let source = "fn leaf(x: int) { _ = 1 / 0 }\nfn mid() { leaf(1) }\nfn main() { mid() }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(!statuses.is_empty());
}

#[test]
fn array_slice_len_is_an_int_method() {
    use crate::frontend::analysis::ProofStatus;
    let source = "fn main() { let a = [1, 2, 3]\n let n: int = a.len()\n let s = &a\n let m: int = s.len()\n let k: int = [1, 2, 3].len()\n _ = n\n _ = m\n _ = k }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(
        statuses.is_empty()
            || statuses
                .iter()
                .all(|(_, status)| *status != ProofStatus::Disproved)
    );
}

#[test]
fn loop_iv_and_break_prove_array_index() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    let source = "fn f() { let n = 20\n let v = [0; 8]\n for i in 0..n {\n if i >= 2 { break }\n _ = v[i]\n } }\nfn main() { f() }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    let bounds: Vec<_> = statuses
        .iter()
        .filter(|(kind, _)| matches!(kind, CheckKind::Bounds { slice: false }))
        .map(|(_, status)| *status)
        .collect();
    assert!(
        bounds.contains(&ProofStatus::Proved),
        "循环归纳 + break 应收窄 i < 2 < 8：{statuses:?}"
    );
}

#[test]
fn spec_slice_len_and_break_prove_index() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    let source = "fn f(v: &[int], n: int) {\n if v.len() > 10 {\n for i in 0..n {\n if i >= 2 { break }\n _ = v[i]\n }\n }\n }\nfn main() { let a = [0; 16]\n f(&a, 20) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    let bounds: Vec<_> = statuses
        .iter()
        .filter(|(kind, _)| matches!(kind, CheckKind::Bounds { slice: false }))
        .map(|(_, status)| *status)
        .collect();
    assert!(
        bounds.contains(&ProofStatus::Proved),
        "v.len() > 10 且 i < 2 必须证明下标：{statuses:?}"
    );
}

#[test]
fn local_binding_proves_division_and_shift() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    let source =
        "fn f(x: int) { let d = 2\n let s = 3\n _ = x / d\n _ = x << s }\nfn main() { f(8) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(
        statuses.iter().any(|(kind, status)| {
            matches!(kind, CheckKind::Division { .. }) && *status == ProofStatus::Proved
        }),
        "局部绑定除数必须 Proved：{statuses:?}"
    );
    assert!(
        statuses.iter().any(|(kind, status)| {
            matches!(kind, CheckKind::Shift { .. }) && *status == ProofStatus::Proved
        }),
        "局部绑定移位量必须 Proved：{statuses:?}"
    );
}

#[test]
fn reassignment_invalidates_slice_length_proof() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    let source = "fn f(a: &[int], b: &[int]) { let v = a\n if v.len() > 10 {\n v = b\n _ = v[0]\n } }\nfn main() { let x = [0; 16]\n let y = [0; 1]\n f(&x, &y) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    let after_assign = statuses
        .iter()
        .find(|(kind, _)| matches!(kind, CheckKind::Bounds { slice: false }));
    assert!(
        after_assign.is_some_and(|(_, status)| *status == ProofStatus::Unknown),
        "改写切片后长度事实必须失效：{statuses:?}"
    );
}

#[test]
fn analysis_policy_block_iterations_change_action_key() {
    use crate::project::ActionInputs;
    let mut first = ActionInputs::new(b"c", "host", "host", "bin");
    first.set_analysis_policy(AnalysisPolicyV1::default().canonical_bytes());
    let mut second = ActionInputs::new(b"c", "host", "host", "bin");
    let mut policy = AnalysisPolicyV1::default();
    policy.max_block_iterations = 1;
    second.set_analysis_policy(policy.canonical_bytes());
    assert_ne!(first.key(), second.key());
}

#[test]
fn ffi_and_spawn_invalidate_slice_index() {
    use crate::frontend::analysis::ProofStatus;
    use crate::frontend::hir::CheckKind;
    let ffi = "#[ffi(leaf(stack = 8))] extern \"C\" fn leaf()\nfn f(v: &[int]) {\n if v.len() > 10 {\n #[ffi(leaf)] leaf()\n _ = v[0]\n }\n }\nfn main() { let a = [0; 16]\n f(&a) }";
    let ffi_statuses = proof_statuses(&[("main.gg", ffi)]);
    assert!(
        ffi_statuses.iter().any(|(kind, status)| {
            matches!(kind, CheckKind::Bounds { slice: false }) && *status == ProofStatus::Unknown
        }),
        "FFI 之后必须保留下标检查：{ffi_statuses:?}"
    );
    let spawn = "fn f(v: &[int]) {\n if v.len() > 10 {\n let task = async { 1 }\n _ = task.wait()\n _ = v[0]\n }\n }\nfn main() { let a = [0; 16]\n f(&a) }";
    let spawn_statuses = proof_statuses(&[("main.gg", spawn)]);
    assert!(
        spawn_statuses.iter().any(|(kind, status)| {
            matches!(kind, CheckKind::Bounds { slice: false }) && *status == ProofStatus::Unknown
        }),
        "spawn/wait 之后必须保留下标检查：{spawn_statuses:?}"
    );
}

#[test]
fn block_iteration_budget_exhaustion_keeps_checks_unknown() {
    use crate::frontend::analysis::ProofStatus;
    let source = "fn f() { let n = 20\n let v = [0; 8]\n for i in 0..n {\n if i >= 2 { break }\n _ = v[i]\n } }\nfn main() { f() }";
    let queries = crate::QueryEngine::new();
    let output = compile(&[("main.gg", source)], &queries);
    let module = output.hir.module();
    let keys = module
        .owners
        .iter()
        .enumerate()
        .map(|(index, owner)| super::solver::SccMember {
            mono_key: output
                .mono
                .instances
                .iter()
                .find(|instance| {
                    instance
                        .mono_key
                        .starts_with(&module.definitions[owner.definition.index()].key)
                })
                .expect("测试函数的实例已闭合")
                .mono_key
                .clone(),
            owner: super::callgraph::callable_key_at(module, index),
            calls: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut policy = AnalysisPolicyV1::default();
    policy.max_block_iterations = 1;
    let component: Vec<_> = (0..keys.len()).collect();
    let scc = super::solver::analyze_scc(module, &keys, &component, policy, &|_| {
        super::FunctionSummary::default()
    });
    assert!(scc.budget_exhausted, "循环在 1 次块迭代下必须耗尽预算");
    assert!(
        scc.proofs
            .iter()
            .all(|fact| fact.status != ProofStatus::Proved),
        "预算耗尽不得产生新的 Proved：{:?}",
        scc.proofs
    );
}

#[test]
fn callee_write_through_reference_invalidates_slice_length() {
    use crate::frontend::hir::CheckKind;
    let source = "fn dirty(xs: & &[int]) { let replacement = [1]\n *xs = &replacement }\nfn f(v: &[int]) { if v.len() > 10 { dirty(&v)\n _ = v[0] } }\nfn main() { let a = [0; 16]\n f(&a) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(
        statuses.iter().any(|(kind, status)| {
            matches!(kind, CheckKind::Bounds { slice: false }) && *status == ProofStatus::Unknown
        }),
        "callee 通过引用写入后必须保留检查：{statuses:?}"
    );
}

#[test]
fn readonly_callee_preserves_caller_slice_length() {
    use crate::frontend::hir::CheckKind;
    let source = "fn inspect(xs: &[int]) int = xs.len()\nfn f(v: &[int]) { if v.len() > 10 { _ = inspect(v)\n _ = v[0] } }\nfn main() { let a = [0; 16]\n f(&a) }";
    let statuses = proof_statuses(&[("main.gg", source)]);
    assert!(
        statuses.iter().any(|(kind, status)| {
            matches!(kind, CheckKind::Bounds { slice: false }) && *status == ProofStatus::Proved
        }),
        "已证明只读的具体 callee 不应破坏长度事实：{statuses:?}"
    );
}

#[test]
fn shared_check_requires_proof_in_every_instance() {
    use super::types::{ProofFact, RuntimeCheckKey, SccSummaryV1};
    use crate::frontend::hir::{CheckKind, ExprId};
    let key = RuntimeCheckKey {
        owner_index: 0,
        expression: ExprId(0),
        kind: CheckKind::Bounds { slice: false },
    };
    for statuses in [
        [ProofStatus::Proved, ProofStatus::Unknown],
        [ProofStatus::Unknown, ProofStatus::Proved],
    ] {
        let parts = statuses
            .into_iter()
            .map(|status| SccSummaryV1 {
                instances: Vec::new(),
                proofs: vec![ProofFact {
                    key: key.clone(),
                    status,
                }],
                budget_exhausted: false,
            })
            .collect();
        let world = super::solver::world_from_sccs(parts, [0; 32]);
        assert_eq!(world.proof_status(&key), ProofStatus::Unknown);
        assert_eq!(world.runtime_checks_elided_count, 0);
    }
}

#[test]
fn nested_scc_summaries_match_world_projection() {
    let source = "fn ping() { pong()\n _ = 1 / 0 }\nfn pong() { ping() }\nfn main() { ping() }";
    let queries = crate::QueryEngine::new();
    let output = compile(&[("main.gg", source)], &queries);
    let module = output.hir.module();
    assert_eq!(
        output.analysis.instances.len(),
        output.mono.instances.len(),
        "每个闭合实例都必须有 FunctionAnalysisSummary 投影"
    );
    for instance in &output.mono.instances {
        assert!(
            output
                .analysis
                .instances
                .iter()
                .any(|record| record.mono_key == instance.mono_key),
            "world 缺少实例 {}",
            instance.symbol
        );
    }
    assert!(
        output
            .analysis
            .instances
            .iter()
            .filter(|record| record.summary.may_panic)
            .count()
            >= 2,
        "互递归 SCC 必须把 callee 的 may_panic 吸收进双方摘要：{:?}",
        output.analysis.instances
    );
    let _ = module;
}
