//! C 入口只封送与等待；用户函数始终由 managed callback adapter 执行。
use super::*;
use crate::lir::exports::Export;
use crate::runtime::callback as q;
use crate::x64::abi::{self, AbiSlot};
use crate::x64::reg::{Reg, Xmm};

const SAVED: [Gpr; 6] = [Rbp, Rbx, R12, R13, R14, R15];
const C_INTEGER: [Gpr; 6] = [Rdi, Rsi, Rdx, Rcx, R8, R9];
const C_FLOAT: [Xmm; 8] = [
    Xmm::Xmm0,
    Xmm::Xmm1,
    Xmm::Xmm2,
    Xmm::Xmm3,
    Xmm::Xmm4,
    Xmm::Xmm5,
    Xmm::Xmm6,
    Xmm::Xmm7,
];

pub(crate) fn build(
    export: &Export,
    target: TargetName,
) -> Result<crate::x64::codegen::NativeExport, String> {
    let sequence = sequence(export, target)?.finish().sequence;
    let encoded = encode::assemble(&sequence).map_err(|error| error.to_string())?;
    let prologue_end = encoded.instruction_offsets[7].1 as u8;
    let mut unwind = vec![1, prologue_end, 7, 5, prologue_end, 3];
    for (index, register) in SAVED.into_iter().enumerate().rev() {
        unwind.extend([
            encoded.instruction_offsets[index + 1].1 as u8,
            (register as u8) << 4,
        ]);
    }
    unwind.resize(20, 0);
    Ok(crate::x64::codegen::NativeExport {
        name: export.name.clone(),
        bytes: encoded.bytes,
        relocations: encoded.relocations,
        unwind,
    })
}

pub(super) fn sequence(export: &Export, target: TargetName) -> Result<Builder, String> {
    let abi = abi::classify_call(&export.call, target).map_err(|error| error.to_string())?;
    let scratch = i32::try_from(export.packet_bytes).map_err(|_| "回调包超出栈偏移")?;
    let frame = scratch.checked_add(168).ok_or("回调帧过大")?;
    let mut b = Builder::new();
    for r in SAVED {
        push(&mut b, r);
    }
    rr(&mut b, Rbp, Rsp);
    rr(&mut b, R13, Rsp);
    adjust(&mut b, "add", R13, 48); // 原始 C 入口 rsp。
    // 每页触碰，Windows guard page 不可越过；R11 不是 C 参数寄存器。
    let mut remaining = frame;
    while remaining > 4096 {
        adjust(&mut b, "sub", Rsp, 4096);
        set(&mut b, Rsp, 0, 0);
        remaining -= 4096;
    }
    adjust(&mut b, "sub", Rsp, remaining);
    rr(&mut b, R12, Rsp);
    adjust(&mut b, "add", R12, 32); // 保留 Win64 shadow space。
    for (index, r) in C_INTEGER.into_iter().enumerate() {
        store(&mut b, R12, scratch + index as i32 * 8, r);
    }
    for (index, r) in C_FLOAT.into_iter().enumerate() {
        float(
            &mut b,
            r,
            R12,
            scratch + 48 + index as i32 * 8,
            Access::Write,
        );
    }
    for offset in (0..240).step_by(8) {
        set(&mut b, R12, offset, 0);
    }
    address_symbol(&mut b, Rax, Symbol::Instance(export.adapter));
    store(&mut b, R12, q::ENTRY, Rax);
    store(&mut b, R12, q::PACK, R12);
    for (value, lane) in abi.arguments.iter().zip(&export.arguments) {
        let Some(slot) = value.slot else { continue };
        let offset = lane.offset as i32;
        match slot {
            AbiSlot::Integer(r) => load(
                &mut b,
                Rax,
                R12,
                scratch
                    + C_INTEGER
                        .iter()
                        .position(|saved| *saved == r)
                        .ok_or("C 整数车道未保存")? as i32
                        * 8,
            ),
            AbiSlot::Float(r) => load(&mut b, Rax, R12, scratch + 48 + r as i32 * 8),
            AbiSlot::Stack { offset } => load(&mut b, Rax, R13, 8 + offset as i32),
        }
        if lane.indirect {
            rr(&mut b, Rdx, Rax);
            address(&mut b, R8, R12, offset);
            copy(&mut b, lane.bytes);
        } else {
            store(&mut b, R12, offset, Rax);
        }
    }
    runtime_call(&mut b, "thread_id");
    store(&mut b, R12, q::THREAD_ID, Rax);
    runtime_call(&mut b, "callback_control");
    rr(&mut b, R15, Rax);
    let failed = b.label();
    queue(&mut b, q::SUBMIT, failed);
    let wait = b.label();
    let complete = b.label();
    b.define(wait);
    load(&mut b, Rax, R12, q::STATE);
    compare(&mut b, Rax, q::COMPLETE);
    jump(&mut b, "je", complete);
    let sleep = b.label();
    compare(&mut b, Rax, q::REQUEST_READY);
    jump(&mut b, "jne", sleep);
    assist(&mut b, target);
    jump(&mut b, "jmp", wait);
    b.define(sleep);
    wait_thread(&mut b, target, scratch + 112);
    jump(&mut b, "jmp", wait);
    b.define(complete);
    queue(&mut b, q::DETACH, failed);
    if export.call.sret.is_some() {
        address(&mut b, Rdx, R12, export.result_offset as i32);
        load(&mut b, R8, R12, crate::lir::exports::SRET_OFFSET as i32);
        copy(&mut b, export.result_bytes);
        load(&mut b, Rax, R12, crate::lir::exports::SRET_OFFSET as i32);
    } else {
        for (value, lane) in abi.results.iter().zip(&export.results) {
            match value.slot {
                Some(AbiSlot::Integer(r)) => load(&mut b, r, R12, lane.offset as i32),
                Some(AbiSlot::Float(r)) => float(&mut b, r, R12, lane.offset as i32, Access::Read),
                _ => return Err("C 回调返回车道非法".into()),
            }
        }
    }
    address(&mut b, Rsp, Rbp, 0);
    for r in SAVED.into_iter().rev() {
        pop(&mut b, r);
    }
    ret(&mut b);
    b.define(failed);
    b.emit("ud2", &[], Access::Read, vec![]);
    Ok(b)
}

fn assist(b: &mut Builder, target: TargetName) {
    let argument = if target == TargetName::X86_64Linux {
        Rdi
    } else {
        Rcx
    };
    address(b, argument, R12, q::REQUEST);
    load(b, Rax, R12, q::REQUEST + OPERATION);
    let file = b.label();
    let done = b.label();
    compare(b, Rax, -1);
    jump(b, "jne", file);
    runtime_call(b, "foreign_native_worker");
    jump(b, "jmp", done);
    b.define(file);
    runtime_call(b, "callback_file_worker");
    b.define(done);
    // x64 普通写提供 release；结果全部可见后才通知 managed owner。
    set(b, R12, q::STATE, q::REQUEST_DONE);
}

fn queue(b: &mut Builder, action: i32, failed: LabelId) {
    let retry = b.label();
    b.define(retry);
    rr(b, Rax, R15);
    rr(b, Rbx, R12);
    number(b, Rcx, action);
    runtime_call(b, "callback_queue");
    compare(b, Rax, q::BUSY);
    jump(b, "je", retry);
    compare(b, Rax, q::OK);
    jump(b, "jne", failed);
}

fn wait_thread(b: &mut Builder, target: TargetName, scratch: i32) {
    match target {
        TargetName::X86_64Linux => {
            // nanosleep 的原始 syscall 不修改 libc errno；等待者不占 managed processor。
            set(b, R12, scratch, 0);
            set(b, R12, scratch + 8, 1_000_000);
            push(b, Rdi);
            push(b, Rsi);
            address(b, Rdi, R12, scratch);
            number(b, Rsi, 0);
            number(b, Rax, 35);
            syscall(b);
            pop(b, Rsi);
            pop(b, Rdi);
        }
        TargetName::X86_64Windows => {
            number(b, Rcx, 1);
            win_call(b, "Sleep");
        }
    }
}

fn runtime_call(b: &mut Builder, name: &'static str) {
    b.emit(
        "call",
        &[K::Rel32],
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol(name)),
            RelocKind::PcRel32,
        )],
    );
}
fn address_symbol(b: &mut Builder, dst: Gpr, symbol: Symbol) {
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![Operand::Rip(RelocTarget::Lir(symbol), 0), reg(gpr(dst))],
    );
}
fn address(b: &mut Builder, dst: Gpr, base: Gpr, offset: i32) {
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(base), offset), reg(gpr(dst))],
    );
}
fn float(b: &mut Builder, r: Xmm, base: Gpr, offset: i32, access: Access) {
    b.emit(
        "movsd",
        &[K::XmmRm, K::Xmm],
        access,
        vec![mem_base(gpr(base), offset), reg(Reg::Xmm(r))],
    );
}
/// 精确复制最后不足八字节的尾部，不越过调用者的对象边界。
fn copy(b: &mut Builder, bytes: u64) {
    let mut offset = 0;
    for (width, shape) in [
        (8, &[K::Rm64, K::R64][..]),
        (4, &[K::Rm32, K::R32][..]),
        (2, &[K::Rm16, K::R16][..]),
        (1, &[K::Rm8, K::R8][..]),
    ] {
        while offset + width <= bytes {
            b.emit(
                "mov",
                shape,
                Access::Read,
                vec![mem_base(gpr(Rdx), offset as i32), reg(gpr(Rax))],
            );
            b.emit(
                "mov",
                shape,
                Access::Write,
                vec![mem_base(gpr(R8), offset as i32), reg(gpr(Rax))],
            );
            offset += width;
        }
    }
}
