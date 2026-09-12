//! channel / Join / select 等待协议的确定性回归；不睡眠、不读 OS 熵。

use super::RawWorld;
use super::coroutine_impl::CoroutineEntry;
use crate::runtime::PlatformProfile;
use crate::runtime::channel::{RecvOutcome, SendOutcome, TryRecvErr, TrySendErr};
use crate::runtime::coroutine::{CompletionValue, CoroutineHandle, CoroutineState};
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::message::{BatchLimits, ReturnKind};
use crate::runtime::select::{SelectCase, SelectOp, SelectOutcome, building_cas_winner};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::SlotState;
use crate::runtime::wait::{SelectTxn, phase_building, winner_unset};
use crate::runtime::wait_schema::{
    INLINE_SELECT_CASES, WAIT_SCHEMA, WaitDemand, WaitRuntimeContract,
};
use crate::{CompileRequest, Compiler, DiagnosticCode, TargetName};

fn entry() -> CoroutineEntry {
    CoroutineEntry {
        pc: 0x1000,
        required_frame: 64,
    }
}

fn plane(owners: u32) -> RawWorld {
    RawWorld::new(7, owners, 64, BatchLimits::default()).expect("raw world")
}

fn booted() -> RawWorld {
    let mut world = plane(2);
    world
        .boot(
            vec![],
            vec![("GUGU_RUNTIME_STACK_MAX".to_owned(), "64KiB".to_owned())],
            "/".to_owned(),
            2,
            entry(),
        )
        .expect("boot");
    world
}

fn running(world: &mut RawWorld) -> CoroutineHandle {
    let child = world
        .spawn_user_coroutine(0, entry())
        .expect("创建")
        .expect("接纳");
    world.enter_coroutine(child).expect("切入");
    child
}

fn lifecycle(world: &RawWorld, handle: CoroutineHandle) -> CoroutineState {
    world
        .controls
        .get(handle)
        .expect("control")
        .0
        .hot
        .lifecycle()
        .expect("lifecycle")
}

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

#[test]
fn buffered_close_does_not_drop_linearized_send() {
    let mut world = booted();
    let channel = world.channel_new(1).expect("channel");
    let sender = running(&mut world);
    assert!(matches!(
        world.channel_send(channel, sender, 7, false).unwrap(),
        SendOutcome::Sent { wake: None }
    ));
    world.channel_close(channel).expect("close");
    let receiver = running(&mut world);
    assert!(matches!(
        world.channel_recv(channel, receiver).unwrap(),
        RecvOutcome::Value { payload: 7, .. }
    ));
    let drained = running(&mut world);
    assert!(matches!(
        world.channel_recv(channel, drained).unwrap(),
        RecvOutcome::Closed
    ));
    assert_eq!(
        world.channel_try_recv(channel).unwrap(),
        Err(TryRecvErr::Closed)
    );
}

#[test]
fn close_before_send_panics_and_second_close_panics() {
    let mut world = booted();
    let channel = world.channel_new(1).expect("channel");
    world.channel_close(channel).expect("close");
    let sender = running(&mut world);
    let error = world
        .channel_send(channel, sender, 1, false)
        .expect_err("向已关闭 channel send");
    assert!(error.message().contains("send on closed channel"));
    assert!(world.channel_close(channel).is_err());
}

#[test]
fn unbuffered_rendezvous_and_try_ops() {
    let mut world = booted();
    let channel = world.channel_new(0).expect("无缓冲");
    assert_eq!(
        world.channel_try_send(channel, 3).unwrap(),
        Err(TrySendErr::Full)
    );
    assert_eq!(
        world.channel_try_recv(channel).unwrap(),
        Err(TryRecvErr::Empty)
    );
    let receiver = running(&mut world);
    assert!(matches!(
        world.channel_recv(channel, receiver).unwrap(),
        RecvOutcome::Parked(_)
    ));
    assert_eq!(lifecycle(&world, receiver), CoroutineState::Waiting);
    assert_eq!(world.channel_try_send(channel, 11).unwrap(), Ok(()));
    assert_eq!(lifecycle(&world, receiver), CoroutineState::Runnable);
    assert_eq!(world.wait.take_delivered(receiver), Some(11));
    world.channel_close(channel).expect("close");
    assert_eq!(
        world.channel_try_send(channel, 1).unwrap(),
        Err(TrySendErr::Closed)
    );
}

#[test]
fn large_payload_reservation_is_invisible_until_publish() {
    let mut world = booted();
    let channel = world.channel_new(1).expect("channel");
    let sender = running(&mut world);
    assert!(matches!(
        world.channel_send(channel, sender, 64, true).unwrap(),
        SendOutcome::Sent { wake: None }
    ));
    assert_eq!(world.channel_try_recv(channel).unwrap(), Ok(64));
}

#[test]
fn join_wait_repeats_record_and_detach_does_not_cancel() {
    let mut world = booted();
    let child = running(&mut world);
    let waiter = running(&mut world);
    assert!(world.join_wait(child, waiter).unwrap().is_err());
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Waiting);
    world
        .coroutine_finished(child, 0, CompletionValue::Bits(42))
        .expect("完成");
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Runnable);
    assert_eq!(
        world.join_wait(child, waiter).unwrap(),
        Ok(CompletionValue::Bits(42))
    );
    assert_eq!(
        world.join_wait(child, waiter).unwrap(),
        Ok(CompletionValue::Bits(42))
    );
    let detached = running(&mut world);
    world.release_join(detached).expect("分离");
    world
        .coroutine_finished(detached, 0, CompletionValue::Bits(9))
        .expect("分离协程仍能完成");
}

#[test]
fn select_ready_beats_default_and_try_lock_fallback() {
    let mut world = booted();
    let channel = world.channel_new(1).expect("channel");
    world.channel_try_send(channel, 5).unwrap().unwrap();
    let waiter = running(&mut world);
    let cases = [SelectCase {
        op: SelectOp::Recv { channel },
        index: 0,
    }];
    assert_eq!(
        world.select(waiter, &cases, true).unwrap(),
        SelectOutcome::Case(0)
    );
    world.wait.fail_try_lock = true;
    let other = world.channel_new(1).expect("channel");
    world.channel_try_send(other, 8).unwrap().unwrap();
    let waiter = running(&mut world);
    let cases = [SelectCase {
        op: SelectOp::Recv { channel: other },
        index: 0,
    }];
    assert_eq!(
        world.select(waiter, &cases, true).unwrap(),
        SelectOutcome::Case(0)
    );
}

#[test]
fn select_scan_path_covers_more_than_inline_cases() {
    let mut world = booted();
    let mut channels = Vec::new();
    for _ in 0..9 {
        channels.push(world.channel_new(1).expect("channel"));
    }
    world.channel_try_send(channels[4], 21).unwrap().unwrap();
    let cases: Vec<_> = channels
        .iter()
        .enumerate()
        .map(|(index, channel)| SelectCase {
            op: SelectOp::Recv { channel: *channel },
            index: index as u32,
        })
        .collect();
    let waiter = running(&mut world);
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Case(4)
    );
}

#[test]
fn select_never_parks_without_ordinary_waker() {
    let mut world = booted();
    let waiter = running(&mut world);
    assert_eq!(
        world.select(waiter, &[], false).unwrap(),
        SelectOutcome::Never
    );
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Waiting);
}

#[test]
fn select_loser_is_unlinked_and_wait_generation_readies_once() {
    let mut world = booted();
    let first = world.channel_new(0).expect("channel");
    let second = world.channel_new(0).expect("channel");
    let waiter = running(&mut world);
    let cases = [
        SelectCase {
            op: SelectOp::Recv { channel: first },
            index: 0,
        },
        SelectCase {
            op: SelectOp::Recv { channel: second },
            index: 1,
        },
    ];
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Parked
    );
    let sender = running(&mut world);
    world.channel_send(first, sender, 3, false).expect("会合");
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Runnable);
    assert_eq!(
        world.channel_try_send(second, 4).unwrap(),
        Err(TrySendErr::Full),
        "loser 必须已从第二通道注销"
    );
    let node = world
        .wait
        .alloc_node(waiter, world.wait.never_source(), 0, 0, 0, 0, 1)
        .expect("node");
    world.wait.release_node(node).expect("归还");
    assert!(world.wait.mark_ready(node).is_err());
}

#[test]
fn building_winner_cas_does_not_ready() {
    let mut txn = SelectTxn {
        phase_winner: phase_building() << 32,
        case_count: 1,
        scratch_handle: 0,
        wait_block: 0,
    };
    assert_eq!(txn.winner(), winner_unset());
    assert!(building_cas_winner(&mut txn, 0));
    assert!(!building_cas_winner(&mut txn, 1));
}

#[test]
fn wait_node_return_is_exactly_once_and_cross_owner() {
    let mut world = plane(2);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(world.classes().get(class).expect("class").slot_stride);
    let slot = world.return_wait_node(0, 1).expect("WaitNode 归还");
    assert!(world.queue_return(0, slot, stride).is_err());
    let budget = ServiceBudget::new(64, 1 << 16);
    let report = world
        .service(
            1,
            super::super::inbox::ShardIndex::from_raw(0).unwrap(),
            &budget,
        )
        .expect("service");
    assert_eq!(report.items, 1);
    assert_eq!(
        world.table().state(slot.descriptor, slot.index),
        Ok(SlotState::Returned)
    );
    let _ = ReturnKind::WaitNode;
}

#[test]
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
