//! 深度转为栈地址后不再进入可增长栈的 Gugu 函数；复制尾跳既有内存片段。
use super::*;

pub(super) fn cage_control() -> Builder {
    let mut b = Builder::new();
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![Operand::Rip(RelocTarget::CageControl, 0), reg(gpr(Rax))],
    );
    ret(&mut b);
    b
}

pub(super) fn read_word() -> Builder {
    let mut b = Builder::new();
    address(&mut b);
    load(&mut b, Rax, R11, 0);
    ret(&mut b);
    b
}

fn address(b: &mut Builder) {
    load(b, R11, R14, 80);
    b.emit(
        "sub",
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(R11)), reg(gpr(Rax))],
    );
}

pub(super) fn copy_raw() -> Builder {
    let mut b = Builder::new();
    address(&mut b);
    let to_stack = b.label();
    let copy = b.label();
    compare(&mut b, Rdx, 0);
    jump(&mut b, "jne", to_stack);
    rr(&mut b, Rax, Rbx);
    rr(&mut b, Rbx, R11);
    jump(&mut b, "jmp", copy);
    b.define(to_stack);
    rr(&mut b, Rax, R11);
    b.define(copy);
    b.emit(
        "jmp",
        &[K::Rel32],
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::glue_symbol("memcpy")),
            RelocKind::PcRel32,
        )],
    );
    b
}
