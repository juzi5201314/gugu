use super::linux_syscall::{ZERO, branch, branch_cond, compare, enter, leave, negate, patch_cond};
use super::{Builder, Fragment, LR, SP, add_imm, imm, movz, reg, unshifted};
use crate::{
    NativeImport,
    a64::{macho::import_stub_symbol, reg::Reg},
    macho::LIB_SYSTEM,
    runtime::platform_schema::PlatformOp,
};

const RESULT: Reg = Reg::x(0);
const EINVAL: u16 = 22;
const EQ: u8 = 0;
const NE: u8 = 1;
const CS: u8 = 2;

/// VM 操作需要的 libSystem 导入；`getpagesize` 仅供 `darwin_page_size`。
pub fn darwin_vm_imports() -> Vec<NativeImport> {
    ["getpagesize", "mmap", "mprotect", "munmap", "__error"]
        .into_iter()
        .map(|name| NativeImport::function(LIB_SYSTEM, name))
        .collect()
}

/// 返回宿主原生页大小：`getpagesize()` 的 int 结果零扩展到 x0。
pub fn darwin_page_size() -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 8);
    call_c(&mut b, "getpagesize");
    b.emit("ubfm.X", [reg(RESULT), reg(RESULT), imm(0), imm(31)]);
    b.memory("ldr.X.Unsigned", LR, SP, 8);
    b.add(SP, SP, 16);
    b.ret();
    b.finish()
}

/// ReserveAligned: x0 = OS 页取整后的 bytes, x1 = 二次幂对齐 → x0 = 对齐基址或 -errno。
/// 其他操作: x0 = (base, bytes)* → x0 = 0 或 -errno。只在已进入 native 栈后调用。
/// Decommit 用 `mmap(MAP_FIXED)` 覆盖清零：Darwin 的 `madvise(MADV_DONTNEED)`
/// 不保证再次访问读到零页。
pub fn darwin_vm(op: PlatformOp) -> Option<Fragment> {
    if op == PlatformOp::ReserveAligned {
        return Some(reserve());
    }
    if op == PlatformOp::Decommit {
        return Some(decommit());
    }
    let (name, argument) = match op {
        PlatformOp::Commit | PlatformOp::Unprotect => ("mprotect", Some(3)),
        PlatformOp::ProtectGuard => ("mprotect", Some(0)),
        PlatformOp::Release => ("munmap", None),
        _ => return None,
    };
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 8);
    b.memory("ldr.X.Unsigned", Reg::x(9), RESULT, 0);
    b.memory("ldr.X.Unsigned", Reg::x(1), RESULT, 8);
    b.add(RESULT, Reg::x(9), 0);
    if let Some(value) = argument {
        b.emit("movz.X", [reg(Reg::x(2)), movz(value)]);
    }
    call_c(&mut b, name);
    b.emit("sbfm.X", [reg(RESULT), reg(RESULT), imm(0), imm(31)]);
    finish_int(&mut b);
    Some(b.finish())
}

/// mmap( base, len, PROT_NONE, MAP_ANON|MAP_PRIVATE|MAP_FIXED, -1, 0 )：
/// 以新的 PROT_NONE 匿名映射替换区间，成功返回 0，失败返回 -errno。
fn decommit() -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 8);
    b.memory("ldr.X.Unsigned", Reg::x(9), RESULT, 0);
    b.memory("ldr.X.Unsigned", Reg::x(1), RESULT, 8);
    b.add(RESULT, Reg::x(9), 0);
    for (r, value) in [(2u8, 0u16), (3, 0x1012), (5, 0)] {
        b.emit("movz.X", [reg(Reg::x(r)), movz(value)]);
    }
    b.emit("movn.X", [reg(Reg::x(4)), movz(0)]);
    call_c(&mut b, "mmap");
    minus_one(&mut b);
    let failed = branch_cond(&mut b, EQ);
    b.emit("movz.X", [reg(RESULT), movz(0)]);
    let done = branch(&mut b);
    patch_cond(&mut b, failed, EQ);
    errno_result(&mut b);
    b.patch_branch(done, "b", &[], &[]);
    b.memory("ldr.X.Unsigned", LR, SP, 8);
    b.add(SP, SP, 16);
    b.ret();
    b.finish()
}

/// int 返回的 C 调用收尾：返回 -1 时经 __error 取 -errno，否则返回 0。
/// 调用方须先把 int 结果符号扩展到 x0。
fn finish_int(b: &mut Builder) {
    minus_one(b);
    let failed = branch_cond(b, EQ);
    b.memory("ldr.X.Unsigned", LR, SP, 8);
    b.add(SP, SP, 16);
    b.ret();
    patch_cond(b, failed, EQ);
    errno_result(b);
    b.memory("ldr.X.Unsigned", LR, SP, 8);
    b.add(SP, SP, 16);
    b.ret();
}

/// cmn：x0 == -1 时置 Z。
fn minus_one(b: &mut Builder) {
    b.emit("adds.X.imm", [reg(ZERO), reg(RESULT), add_imm(1)]);
}

fn errno_result(b: &mut Builder) {
    call_c(b, "__error");
    b.memory("ldr.W.Unsigned", Reg::w(0), RESULT, 0);
    negate(b, RESULT, RESULT);
}

fn call_c(b: &mut Builder, name: &str) {
    b.external_branch("bl", &import_stub_symbol(name));
}

fn reserve() -> Fragment {
    let mut b = Builder::default();
    enter(&mut b, 96);
    for i in 0..4 {
        b.memory(
            "str.X.Unsigned",
            Reg::x(21 + i),
            SP,
            64 + usize::from(i) * 8,
        );
    }
    let (invalid, failed) = map(&mut b);
    let trims = trim(&mut b);
    b.add(RESULT, Reg::x(22), 0);
    let done = branch(&mut b);
    // 参数无效：-EINVAL。
    for (at, cond) in invalid {
        patch_cond(&mut b, at, cond);
    }
    b.emit("movn.X", [reg(RESULT), movz(EINVAL - 1)]);
    let invalid_done = branch(&mut b);
    // trim 失败：经 __error 取 errno 后释放仍拥有的映射范围，返回 -errno。
    for at in trims {
        patch_cond(&mut b, at, EQ);
    }
    errno_result(&mut b);
    b.add(Reg::x(24), RESULT, 0);
    b.add(RESULT, Reg::x(21), 0);
    b.add(Reg::x(1), Reg::x(23), 0);
    call_c(&mut b, "munmap");
    b.add(RESULT, Reg::x(24), 0);
    let trim_done = branch(&mut b);
    // mmap 失败：-errno。
    patch_cond(&mut b, failed, EQ);
    errno_result(&mut b);
    b.patch_branch(done, "b", &[], &[]);
    b.patch_branch(invalid_done, "b", &[], &[]);
    b.patch_branch(trim_done, "b", &[], &[]);
    for i in 0..4 {
        b.memory(
            "ldr.X.Unsigned",
            Reg::x(21 + i),
            SP,
            64 + usize::from(i) * 8,
        );
    }
    leave(&mut b, 96);
    b.finish()
}

/// 校验 x0/x1，把请求长度取整到宿主 OS 页后发出过量预留的 mmap。
/// x19 = 页取整后的 bytes, x20 = align, x23 = bytes + align；返回非法输入分支（保留各自
/// 条件）与 mmap 失败分支偏移（EQ 待补）。
fn map(b: &mut Builder) -> (Vec<(u32, u8)>, u32) {
    b.add(Reg::x(19), RESULT, 0);
    b.add(Reg::x(20), Reg::x(1), 0);
    let mut invalid: Vec<(u32, u8)> = Vec::new();
    for r in [19, 20] {
        compare(b, Reg::x(r), 0);
        invalid.push((branch_cond(b, EQ), EQ));
    }
    b.emit("sub.X.imm", [reg(Reg::x(9)), reg(Reg::x(20)), add_imm(1)]);
    binary(b, "and.X.shift", 10, 20, 9);
    compare(b, Reg::x(10), 0);
    invalid.push((branch_cond(b, NE), NE));
    // 契约要求先把请求长度取整到宿主 OS 页（arm64 Darwin 为 16 KiB，
    // 由 getpagesize 取得，不硬编码），再做过量预留的 mmap；取整溢出按非法输入处理。
    call_c(b, "getpagesize");
    b.add(Reg::x(9), RESULT, 0);
    b.emit("sub.X.imm", [reg(Reg::x(10)), reg(Reg::x(9)), add_imm(1)]);
    b.emit(
        "adds.X.shift",
        [
            reg(Reg::x(19)),
            reg(Reg::x(19)),
            reg(Reg::x(10)),
            unshifted(),
        ],
    );
    invalid.push((branch_cond(b, CS), CS));
    negate(b, Reg::x(9), Reg::x(9));
    binary(b, "and.X.shift", 19, 19, 9);
    b.emit(
        "adds.X.shift",
        [
            reg(Reg::x(23)),
            reg(Reg::x(19)),
            reg(Reg::x(20)),
            unshifted(),
        ],
    );
    invalid.push((branch_cond(b, CS), CS));
    // mmap(0, bytes + align, PROT_NONE, MAP_PRIVATE | MAP_ANON, -1, 0)
    b.add(Reg::x(1), Reg::x(23), 0);
    for (r, value) in [(0u8, 0u16), (2, 0), (3, 0x1002), (5, 0)] {
        b.emit("movz.X", [reg(Reg::x(r)), movz(value)]);
    }
    b.emit("movn.X", [reg(Reg::x(4)), movz(0)]);
    call_c(b, "mmap");
    minus_one(b);
    let failed = branch_cond(b, EQ);
    (invalid, failed)
}

/// x21 = 当前拥有基址, x22 = 对齐基址, x23 = 剩余映射长度, x24 = 保存的 errno。
/// 分别 munmap 过量预留的首尾范围；返回前缀/尾部失败分支偏移（EQ 待补）。
fn trim(b: &mut Builder) -> Vec<u32> {
    // x22 = (base + align - 1) & -align
    b.add(Reg::x(21), RESULT, 0);
    binary(b, "add.X.shift", 22, 21, 20);
    b.emit("sub.X.imm", [reg(Reg::x(22)), reg(Reg::x(22)), add_imm(1)]);
    negate(b, Reg::x(9), Reg::x(20));
    binary(b, "and.X.shift", 22, 22, 9);
    // prefix: [base, aligned)
    binary(b, "sub.X.shift", 1, 22, 21);
    compare(b, Reg::x(1), 0);
    let no_prefix = branch_cond(b, EQ);
    call_c(b, "munmap");
    b.emit("sbfm.X", [reg(RESULT), reg(RESULT), imm(0), imm(31)]);
    minus_one(b);
    let prefix_failed = branch_cond(b, EQ);
    patch_cond(b, no_prefix, EQ);
    // tail: [aligned + bytes, aligned + align - prefix + bytes)
    binary(b, "sub.X.shift", 9, 22, 21);
    binary(b, "sub.X.shift", 1, 20, 9);
    binary(b, "sub.X.shift", 23, 23, 9);
    b.add(Reg::x(21), Reg::x(22), 0);
    binary(b, "add.X.shift", 0, 22, 19);
    call_c(b, "munmap");
    b.emit("sbfm.X", [reg(RESULT), reg(RESULT), imm(0), imm(31)]);
    minus_one(b);
    let tail_failed = branch_cond(b, EQ);
    vec![prefix_failed, tail_failed]
}

fn binary(b: &mut Builder, key: &str, dst: u8, left: u8, right: u8) {
    b.emit(
        key,
        [
            reg(Reg::x(dst)),
            reg(Reg::x(left)),
            reg(Reg::x(right)),
            unshifted(),
        ],
    );
}
