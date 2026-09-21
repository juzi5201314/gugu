//! 不调用 IAT 的运行时入口。返回约定与 Linux 写出相同。

use super::super::elf::asm::Asm;
use super::runtime::{CallSite, ImportUse, Routine};

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
const CHAN_CLOSED: i32 = 4088;

pub(super) fn routines() -> Vec<Routine> {
    vec![
        value_copy(),
        value_transfer(),
        value_repeat(),
        utf8_boundary(),
        downcast(),
        downcast_copy(),
        defer_action(),
        defer_environment(),
        defer_pop(),
        channel_try_send(),
        channel_try_recv(),
        platform_wait(),
        platform_wake(),
        low_memory(),
        zero_range(),
    ]
}

pub(super) fn empty_pair(name: &'static str) -> Routine {
    let mut asm = Asm::new();
    asm.xor_self32(RAX);
    asm.xor_self32(RBX);
    asm.ret();
    pure(name, asm.finish())
}

fn value_copy() -> Routine {
    let mut asm = Asm::new();
    let skip = asm.label();
    asm.push(RAX);
    asm.mov_load(RCX, RCX, 0);
    asm.test_self(RCX);
    asm.je(skip);
    copy_bytes(&mut asm);
    asm.bind(skip);
    asm.pop(RAX);
    asm.ret();
    pure("value_copy", asm.finish())
}

fn value_transfer() -> Routine {
    let mut asm = Asm::new();
    let skip = asm.label();
    asm.push(RAX);
    asm.push(RBX);
    asm.mov_load(RCX, RCX, 0);
    asm.push(RCX);
    asm.test_self(RCX);
    asm.je(skip);
    copy_bytes(&mut asm);
    asm.bind(skip);
    asm.pop(RCX);
    asm.pop(RAX);
    zero_rax(&mut asm);
    asm.pop(RAX);
    asm.ret();
    pure("value_transfer", asm.finish())
}

fn value_repeat() -> Routine {
    let mut asm = Asm::new();
    let step = asm.label();
    let done = asm.label();
    asm.push(RAX);
    asm.mov_reg(R8, RBX);
    asm.mov_reg(R9, RCX);
    asm.mov_load(R10, RDX, 0);
    asm.mov_reg(R11, RAX);
    asm.bind(step);
    repeat_step(&mut asm, step, done);
    asm.bind(done);
    asm.pop(RAX);
    asm.ret();
    pure("value_repeat", asm.finish())
}

fn repeat_step(asm: &mut Asm, step: u32, done: u32) {
    asm.test_self(R9);
    asm.je(done);
    asm.test_self(R10);
    asm.je(done);
    asm.mov_reg(RAX, R11);
    asm.mov_reg(RBX, R8);
    asm.mov_reg(RCX, R10);
    copy_bytes(asm);
    asm.add_reg(R11, R10);
    asm.dec_reg(R9);
    asm.jmp(step);
}

fn utf8_boundary() -> Routine {
    let mut asm = Asm::new();
    let too_big = asm.label();
    let clamped = asm.label();
    let scan = asm.label();
    let found = asm.label();
    asm.cmp_reg(RCX, RBX);
    asm.ja(too_big);
    asm.jmp(clamped);
    asm.bind(too_big);
    asm.mov_reg(RCX, RBX);
    asm.bind(clamped);
    scan_utf8(&mut asm, scan, found);
    asm.bind(found);
    asm.mov_reg(RAX, RCX);
    asm.ret();
    pure("utf8_boundary", asm.finish())
}

fn scan_utf8(asm: &mut Asm, scan: u32, found: u32) {
    asm.cmp_reg(RCX, RBX);
    asm.je(found);
    asm.bind(scan);
    asm.lea_disp(RDI, RAX, RCX, 1, 0);
    asm.movzx8(RDX, RDI, 0);
    asm.and_imm32(RDX, 0xC0);
    asm.cmp_imm32(RDX, 0x80);
    asm.jne(found);
    asm.test_self(RCX);
    asm.je(found);
    asm.dec_reg(RCX);
    asm.jmp(scan);
}

fn downcast() -> Routine {
    let mut asm = Asm::new();
    let miss = asm.label();
    asm.cmp_mem(RBX, RAX, 0);
    asm.jne(miss);
    asm.mov_reg(RBX, RAX);
    asm.add_imm32(RBX, 8);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(miss);
    miss_pair(&mut asm);
    pure("downcast", asm.finish())
}

fn downcast_copy() -> Routine {
    let mut asm = Asm::new();
    let miss = asm.label();
    asm.cmp_mem(RBX, RAX, 0);
    asm.jne(miss);
    asm.mov_load(RBX, RAX, 8);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(miss);
    miss_pair(&mut asm);
    pure("downcast_copy", asm.finish())
}

fn miss_pair(asm: &mut Asm) {
    asm.mov_imm32(RAX, 1);
    asm.xor_self32(RBX);
    asm.ret();
}

fn defer_action() -> Routine {
    let mut asm = Asm::new();
    asm.mov_load(RAX, RAX, 0);
    asm.ret();
    pure("defer_action", asm.finish())
}

fn defer_environment() -> Routine {
    let mut asm = Asm::new();
    asm.mov_store(RBX, 16, RCX);
    asm.mov_reg(RAX, RBX);
    asm.ret();
    pure("defer_environment", asm.finish())
}

fn defer_pop() -> Routine {
    let mut asm = Asm::new();
    asm.mov_load(RAX, RAX, 8);
    asm.ret();
    pure("defer_pop", asm.finish())
}

fn channel_try_send() -> Routine {
    let mut asm = Asm::new();
    let full = asm.label();
    let closed = asm.label();
    asm.mov_reg(R11, RAX);
    asm.movzx8(RCX, R11, CHAN_CLOSED);
    asm.test_self(RCX);
    asm.jne(closed);
    asm.mov_load(RCX, R11, 8);
    asm.cmp_mem(RCX, R11, 0);
    asm.jae(full);
    enqueue(&mut asm);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(full);
    asm.mov_imm32(RAX, 1);
    asm.ret();
    asm.bind(closed);
    asm.mov_imm32(RAX, 0x101);
    asm.ret();
    pure("channel_try_send", asm.finish())
}

fn channel_try_recv() -> Routine {
    let mut asm = Asm::new();
    let empty = asm.label();
    let closed = asm.label();
    asm.mov_reg(R11, RAX);
    asm.mov_load(RCX, R11, 8);
    asm.test_self(RCX);
    asm.je(empty);
    dequeue(&mut asm);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(empty);
    closed_or_empty(&mut asm, closed);
    pure("channel_try_recv", asm.finish())
}

fn closed_or_empty(asm: &mut Asm, closed: u32) {
    asm.movzx8(RCX, R11, CHAN_CLOSED);
    asm.test_self(RCX);
    asm.jne(closed);
    asm.mov_imm32(RAX, 1);
    asm.xor_self32(RBX);
    asm.ret();
    asm.bind(closed);
    asm.mov_imm32(RAX, 1);
    asm.mov_imm32(RBX, 1);
    asm.ret();
}

fn enqueue(asm: &mut Asm) {
    asm.mov_load(RDX, R11, 16);
    asm.add_reg(RDX, RCX);
    asm.mov_load(RCX, R11, 0);
    asm.mov_reg(RAX, RDX);
    asm.xor_self32(RDX);
    asm.div_r64(RCX);
    asm.lea_disp(RAX, R11, RDX, 8, 24);
    asm.mov_store(RAX, 0, RBX);
    asm.inc_mem64(R11, 8);
}

fn dequeue(asm: &mut Asm) {
    asm.mov_load(RDX, R11, 16);
    asm.lea_disp(RAX, R11, RDX, 8, 24);
    asm.mov_load(RBX, RAX, 0);
    asm.mov_reg(RAX, RDX);
    asm.add_imm32(RAX, 1);
    asm.xor_self32(RDX);
    asm.mov_load(RCX, R11, 0);
    asm.div_r64(RCX);
    asm.mov_store(R11, 16, RDX);
    asm.dec_mem64(R11, 8);
}

fn platform_wait() -> Routine {
    let mut asm = Asm::new();
    let changed = asm.label();
    asm.mov_load(RCX, RAX, 0);
    asm.cmp_reg(RCX, RBX);
    asm.jne(changed);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(changed);
    asm.mov_imm32(RAX, 1);
    asm.ret();
    pure("platform_wait", asm.finish())
}

fn platform_wake() -> Routine {
    let mut asm = Asm::new();
    asm.mfence();
    asm.xor_self32(RAX);
    asm.ret();
    pure("platform_wake", asm.finish())
}

fn low_memory() -> Routine {
    let mut asm = Asm::new();
    asm.xor_self32(RAX);
    asm.ret();
    pure("platform_low_memory_hint", asm.finish())
}

fn zero_range() -> Routine {
    let mut asm = Asm::new();
    asm.mov_load(RCX, RAX, 16);
    asm.mov_load(RAX, RAX, 0);
    zero_rax(&mut asm);
    asm.ret();
    pure("platform_zero", asm.finish())
}

fn copy_bytes(asm: &mut Asm) {
    let done = asm.label();
    let step = asm.label();
    asm.test_self(RCX);
    asm.je(done);
    asm.bind(step);
    asm.copy_byte();
    asm.add_imm32(RAX, 1);
    asm.add_imm32(RBX, 1);
    asm.dec_reg(RCX);
    asm.jne(step);
    asm.bind(done);
}

fn zero_rax(asm: &mut Asm) {
    let done = asm.label();
    let step = asm.label();
    asm.test_self(RCX);
    asm.je(done);
    asm.xor_self32(RBX);
    asm.bind(step);
    asm.store_bl();
    asm.add_imm32(RAX, 1);
    asm.dec_reg(RCX);
    asm.jne(step);
    asm.bind(done);
}

fn pure(name: &'static str, bytes: Vec<u8>) -> Routine {
    Routine {
        name,
        bytes,
        imports: Vec::<ImportUse>::new(),
        calls: Vec::<CallSite>::new(),
        runtime: true,
    }
}
