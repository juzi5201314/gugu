//! 其余运行时入口。参数与返回按内部 ABI；不把 `exit_group` 当成成功路径。

use super::BootOffsets;
use super::asm::Asm;
use super::runtime::{CHAN_CLOSED, PROT_RW, SYSCALL_MPROTECT, exit_group, mmap, mmap_reg};

const PROT_NONE: u32 = 0;
const SYSCALL_MUNMAP: u32 = 11;
const SYSCALL_GETRANDOM: u32 = 318;
const PAGE: i32 = 4096;

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

pub(super) fn bodies(boot: BootOffsets) -> Vec<(&'static str, Vec<u8>, bool)> {
    let mut out = Vec::new();
    out.extend(memory_bodies());
    out.extend(channel_bodies());
    out.extend(sched_bodies(boot));
    out.extend(platform_bodies());
    out
}

fn memory_bodies() -> Vec<(&'static str, Vec<u8>, bool)> {
    let fence = fence_ret();
    let mut out = vec![
        ("value_copy", value_copy(), true),
        ("value_transfer", value_transfer(), true),
        ("value_repeat", value_repeat(), true),
        ("cow_snapshot", cow_snapshot(), true),
        ("concat", concat_strings(), true),
        ("format", empty_pair(), true),
        ("utf8_boundary", utf8_boundary(), true),
        ("wide_div", wide_div(), true),
        ("type_is", type_is(), true),
        ("downcast", downcast(), true),
        ("downcast_copy", downcast_copy(), true),
        ("join_wait", join_wait(), true),
        ("defer_push", defer_push(), true),
        ("defer_action", defer_action(), true),
        ("defer_environment", defer_environment(), true),
        ("defer_pop", defer_pop(), true),
        ("coroutine_switch", coroutine_switch(), true),
    ];
    publish(
        &[
            "value_publish",
            "value_drop",
            "value_forget",
            "pin",
            "unpin",
            "resource_transfer",
            "gc_region_promote",
            "gc_region_transfer",
            "gc_mark_ticket_batch",
            "gc_edge_delta_batch",
            "gc_shared_field_barrier",
            "gc_forward_shared_handle",
        ],
        fence,
        &mut out,
    );
    out
}

fn channel_bodies() -> Vec<(&'static str, Vec<u8>, bool)> {
    vec![
        ("channel_close", channel_close(), true),
        ("channel_try_send", channel_try_send(), true),
        ("channel_try_recv", channel_try_recv(), true),
    ]
}

fn sched_bodies(boot: BootOffsets) -> Vec<(&'static str, Vec<u8>, bool)> {
    let poll = clear_poll(boot);
    let mut out = vec![
        ("resource_acquire", resource_acquire(boot), true),
        ("resource_release", resource_release(boot), true),
        ("resource_finalize", resource_release(boot), true),
    ];
    publish(&["sched_park", "sched_ready"], poll, &mut out);
    out
}

fn platform_bodies() -> Vec<(&'static str, Vec<u8>, bool)> {
    let mut out = vec![
        ("platform_reserve_aligned", reserve_aligned(), true),
        ("platform_commit", protect_header(PROT_RW), true),
        ("platform_decommit", protect_header(PROT_NONE), true),
        ("platform_release", release_range(), true),
        ("platform_protect_guard", guard_page(PROT_NONE), true),
        ("platform_unprotect", guard_page(PROT_RW), true),
        ("platform_zero", zero_range(), true),
        ("platform_entropy", entropy(), true),
        ("platform_wait", platform_wait(), true),
        ("platform_wake", platform_wake(), true),
        ("platform_low_memory_hint", low_memory(), true),
    ];
    publish(
        &["platform_huge_page_hint", "platform_set_dump_policy"],
        fence_ret(),
        &mut out,
    );
    out
}

fn publish(names: &[&'static str], bytes: Vec<u8>, out: &mut Vec<(&'static str, Vec<u8>, bool)>) {
    for name in names {
        out.push((*name, bytes.clone(), true));
    }
}

fn fence_ret() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mfence();
    asm.ret();
    asm.finish()
}

fn empty_pair() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.xor_self32(RAX);
    asm.xor_self32(RBX);
    asm.ret();
    asm.finish()
}

fn clear_poll(boot: BootOffsets) -> Vec<u8> {
    let mut asm = Asm::new();
    let poll = i32::try_from(boot.poll).expect("poll 偏移");
    asm.xor_self32(RAX);
    asm.mov_store32(15, poll, RAX);
    asm.ret();
    asm.finish()
}

fn resource_acquire(boot: BootOffsets) -> Vec<u8> {
    let mut asm = Asm::new();
    asm.inc_mem64(15, lease_slot(boot));
    asm.ret();
    asm.finish()
}

fn resource_release(boot: BootOffsets) -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let slot = lease_slot(boot);
    asm.mov_load(RAX, 15, slot);
    asm.test_self(RAX);
    asm.je(fail);
    asm.dec_mem64(15, slot);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn lease_slot(boot: BootOffsets) -> i32 {
    i32::try_from(boot.barrier + 16).expect("租约槽")
}

fn value_copy() -> Vec<u8> {
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
    asm.finish()
}

fn value_transfer() -> Vec<u8> {
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
    asm.finish()
}

fn value_repeat() -> Vec<u8> {
    let mut asm = Asm::new();
    let step = asm.label();
    let done = asm.label();
    asm.push(RAX);
    asm.mov_reg(R8, RBX);
    asm.mov_reg(R9, RCX);
    asm.mov_load(R10, RDX, 0);
    asm.mov_reg(R11, RAX);
    asm.bind(step);
    asm.test_self(R9);
    asm.je(done);
    asm.test_self(R10);
    asm.je(done);
    asm.mov_reg(RAX, R11);
    asm.mov_reg(RBX, R8);
    asm.mov_reg(RCX, R10);
    copy_bytes(&mut asm);
    asm.add_reg(R11, R10);
    asm.dec_reg(R9);
    asm.jmp(step);
    asm.bind(done);
    asm.pop(RAX);
    asm.ret();
    asm.finish()
}

fn cow_snapshot() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let empty = asm.label();
    asm.mov_load(RCX, RBX, 0);
    asm.test_self(RCX);
    asm.je(empty);
    asm.push(RAX);
    asm.push(RCX);
    asm.mov_reg(RBX, RCX);
    mmap_reg(&mut asm, RBX, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.pop(RCX);
    asm.pop(RBX);
    asm.push(RAX);
    copy_bytes(&mut asm);
    asm.pop(RAX);
    asm.ret();
    asm.bind(empty);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn concat_strings() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let empty = asm.label();
    asm.push(RAX);
    asm.push(RBX);
    asm.push(RCX);
    asm.push(RDX);
    asm.mov_load(RAX, RSP, 16);
    asm.add_reg(RAX, RDX);
    asm.jb(fail);
    asm.test_self(RAX);
    asm.je(empty);
    asm.mov_reg(RBX, RAX);
    mmap_reg(&mut asm, RBX, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.push(RAX);
    copy_piece(&mut asm, 32, 24, false);
    copy_piece(&mut asm, 16, 8, true);
    asm.mov_load(RBX, RSP, 24);
    asm.mov_load(RCX, RSP, 8);
    asm.add_reg(RBX, RCX);
    asm.pop(RAX);
    asm.add_imm32(RSP, 32);
    asm.ret();
    asm.bind(empty);
    asm.xor_self32(RAX);
    asm.xor_self32(RBX);
    asm.add_imm32(RSP, 32);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn copy_piece(asm: &mut Asm, src: i32, len: i32, shift: bool) {
    asm.mov_load(RAX, RSP, 0);
    if shift {
        asm.mov_load(RCX, RSP, 24);
        asm.add_reg(RAX, RCX);
    }
    asm.mov_load(RBX, RSP, src);
    asm.mov_load(RCX, RSP, len);
    copy_bytes(asm);
}

fn utf8_boundary() -> Vec<u8> {
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
    asm.bind(found);
    asm.mov_reg(RAX, RCX);
    asm.ret();
    asm.finish()
}

fn wide_div() -> Vec<u8> {
    let mut asm = Asm::new();
    let slow = asm.label();
    let fail = asm.label();
    asm.test_self(RDX);
    asm.jne(slow);
    asm.test_self(RCX);
    asm.je(fail);
    asm.push(RAX);
    asm.mov_reg(RAX, RBX);
    asm.xor_self32(RDX);
    asm.div_r64(RCX);
    asm.mov_reg(RBX, RAX);
    asm.pop(RAX);
    asm.div_r64(RCX);
    asm.ret();
    asm.bind(slow);
    emit_wide_long(&mut asm);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn emit_wide_long(asm: &mut Asm) {
    let step = asm.label();
    let subtract = asm.label();
    let next = asm.label();
    asm.mov_reg(R8, RAX);
    asm.mov_reg(R9, RBX);
    asm.mov_reg(R10, RCX);
    asm.mov_reg(R11, RDX);
    asm.xor_self32(RAX);
    asm.xor_self32(RBX);
    asm.xor_self32(RCX);
    asm.xor_self32(RDX);
    asm.mov_imm32(RSI, 128);
    asm.bind(step);
    asm.shl1(RAX);
    asm.rcl1(RBX);
    asm.shl1(R8);
    asm.rcl1(R9);
    asm.rcl1(RCX);
    asm.rcl1(RDX);
    asm.cmp_reg(RDX, R11);
    asm.jb(next);
    asm.ja(subtract);
    asm.cmp_reg(RCX, R10);
    asm.jb(next);
    asm.bind(subtract);
    asm.sub_reg(RCX, R10);
    asm.sbb_reg(RDX, R11);
    asm.or_imm8(RAX, 1);
    asm.bind(next);
    asm.dec_reg(RSI);
    asm.jne(step);
}

fn type_is() -> Vec<u8> {
    let mut asm = Asm::new();
    let same = asm.label();
    asm.mov_imm32(RCX, 1);
    asm.cmp_mem(RBX, RAX, 0);
    asm.je(same);
    asm.xor_self32(RCX);
    asm.bind(same);
    asm.mov_reg(RAX, RCX);
    asm.ret();
    asm.finish()
}

fn downcast() -> Vec<u8> {
    let mut asm = Asm::new();
    let miss = asm.label();
    asm.cmp_mem(RBX, RAX, 0);
    asm.jne(miss);
    asm.mov_reg(RBX, RAX);
    asm.add_imm32(RBX, 8);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(miss);
    asm.mov_imm32(RAX, 1);
    asm.xor_self32(RBX);
    asm.ret();
    asm.finish()
}

fn downcast_copy() -> Vec<u8> {
    let mut asm = Asm::new();
    let miss = asm.label();
    asm.cmp_mem(RBX, RAX, 0);
    asm.jne(miss);
    asm.mov_load(RBX, RAX, 8);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(miss);
    asm.mov_imm32(RAX, 1);
    asm.xor_self32(RBX);
    asm.ret();
    asm.finish()
}

fn join_wait() -> Vec<u8> {
    let mut asm = Asm::new();
    let done = asm.label();
    let fail = asm.label();
    asm.mov_load(RCX, RAX, 16);
    asm.test_self(RCX);
    asm.jne(done);
    asm.mov_load(RCX, RAX, 0);
    asm.test_self(RCX);
    asm.je(fail);
    asm.push(RAX);
    asm.mov_load(RAX, RAX, 8);
    asm.call_reg(RCX);
    asm.pop(RCX);
    asm.mov_store(RCX, 24, RAX);
    asm.mov_imm32(RDX, 1);
    asm.mov_store(RCX, 16, RDX);
    asm.mov_reg(RBX, RAX);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(done);
    asm.mov_load(RBX, RAX, 24);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn defer_push() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    asm.push(RAX);
    asm.push(RBX);
    mmap(&mut asm, 32, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.pop(RBX);
    asm.pop(RCX);
    asm.mov_store(RAX, 0, RCX);
    asm.mov_store(RAX, 8, RBX);
    asm.xor_self32(RCX);
    asm.mov_store(RAX, 16, RCX);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn defer_action() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mov_load(RAX, RAX, 0);
    asm.ret();
    asm.finish()
}

fn defer_environment() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mov_store(RBX, 16, RCX);
    asm.mov_reg(RAX, RBX);
    asm.ret();
    asm.finish()
}

fn defer_pop() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mov_load(RAX, RAX, 8);
    asm.ret();
    asm.finish()
}

fn coroutine_switch() -> Vec<u8> {
    crate::runtime::ContextSwitchCode::fixed().bytes
}

fn channel_close() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    asm.movzx8(RCX, RAX, CHAN_CLOSED);
    asm.test_self(RCX);
    asm.jne(fail);
    asm.mov_imm32(RCX, 1);
    asm.mov_store(RAX, CHAN_CLOSED, RCX);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 101);
    asm.finish()
}

fn channel_try_send() -> Vec<u8> {
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
    asm.finish()
}

fn channel_try_recv() -> Vec<u8> {
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
    asm.finish()
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

fn reserve_aligned() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    asm.test_self(RAX);
    asm.je(fail);
    asm.test_self(RBX);
    asm.je(fail);
    asm.mov_reg(RCX, RBX);
    asm.sub_imm32(RCX, 1);
    asm.and_reg(RCX, RBX);
    asm.jne(fail);
    asm.mov_reg(RCX, RAX);
    asm.add_imm32(RCX, PAGE - 1);
    asm.jb(fail);
    asm.and_imm32(RCX, -PAGE);
    asm.push(RAX);
    asm.push(RCX);
    mmap(&mut asm, 4096, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.push(RAX);
    asm.mov_load(RBX, RSP, 8);
    mmap_reg(&mut asm, RBX, PROT_NONE);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    store_reservation(&mut asm);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn store_reservation(asm: &mut Asm) {
    asm.mov_load(RDI, RSP, 0);
    asm.mov_store(RDI, 0, RAX);
    asm.mov_load(RCX, RSP, 8);
    asm.mov_store(RDI, 8, RCX);
    asm.mov_load(RCX, RSP, 16);
    asm.mov_store(RDI, 16, RCX);
    asm.mov_reg(RAX, RDI);
    asm.add_imm32(RSP, 24);
}

fn protect_header(prot: u32) -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    asm.mov_load(RDI, RAX, 0);
    asm.mov_load(RSI, RAX, 8);
    asm.mov_imm32(RDX, prot);
    asm.mov_imm32(RAX, SYSCALL_MPROTECT);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn release_range() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    asm.push(RAX);
    asm.mov_load(RDI, RAX, 0);
    asm.mov_load(RSI, RAX, 8);
    asm.mov_imm32(RAX, SYSCALL_MUNMAP);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.pop(RDI);
    asm.mov_imm64(RSI, 4096);
    asm.mov_imm32(RAX, SYSCALL_MUNMAP);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn guard_page(prot: u32) -> Vec<u8> {
    let mut asm = Asm::new();
    let tail = asm.label();
    let protect = asm.label();
    let fail = asm.label();
    asm.mov_load(RDI, RAX, 0);
    asm.mov_load(RSI, RAX, 8);
    asm.cmp_imm32(RSI, PAGE);
    asm.ja(tail);
    asm.jmp(protect);
    asm.bind(tail);
    asm.add_reg(RDI, RSI);
    asm.sub_imm32(RDI, PAGE);
    asm.mov_imm64(RSI, u64::try_from(PAGE).expect("页大小"));
    asm.bind(protect);
    asm.mov_imm32(RDX, prot);
    asm.mov_imm32(RAX, SYSCALL_MPROTECT);
    asm.syscall();
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn zero_range() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mov_load(RCX, RAX, 16);
    asm.mov_load(RAX, RAX, 0);
    zero_rax(&mut asm);
    asm.ret();
    asm.finish()
}

fn entropy() -> Vec<u8> {
    let mut asm = Asm::new();
    let fail = asm.label();
    let empty = asm.label();
    asm.test_self(RAX);
    asm.je(empty);
    asm.mov_reg(RBX, RAX);
    mmap_reg(&mut asm, RBX, PROT_RW);
    asm.cmp_imm32(RAX, -4096);
    asm.jae(fail);
    asm.push(RAX);
    asm.mov_reg(RDI, RAX);
    asm.mov_reg(RSI, RBX);
    asm.xor_self32(RDX);
    asm.mov_imm32(RAX, SYSCALL_GETRANDOM);
    asm.syscall();
    asm.cmp_reg(RAX, RBX);
    asm.jne(fail);
    asm.pop(RAX);
    asm.ret();
    asm.bind(empty);
    asm.xor_self32(RAX);
    asm.ret();
    asm.bind(fail);
    exit_group(&mut asm, 127);
    asm.finish()
}

fn platform_wait() -> Vec<u8> {
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
    asm.finish()
}

fn platform_wake() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.mfence();
    asm.xor_self32(RAX);
    asm.ret();
    asm.finish()
}

fn low_memory() -> Vec<u8> {
    let mut asm = Asm::new();
    asm.xor_self32(RAX);
    asm.ret();
    asm.finish()
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
