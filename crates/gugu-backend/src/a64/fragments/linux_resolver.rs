use super::{Builder, Fragment, Operand, Target, add_imm, condition, imm, next, reg};
use crate::{
    NativeImport,
    a64::reg::{Reg, Width},
};

const METHODS: [&str; 2] = ["getaddrinfo", "freeaddrinfo"];

pub fn linux_resolver_imports(library: &str) -> Vec<NativeImport> {
    METHODS
        .map(|name| NativeImport::function(library, name))
        .into()
}

pub fn linux_import_slot_symbol(name: &str) -> String {
    format!("__gugu_linux_import_{name}")
}

pub fn linux_import_stub(name: &str) -> Fragment {
    let mut b = Builder::default();
    let target = Operand::Target(Target::Symbol {
        name: linux_import_slot_symbol(name),
        addend: 0,
    });
    b.emit("adrp", [reg(Reg::x(16)), target.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(16)), reg(Reg::x(16)), target]);
    b.memory("ldr.X.Unsigned", Reg::x(16), Reg::x(16), 0);
    b.emit("br", [reg(Reg::x(16))]);
    b.finish()
}

/// x0 selects getaddrinfo/freeaddrinfo; returns a C ABI import stub address.
pub fn linux_resolver_symbol() -> Fragment {
    let mut b = Builder::default();
    for (op, name) in METHODS.into_iter().enumerate() {
        b.emit(
            "subs.X.imm",
            [reg(Reg::zr(Width::X)), reg(Reg::x(0)), add_imm(op)],
        );
        let skip = b.offset();
        b.emit("b.cond", [next(), condition(1)]);
        let target = Operand::Target(Target::Symbol {
            name: name.into(),
            addend: 0,
        });
        b.emit("adrp", [reg(Reg::x(0)), target.clone()]);
        b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), target]);
        b.ret();
        b.patch_branch(skip, "b.cond", &[], &[condition(1)]);
    }
    b.emit("brk", [imm(0)]);
    b.finish()
}
