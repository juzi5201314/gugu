//! 通过 kernel32 / ntdll IAT 完成的平台与分配入口。

use super::runtime::{self, Gen, Routine};

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSP: u8 = 4;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;

const PROTECT: &str = "kernel32.dll!VirtualProtect";
const RANDOM: &str = "ntdll.dll!RtlRandomEx";

const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READWRITE: u32 = 0x04;
const PAGE: i32 = 4096;

pub(super) fn routines() -> Vec<Routine> {
    vec![
        cow_snapshot(),
        concat_strings(),
        defer_push(),
        join_wait(),
        reserve_aligned(),
        protect_header(PAGE_READWRITE, "platform_commit"),
        protect_header(PAGE_NOACCESS, "platform_decommit"),
        guard_page(PAGE_NOACCESS, "platform_protect_guard"),
        guard_page(PAGE_READWRITE, "platform_unprotect"),
        entropy(),
    ]
}

fn cow_snapshot() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    let empty = code.asm.label();
    code.asm.mov_load(RCX, RBX, 0);
    code.asm.test_self(RCX);
    code.asm.je(empty);
    code.asm.push(RAX);
    code.asm.push(RCX);
    code.asm.mov_reg(RSI, RCX);
    code.alloc_reg(RSI, PAGE_READWRITE, fail);
    copy_alloc(&mut code);
    code.asm.ret();
    code.asm.bind(empty);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("cow_snapshot", true)
}

fn copy_alloc(code: &mut Gen) {
    code.asm.pop(RCX);
    code.asm.pop(RBX);
    code.asm.push(RAX);
    copy_bytes(&mut code.asm);
    code.asm.pop(RAX);
}

fn concat_strings() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    let empty = code.asm.label();
    push_pair(&mut code);
    code.asm.mov_load(RAX, RSP, 16);
    code.asm.add_reg(RAX, RDX);
    code.asm.jb(fail);
    code.asm.test_self(RAX);
    code.asm.je(empty);
    code.asm.mov_reg(RBX, RAX);
    code.alloc_reg(RBX, PAGE_READWRITE, fail);
    finish_concat(&mut code);
    code.asm.bind(empty);
    code.asm.xor_self32(RAX);
    code.asm.xor_self32(RBX);
    code.asm.add_imm32(RSP, 32);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("concat", true)
}

fn push_pair(code: &mut Gen) {
    code.asm.push(RAX);
    code.asm.push(RBX);
    code.asm.push(RCX);
    code.asm.push(RDX);
}

fn finish_concat(code: &mut Gen) {
    code.asm.push(RAX);
    copy_piece(&mut code.asm, 32, 24, false);
    copy_piece(&mut code.asm, 16, 8, true);
    code.asm.mov_load(RBX, RSP, 24);
    code.asm.mov_load(RCX, RSP, 8);
    code.asm.add_reg(RBX, RCX);
    code.asm.pop(RAX);
    code.asm.add_imm32(RSP, 32);
    code.asm.ret();
}

fn copy_piece(asm: &mut super::super::elf::asm::Asm, src: i32, len: i32, shift: bool) {
    asm.mov_load(RAX, RSP, 0);
    if shift {
        asm.mov_load(RCX, RSP, 24);
        asm.add_reg(RAX, RCX);
    }
    asm.mov_load(RBX, RSP, src);
    asm.mov_load(RCX, RSP, len);
    copy_bytes(asm);
}

fn defer_push() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.asm.push(RAX);
    code.asm.push(RBX);
    code.alloc(32, PAGE_READWRITE, fail);
    code.asm.pop(RBX);
    code.asm.pop(RCX);
    code.asm.mov_store(RAX, 0, RCX);
    code.asm.mov_store(RAX, 8, RBX);
    code.asm.xor_self32(RCX);
    code.asm.mov_store(RAX, 16, RCX);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("defer_push", true)
}

fn join_wait() -> Routine {
    let mut code = Gen::new();
    let done = code.asm.label();
    let fail = code.asm.label();
    code.asm.mov_load(RCX, RAX, 16);
    code.asm.test_self(RCX);
    code.asm.jne(done);
    invoke_join(&mut code, fail);
    code.asm.bind(done);
    code.asm.mov_load(RBX, RAX, 24);
    code.asm.xor_self32(RAX);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("join_wait", true)
}

fn invoke_join(code: &mut Gen, fail: u32) {
    code.asm.mov_load(RCX, RAX, 0);
    code.asm.test_self(RCX);
    code.asm.je(fail);
    code.asm.push(RAX);
    code.asm.mov_load(RAX, RAX, 8);
    code.asm.call_reg(RCX);
    code.asm.pop(RCX);
    code.asm.mov_store(RCX, 24, RAX);
    code.asm.mov_imm32(RDX, 1);
    code.asm.mov_store(RCX, 16, RDX);
    code.asm.mov_reg(RBX, RAX);
    code.asm.xor_self32(RAX);
    code.asm.ret();
}

fn reserve_aligned() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    check_align(&mut code, fail);
    code.asm.mov_reg(RSI, RAX);
    code.asm.mov_reg(RDI, RCX);
    code.alloc(4096, PAGE_READWRITE, fail);
    code.asm.mov_reg(RBX, RAX);
    code.alloc_reg(RDI, PAGE_NOACCESS, fail);
    store_reservation(&mut code);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("platform_reserve_aligned", true)
}

fn check_align(code: &mut Gen, fail: u32) {
    code.asm.test_self(RAX);
    code.asm.je(fail);
    code.asm.test_self(RBX);
    code.asm.je(fail);
    code.asm.mov_reg(RCX, RBX);
    code.asm.sub_imm32(RCX, 1);
    code.asm.and_reg(RCX, RBX);
    code.asm.jne(fail);
    code.asm.mov_reg(RCX, RAX);
    code.asm.add_imm32(RCX, PAGE - 1);
    code.asm.jb(fail);
    code.asm.and_imm32(RCX, -PAGE);
}

fn store_reservation(code: &mut Gen) {
    code.asm.mov_store(RBX, 0, RAX);
    code.asm.mov_store(RBX, 8, RDI);
    code.asm.mov_store(RBX, 16, RSI);
    code.asm.mov_reg(RAX, RBX);
}

fn protect_header(prot: u32, name: &'static str) -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.asm.mov_load(RDI, RAX, 0);
    code.asm.mov_load(RSI, RAX, 8);
    virtual_protect(&mut code, prot, fail);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish(name, true)
}

fn guard_page(prot: u32, name: &'static str) -> Routine {
    let mut code = Gen::new();
    let tail = code.asm.label();
    let protect = code.asm.label();
    let fail = code.asm.label();
    code.asm.mov_load(RDI, RAX, 0);
    code.asm.mov_load(RSI, RAX, 8);
    code.asm.cmp_imm32(RSI, PAGE);
    code.asm.ja(tail);
    code.asm.jmp(protect);
    code.asm.bind(tail);
    code.asm.add_reg(RDI, RSI);
    code.asm.sub_imm32(RDI, PAGE);
    code.asm.mov_imm32(RSI, PAGE as u32);
    code.asm.bind(protect);
    virtual_protect(&mut code, prot, fail);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish(name, true)
}

fn virtual_protect(code: &mut Gen, prot: u32, fail: u32) {
    code.asm.sub_imm32(RSP, 40);
    code.asm.mov_reg(RCX, RDI);
    code.asm.mov_reg(RDX, RSI);
    code.asm.mov_imm32(R8, prot);
    runtime::lea_rsp(&mut code.asm, R9, 32);
    code.call_import(PROTECT);
    code.asm.add_imm32(RSP, 40);
    code.asm.test_self(RAX);
    code.asm.je(fail);
}

fn entropy() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    let empty = code.asm.label();
    let step = code.asm.label();
    let done = code.asm.label();
    code.asm.test_self(RAX);
    code.asm.je(empty);
    code.asm.mov_reg(RSI, RAX);
    code.alloc_reg(RSI, PAGE_READWRITE, fail);
    fill_entropy(&mut code, step, done);
    code.asm.bind(empty);
    code.asm.xor_self32(RAX);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("platform_entropy", true)
}

fn fill_entropy(code: &mut Gen, step: u32, done: u32) {
    code.asm.mov_reg(RDI, RAX);
    code.asm.mov_reg(RBX, RSI);
    code.asm.push(RAX);
    code.asm.sub_imm32(RSP, 48);
    code.asm.mov_store(RSP, 32, RAX);
    code.asm.bind(step);
    code.asm.test_self(RBX);
    code.asm.je(done);
    runtime::lea_rsp(&mut code.asm, RCX, 32);
    code.call_import(RANDOM);
    code.asm.emit(&[0x88, 0x07]);
    code.asm.add_imm32(RDI, 1);
    code.asm.dec_reg(RBX);
    code.asm.jmp(step);
    code.asm.bind(done);
    code.asm.add_imm32(RSP, 48);
    code.asm.pop(RAX);
    code.asm.ret();
}

fn copy_bytes(asm: &mut super::super::elf::asm::Asm) {
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
