//! gc_enter 使用现行 CallReturn 落栈契约；验证系统栈切换与原协程恢复。
use super::*;
use crate::runtime::{collect_layout as layout, entry, processor};

const PROCESSOR: u64 = 1024;
const STATE: u64 = 5504;
const COORDINATOR: u64 = 6656;
const SYSTEM_TOP: u64 = 12288;
const MUTATOR_SLOT: u64 = 128;
const SYSTEM_SLOT: u64 = 384;
const ROOT_SLOT: u64 = STACK + 24;

fn dispatcher(machine: &mut Machine, symbol: &Symbol) {
    assert_eq!(*symbol, crate::x64::mangle::runtime_symbol("gc_dispatch"));
    assert_eq!(machine.get(Rdi), REQUEST);
    assert_eq!(machine.get(Rsi), STACK);
    assert_eq!(machine.get(Rbx), STACK);
    assert_eq!(machine.get(Rsp), SYSTEM_TOP);
    assert_eq!(machine.get(Rsp) % 16, 0);
    assert_eq!(machine.get(R14), SYSTEM_SLOT);
    assert_eq!(machine.get(R15), PROCESSOR);
    assert_eq!(
        machine.word(COORDINATOR + layout::COORDINATOR_ENTRY_RSP_OFFSET as u64),
        STACK
    );
    assert_eq!(
        machine.word(COORDINATOR + layout::COORDINATOR_SAVED_RSP_OFFSET as u64),
        STACK
    );
    assert_eq!(
        machine.word(COORDINATOR + layout::COORDINATOR_SAVED_R14_OFFSET as u64),
        MUTATOR_SLOT
    );
    assert_eq!(machine.word(STACK), 0xcafe, "入口返回 PC 必须留在原栈");
    assert_eq!(machine.word(ROOT_SLOT), 0x10000);
    machine.put(ROOT_SLOT, 0x20000); // 模拟收集器通过栈图修正已经落栈的根。
    for r in [Rax, Rbx, Rcx, Rdx, Rsi, Rdi, R8, R9, R10, R11] {
        machine.set_reg(r, 0xbad);
    }
    machine.set_reg(Rax, machine.word(REQUEST));
    machine.calls.push(("gc_dispatch".into(), vec![]));
}

#[test]
fn poll_tail_entry_preserves_return_address_and_rejects_reentry() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let fragments = crate::x64::fragments::registered_fragments(target);
        let poll = fragments
            .iter()
            .find(|blob| {
                blob.entries
                    .iter()
                    .any(|entry| entry.glue == "safepoint_slow")
            })
            .unwrap();
        assert_eq!(poll.relocations.len(), 2);
        assert_eq!(
            poll.relocations[0].target,
            RelocTarget::Lir(mangle::runtime_symbol("callback_poll"))
        );
        assert_eq!(
            poll.relocations[1].target,
            RelocTarget::Lir(mangle::runtime_symbol("gc_enter"))
        );
    }
    for (pressure, ready, used, expected) in [
        (1, 1, 101, Stop::Worker),
        (1, 0, 101, Stop::Return),
        (1, 2, 101, Stop::Return),
        (2, 1, 101, Stop::Return),
        (1, 1, 100, Stop::Return),
    ] {
        let mut machine = Machine::new(&[]);
        machine.set_reg(R15, PROCESSOR);
        machine.put(PROCESSOR, 3);
        machine.put(PROCESSOR + 3888, pressure);
        machine.put(PROCESSOR + 3912, ready);
        machine.put(PROCESSOR + 3928, used);
        machine.put(PROCESSOR + 3952, 100);
        machine.put(PROCESSOR + 3616, 99);
        machine.put(STACK, 0xcafe);
        assert_eq!(
            machine.execute(crate::x64::fragments::safepoint_slow_sequence()),
            expected
        );
        assert_eq!(machine.get(Rsp), STACK);
        assert_eq!(machine.word(STACK), 0xcafe);
        assert_eq!(machine.word(PROCESSOR), 2);
        assert_eq!(
            machine.word(PROCESSOR + 3616),
            if expected == Stop::Worker { 1 } else { 99 }
        );
    }
}

#[test]
fn callback_poll_tail_keeps_precise_parent_pc() {
    let mut machine = Machine::new(&[]);
    machine.set_reg(R15, PROCESSOR);
    machine.put(PROCESSOR, 5);
    machine.put(STACK, 0xcafe);
    machine.put(PROCESSOR + 3616, 99);
    assert_eq!(
        machine.execute(crate::x64::fragments::safepoint_slow_sequence()),
        Stop::Worker
    );
    assert_eq!(machine.get(Rsp), STACK);
    assert_eq!(machine.word(STACK), 0xcafe);
    assert_eq!(machine.word(PROCESSOR), 5);
    assert_eq!(machine.word(PROCESSOR + 3616), 99);
}

#[test]
fn gc_entry_switches_stack_and_restores_mutator_after_root_rewrite() {
    for status in [0, 9] {
        let mut machine = Machine::new(&[]);
        machine.managed = Some(dispatcher);
        machine.set_reg(R14, MUTATOR_SLOT);
        machine.set_reg(R15, PROCESSOR);
        machine.put(PROCESSOR + processor::PROCESSOR_STATE_OFFSET as u64, STATE);
        machine.put(STATE + entry::state::COORDINATOR, COORDINATOR);
        machine.put(
            COORDINATOR + layout::COORDINATOR_SYSTEM_TOP_OFFSET as u64,
            SYSTEM_TOP,
        );
        machine.put(
            COORDINATOR + layout::COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET as u64,
            SYSTEM_SLOT,
        );
        machine.put(STACK, 0xcafe);
        machine.put(ROOT_SLOT, 0x10000);
        machine.put(REQUEST, status);
        assert_eq!(
            machine.execute(crate::x64::fragments::gc_enter_sequence()),
            Stop::Return
        );
        assert_eq!(machine.calls.len(), 1);
        assert_eq!(machine.get(Rax), status);
        assert_eq!(machine.get(Rsp), STACK);
        assert_eq!(machine.get(R14), MUTATOR_SLOT);
        assert_eq!(machine.get(R15), PROCESSOR);
        assert_eq!(machine.word(STACK), 0xcafe);
        assert_eq!(machine.word(ROOT_SLOT), 0x20000);
    }
}
