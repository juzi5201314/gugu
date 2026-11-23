//! 进程创建与非阻塞观察的薄机器入口；生命周期、排队和取消归 Gugu。
use super::*;
use crate::runtime::process_schema::{
    REQUEST_ARGUMENTS as A, REQUEST_ERROR as E, REQUEST_VALUE as V,
};
mod darwin;
mod linux;
mod windows;
pub(crate) use windows::SYMBOLS as WINDOWS_SYMBOLS;

#[cfg(test)]
pub(super) fn test_call(target: TargetName) -> Builder {
    match target {
        TargetName::X86_64Linux => linux::call(),
        TargetName::X86_64Windows => windows::call(),
        TargetName::X86_64Macos => darwin::call(),
        TargetName::Aarch64Linux | TargetName::Aarch64Macos => {
            unreachable!("aarch64 目标不进入 x64 平台片段")
        }
    }
}
#[cfg(test)]
pub(super) fn test_worker(target: TargetName) -> Builder {
    let mut b = Builder::new();
    match target {
        TargetName::X86_64Linux => linux::worker(&mut b),
        TargetName::X86_64Windows => windows::worker(&mut b),
        TargetName::X86_64Macos => darwin::worker(&mut b),
        TargetName::Aarch64Linux | TargetName::Aarch64Macos => {
            unreachable!("aarch64 目标不进入 x64 平台片段")
        }
    }
    b
}

pub(super) fn call(target: TargetName) -> FragmentBlob {
    blob(
        "process_call",
        match target {
            TargetName::X86_64Linux => linux::call(),
            TargetName::X86_64Windows => windows::call(),
            TargetName::X86_64Macos => darwin::call(),
            TargetName::Aarch64Linux | TargetName::Aarch64Macos => {
                unreachable!("aarch64 目标不进入 x64 平台片段")
            }
        },
    )
}
pub(super) fn worker(target: TargetName) -> FragmentBlob {
    let mut b = Builder::new();
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(mangle::runtime_symbol("process_native_worker")),
                0,
            ),
            reg(gpr(Rax)),
        ],
    );
    ret(&mut b);
    let start = b.instruction_count();
    match target {
        TargetName::X86_64Linux => linux::worker(&mut b),
        TargetName::X86_64Windows => windows::worker(&mut b),
        TargetName::X86_64Macos => darwin::worker(&mut b),
        TargetName::Aarch64Linux | TargetName::Aarch64Macos => {
            unreachable!("aarch64 目标不进入 x64 平台片段")
        }
    }
    let encoded = encode::assemble(&b.finish().sequence).expect("进程 worker 片段编码");
    let offset = encoded
        .instruction_offsets
        .iter()
        .find(|(index, _)| *index == start)
        .expect("worker 入口偏移")
        .1;
    FragmentBlob {
        alignment: 16,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![
            FragmentEntry {
                glue: "process_worker",
                offset: 0,
            },
            FragmentEntry {
                glue: "process_native_worker",
                offset,
            },
        ],
        relocations: encoded.relocations,
    }
}
fn clear_result(b: &mut Builder) {
    set(b, R12, V, 0);
    set(b, R12, E, 0);
}
fn linux_result(b: &mut Builder) {
    let success = b.label();
    let done = b.label();
    compare(b, Rax, 0);
    jump(b, "jge", success);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
    store(b, R12, E, Rax);
    jump(b, "jmp", done);
    b.define(success);
    store(b, R12, V, Rax);
    b.define(done);
}
fn address(b: &mut Builder, dst: Gpr, base: Gpr, displacement: i32) {
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(base), displacement), reg(gpr(dst))],
    );
}
fn dword_load(b: &mut Builder, dst: Gpr, base: Gpr, displacement: i32) {
    b.emit(
        "mov",
        &[K::Rm32, K::R32],
        Access::Read,
        vec![mem_base(gpr(base), displacement), reg(gpr(dst))],
    );
}
fn trap(b: &mut Builder) {
    b.emit("ud2", &[], Access::Read, vec![]);
}
