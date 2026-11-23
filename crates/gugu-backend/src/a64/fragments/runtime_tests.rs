use std::mem::offset_of;

use super::*;
use crate::a64::{foreign::Platform, macho::import_stub_symbol};
use crate::runtime::collect_layout::{
    COORDINATOR_ENTRY_RSP_OFFSET, COORDINATOR_SAVED_R14_OFFSET, COORDINATOR_SAVED_RSP_OFFSET,
    COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET, COORDINATOR_SYSTEM_TOP_OFFSET,
};
use crate::runtime::coroutine::A64MorestackMailbox as MorestackMailbox;
use crate::runtime::coroutine_pack::{coroutine_pack_float_offset, coroutine_pack_integer_offset};
use crate::runtime::entry::state;
use crate::runtime::processor::{PROCESSOR_STATE_OFFSET, PROCESSOR_SYSTEM_SLOT_OFFSET};

const X9: Reg = Reg::x(9);
const X10: Reg = Reg::x(10);

fn branch(fragment: &Fragment, kind: RelocKind, name: &str, addend: i64) {
    assert_eq!(fragment.relocations.len(), 1);
    let fixup = &fragment.relocations[0];
    assert_eq!(fixup.relocation.kind, kind);
    assert_eq!(fixup.relocation.target, name);
    assert_eq!(fixup.relocation.addend, addend);
}

fn syscall(code: &[Inst], at: usize, number: u16) {
    assert_eq!(form(&code[at]), "movz.X");
    assert_eq!(code[at].operands, [reg(Reg::x(8)), movz(number)]);
    assert_eq!(form(&code[at + 1]), "svc");
}

#[test]
fn context_restore_tail_branches_into_switch_restore_entry() {
    let fragment = context_restore("switch", 0x1234);
    branch(&fragment, RelocKind::Branch26, "switch", 0x1234);
    let code = instructions(&fragment);
    assert_eq!(code.len(), 1);
    assert_eq!(form(&code[0]), "b");
}

#[test]
fn current_slot_returns_the_hot_register() {
    let fragment = current_slot();
    assert!(fragment.relocations.is_empty());
    let code = instructions(&fragment);
    assert_eq!(code.len(), 2);
    assert_eq!(form(&code[0]), "add.X.imm");
    assert_eq!(
        code[0].operands,
        [reg(Reg::x(0)), reg(Reg::x(27)), add_imm(0)]
    );
    assert_eq!(form(&code[1]), "ret");
}

#[test]
fn process_exit_syscalls_on_linux_and_tail_calls_on_darwin() {
    let linux = process_exit(Platform::Linux);
    assert!(linux.relocations.is_empty());
    let code = instructions(&linux);
    syscall(&code, 0, 94);
    assert_eq!(form(&code[2]), "brk");

    let darwin = process_exit(Platform::Darwin);
    branch(&darwin, RelocKind::Branch26, &import_stub_symbol("exit"), 0);
    assert_eq!(form(&instructions(&darwin)[0]), "b");
}

#[test]
fn report_write_syscalls_on_linux_and_tail_calls_on_darwin() {
    let linux = report_write(Platform::Linux);
    assert!(linux.relocations.is_empty());
    let code = instructions(&linux);
    syscall(&code, 0, 64);
    assert_eq!(form(&code[2]), "ret");

    let darwin = report_write(Platform::Darwin);
    branch(
        &darwin,
        RelocKind::Branch26,
        &import_stub_symbol("write"),
        0,
    );
}

/// 校验 restore_system 尾巴：登记 reason=2 后经 switch + restore_offset 恢复系统上下文。
fn assert_restore_tail(code: &[Inst], from: usize, fragment: &Fragment, switch: &str) {
    assert_eq!(code[from].operands, [reg(X10), movz(2)],);
    assert!(memory(
        &code[from + 1..from + 4],
        "str.X.Unsigned",
        X10,
        X9,
        state::REASON as usize
    ));
    assert!(memory(
        &code[from..from + 4],
        "ldr.X.Unsigned",
        Reg::x(0),
        X9,
        offset_of!(MorestackMailbox, system_context)
    ));
    assert!(memory(
        &code[from + 1..from + 4],
        "ldr.X.Unsigned",
        Reg::x(1),
        PROCESSOR,
        PROCESSOR_SYSTEM_SLOT_OFFSET
    ));
    assert_eq!(form(&code[from + 4]), "add.X.imm");
    assert_eq!(
        code[from + 4].operands,
        [reg(Reg::x(2)), reg(PROCESSOR), add_imm(0)]
    );
    assert_eq!(form(&code[from + 5]), "b");
    let fixup = fragment.relocations.last().unwrap();
    assert_eq!(fixup.offset as usize / 4, from + 5);
    assert_eq!(fixup.relocation.target, switch);
}

#[test]
fn main_return_records_value_and_restores_system_context() {
    let fragment = main_return("switch", 0x40);
    branch(&fragment, RelocKind::Branch26, "switch", 0x40);
    let code = instructions(&fragment);
    assert!(memory(
        &code[0..1],
        "ldr.X.Unsigned",
        X9,
        PROCESSOR,
        PROCESSOR_STATE_OFFSET
    ));
    assert!(memory(
        &code[1..3],
        "str.X.Unsigned",
        Reg::x(0),
        X9,
        offset_of!(MorestackMailbox, return_value)
    ));
    assert!(memory(
        &code[1..3],
        "str.X.Unsigned",
        PROCESSOR,
        X9,
        offset_of!(MorestackMailbox, processor)
    ));
    assert_restore_tail(&code, 3, &fragment, "switch");
}

#[test]
fn coroutine_enter_loads_pack_lanes_calls_code_and_restores() {
    let fragment = coroutine_enter("switch", 0x40);
    branch(&fragment, RelocKind::Branch26, "switch", 0x40);
    let code = instructions(&fragment);
    assert!(memory(&code[0..1], "ldr.X.Unsigned", X9, SP, 0));
    assert!(memory(&code[1..2], "ldr.X.Unsigned", X10, SP, 8));
    for lane in 0..8 {
        assert!(
            code[2..10].iter().any(|instruction| {
                form(instruction) == "ldr.Q.Unscaled"
                    && instruction.operands
                        == [
                            reg(Reg::v(lane, Width::Q)),
                            mem(
                                X10,
                                AddressMode::Unscaled(
                                    coroutine_pack_float_offset(lane as usize) as i64
                                ),
                            ),
                        ]
            })
        );
    }
    for lane in 0..9 {
        assert!(memory(
            &code[10..19],
            "ldr.X.Unsigned",
            Reg::x(lane),
            X10,
            coroutine_pack_integer_offset(lane as usize)
        ));
    }
    assert_eq!(form(&code[19]), "blr");
    assert!(memory(
        &code[20..21],
        "ldr.X.Unsigned",
        X9,
        PROCESSOR,
        PROCESSOR_STATE_OFFSET
    ));
    assert!(memory(
        &code[21..23],
        "str.X.Unsigned",
        Reg::x(0),
        X9,
        offset_of!(MorestackMailbox, return_value)
    ));
    assert!(memory(
        &code[21..23],
        "str.X.Unsigned",
        PROCESSOR,
        X9,
        offset_of!(MorestackMailbox, processor)
    ));
    assert!(memory(
        &code[23..25],
        "str.X.Unsigned",
        X10,
        X9,
        offset_of!(MorestackMailbox, candidate)
    ));
    assert_restore_tail(&code, 25, &fragment, "switch");
}

#[test]
fn gc_enter_swaps_to_system_stack_calls_dispatch_and_restores() {
    let fragment = gc_enter("dispatch");
    branch(&fragment, RelocKind::Call26, "dispatch", 0);
    let code = instructions(&fragment);
    // 前缀：16 字节槽高 8 字节压调用方返回地址，entry_rsp=SP+8 与 x64 `call` 落栈契约一致。
    assert_eq!(form(&code[0]), "sub.X.imm");
    assert_eq!(code[0].operands, [reg(SP), reg(SP), add_imm(16)]);
    assert!(memory(&code[1..2], "str.X.Unsigned", LR, SP, 8));
    // 前段：coordinator → SAVED_R14=HOT，HOT=SYSTEM_DESCRIPTOR，ENTRY_RSP/SAVED_RSP=SP，SP=SYSTEM_TOP。
    assert!(memory(
        &code[3..5],
        "ldr.X.Unsigned",
        X9,
        X9,
        state::COORDINATOR as usize
    ));
    assert!(memory(
        &code[4..6],
        "str.X.Unsigned",
        Reg::x(27),
        X9,
        COORDINATOR_SAVED_R14_OFFSET as usize
    ));
    assert!(memory(
        &code[5..7],
        "ldr.X.Unsigned",
        Reg::x(27),
        X9,
        COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET as usize
    ));
    assert!(memory(
        &code[7..10],
        "str.X.Unsigned",
        X10,
        X9,
        COORDINATOR_ENTRY_RSP_OFFSET as usize
    ));
    assert!(memory(
        &code[7..10],
        "str.X.Unsigned",
        X10,
        X9,
        COORDINATOR_SAVED_RSP_OFFSET as usize
    ));
    assert!(memory(
        &code[9..11],
        "ldr.X.Unsigned",
        X10,
        X9,
        COORDINATOR_SYSTEM_TOP_OFFSET as usize
    ));
    // call site：bl 之后恢复 HOT/SP、弹回 LR 并返回；LR 以 temp=entry_rsp 为基址，
    // SP 一步归位到 entry_rsp+8，全程保持 16 字节对齐。
    let call = fragment.relocations[0].offset as usize / 4;
    assert_eq!(form(&code[call]), "bl");
    let tail = &code[code.len() - 3..];
    assert!(memory(&tail[..1], "ldr.X.Unsigned", LR, X10, 0));
    assert_eq!(form(&tail[1]), "add.X.imm");
    assert_eq!(tail[1].operands, [reg(SP), reg(X10), add_imm(8)]);
    assert_eq!(form(&tail[2]), "ret");
}

#[test]
fn gc_config_materializes_the_record_address() {
    let fragment = gc_config("record");
    assert_eq!(fragment.relocations.len(), 2);
    assert_eq!(
        fragment.relocations[0].relocation.kind,
        RelocKind::AdrpPage21
    );
    assert_eq!(fragment.relocations[1].relocation.kind, RelocKind::AddLo12);
    for fixup in &fragment.relocations {
        assert_eq!(fixup.relocation.target, "record");
        assert_eq!(fixup.relocation.addend, 0);
    }
    let code = instructions(&fragment);
    assert_eq!(form(&code[0]), "adrp");
    assert_eq!(form(&code[1]), "add.X.imm");
    assert_eq!(form(&code[2]), "ret");
}

#[test]
fn runtime_poll_defer_is_a_bare_return() {
    let fragment = runtime_poll_defer();
    assert!(fragment.relocations.is_empty());
    let code = instructions(&fragment);
    assert_eq!(code.len(), 1);
    assert_eq!(form(&code[0]), "ret");
}

#[test]
fn thread_id_syscalls_on_linux_and_calls_pthread_on_darwin() {
    let linux = thread_id(Platform::Linux);
    assert!(linux.relocations.is_empty());
    let code = instructions(&linux);
    syscall(&code, 0, 178);
    assert_eq!(form(&code[2]), "ret");

    let darwin = thread_id(Platform::Darwin);
    branch(
        &darwin,
        RelocKind::Call26,
        &import_stub_symbol("pthread_threadid_np"),
        0,
    );
    let code = instructions(&darwin);
    assert_eq!(form(&code[0]), "sub.X.imm");
    assert_eq!(code[0].operands, [reg(SP), reg(SP), add_imm(16)]);
    assert!(memory(&code[1..2], "str.X.Unsigned", LR, SP, 8));
    assert_eq!(code[2].operands, [reg(Reg::x(0)), movz(0)]);
    assert_eq!(form(&code[4]), "bl");
    assert!(memory(&code[5..6], "ldr.X.Unsigned", LR, SP, 8));
    assert!(memory(&code[6..7], "ldr.X.Unsigned", Reg::x(0), SP, 0));
    assert_eq!(form(&code[7]), "add.X.imm");
    assert_eq!(form(&code[8]), "ret");
}
