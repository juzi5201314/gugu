use std::mem::offset_of;

use crate::a64::{decode, inst::RelocKind};
use crate::gir::body::MemoryOrdering::*;
use crate::lir::body::AtomicOp::*;
use crate::runtime::{entry::Rt0Payload, processor};

use super::*;
use AtomicWidth::*;

#[path = "context_vm_tests.rs"]
mod context_vm;

fn instructions(fragment: &Fragment) -> Vec<Inst> {
    let (words, remainder) = fragment.bytes.as_chunks::<4>();
    assert!(remainder.is_empty());
    words
        .iter()
        .map(|bytes| {
            let word = u32::from_le_bytes(*bytes);
            let instruction = decode::decode(word).unwrap();
            assert_eq!(encode::encode(&instruction).unwrap().word, word);
            instruction
        })
        .collect()
}

fn at(code: &[Inst], offset: u32) -> &Inst {
    &code[offset as usize / 4]
}

fn form(instruction: &Inst) -> &str {
    table::get(instruction.form).unwrap().key
}

fn memory(code: &[Inst], kind: &str, value: Reg, base: Reg, offset: usize) -> bool {
    let operands = [reg(value), mem(base, AddressMode::Unsigned(offset as i64))];
    code.iter()
        .any(|inst| form(inst) == kind && inst.operands == operands)
}

fn target(code: &[Inst], offset: u32) -> u32 {
    let displacement = at(code, offset)
        .operands
        .iter()
        .find_map(|op| match op {
            Operand::Target(Target::Offset(displacement)) => Some(*displacement),
            _ => None,
        })
        .unwrap();
    u32::try_from(i64::from(offset) + displacement).unwrap()
}

#[test]
fn darwin_fd_dispatch_uses_native_packet_and_arm64_libsystem_abi() {
    use crate::{NativeImport, macho::LIB_SYSTEM, runtime::foreign_schema::*};

    let names = [
        "read",
        "write",
        "open",
        "close",
        "lseek",
        "stat",
        "lstat",
        "fstat",
        "ftruncate",
        "fsync",
        "fcntl",
        "opendir",
        "readdir",
        "closedir",
        "mkdir",
        "unlink",
        "rmdir",
        "rename",
        "link",
        "symlink",
        "readlink",
        "realpath",
        "__error",
    ];
    assert_eq!(
        darwin_fd_imports(),
        names.map(|name| NativeImport::function(LIB_SYSTEM, name)),
    );
    let fragment = darwin_fd_dispatch();
    let code = instructions(&fragment);
    let x0 = Reg::x(0);
    let request = Reg::x(19);
    assert_eq!(code[0].operands, [reg(SP), reg(SP), add_imm(32)]);
    assert!(memory(
        &code,
        "ldr.X.Unsigned",
        Reg::x(9),
        request,
        NATIVE_REQUEST_OPERATION
    ));
    assert!(memory(
        &code,
        "str.X.Unsigned",
        x0,
        request,
        NATIVE_REQUEST_VALUE
    ));
    assert!(memory(
        &code,
        "str.X.Unsigned",
        x0,
        request,
        NATIVE_REQUEST_ERROR
    ));
    assert_eq!(fragment.relocations.len(), names.len() + 1);
    let errno = fragment.relocations.last().unwrap().offset;
    assert!(memory(
        &code[(errno / 4) as usize..],
        "ldr.W.Unsigned",
        Reg::w(0),
        x0,
        0
    ));
    let clear_call = &fragment.relocations[12];
    assert_eq!(clear_call.relocation.kind, RelocKind::Call26);
    assert_eq!(clear_call.relocation.target, "__gugu_darwin_c___error");
    let clear = at(&code, clear_call.offset + 4);
    assert_eq!(form(clear), "str.W.Unsigned");
    assert_eq!(
        clear.operands,
        [reg(Reg::zr(Width::W)), mem(x0, AddressMode::Unsigned(0))]
    );
    let calls = fragment
        .relocations
        .iter()
        .enumerate()
        .filter_map(|(index, call)| (index != 12).then_some(call));
    for (index, (site, name)) in calls.zip(names).enumerate() {
        assert_eq!(site.relocation.kind, RelocKind::Call26);
        assert_eq!(site.relocation.target, format!("__gugu_darwin_c_{name}"));
        if name == "__error" {
            break;
        }
        let variadic = matches!(name, "open" | "fcntl");
        let fixed = [
            3usize, 3, 2, 1, 3, 2, 2, 2, 2, 1, 2, 1, 1, 1, 2, 1, 1, 2, 2, 2, 3, 2,
        ][index];
        let end = (site.offset / 4) as usize;
        let args_start = end - fixed - if variadic { 2 } else { 0 };
        let args = &code[args_start..end];
        let dispatch = args_start - if name == "readdir" { 4 } else { 2 };
        assert_eq!(form(&code[dispatch]), "subs.X.imm");
        assert_eq!(code[dispatch].operands[2], add_imm(index));
        for i in 0..fixed {
            assert!(memory(
                args,
                "ldr.X.Unsigned",
                Reg::x(i as u8),
                request,
                NATIVE_REQUEST_ARGUMENTS + i * 8
            ));
        }
        if variadic {
            let mode = Reg::w(10);
            assert!(!memory(
                args,
                "ldr.X.Unsigned",
                Reg::x(2),
                request,
                NATIVE_REQUEST_ARGUMENTS + 16
            ));
            assert!(memory(
                args,
                "ldr.W.Unsigned",
                mode,
                request,
                NATIVE_REQUEST_ARGUMENTS + 16
            ));
            assert!(memory(args, "str.W.Unsigned", mode, SP, 0));
        }
        let wide = matches!(name, "read" | "write" | "lseek" | "readlink");
        if matches!(name, "opendir" | "readdir" | "realpath") {
            let readdir = name == "readdir";
            let compare = end + if readdir { 2 } else { 1 };
            assert_eq!(form(&code[compare]), "subs.X.imm");
            assert_eq!(
                code[compare].operands,
                [reg(Reg::zr(Width::X)), reg(x0), add_imm(0)]
            );
            assert_eq!(target(&code, (compare as u32 + 1) * 4), errno);
            let store = if readdir { end + 1 } else { end + 3 };
            assert!(memory(
                &code[store..=store],
                "str.X.Unsigned",
                x0,
                request,
                NATIVE_REQUEST_VALUE
            ));
            continue;
        }
        assert_eq!(
            form(&code[end + 1]),
            if wide { "adds.X.imm" } else { "sbfm.X" }
        );
        assert_eq!(
            target(&code, site.offset + if wide { 8 } else { 12 }),
            errno
        );
    }
}

#[test]
fn context_saves_machine_state_and_restore_entry_joins_the_shared_body() {
    let context = context_switch();
    assert!(context.fragment.relocations.is_empty());
    let code = instructions(&context.fragment);
    let body_offset = target(&code, context.restore_offset + 12);
    let (saved, rest) = code.split_at(body_offset as usize / 4);
    let restored = &rest[..(context.restore_offset - body_offset) as usize / 4];
    for (index, value) in [(0, Reg::x(9)), (1, LR), (10, Reg::x(29))]
        .into_iter()
        .chain((0..8).map(|lane| (2 + usize::from(lane), Reg::x(19 + lane))))
    {
        assert!(memory(saved, "str.X.Unsigned", value, Reg::x(0), index * 8));
        assert!(memory(
            restored,
            "ldr.X.Unsigned",
            value,
            Reg::x(1),
            index * 8
        ));
    }
    for lane in 0..8 {
        let register = Reg::v(8 + lane, Width::D);
        let offset = (11 + usize::from(lane)) * 8;
        assert!(memory(saved, "str.D.Unsigned", register, Reg::x(0), offset));
        assert!(memory(
            restored,
            "ldr.D.Unsigned",
            register,
            Reg::x(1),
            offset
        ));
    }
    assert_eq!(restored[0].operands, [reg(HOT), reg(Reg::x(2)), add_imm(0)]);
    assert_eq!(
        restored[1].operands,
        [reg(PROCESSOR), reg(Reg::x(3)), add_imm(0)]
    );
    assert_eq!(
        restored[restored.len() - 2].operands,
        [reg(SP), reg(Reg::x(9)), add_imm(0)]
    );
    assert_eq!(restored.last().unwrap().operands, [reg(LR)]);
}

#[test]
fn morestack_has_a_stackless_entry_and_reloads_scratch_after_switch() {
    let morestack = morestack_or_poll("switch");
    let code = instructions(&morestack.fragment);
    let [call] = morestack.fragment.relocations.as_slice() else {
        panic!("expected context call")
    };
    assert_eq!(call.relocation.kind, RelocKind::Call26);
    assert_eq!(call.relocation.target, "switch");
    assert_eq!(call.offset + 4, morestack.resume_offset);
    let (saved, restored) = code.split_at(morestack.resume_offset as usize / 4);
    let scratch = Reg::x(13);
    for (register, offset) in [(LR, 0), (Reg::x(10), 8)]
        .into_iter()
        .chain((0..9).map(|lane| (Reg::x(lane), 144 + usize::from(lane) * 8)))
    {
        assert!(memory(saved, "str.X.Unsigned", register, scratch, offset));
        assert!(memory(
            restored,
            "ldr.X.Unsigned",
            register,
            scratch,
            offset
        ));
    }
    for lane in 0..8 {
        let register = Reg::v(lane, Width::Q);
        let offset = 16 + usize::from(lane) * 16;
        assert!(memory(saved, "str.Q.Unsigned", register, scratch, offset));
        assert!(memory(
            restored,
            "ldr.Q.Unsigned",
            register,
            scratch,
            offset
        ));
    }
    assert_eq!(form(&saved[5]), "sub.X.shift");
    assert_eq!(
        saved[5].operands,
        [
            reg(Reg::x(14)),
            reg(Reg::x(14)),
            reg(Reg::x(11)),
            unshifted()
        ]
    );
    assert_eq!(
        saved[6].operands,
        [reg(Reg::x(14)), mem(Reg::x(12), AddressMode::Unsigned(24))]
    );
    assert!(memory(
        saved,
        "ldr.X.Unsigned",
        Reg::x(2),
        PROCESSOR,
        processor::PROCESSOR_SYSTEM_SLOT_OFFSET
    ));
    assert!(
        code.iter()
            .flat_map(|inst| &inst.operands)
            .all(|operand| !matches!(
                operand, Operand::Address(address) if address.base == SP
            ))
    );
}

#[test]
fn poll_retries_the_flag_update_and_tail_calls_preserve_the_link_register() {
    let fragment = safepoint_slow("callback", "collect");
    let code = instructions(&fragment);
    assert_eq!(form(&code[1]), "ldaxr.W");
    assert_eq!(form(&code[4]), "stlxr.W");
    assert_eq!(target(&code, 20), 4);
    assert_eq!(form(at(&code, target(&code, 8))), "clrex");
    let [gc, callback] = fragment.relocations.as_slice() else {
        panic!("expected two tail branches")
    };
    for (relocation, name) in [(gc, "collect"), (callback, "callback")] {
        assert_eq!(relocation.relocation.target, name);
        assert_eq!(relocation.relocation.kind, RelocKind::Branch26);
        assert_eq!(form(at(&code, relocation.offset)), "b");
    }
    assert_eq!(target(&code, 8) + 4, callback.offset);
    assert_eq!(form(at(&code, gc.offset + 4)), "ret");
    assert_eq!(
        at(&code, gc.offset - 4).operands,
        [reg(Reg::x(10)), mem(Reg::x(0), AddressMode::Unsigned(0)),]
    );
    assert!(
        code.iter()
            .flat_map(|inst| &inst.operands)
            .all(|operand| !matches!(
                operand, Operand::Reg(register) if *register == SP
            ))
    );
}

#[test]
fn poll_skips_gc_when_committed_is_at_or_below_the_soft_limit() {
    let fragment = safepoint_slow("callback", "collect");
    let code = instructions(&fragment);
    let gc_offset = fragment.relocations[0].offset;
    let branch_offset = gc_offset - 16;
    let committed = Reg::x(10);
    let limit = Reg::x(11);
    for (offset, register, field) in [(12, committed, 3928), (8, limit, 3952)] {
        let load = at(&code, branch_offset - offset);
        assert_eq!(form(load), "ldr.X.Unsigned");
        assert_eq!(
            load.operands,
            [reg(register), mem(PROCESSOR, AddressMode::Unsigned(field))]
        );
    }
    let compare = at(&code, branch_offset - 4);
    assert_eq!(form(compare), "subs.X.shift");
    assert_eq!(
        compare.operands,
        [
            reg(Reg::zr(Width::X)),
            reg(committed),
            reg(limit),
            unshifted()
        ]
    );
    let branch = at(&code, branch_offset);
    assert_eq!(form(branch), "b.cond");
    assert_eq!(branch.operands[1], condition(9)); // LS: !C || Z
    assert_eq!(target(&code, branch_offset), gc_offset + 4);
    assert_eq!(form(at(&code, gc_offset + 4)), "ret");
}

#[test]
fn rt0_writes_native_boot_fields_and_restores_the_host_registers() {
    let fragment = rt0("start");
    let code = instructions(&fragment);
    let [call] = fragment.relocations.as_slice() else {
        panic!("expected runtime call")
    };
    assert_eq!(call.relocation.kind, RelocKind::Call26);
    assert_eq!(call.relocation.target, "start");
    assert_eq!(&fragment.bytes[..4], &0xd10083ffu32.to_le_bytes());
    let (before, after) = code.split_at(call.offset as usize / 4);
    for (value, field) in [
        (Reg::x(2), offset_of!(Rt0Payload, boot_environment)),
        (Reg::x(3), offset_of!(Rt0Payload, boot_argc)),
        (Reg::x(4), offset_of!(Rt0Payload, boot_argv)),
    ] {
        assert!(memory(before, "str.X.Unsigned", value, Reg::x(9), field));
    }
    for (index, register) in [HOT, PROCESSOR, Reg::x(29), LR].into_iter().enumerate() {
        assert!(memory(before, "str.X.Unsigned", register, SP, index * 8));
        assert!(memory(after, "ldr.X.Unsigned", register, SP, index * 8));
    }
    assert_eq!(
        after[after.len() - 2].operands,
        [reg(SP), reg(SP), add_imm(32)]
    );
    assert_eq!(before[5].operands, [reg(Reg::x(29)), reg(SP), add_imm(16)]);
    assert_eq!(form(after.last().unwrap()), "ret");
}

#[test]
fn rmw_uses_width_specific_exclusives_and_retries_store_failure() {
    for (width, tag) in [(Byte, "B"), (Half, "H"), (Word, "W"), (Doubleword, "X")] {
        for (ordering, load, store) in [
            (Relaxed, "ldxr", "stxr"),
            (Acquire, "ldaxr", "stxr"),
            (Release, "ldxr", "stlxr"),
            (AcqRel, "ldaxr", "stlxr"),
            (SeqCst, "ldaxr", "stlxr"),
        ] {
            for (op, mnemonic) in [
                (Add, "add"),
                (Sub, "sub"),
                (And, "and"),
                (Or, "orr"),
                (Xor, "eor"),
                (Exchange, ""),
            ] {
                let code = instructions(&atomic(op, width, ordering, None).unwrap());
                let store_index = if op == Exchange { 1 } else { 2 };
                assert_eq!(form(&code[0]), format!("{load}.{tag}"));
                assert_eq!(form(&code[store_index]), format!("{store}.{tag}"));
                if op != Exchange {
                    let tag = if width == Doubleword { "X" } else { "W" };
                    assert_eq!(form(&code[1]), format!("{mnemonic}.{tag}.shift"));
                }
                assert_eq!(form(&code[store_index + 1]), "cbnz.W");
                assert_eq!(target(&code, (store_index as u32 + 1) * 4), 0);
                assert_eq!(
                    code[store_index + 2].operands,
                    [reg(Reg::x(0)), reg(Reg::x(4)), add_imm(0)]
                );
            }
        }
    }
}

#[test]
fn cas_mismatch_clears_monitor_and_reports_observed_value_without_a_store() {
    let fragment = atomic(CompareExchange, Byte, AcqRel, Some(Acquire)).unwrap();
    assert!(atomic(CompareExchange, Byte, Release, Some(Acquire)).is_none());
    assert!(atomic(CompareExchange, Byte, AcqRel, Some(Release)).is_none());
    assert!(atomic(CompareExchange, Byte, Acquire, None).is_none());
    let code = instructions(&fragment);
    assert_eq!(code[0].operands.last(), Some(&Operand::Bits(0xff)));
    assert_eq!(form(&code[1]), "ldaxr.B");
    assert_eq!(form(&code[4]), "stlxr.B");
    assert_eq!(target(&code, 20), 4);
    let failure = target(&code, 12);
    assert_eq!(form(at(&code, failure)), "clrex");
    assert_eq!(at(&code, 28).operands, [reg(Reg::w(1)), movz(1)]);
    assert_eq!(
        at(&code, failure + 4).operands,
        [reg(Reg::x(0)), reg(Reg::x(4)), add_imm(0)]
    );
    assert_eq!(at(&code, failure + 8).operands, [reg(Reg::w(1)), movz(0)]);
}

#[test]
fn ordinary_atomics_have_the_expected_machine_encodings() {
    for (width, relaxed, acquire, release) in [
        (Byte, 0x39400000u32, 0x08dffc00u32, 0x089ffc01u32),
        (Half, 0x79400000, 0x48dffc00, 0x489ffc01),
        (Word, 0xb9400000, 0x88dffc00, 0x889ffc01),
        (Doubleword, 0xf9400000, 0xc8dffc00, 0xc89ffc01),
    ] {
        for (op, order, word) in [
            (Load, Relaxed, relaxed),
            (Load, Acquire, acquire),
            (Load, SeqCst, acquire),
            (Store, Relaxed, relaxed - 0x00400000 + 1),
            (Store, Release, release),
            (Store, SeqCst, release),
            (Fence, Acquire, 0xd50339bf),
            (Fence, SeqCst, 0xd5033bbf),
        ] {
            let fragment = atomic(op, width, order, None).unwrap();
            assert_eq!(&fragment.bytes[..4], &word.to_le_bytes());
            assert_eq!(form(instructions(&fragment).last().unwrap()), "ret");
        }
        assert!(atomic(Load, width, Release, None).is_none());
        assert!(atomic(Store, width, Acquire, None).is_none());
        assert!(atomic(Fence, width, Relaxed, None).is_none());
        assert!(atomic(Load, width, Acquire, Some(Relaxed)).is_none());
    }
}
