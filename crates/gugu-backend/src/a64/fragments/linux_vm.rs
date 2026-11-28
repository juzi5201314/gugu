use super::linux_syscall::*;
use super::{Builder, Fragment, SP, add_imm, inst_target, mem, movz, reg, unshifted};
use crate::{
    a64::{inst::AddressMode, inst::Operand, reg::Reg},
    runtime::{cage_control::CAGE_CANONICAL_LIMIT, platform_schema::PlatformOp},
};

pub(crate) const LINUX_RESERVE_CURSOR: &str = "__gugu_a64_linux_reserve_cursor";
pub(crate) const LINUX_RESERVE_START: u64 = 1 << 32;
const RESERVE_ATTEMPTS: u16 = 16;
const HINT_GRANULE: u64 = 65536;

/// x0 = envp from Linux process entry; returns AT_PAGESZ, or zero if absent.
pub fn linux_page_size() -> Fragment {
    let mut b = Builder::default();
    compare(&mut b, Reg::x(0), 0);
    let null = branch_cond(&mut b, 0);
    let environment = b.offset();
    b.memory("ldr.X.Unsigned", Reg::x(9), Reg::x(0), 0);
    b.add(Reg::x(0), Reg::x(0), 8);
    b.branch("cbnz.X", &[reg(Reg::x(9))], environment);
    let aux = b.offset();
    b.memory("ldr.X.Unsigned", Reg::x(9), Reg::x(0), 0);
    let other = skip(&mut b, Reg::x(9), 6);
    b.memory("ldr.X.Unsigned", Reg::x(0), Reg::x(0), 8);
    b.ret();
    patch_cond(&mut b, other, 1);
    compare(&mut b, Reg::x(9), 0);
    let missing = branch_cond(&mut b, 0);
    b.add(Reg::x(0), Reg::x(0), 16);
    b.branch("b", &[], aux);
    patch_cond(&mut b, missing, 0);
    patch_cond(&mut b, null, 0);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    b.ret();
    b.finish()
}

/// Reserve: x0 = OS-page-rounded bytes, x1 = alignment. Others: x0 = (base, bytes)*.
/// Raw negative errno returns; native stack and OS-aligned extents are required.
pub fn linux_vm(op: PlatformOp) -> Option<Fragment> {
    if op == PlatformOp::ReserveAligned {
        return Some(reserve());
    }
    let (number, argument) = match op {
        PlatformOp::Commit | PlatformOp::Unprotect => (226, Some(3)),
        PlatformOp::ProtectGuard => (226, Some(0)),
        PlatformOp::Decommit => (233, Some(4)),
        PlatformOp::Release => (215, None),
        _ => return None,
    };
    let mut b = Builder::default();
    b.add(Reg::x(9), Reg::x(0), 0);
    b.memory("ldr.X.Unsigned", Reg::x(0), Reg::x(9), 0);
    b.memory("ldr.X.Unsigned", Reg::x(1), Reg::x(9), 8);
    if let Some(value) = argument {
        b.emit("movz.X", [reg(Reg::x(2)), movz(value)]);
    }
    syscall(&mut b, number);
    b.ret();
    Some(b.finish())
}

fn reserve() -> Fragment {
    let mut b = Builder::default();
    enter(&mut b, 112);
    for i in 0..6 {
        b.memory(
            "str.X.Unsigned",
            Reg::x(21 + i),
            SP,
            64 + usize::from(i) * 8,
        );
    }
    let invalid = validate(&mut b);
    map(&mut b);
    compare(&mut b, Reg::x(0), 0);
    let failed = branch_cond(&mut b, 11);
    let trims = trim(&mut b);
    let done = branch(&mut b);
    for (at, code) in invalid {
        patch_cond(&mut b, at, code);
    }
    b.emit("movn.X", [reg(Reg::x(0)), movz(21)]);
    let invalid_done = branch(&mut b);
    for at in trims {
        patch_cond(&mut b, at, 11);
    }
    b.add(Reg::x(24), Reg::x(0), 0);
    b.add(Reg::x(0), Reg::x(21), 0);
    b.add(Reg::x(1), Reg::x(23), 0);
    syscall(&mut b, 215);
    b.add(Reg::x(0), Reg::x(24), 0);
    b.patch_branch(done, "b", &[], &[]);
    b.patch_branch(invalid_done, "b", &[], &[]);
    patch_cond(&mut b, failed, 11);
    for i in 0..6 {
        b.memory(
            "ldr.X.Unsigned",
            Reg::x(21 + i),
            SP,
            64 + usize::from(i) * 8,
        );
    }
    leave(&mut b, 112);
    b.finish()
}

fn validate(b: &mut Builder) -> Vec<(u32, u8)> {
    b.add(Reg::x(19), Reg::x(0), 0);
    b.add(Reg::x(20), Reg::x(1), 0);
    let mut invalid = Vec::new();
    for r in [19, 20] {
        compare(b, Reg::x(r), 0);
        invalid.push((branch_cond(b, 0), 0));
    }
    b.emit("sub.X.imm", [reg(Reg::x(9)), reg(Reg::x(20)), add_imm(1)]);
    binary(b, "and.X.shift", 10, 20, 9);
    compare(b, Reg::x(10), 0);
    invalid.push((branch_cond(b, 1), 1));
    binary(b, "adds.X.shift", 23, 19, 20);
    invalid.push((branch_cond(b, 2), 2));
    number(b, 9, CAGE_CANONICAL_LIMIT - LINUX_RESERVE_START);
    binary(b, "subs.X.shift", 31, 23, 9);
    invalid.push((branch_cond(b, 8), 8));
    invalid
}

fn map(b: &mut Builder) {
    // 候选地址以最大原生页对齐；跨度和游标均不超过低 47 位上界，和不溢出 uint。
    debug_assert!(HINT_GRANULE.is_power_of_two());
    debug_assert_eq!(
        (CAGE_CANONICAL_LIMIT | LINUX_RESERVE_START) % HINT_GRANULE,
        0
    );
    number(b, 9, HINT_GRANULE - 1);
    binary(b, "add.X.shift", 24, 23, 9);
    number(b, 9, !(HINT_GRANULE - 1));
    binary(b, "and.X.shift", 24, 24, 9);
    let cursor = inst_target(LINUX_RESERVE_CURSOR, 0);
    b.emit("adrp", [reg(Reg::x(26)), cursor.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(26)), reg(Reg::x(26)), cursor]);
    b.emit("movz.X", [reg(Reg::x(25)), movz(RESERVE_ATTEMPTS)]);
    let retry = b.offset();
    compare(b, Reg::x(25), 0);
    let exhausted = branch_cond(b, 0);
    b.emit("sub.X.imm", [reg(Reg::x(25)), reg(Reg::x(25)), add_imm(1)]);
    take_hint(b, retry);
    b.add(Reg::x(1), Reg::x(23), 0);
    for (r, value) in [(2, 0), (3, 34), (5, 0)] {
        b.emit("movz.X", [reg(Reg::x(r)), movz(value)]);
    }
    b.emit("movn.X", [reg(Reg::x(4)), movz(0)]);
    syscall(b, 222);
    compare(b, Reg::x(0), 0);
    let failed = branch_cond(b, 11);
    let zero = branch_cond(b, 0);
    number(b, 9, CAGE_CANONICAL_LIMIT);
    binary(b, "sub.X.shift", 9, 9, 23);
    binary(b, "subs.X.shift", 31, 0, 9);
    let accepted = branch_cond(b, 9);
    patch_cond(b, zero, 0);
    // 普通 hint 不覆盖既有映射；内核可忽略它，因此先验证整段，再释放超界回包。
    b.add(Reg::x(1), Reg::x(23), 0);
    syscall(b, 215);
    compare(b, Reg::x(0), 0);
    let release_failed = branch_cond(b, 11);
    b.branch("b", &[], retry);
    patch_cond(b, exhausted, 0);
    b.emit("movn.X", [reg(Reg::x(0)), movz(11)]);
    patch_cond(b, failed, 11);
    patch_cond(b, release_failed, 11);
    patch_cond(b, accepted, 9);
}

fn take_hint(b: &mut Builder, retry: u32) {
    let address = mem(Reg::x(26), AddressMode::Base);
    b.emit("ldxr.X", [reg(Reg::x(0)), address.clone()]);
    binary(b, "add.X.shift", 10, 0, 24);
    number(b, 9, CAGE_CANONICAL_LIMIT);
    binary(b, "subs.X.shift", 31, 10, 9);
    let fits = branch_cond(b, 9);
    number(b, 0, LINUX_RESERVE_START);
    binary(b, "add.X.shift", 10, 0, 24);
    patch_cond(b, fits, 9);
    // 游标只分配 hint，无关联数据发布；沿用平台原子 RMW 的 relaxed 独占读写。
    b.emit("stxr.X", [reg(Reg::w(11)), reg(Reg::x(10)), address]);
    b.branch("cbnz.W", &[reg(Reg::w(11))], retry);
}

fn number(b: &mut Builder, register: u8, value: u64) {
    for shift in [0, 16, 32, 48] {
        let part = (value >> shift) as u16;
        if shift == 0 || part != 0 {
            b.emit(
                if shift == 0 { "movz.X" } else { "movk.X" },
                [reg(Reg::x(register)), Operand::Wide { imm16: part, shift }],
            );
        }
    }
}

fn trim(b: &mut Builder) -> Vec<u32> {
    b.add(Reg::x(21), Reg::x(0), 0);
    binary(b, "add.X.shift", 22, 21, 20);
    b.emit("sub.X.imm", [reg(Reg::x(22)), reg(Reg::x(22)), add_imm(1)]);
    negate(b, Reg::x(9), Reg::x(20));
    binary(b, "and.X.shift", 22, 22, 9);
    binary(b, "sub.X.shift", 1, 22, 21);
    compare(b, Reg::x(1), 0);
    let no_prefix = branch_cond(b, 0);
    syscall(b, 215);
    compare(b, Reg::x(0), 0);
    let prefix_failed = branch_cond(b, 11);
    patch_cond(b, no_prefix, 0);
    binary(b, "sub.X.shift", 9, 22, 21);
    binary(b, "sub.X.shift", 1, 20, 9);
    binary(b, "sub.X.shift", 23, 23, 9);
    b.add(Reg::x(21), Reg::x(22), 0);
    binary(b, "add.X.shift", 0, 22, 19);
    syscall(b, 215);
    compare(b, Reg::x(0), 0);
    let tail_failed = branch_cond(b, 11);
    b.add(Reg::x(0), Reg::x(22), 0);
    vec![prefix_failed, tail_failed]
}

fn binary(b: &mut Builder, key: &str, dst: u8, left: u8, right: u8) {
    b.emit(
        key,
        [
            reg(if dst == 31 { ZERO } else { Reg::x(dst) }),
            reg(Reg::x(left)),
            reg(Reg::x(right)),
            unshifted(),
        ],
    );
}
