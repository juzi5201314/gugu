//! AbstractAnalysis 单测：证明状态与 action key 策略敏感。

use crate::frontend::analysis::proved_bounds_count;
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
fn empty_world_schema_is_stable() {
    let world = crate::frontend::analysis::empty_world();
    assert_eq!(world.schema, WORLD_SCHEMA_VERSION);
    assert_eq!(proved_bounds_count(&world), 0);
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
