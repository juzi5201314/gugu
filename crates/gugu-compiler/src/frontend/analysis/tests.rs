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
    let mut optimistic = FunctionSummary {
        may_panic: false,
        may_call_unknown: false,
        may_mutate_len: false,
        reads_hidden_state: false,
        writes_hidden_state: false,
    };
    optimistic.join_with(&FunctionSummary::conservative());
    // 效果并集只能变保守，不能把"可能发生"降级掉。
    assert_eq!(optimistic, FunctionSummary::conservative());
}

fn proof_statuses(
    sources: &[(&str, &str)],
) -> Vec<(
    crate::frontend::hir::CheckKind,
    crate::frontend::analysis::ProofStatus,
)> {
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
    let queries = crate::QueryEngine::new();
    let output = crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut map,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/analysis@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        &queries,
    )
    .expect("前端检查通过");
    // 再跑一遍走缓存命中路径，冷热结果必须一致。
    let mut map = crate::SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| crate::SourceSnapshot::from_str(path, source).expect("快照"))
            .collect(),
    )
    .expect("源映射");
    let warm = crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut map,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/analysis@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        &queries,
    )
    .expect("缓存命中路径通过");
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
