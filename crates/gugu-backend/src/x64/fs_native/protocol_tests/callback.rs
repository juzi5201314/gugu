//! 直接解释并装配回调队列生产序列，不启动线程。
use super::*;
use crate::runtime::callback as q;
const CONTROL: u64 = 64;
const FIRST: u64 = 8192;
const SECOND: u64 = 8704;

fn operation(machine: &mut Machine, action: i32, node: u64) -> u64 {
    if action == q::SUBMIT && node >= FIRST && machine.word(node + q::THREAD_ID as u64) == 0 {
        machine.put(node + q::THREAD_ID as u64, node);
    }
    machine.set_reg(Rax, CONTROL);
    machine.set_reg(Rbx, if action == q::OPEN { 1024 } else { node });
    machine.set_reg(Rcx, action as u64);
    let sequence = super::super::callback::queue().finish().sequence;
    assert_eq!(machine.run(&sequence), Stop::Return);
    assert_eq!(machine.get(Rsp), STACK);
    assert_eq!(machine.get(R12), 0x1234);
    machine.get(Rax)
}

#[test]
fn callback_queue_closes_admission_but_drains_registered_threads_once() {
    let mut machine = Machine::new(&[]);
    assert_eq!(operation(&mut machine, q::SUBMIT, FIRST), q::CLOSED as u64);
    assert_eq!(operation(&mut machine, q::OPEN, 0), 0);
    machine.put(1024, 1);
    for node in [FIRST, SECOND] {
        assert_eq!(operation(&mut machine, q::SUBMIT, node), 0);
        assert_eq!(operation(&mut machine, q::SUBMIT, node), q::INVALID as u64);
    }
    assert_eq!(machine.word(1024), 1 | u64::from(q::POLL_BIT));
    assert_eq!(machine.word(CONTROL + q::REGISTERED as u64), 2);
    assert_eq!(operation(&mut machine, q::FINISH, FIRST), q::INVALID as u64);
    assert_eq!(operation(&mut machine, q::CLOSE, 0), 0);
    assert_eq!(operation(&mut machine, q::SUBMIT, 768), q::CLOSED as u64);
    assert_eq!(operation(&mut machine, q::OPEN, 0), q::INVALID as u64);
    for node in [FIRST, SECOND] {
        assert_eq!(operation(&mut machine, q::TAKE, 0), node);
        assert_eq!(operation(&mut machine, q::DETACH, node), q::INVALID as u64);
        assert_eq!(operation(&mut machine, q::FINISH, node), 0);
        assert_eq!(operation(&mut machine, q::FINISH, node), q::INVALID as u64);
        assert_eq!(operation(&mut machine, q::DETACH, node), 0);
        assert_eq!(operation(&mut machine, q::DETACH, node), q::INVALID as u64);
    }
    for offset in [q::LOCK, q::ACCEPTING, q::HEAD, q::TAIL, q::REGISTERED] {
        assert_eq!(machine.word(CONTROL + offset as u64), 0);
    }
    assert_eq!(operation(&mut machine, q::TAKE, 0), 0);
    assert!(machine.calls.is_empty());
    encode::assemble(&super::super::callback::queue().finish().sequence).unwrap();
}

#[test]
fn callback_threads_deduplicate_nested_entries_and_reject_dirty_workers() {
    let mut machine = Machine::new(&[]);
    assert_eq!(operation(&mut machine, q::OPEN, 0), 0);
    machine.put(CONTROL + q::MANAGED_THREAD as u64, 7);
    machine.put(1024 + 4024, 10240);
    machine.put(10240 + 64 + 104, 128);
    machine.put(10240 + 64, 11);
    machine.put(10240 + 2048 + 64 + 104, 256);
    machine.put(10240 + 2048 + 64, 13);
    for tid in [7, 13] {
        machine.put(FIRST + q::THREAD_ID as u64, tid);
        let before = machine.memory.clone();
        assert_eq!(operation(&mut machine, q::SUBMIT, FIRST), q::INVALID as u64);
        assert_eq!(machine.memory, before);
    }
    for tid in [11, 17] {
        for node in [FIRST, SECOND] {
            machine.put(node + q::THREAD_ID as u64, tid);
            assert_eq!(operation(&mut machine, q::SUBMIT, node), 0);
            assert_eq!(
                machine.word(node + q::BORROW_CREDIT as u64),
                u64::from(tid == 11)
            );
            assert_eq!(operation(&mut machine, q::TAKE, 0), node);
        }
        let external = u64::from(tid == 17);
        assert_eq!(machine.word(CONTROL + q::EXTERNAL_THREADS as u64), external);
        assert_eq!(operation(&mut machine, q::FINISH, FIRST), 0);
        assert_eq!(operation(&mut machine, q::DETACH, FIRST), 0);
        assert_eq!(machine.word(CONTROL + q::EXTERNAL_THREADS as u64), external);
        assert_eq!(operation(&mut machine, q::FINISH, SECOND), 0);
        assert_eq!(operation(&mut machine, q::DETACH, SECOND), 0);
        for offset in [q::REGISTRY, q::ACTIVE, q::REGISTERED, q::EXTERNAL_THREADS] {
            assert_eq!(machine.word(CONTROL + offset as u64), 0);
        }
    }
}

#[test]
fn callback_capacity_rejection_keeps_packet_unregistered() {
    let mut machine = Machine::new(&[]);
    assert_eq!(operation(&mut machine, q::OPEN, 0), 0);
    machine.put(CONTROL + q::REGISTERED as u64, q::MAX_REGISTERED as u64);
    machine.put(FIRST + q::THREAD_ID as u64, 91);
    let before = machine.memory.clone();
    assert_eq!(operation(&mut machine, q::SUBMIT, FIRST), q::INVALID as u64);
    assert_eq!(machine.memory, before);
}

#[test]
fn callback_queue_contention_preserves_nodes_and_owner_lock() {
    let mut machine = Machine::new(&[]);
    assert_eq!(operation(&mut machine, q::OPEN, 0), 0);
    machine.put(CONTROL + q::LOCK as u64, 1);
    machine.put(FIRST + q::THREAD_ID as u64, FIRST);
    let before = machine.memory.clone();
    for action in q::OPEN..=q::CLOSE {
        assert_eq!(operation(&mut machine, action, FIRST), q::BUSY as u64);
        assert_eq!(machine.memory, before);
    }
    machine.put(CONTROL + q::LOCK as u64, 0);
    assert_eq!(operation(&mut machine, q::SUBMIT, FIRST), 0);
    assert_eq!(operation(&mut machine, q::TAKE, 0), FIRST);
    assert_eq!(machine.word(CONTROL + q::REGISTERED as u64), 1);
}
