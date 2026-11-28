use super::{Builder, Fragment, LR, SP, add_imm, condition, imm, movz, next, reg};
use crate::{
    NativeImport,
    a64::{
        macho::import_stub_symbol,
        reg::{Reg, Width},
    },
    macho::LIB_SYSTEM,
    runtime::foreign_schema::{
        NATIVE_REQUEST_ARGUMENTS, NATIVE_REQUEST_ERROR, NATIVE_REQUEST_OPERATION,
        NATIVE_REQUEST_THREAD_HANDLE, NATIVE_REQUEST_VALUE,
    },
};

struct Method {
    name: &'static str,
    fixed: u8,
    variadic_int: bool,
    result: ResultAbi,
}

enum ResultAbi {
    Int,
    WideInt,
    Pointer,
    DirectoryEntry,
}

use ResultAbi::{DirectoryEntry, Int, Pointer, WideInt};

const METHODS: [Method; 23] = [
    method("read", 3, false, WideInt),
    method("write", 3, false, WideInt),
    method("open", 2, true, Int),
    method("close", 1, false, Int),
    method("lseek", 3, false, WideInt),
    method("stat", 2, false, Int),
    method("lstat", 2, false, Int),
    method("fstat", 2, false, Int),
    method("ftruncate", 2, false, Int),
    method("fsync", 1, false, Int),
    method("fcntl", 2, true, Int),
    method("opendir", 1, false, Pointer),
    method("readdir", 1, false, DirectoryEntry),
    method("closedir", 1, false, Int),
    method("mkdir", 2, false, Int),
    method("unlink", 1, false, Int),
    method("rmdir", 1, false, Int),
    method("rename", 2, false, Int),
    method("link", 2, false, Int),
    method("symlink", 2, false, Int),
    method("readlink", 3, false, WideInt),
    method("realpath", 2, false, Pointer),
    method("getentropy", 2, false, Int),
];

const fn method(name: &'static str, fixed: u8, variadic_int: bool, result: ResultAbi) -> Method {
    Method {
        name,
        fixed,
        variadic_int,
        result,
    }
}

const REQUEST: Reg = Reg::x(19);
const FP: Reg = Reg::x(29);
const RESULT: Reg = Reg::x(0);
const XZR: Reg = Reg::zr(Width::X);
const EINVAL: u16 = 22;
const EQ: u8 = 0;
const NE: u8 = 1;

pub fn darwin_fd_imports() -> Vec<NativeImport> {
    METHODS
        .iter()
        .map(|method| method.name)
        .chain(["__error"])
        .map(|name| NativeImport::function(LIB_SYSTEM, name))
        .collect()
}

/// x0 = NativeRequest；只在已进入 native 栈后调用，不管理线程。
/// `publish_done` 区分 x64 `worker(thread)` 两个变体：fs_native_worker 线程入口
/// 返回前发布 DONE=1，callback_file_worker 路径不发布。
pub fn darwin_fd_dispatch(publish_done: bool) -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(32)]);
    for (offset, saved) in [(8, REQUEST), (16, FP), (24, LR)] {
        b.memory("str.X.Unsigned", saved, SP, offset);
    }
    b.add(FP, SP, 16);
    b.add(REQUEST, RESULT, 0);
    let operation = Reg::x(9);
    b.memory(
        "ldr.X.Unsigned",
        operation,
        REQUEST,
        NATIVE_REQUEST_OPERATION,
    );
    let mut errors = Vec::new();
    let mut exits = Vec::new();
    for (number, method) in METHODS.iter().enumerate() {
        b.emit("subs.X.imm", [reg(XZR), reg(operation), add_imm(number)]);
        let skip = branch_cond(&mut b, NE);
        call_method(&mut b, method);
        errors.push(branch_cond(&mut b, EQ));
        if !matches!(method.result, DirectoryEntry) {
            store_value(&mut b);
        }
        exits.push(branch(&mut b));
        b.patch_branch(skip, "b.cond", &[], &[condition(NE)]);
    }
    b.emit("movz.X", [reg(RESULT), movz(EINVAL)]);
    let invalid = branch(&mut b);
    for at in errors {
        b.patch_branch(at, "b.cond", &[], &[condition(EQ)]);
    }
    b.external_branch("bl", &import_stub_symbol("__error"));
    b.memory("ldr.W.Unsigned", Reg::w(0), RESULT, 0);
    b.patch_branch(invalid, "b", &[], &[]);
    b.memory("str.X.Unsigned", RESULT, REQUEST, NATIVE_REQUEST_ERROR);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    if publish_done {
        // 与 x64 worker 同一完成契约：线程入口在返回前发布 DONE=1，
        // fs_poll 读 [THREAD_HANDLE] 非零即完成。
        b.emit("movz.X", [reg(Reg::x(9)), movz(1)]);
        b.memory(
            "str.X.Unsigned",
            Reg::x(9),
            REQUEST,
            NATIVE_REQUEST_THREAD_HANDLE,
        );
    }
    for (offset, saved) in [(8, REQUEST), (16, FP), (24, LR)] {
        b.memory("ldr.X.Unsigned", saved, SP, offset);
    }
    b.emit("movz.X", [reg(RESULT), movz(0)]);
    b.add(SP, SP, 32);
    b.ret();
    b.finish()
}

fn call_method(b: &mut Builder, method: &Method) {
    if matches!(method.result, DirectoryEntry) {
        b.external_branch("bl", &import_stub_symbol("__error"));
        b.memory("str.W.Unsigned", Reg::zr(Width::W), RESULT, 0);
    }
    for i in 0..method.fixed {
        b.memory(
            "ldr.X.Unsigned",
            Reg::x(i),
            REQUEST,
            NATIVE_REQUEST_ARGUMENTS + usize::from(i) * 8,
        );
    }
    if method.variadic_int {
        let mode = Reg::w(10);
        b.memory(
            "ldr.W.Unsigned",
            mode,
            REQUEST,
            NATIVE_REQUEST_ARGUMENTS + 16,
        );
        b.memory("str.W.Unsigned", mode, SP, 0);
    }
    b.external_branch("bl", &import_stub_symbol(method.name));
    if matches!(method.result, Int) {
        b.emit("sbfm.X", [reg(RESULT), reg(RESULT), imm(0), imm(31)]);
    }
    if matches!(method.result, DirectoryEntry) {
        store_value(b);
    }
    match method.result {
        Pointer | DirectoryEntry => b.emit("subs.X.imm", [reg(XZR), reg(RESULT), add_imm(0)]),
        Int | WideInt => b.emit("adds.X.imm", [reg(XZR), reg(RESULT), add_imm(1)]),
    }
}

fn store_value(b: &mut Builder) {
    b.memory("str.X.Unsigned", RESULT, REQUEST, NATIVE_REQUEST_VALUE);
}

fn branch_cond(b: &mut Builder, cond: u8) -> u32 {
    let at = b.offset();
    b.emit("b.cond", [next(), condition(cond)]);
    at
}

fn branch(b: &mut Builder) -> u32 {
    let at = b.offset();
    b.emit("b", [next()]);
    at
}
