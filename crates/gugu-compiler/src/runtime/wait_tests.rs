//! channel / Join / select 的契约与前端闸门。
//!
//! 镜像内执行由 `linux_pie` 验收。这里只保留契约漂移、demand 与非法源码拒绝。

use crate::runtime::PlatformProfile;
use crate::runtime::wait_schema::{
    INLINE_SELECT_CASES, WAIT_SCHEMA, WaitDemand, WaitRuntimeContract,
};
use crate::{CompileRequest, Compiler, DiagnosticCode, TargetName};

#[test]
fn wait_contract_matches_profile_and_rejects_drift() {
    let contract = WaitRuntimeContract::build(WaitDemand::default(), PlatformProfile::Linux)
        .expect("等待契约可构建");
    contract.verify().expect("等待契约自洽");
    assert_eq!(contract.schema(), WAIT_SCHEMA);
    assert_eq!(contract.inline_select_cases(), INLINE_SELECT_CASES);
    assert_eq!(contract.scratch_class_count(), 11);
    assert_eq!(contract.wait_node_class_count(), 2);
    assert_ne!(contract.fingerprint(), [0_u8; 32]);
    assert!(contract.dump().contains("wait schema=1"));
    let mut drifted = contract.clone();
    drifted.inline_select_cases = 7;
    assert!(drifted.verify().is_err());
}

#[cfg(feature = "slow-tests")]
fn wait_demand_and_frontend_gates_reach_image_plan() {
    let compiler = Compiler::new();
    let idle = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let value = 1\n _ = value }",
        TargetName::X86_64Linux,
    ));
    assert!(idle.is_success(), "{:?}", idle.diagnostics().items());
    let idle_plan = idle.image_plan().expect("image-plan");
    assert_eq!(idle_plan.wait_inline_select_cases(), 8);
    assert_eq!(idle_plan.wait_scratch_class_count(), 11);
    assert_eq!(idle_plan.wait_node_class_count(), 2);
    assert_ne!(idle_plan.wait_contract_fingerprint(), [0_u8; 32]);
    let dump = idle.dump_runtime().expect("dump");
    assert!(dump.contains("wait schema=1"));
    assert_eq!(idle_plan.wait_demand().channel_ops(), 0);

    let source = r#"fn work() int { 1 }
fn main() {
    let c = chan[int](1)
    c.send(1)
    _ = c.try_send(1)
    _ = c.try_recv()
    _ = c.recv()
    let j = async { work() }
    _ = j.wait()
    select {
        c.send(1) => {}
        let v = c.recv() => { _ = v }
        let r = j.wait() => { _ = r }
        _ => {}
    }
}
"#;
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let compile = || compiler.compile(CompileRequest::single_file("main.gg", source, target));
        let cold = compile();
        let warm = compile();
        assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
        let plan = cold.image_plan().expect("image-plan");
        let demand = plan.wait_demand();
        assert!(demand.channel_new > 0);
        assert!(demand.channel_send > 0);
        assert!(demand.channel_try_send > 0);
        assert!(demand.channel_try_recv > 0);
        assert!(demand.channel_receive > 0);
        assert!(demand.join_wait > 0);
        assert!(demand.select_commit > 0);
        assert!(demand.select_safepoints > 0);
        assert_ne!(
            plan.wait_contract_fingerprint(),
            idle_plan.wait_contract_fingerprint()
        );
        assert_eq!(cold.action_key(), warm.action_key());
        assert_eq!(cold.dump_runtime(), warm.dump_runtime());
    }

    let negative = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let c = chan[int](-1)\n _ = c }",
        TargetName::X86_64Linux,
    ));
    assert!(!negative.is_success());
    assert!(negative.image_plan().is_none());
    assert!(
        negative
            .diagnostics()
            .items()
            .iter()
            .any(|item| item.message().contains("channel 缓冲长度不能为负"))
    );

    let illegal = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let c = chan[int](1)\n select { c.try_send(1) => {} } }",
        TargetName::X86_64Linux,
    ));
    assert!(!illegal.is_success());
    assert!(illegal.image_plan().is_none());
    assert_eq!(
        illegal.diagnostics().items()[0].code(),
        DiagnosticCode::ParseInvalidSelectArm
    );
    assert!(
        illegal
            .diagnostics()
            .items()
            .iter()
            .any(|item| item.message().contains("try_send"))
    );
}
