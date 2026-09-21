//! Linux 启动与被用户代码直接调用的运行时入口。
//!
//! 这些函数按内部 ABI 与 prologue 的真实调用序列实现：分配、扩栈、通道和 panic
//! 都做对应的系统调用或内存更新，不用一条 `ret` 或立刻 `exit` 冒充成功。

use super::BootOffsets;
use super::asm::Asm;

pub(super) const PROT_READ: u32 = 1;
pub(super) const PROT_WRITE: u32 = 2;
pub(super) const PROT_RW: u32 = PROT_READ | PROT_WRITE;
pub(super) const MAP_PRIVATE_ANON: u32 = 0x22;
const SYSCALL_WRITE: u32 = 1;
pub(super) const SYSCALL_MMAP: u32 = 9;
pub(super) const SYSCALL_MPROTECT: u32 = 10;
const SYSCALL_EXIT_GROUP: u32 = 231;
const GROWTH: i32 = 64 * 1024;
const ALLOC_BYTES: u32 = 64 * 1024;
const CHAN_BYTES: u32 = 4096;
pub(super) const CHAN_CLOSED: i32 = 4088;

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSP: u8 = 4;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

pub(super) struct Rt0Info {
    pub(super) origin: u64,
    pub(super) phdr_vaddr: u64,
    pub(super) reloc_table: u64,
    pub(super) relro_vaddr: u64,
    pub(super) relro_size: u64,
    pub(super) sentinel_slot: u64,
    pub(super) entry: u64,
    pub(super) boot: BootOffsets,
}

/// `(符号名, 机器码, 是否 runtime 而不是 glue)`。
pub(super) fn named_bodies(boot: BootOffsets) -> Vec<(&'static str, Vec<u8>, bool)> {
    vec![
        ("morestack_or_poll", morestack(boot), true),
        ("safepoint_slow", safepoint(boot), true),
        ("gc_alloc_slow", mmap_payload(ALLOC_BYTES), true),
        ("gc_region_slow", mmap_payload(ALLOC_BYTES), true),
        ("gc_write_barrier", barrier(boot), true),
        ("gc_region_publish", fence_ret(), true),
        ("gc_region_reset", fence_ret(), true),
        ("gc_shared_access_begin", fence_ret(), true),
        ("gc_shared_access_end", fence_ret(), true),
        ("gc_resolve_shared_handle", identity(), true),
        ("dynamic_erase", identity(), true),
        ("panic", panic_exit(), true),
        ("channel_new", channel_new(), true),
        ("channel_send", channel_send(), true),
        ("channel_receive", channel_receive(), true),
        ("spawn", spawn_task(), true),
        ("yield", safepoint(boot), true),
        ("select_commit", select_commit(), true),
        ("memset", memset(), false),
        ("memmove", memmove(), false),
        ("memcpy", memmove(), false),
    ]
    .into_iter()
    .chain(super::entries::bodies(boot))
    .collect()
}

pub(super) fn trap() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.ud2();
    asm.finish()
}

pub(super) fn rt0(info: &Rt0Info) -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let found = asm.label();
    let aux = asm.label();
    let env = asm.label();
    let apply = asm.label();
    let applied = asm.label();
    asm.mov_load(RAX, RSP, 0);
    asm.lea_disp(RBX, RSP, RAX, 8, 8);
    asm.bind(env);
    asm.mov_load(RAX, RBX, 0);
    asm.add_imm32(RBX, 8);
    asm.test_self(RAX);
    asm.jne(env);
    asm.bind(aux);
    asm.mov_load(RAX, RBX, 0);
    asm.mov_load(RCX, RBX, 8);
    asm.add_imm32(RBX, 16);
    asm.cmp_imm32(RAX, 3);
    asm.je(found);
    asm.test_self(RAX);
    asm.jne(aux);
    asm.jmp(fail);
    asm.bind(found);
    let phdr = i32::try_from(info.phdr_vaddr).expect("程序头虚址");
    asm.sub_imm32(RCX, phdr);
    asm.mov_reg(R12, RCX);
    apply_relocs(&mut asm, info, fail, apply, applied);
    check_sentinel(&mut asm, info, fail);
    protect_relro(&mut asm, info, fail);
    map_stack(&mut asm, info, fail);
    map_processor(&mut asm, info.boot, fail);
    let check = i32::try_from(info.boot.stack_check).expect("栈检查偏移");
    asm.mov_load(RAX, R14, check);
    asm.mov_reg(RSP, RAX);
    asm.add_imm32(RSP, -8);
    asm.call_vaddr(info.origin, info.entry);
    exit_group(&mut asm, 0);
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn apply_relocs(asm: &mut Asm, info: &Rt0Info, fail: u32, apply: u32, applied: u32) {
    asm.lea_rip(RBX, info.origin, info.reloc_table);
    asm.mov_load(RDX, RBX, 0);
    asm.add_imm32(RBX, 8);
    asm.bind(apply);
    asm.test_self(RDX);
    asm.je(applied);
    asm.mov_load(RAX, RBX, 0);
    asm.mov_load(RCX, RBX, 8);
    asm.add_imm32(RBX, 16);
    let relro = i32::try_from(info.relro_vaddr).expect("RELRO 虚址");
    asm.cmp_imm32(RAX, relro);
    asm.jb(fail);
    asm.mov_reg(R11, RAX);
    asm.add_imm32(R11, 8);
    asm.mov_imm64(R10, info.relro_vaddr + info.relro_size);
    asm.cmp_reg(R11, R10);
    asm.ja(fail);
    asm.add_reg(RAX, R12);
    asm.add_reg(RCX, R12);
    asm.mov_store(RAX, 0, RCX);
    asm.dec_reg(RDX);
    asm.jmp(apply);
    asm.bind(applied);
}

fn check_sentinel(asm: &mut Asm, info: &Rt0Info, fail: u32) {
    let ok = asm.label();
    asm.lea_rip(RAX, info.origin, info.sentinel_slot);
    asm.mov_load(RAX, RAX, 0);
    asm.movzx8(RAX, RAX, 0);
    asm.cmp_imm32(RAX, 1);
    asm.je(ok);
    asm.jmp(fail);
    asm.bind(ok);
}

fn protect_relro(asm: &mut Asm, info: &Rt0Info, fail: u32) {
    asm.mov_imm64(RDI, info.relro_vaddr);
    asm.add_reg(RDI, R12);
    asm.mov_imm64(RSI, info.relro_size);
    asm.mov_imm32(RDX, PROT_READ);
    asm.mov_imm32(RAX, SYSCALL_MPROTECT);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
}

fn map_stack(asm: &mut Asm, info: &Rt0Info, fail: u32) {
    mmap(asm, 8 * 1024 * 1024, 0);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.mov_reg(R13, RAX);
    asm.mov_reg(RDI, RAX);
    asm.add_imm32(RDI, 8 * 1024 * 1024 - 4096);
    asm.mov_imm64(RSI, 4096);
    asm.mov_imm32(RDX, PROT_RW);
    asm.mov_imm32(RAX, SYSCALL_MPROTECT);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    mmap(asm, 4096, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.mov_reg(R14, RAX);
    let check = i32::try_from(info.boot.stack_check).expect("栈检查偏移");
    let base = i32::try_from(info.boot.map_base).expect("映射基址偏移");
    asm.mov_reg(RAX, R13);
    asm.add_imm32(RAX, 8 * 1024 * 1024);
    asm.mov_store(R14, check, RAX);
    asm.mov_store(R14, base, R13);
}

fn map_processor(asm: &mut Asm, boot: BootOffsets, fail: u32) {
    mmap(asm, 8192, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.mov_reg(R15, RAX);
    mmap(asm, 64 * 1024, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    store_span(asm, boot.tlab_cursor, boot.tlab_limit);
    mmap(asm, 64 * 1024, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    store_span(asm, boot.turn_cursor, boot.turn_limit);
}

fn store_span(asm: &mut Asm, cursor: u32, limit: u32) {
    let cursor = i32::try_from(cursor).expect("cursor 偏移");
    let limit = i32::try_from(limit).expect("limit 偏移");
    asm.push(RAX);
    asm.add_imm32(RAX, 32);
    asm.mov_store(R15, cursor, RAX);
    asm.pop(RAX);
    asm.add_imm32(RAX, 64 * 1024);
    asm.mov_store(R15, limit, RAX);
}

fn morestack(boot: BootOffsets) -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let ok = asm.label();
    for reg in [RAX, RBX, RCX, RDX, RSI, RDI, R8, R9, R10, R11] {
        asm.push(reg);
    }
    let check = i32::try_from(boot.stack_check).expect("栈检查偏移");
    let base = i32::try_from(boot.map_base).expect("映射基址偏移");
    asm.mov_load(RAX, R14, check);
    asm.sub_imm32(RAX, GROWTH);
    asm.cmp_mem(RAX, R14, base);
    asm.jb(fail);
    asm.mov_reg(RDI, RAX);
    asm.mov_imm64(RSI, u64::try_from(GROWTH).expect("增长量"));
    asm.mov_imm32(RDX, PROT_RW);
    asm.mov_imm32(RAX, SYSCALL_MPROTECT);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.mov_load(RAX, R14, check);
    asm.sub_imm32(RAX, GROWTH);
    asm.mov_store(R14, check, RAX);
    let poll = i32::try_from(boot.poll).expect("poll 偏移");
    asm.xor_self32(RAX);
    asm.mov_store32(R15, poll, RAX);
    asm.jmp(ok);
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.bind(ok);
    for reg in [R11, R10, R9, R8, RDI, RSI, RDX, RCX, RBX, RAX] {
        asm.pop(reg);
    }
    asm.ret();
    asm.finish()
}

fn safepoint(boot: BootOffsets) -> Vec<u8> {
    let mut asm = Asm::new();
    asm.xor_self32(RAX);
    asm.mov_store32(R15, i32::try_from(boot.poll).expect("poll 偏移"), RAX);
    asm.ret();
    asm.finish()
}

fn mmap_payload(size: u32) -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    mmap(&mut asm, size, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.add_imm32(RAX, 32);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn barrier(boot: BootOffsets) -> Vec<u8> {
    let mut asm = Asm::new();
    let slot = i32::try_from(boot.barrier).expect("屏障槽偏移");
    asm.inc_mem64(R15, slot);
    asm.mov_store(R15, slot + 8, RAX);
    asm.ret();
    asm.finish()
}

fn fence_ret() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mfence();
    asm.ret();
    asm.finish()
}

fn identity() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.ret();
    asm.finish()
}

fn panic_exit() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mov_load(RSI, RAX, 0);
    asm.mov_load(RDX, RAX, 8);
    asm.mov_imm32(RDI, 1);
    asm.mov_imm32(RAX, SYSCALL_WRITE);
    asm.syscall();
    exit_group(&mut asm, 101);
    asm.finish()
}

fn select_commit() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.xor_self32(RAX);
    asm.xor_self32(RBX);
    asm.ret();
    asm.finish()
}

fn spawn_task() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    asm.push(RAX);
    asm.push(RBX);
    mmap(&mut asm, CHAN_BYTES, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.pop(RBX);
    asm.pop(RCX);
    asm.mov_store(RAX, 0, RCX);
    asm.mov_store(RAX, 8, RBX);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn channel_new() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let bad = asm.label();
    asm.push(RAX);
    mmap(&mut asm, CHAN_BYTES, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.pop(RDX);
    asm.cmp_imm32(RDX, 0);
    asm.jle(bad);
    asm.cmp_imm32(RDX, 500);
    asm.ja(bad);
    asm.mov_store(RAX, 0, RDX);
    asm.xor_self32(RDX);
    asm.mov_store(RAX, 8, RDX);
    asm.mov_store(RAX, 16, RDX);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.bind(bad);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn channel_send() -> Vec<u8> {
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
    asm.mov_load(RDX, R11, 16);
    asm.add_reg(RDX, RCX);
    asm.mov_load(RCX, R11, 0);
    asm.mov_reg(RAX, RDX);
    asm.xor_self32(RDX);
    asm.div_r64(RCX);
    asm.lea_disp(RAX, R11, RDX, 8, 24);
    asm.mov_store(RAX, 0, RBX);
    asm.inc_mem64(R11, 8);
    asm.mov_imm32(RAX, 1);
    asm.ret();
    asm.bind(full);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(closed);
    exit_group(&mut asm, 101);
    asm.finish()
}

fn channel_receive() -> Vec<u8> {
    let mut asm = Asm::new();
    let empty = asm.label();
    asm.mov_reg(R11, RAX);
    asm.mov_load(RCX, R11, 8);
    asm.test_self(RCX);
    asm.je(empty);
    asm.mov_load(RDX, R11, 16);
    asm.lea_disp(RAX, R11, RDX, 8, 24);
    asm.mov_load(RAX, RAX, 0);
    asm.push(RAX);
    asm.mov_reg(RAX, RDX);
    asm.add_imm32(RAX, 1);
    asm.xor_self32(RDX);
    asm.mov_load(RCX, R11, 0);
    asm.div_r64(RCX);
    asm.mov_store(R11, 16, RDX);
    asm.dec_mem64(R11, 8);
    asm.pop(RAX);
    asm.ret();
    asm.bind(empty);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn memset() -> Vec<u8> {
    let mut asm = Asm::new();
    let done = asm.label();
    let fill = asm.label();
    asm.mov_reg(R11, RAX);
    asm.test_self(RCX);
    asm.je(done);
    asm.bind(fill);
    asm.store_bl();
    asm.add_imm32(RAX, 1);
    asm.dec_reg(RCX);
    asm.jne(fill);
    asm.bind(done);
    asm.mov_reg(RAX, R11);
    asm.ret();
    asm.finish()
}

fn memmove() -> Vec<u8> {
    let mut asm = Asm::new();
    let done = asm.label();
    let forward = asm.label();
    let back = asm.label();
    let fwd = asm.label();
    asm.mov_reg(R11, RAX);
    asm.cmp_reg(RAX, RBX);
    asm.je(done);
    asm.jb(forward);
    asm.add_reg(RAX, RCX);
    asm.add_reg(RBX, RCX);
    asm.bind(back);
    asm.test_self(RCX);
    asm.je(done);
    asm.add_imm32(RAX, -1);
    asm.add_imm32(RBX, -1);
    asm.copy_byte();
    asm.dec_reg(RCX);
    asm.jmp(back);
    asm.bind(forward);
    asm.bind(fwd);
    asm.test_self(RCX);
    asm.je(done);
    asm.copy_byte();
    asm.add_imm32(RAX, 1);
    asm.add_imm32(RBX, 1);
    asm.dec_reg(RCX);
    asm.jmp(fwd);
    asm.bind(done);
    asm.mov_reg(RAX, R11);
    asm.ret();
    asm.finish()
}

pub(super) fn mmap(asm: &mut Asm, size: u32, prot: u32) {
    asm.xor_self32(RDI);
    asm.mov_imm64(RSI, u64::from(size));
    asm.mov_imm32(RDX, prot);
    asm.mov_imm32(R10, MAP_PRIVATE_ANON);
    asm.mov_imm64(R8, u64::MAX);
    asm.xor_self32(R9);
    asm.mov_imm32(RAX, SYSCALL_MMAP);
    asm.syscall();
}

/// `len` 寄存器里的字节数。syscall 会毁掉 `rcx` 与 `r11`。
pub(super) fn mmap_reg(asm: &mut Asm, len: u8, prot: u32) {
    asm.xor_self32(RDI);
    asm.mov_reg(RSI, len);
    asm.mov_imm32(RDX, prot);
    asm.mov_imm32(R10, MAP_PRIVATE_ANON);
    asm.mov_imm64(R8, u64::MAX);
    asm.xor_self32(R9);
    asm.mov_imm32(RAX, SYSCALL_MMAP);
    asm.syscall();
}

pub(super) fn exit_group(asm: &mut Asm, code: u32) {
    asm.mov_imm32(RDI, code);
    asm.mov_imm32(RAX, SYSCALL_EXIT_GROUP);
    asm.syscall();
}
