//! channel / Join / select 等待协议的确定性回归；不睡眠、不读 OS 熵。

use super::RawWorld;
use super::coroutine_impl::CoroutineEntry;
use crate::runtime::PlatformProfile;
use crate::runtime::channel::{ChannelHandle, RecvOutcome, SendOutcome, TryRecvErr, TrySendErr};
use crate::runtime::coroutine::{CompletionValue, CoroutineHandle, CoroutineState};
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::message::{BatchLimits, ReturnKind};
use crate::runtime::select::{SelectCase, SelectOp, SelectOutcome, arm_select};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::SlotState;
use crate::runtime::wait::{
    JoinOutcome, SelectTxn, WAIT_NODE_BUILDING, WAIT_NODE_SELECT, WaitResult, phase_building,
};
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
    assert_eq!(
        world.wait.take_delivered(receiver),
        Some(WaitResult::Recv(11))
    );
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
    assert!(matches!(
        world.join_wait(child, waiter).unwrap(),
        JoinOutcome::Parked(_)
    ));
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Waiting);
    world
        .coroutine_finished(child, 0, CompletionValue::Bits(42))
        .expect("完成");
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Runnable);
    assert_eq!(
        world.join_wait(child, waiter).unwrap(),
        JoinOutcome::Completed(CompletionValue::Bits(42))
    );
    assert_eq!(
        world.join_wait(child, waiter).unwrap(),
        JoinOutcome::Completed(CompletionValue::Bits(42))
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
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(5)));
    assert_eq!(
        world.channel_try_recv(channel).unwrap(),
        Err(TryRecvErr::Empty)
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
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(8)));
    assert_eq!(
        world.channel_try_recv(other).unwrap(),
        Err(TryRecvErr::Empty)
    );
}

#[test]
fn select_scan_crosses_word_boundaries() {
    for count in [9_usize, 65, 129] {
        let mut world = booted();
        let channels: Vec<_> = (0..count).map(|_| world.channel_new(1).unwrap()).collect();
        world
            .channel_try_send(channels[count - 1], 21)
            .unwrap()
            .unwrap();
        let cases: Vec<_> = channels
            .iter()
            .enumerate()
            .map(|(index, channel)| SelectCase {
                op: SelectOp::Recv { channel: *channel },
                index: u32::try_from(index).unwrap(),
            })
            .collect();
        let waiter = running(&mut world);
        assert_eq!(
            world.select(waiter, &cases, false).unwrap(),
            SelectOutcome::Case(u32::try_from(count - 1).unwrap())
        );
        assert_eq!(
            world.wait.take_delivered(waiter),
            Some(WaitResult::Recv(21))
        );
        assert_eq!(
            world.channel_try_recv(channels[count - 1]).unwrap(),
            Err(TryRecvErr::Empty)
        );
    }
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
        SelectTxn::from_cold(world.controls.get(waiter).unwrap().1.select_scratch).winner(),
        SelectTxn::encode_case(0)
    );
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(3)));
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

fn prepare_building_recv(
    world: &mut RawWorld,
    coroutine: CoroutineHandle,
    channels: &[ChannelHandle],
    registered: usize,
) {
    let generation = world.wait.begin_wait(coroutine).unwrap();
    world.controls.get_mut(coroutine).unwrap().1.select_scratch = SelectTxn {
        phase_winner: phase_building() << 32,
        case_count: u64::try_from(channels.len()).unwrap(),
        scratch_handle: 0,
        wait_block: 0,
    }
    .to_cold();
    let mut nodes = Vec::new();
    for (index, channel) in channels.iter().enumerate() {
        let source = world.channels.source(*channel).unwrap();
        nodes.push(
            world
                .wait
                .alloc_node(
                    coroutine,
                    source,
                    u32::try_from(index).unwrap(),
                    0,
                    0,
                    WAIT_NODE_SELECT | WAIT_NODE_BUILDING,
                    generation,
                )
                .unwrap(),
        );
    }
    world.wait.arm_nodes(coroutine, nodes);
    for (index, channel) in channels.iter().enumerate().take(registered) {
        let source = world.channels.source(*channel).unwrap();
        let node = world.wait.armed_nodes(coroutine)[index];
        world.wait.lock(source).unwrap();
        world
            .wait
            .enqueue(world.channels.recv_queue_mut(*channel).unwrap(), node)
            .unwrap();
        world.channels.sync_heads(*channel).unwrap();
        world.wait.unlock(source).unwrap();
    }
}

pub(super) fn consume_woken(world: &mut RawWorld, coroutine: CoroutineHandle) {
    let processor = world.scheduler.active_snapshot()[0];
    let runnable = world
        .scheduler
        .schedule_step(processor)
        .unwrap()
        .expect("等待完成必须真的可调度");
    assert_eq!(runnable.coroutine, coroutine);
    world
        .controls
        .get(coroutine)
        .unwrap()
        .0
        .hot
        .take_running()
        .unwrap();
    assert_eq!(
        world.scheduler.schedule_step(processor).unwrap(),
        None,
        "不能重复发布同一个 waiter"
    );
}

#[test]
fn building_winner_commits_payload_without_premature_ready() {
    let mut world = booted();
    let channels = [world.channel_new(0).unwrap(), world.channel_new(0).unwrap()];
    let waiter = running(&mut world);
    prepare_building_recv(&mut world, waiter, &channels, 1);
    world.channel_try_send(channels[0], 11).unwrap().unwrap();
    assert_eq!(lifecycle(&world, waiter), CoroutineState::Running);
    assert_eq!(
        world.wait.armed_nodes(waiter).len(),
        2,
        "未完成登记的节点不能提前释放"
    );
    assert_eq!(
        SelectTxn::from_cold(world.controls.get(waiter).unwrap().1.select_scratch).winner(),
        SelectTxn::encode_case(0)
    );
    assert_eq!(
        arm_select(&mut world.wait, &mut world.controls, waiter).unwrap(),
        SelectOutcome::Case(0)
    );
    world.cleanup_waiters(waiter).unwrap();
    assert_eq!(
        world.wait.take_delivered(waiter),
        Some(WaitResult::Recv(11))
    );
    assert_eq!(
        world.channel_try_send(channels[1], 12).unwrap(),
        Err(TrySendErr::Full)
    );
    assert_eq!(
        world
            .scheduler
            .schedule_step(world.scheduler.active_snapshot()[0])
            .unwrap(),
        None
    );
}

#[test]
fn parking_completion_resumes_inline_or_publishes_after_waiting() {
    for during_transition in [false, true] {
        let mut world = booted();
        let channel = world.channel_new(0).unwrap();
        let waiter = running(&mut world);
        prepare_building_recv(&mut world, waiter, &[channel], 1);
        assert_eq!(
            arm_select(&mut world.wait, &mut world.controls, waiter).unwrap(),
            SelectOutcome::Parked
        );
        assert_eq!(lifecycle(&world, waiter), CoroutineState::Parking);
        if during_transition {
            assert!(
                world
                    .park_current_wait_with(waiter, |world| {
                        world.channel_try_send(channel, 7).unwrap().unwrap();
                    })
                    .unwrap()
            );
            assert_eq!(lifecycle(&world, waiter), CoroutineState::Runnable);
            consume_woken(&mut world, waiter);
        } else {
            world.channel_try_send(channel, 7).unwrap().unwrap();
            assert_eq!(world.wait.armed_nodes(waiter).len(), 1);
            assert!(!world.park_current_wait(waiter).unwrap());
            assert_eq!(lifecycle(&world, waiter), CoroutineState::Running);
            assert_eq!(
                world
                    .scheduler
                    .schedule_step(world.scheduler.active_snapshot()[0])
                    .unwrap(),
                None
            );
        }
        assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(7)));
        assert!(world.wait.armed_nodes(waiter).is_empty());
    }
}

#[test]
fn select_send_commits_buffer_and_both_rendezvous_waitsets() {
    let mut world = booted();
    let buffered = world.channel_new(1).unwrap();
    let sender = running(&mut world);
    let send = [SelectCase {
        op: SelectOp::Send {
            channel: buffered,
            payload: 7,
        },
        index: 0,
    }];
    assert_eq!(
        world.select(sender, &send, false).unwrap(),
        SelectOutcome::Case(0)
    );
    assert_eq!(world.wait.take_delivered(sender), Some(WaitResult::Sent));
    assert_eq!(world.channel_try_recv(buffered).unwrap(), Ok(7));
    let channel = world.channel_new(0).unwrap();
    let receiver = running(&mut world);
    let recv = [SelectCase {
        op: SelectOp::Recv { channel },
        index: 0,
    }];
    assert_eq!(
        world.select(receiver, &recv, false).unwrap(),
        SelectOutcome::Parked
    );
    let send = [SelectCase {
        op: SelectOp::Send {
            channel,
            payload: 13,
        },
        index: 0,
    }];
    assert_eq!(
        world.select(sender, &send, false).unwrap(),
        SelectOutcome::Case(0)
    );
    assert_eq!(world.wait.take_delivered(sender), Some(WaitResult::Sent));
    assert_eq!(
        world.wait.take_delivered(receiver),
        Some(WaitResult::Recv(13))
    );
    assert_eq!(
        SelectTxn::from_cold(world.controls.get(receiver).unwrap().1.select_scratch).winner(),
        SelectTxn::encode_case(0)
    );
    consume_woken(&mut world, receiver);
}

#[test]
fn select_does_not_rendezvous_with_itself_or_consume_through_loser() {
    let mut world = booted();
    let channel = world.channel_new(0).unwrap();
    let waiter = running(&mut world);
    let cases = [
        SelectCase {
            op: SelectOp::Send {
                channel,
                payload: 3,
            },
            index: 0,
        },
        SelectCase {
            op: SelectOp::Recv { channel },
            index: 1,
        },
    ];
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Parked
    );
    world.channel_try_send(channel, 9).unwrap().unwrap();
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(9)));
    assert_eq!(
        SelectTxn::from_cold(world.controls.get(waiter).unwrap().1.select_scratch).winner(),
        SelectTxn::encode_case(1)
    );
    assert_eq!(
        world.channel_try_recv(channel).unwrap(),
        Err(TryRecvErr::Empty)
    );
    consume_woken(&mut world, waiter);

    let other = world.channel_new(0).unwrap();
    let cases = [
        SelectCase {
            op: SelectOp::Recv { channel },
            index: 0,
        },
        SelectCase {
            op: SelectOp::Recv { channel: other },
            index: 1,
        },
    ];
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Parked
    );
    // 留在“已提交、尚未通知”窗口，让另一源真实尝试认领 loser。
    let wake = world
        .channels
        .try_send(&mut world.wait, &mut world.controls, channel, 11)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        world.channel_try_send(other, 12).unwrap(),
        Err(TrySendErr::Full)
    );
    world.wake_node(wake).unwrap();
    assert_eq!(
        world.wait.take_delivered(waiter),
        Some(WaitResult::Recv(11))
    );
    consume_woken(&mut world, waiter);
}

#[test]
fn select_recv_refills_buffer_from_blocked_sender() {
    let mut world = booted();
    let channel = world.channel_new(1).unwrap();
    world.channel_try_send(channel, 5).unwrap().unwrap();
    let sender = running(&mut world);
    assert!(matches!(
        world.channel_send(channel, sender, 7, false).unwrap(),
        SendOutcome::Parked(_)
    ));
    let receiver = running(&mut world);
    assert_eq!(
        world
            .select(
                receiver,
                &[SelectCase {
                    op: SelectOp::Recv { channel },
                    index: 0
                }],
                false
            )
            .unwrap(),
        SelectOutcome::Case(0)
    );
    assert_eq!(
        world.wait.take_delivered(receiver),
        Some(WaitResult::Recv(5))
    );
    assert_eq!(world.wait.take_delivered(sender), Some(WaitResult::Sent));
    assert_eq!(world.channel_try_recv(channel).unwrap(), Ok(7));
    consume_woken(&mut world, sender);
}

#[test]
fn select_preserves_close_results_and_unlocks_error_paths() {
    let mut world = booted();
    let channel = world.channel_new(1).unwrap();
    world.channel_try_send(channel, 0).unwrap().unwrap();
    world.channel_close(channel).unwrap();
    let waiter = running(&mut world);
    assert!(
        world
            .select(
                waiter,
                &[SelectCase {
                    op: SelectOp::Send {
                        channel,
                        payload: 8
                    },
                    index: 0
                }],
                false
            )
            .is_err()
    );
    assert_eq!(world.channel_try_recv(channel).unwrap(), Ok(0));
    assert_eq!(
        world
            .select(
                waiter,
                &[SelectCase {
                    op: SelectOp::Recv { channel },
                    index: 0
                }],
                false
            )
            .unwrap(),
        SelectOutcome::Case(0)
    );
    assert_eq!(
        world.wait.take_delivered(waiter),
        Some(WaitResult::RecvClosed)
    );
    let open = world.channel_new(0).unwrap();
    assert_eq!(
        world
            .select(
                waiter,
                &[SelectCase {
                    op: SelectOp::Recv { channel: open },
                    index: 0
                }],
                false
            )
            .unwrap(),
        SelectOutcome::Parked
    );
    world.channel_close(open).unwrap();
    assert_eq!(
        world.wait.take_delivered(waiter),
        Some(WaitResult::RecvClosed)
    );
    assert_eq!(
        SelectTxn::from_cold(world.controls.get(waiter).unwrap().1.select_scratch).winner(),
        SelectTxn::encode_case(0)
    );
    consume_woken(&mut world, waiter);
}

#[test]
fn select_join_preserves_completion_variants_before_and_after_park() {
    for value in [
        CompletionValue::Bits(42),
        CompletionValue::Managed {
            handle: 11,
            descriptor: 22,
        },
        CompletionValue::Panic {
            handle: 33,
            descriptor: 44,
        },
    ] {
        for parked in [false, true] {
            let mut world = booted();
            let child = running(&mut world);
            let waiter = running(&mut world);
            let cases = [SelectCase {
                op: SelectOp::Wait { join: child },
                index: 0,
            }];
            if parked {
                assert_eq!(
                    world.select(waiter, &cases, false).unwrap(),
                    SelectOutcome::Parked
                );
            }
            world.coroutine_finished(child, 0, value).unwrap();
            if parked {
                consume_woken(&mut world, waiter);
            } else {
                assert_eq!(
                    world.select(waiter, &cases, false).unwrap(),
                    SelectOutcome::Case(0)
                );
            }
            assert_eq!(
                world.wait.take_delivered(waiter),
                Some(WaitResult::Join(value))
            );
            assert_eq!(
                SelectTxn::from_cold(world.controls.get(waiter).unwrap().1.select_scratch).winner(),
                SelectTxn::encode_case(0)
            );
        }
    }
}

#[test]
fn losing_join_does_not_observe_later_panic() {
    let mut world = booted();
    let child = running(&mut world);
    let waiter = running(&mut world);
    let channel = world.channel_new(0).unwrap();
    let cases = [
        SelectCase {
            op: SelectOp::Recv { channel },
            index: 0,
        },
        SelectCase {
            op: SelectOp::Wait { join: child },
            index: 1,
        },
    ];
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Parked
    );
    world.channel_try_send(channel, 5).unwrap().unwrap();
    world
        .coroutine_finished(
            child,
            0,
            CompletionValue::Panic {
                handle: 33,
                descriptor: 44,
            },
        )
        .unwrap();
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(5)));
    assert_eq!(
        world
            .controls
            .get(child)
            .unwrap()
            .1
            .join_state
            .status
            .load(std::sync::atomic::Ordering::Acquire)
            & 4,
        0,
        "loser 不能确认 panic"
    );
    assert_eq!(
        world.read_completion(child).unwrap(),
        CompletionValue::Panic {
            handle: 33,
            descriptor: 44
        }
    );
    assert_eq!(
        world
            .controls
            .get(child)
            .unwrap()
            .1
            .join_state
            .status
            .load(std::sync::atomic::Ordering::Acquire)
            & 4,
        4
    );
}

#[test]
fn select_clears_previous_result_and_rejects_unencodable_case() {
    let mut world = booted();
    let channel = world.channel_new(1).unwrap();
    let waiter = running(&mut world);
    world.channel_try_send(channel, 5).unwrap().unwrap();
    assert!(
        world
            .select(
                waiter,
                &[SelectCase {
                    op: SelectOp::Recv { channel },
                    index: u32::MAX
                }],
                false
            )
            .is_err()
    );
    let cases = [SelectCase {
        op: SelectOp::Recv { channel },
        index: 0,
    }];
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Case(0)
    );
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(5)));
    assert_eq!(
        world.select(waiter, &cases, false).unwrap(),
        SelectOutcome::Parked
    );
    assert_eq!(world.wait.take_delivered(waiter), None);
    world.channel_try_send(channel, 6).unwrap().unwrap();
    assert_eq!(world.wait.take_delivered(waiter), Some(WaitResult::Recv(6)));
    consume_woken(&mut world, waiter);
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
