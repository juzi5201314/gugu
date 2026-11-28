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
const CHANNEL_SOURCE: &str = r#"fn work() int { 1 }
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

/// `target` 上零等待需求的契约指纹；不含 channel 的程序必须落在这条基线上。
#[cfg(feature = "slow-tests")]
fn zero_demand_fingerprint(target: TargetName) -> [u8; 32] {
    WaitRuntimeContract::build(WaitDemand::default(), PlatformProfile::from(target))
        .expect("等待契约可构建")
        .fingerprint()
}

#[test]
#[cfg(feature = "slow-tests")]
fn idle_program_reaches_image_plan_with_default_wait_contract() {
    let idle = Compiler::shared().compile_full_for_test(CompileRequest::single_file(
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
    assert_eq!(idle_plan.wait_demand(), WaitDemand::default());
    assert_eq!(
        idle_plan.wait_contract_fingerprint(),
        zero_demand_fingerprint(TargetName::X86_64Linux)
    );
}

/// channel/join/select 需求进入 `target` 的 image plan，契约指纹偏离零需求基线，且重放稳定。
#[cfg(feature = "slow-tests")]
fn assert_channel_demand_reaches_image_plan(target: TargetName) {
    let compiler = Compiler::shared();
    let compile = || {
        compiler.compile_full_for_test(CompileRequest::single_file(
            "main.gg",
            CHANNEL_SOURCE,
            target,
        ))
    };
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
        zero_demand_fingerprint(target)
    );
    assert_eq!(cold.action_key(), warm.action_key());
    assert_eq!(cold.dump_runtime(), warm.dump_runtime());
}

#[test]
#[cfg(feature = "slow-tests")]
fn channel_demand_reaches_linux_image_plan() {
    assert_channel_demand_reaches_image_plan(TargetName::X86_64Linux);
}

#[test]
#[cfg(feature = "slow-tests")]
fn channel_demand_reaches_windows_image_plan() {
    assert_channel_demand_reaches_image_plan(TargetName::X86_64Windows);
}

#[test]
#[cfg(feature = "slow-tests")]
fn wait_frontend_gates_reject_before_image_plan() {
    let compiler = Compiler::shared();
    let negative = compiler.compile_full_for_test(CompileRequest::single_file(
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

    let illegal = compiler.compile_full_for_test(CompileRequest::single_file(
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

/// select 用作表达式时无 default 的 otherwise 必须终止为不可达，不能向结果槽写 unit。
#[test]
#[cfg(feature = "slow-tests")]
fn select_expression_without_default_reaches_image_plan() {
    let compiler = Compiler::shared();
    for source in [
        "fn main() {\n let a = chan[int](1)\n let b = chan[int](1)\n let r = select {\n let x = a.recv() => 0\n let y = b.recv() => 1\n }\n _ = r\n}",
        "fn main() {\n let a = async { 1 }\n let b = async { 2 }\n let r = select {\n let x = a.wait() => x\n let y = b.wait() => y\n }\n _ = r\n}",
    ] {
        let compile = compiler.compile_full_for_test(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(compile.is_success(), "{:?}", compile.diagnostics().items());
        assert!(compile.image_plan().is_some());
    }
}
