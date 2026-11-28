//! 源码协程进入已验证布局、上下文契约与 query 身份的整管线回归。
//!
//! 控制块、stack arena 与完成交接的确定性回归在 `gugu-runtime-world`。

use crate::{CompileRequest, Compiler, TargetName};

/// `fixtures/coroutines.gg` 在 `target` 上的协程/调度契约与冷热重放一致性。
fn assert_source_coroutines_reach_image_plan(target: TargetName) {
    let compiler = Compiler::shared();
    let source = include_str!("fixtures/coroutines.gg");
    let compile =
        || compiler.compile_full_for_test(CompileRequest::single_file("main.gg", source, target));
    let cold = compile();
    let warm = compile();
    assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
    let plan = cold.image_plan().expect("成功编译有 image-plan");
    let contract = plan.coroutine_runtime();
    assert_eq!(contract.demand.creation_sites, 2);
    assert!(contract.demand.checked_entries > 0);
    assert_eq!(plan.scheduler_local_capacity(), 256);
    assert_eq!(plan.scheduler_remote_shard_count(), 8);
    assert_eq!(plan.scheduler_batch_max_items(), 128);
    assert_eq!(plan.scheduler_service_interval(), 61);
    assert_eq!(plan.scheduler_service_batch(), 128);
    assert!(plan.scheduler_demand().spawn_sites >= 2);
    assert_ne!(plan.scheduler_contract_fingerprint(), [0_u8; 32]);
    assert_eq!(cold.action_key(), warm.action_key());
    let warm_plan = warm.image_plan().expect("成功编译有 image-plan");
    assert_eq!(contract, warm_plan.coroutine_runtime());
    assert_eq!(plan.scheduler_runtime(), warm_plan.scheduler_runtime());
}

#[test]
fn source_coroutines_reach_linux_image_plan() {
    assert_source_coroutines_reach_image_plan(TargetName::X86_64Linux);
}

#[test]
fn source_coroutines_reach_windows_image_plan() {
    assert_source_coroutines_reach_image_plan(TargetName::X86_64Windows);
}
