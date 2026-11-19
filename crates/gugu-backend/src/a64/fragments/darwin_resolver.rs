use super::{Builder, Fragment, Operand, Target, add_imm, condition, imm, next, reg};
use crate::{
    NativeImport,
    a64::{
        macho::import_stub_symbol,
        reg::{Reg, Width},
    },
    macho::LIB_SYSTEM,
};

const METHODS: [&str; 2] = ["getaddrinfo", "freeaddrinfo"];

pub fn darwin_resolver_imports() -> Vec<NativeImport> {
    METHODS
        .map(|name| NativeImport::function(LIB_SYSTEM, name))
        .into()
}

/// x0 = 操作号；返回可按 C ABI 调用的 libSystem 桩地址。
pub fn darwin_resolver_symbol() -> Fragment {
    let mut b = Builder::default();
    let result = reg(Reg::x(0));
    for (op, name) in METHODS.into_iter().enumerate() {
        b.emit(
            "subs.X.imm",
            [reg(Reg::zr(Width::X)), result.clone(), add_imm(op)],
        );
        let skip = b.offset();
        b.emit("b.cond", [next(), condition(1)]);
        let target = Operand::Target(Target::Symbol {
            name: import_stub_symbol(name),
            addend: 0,
        });
        b.emit("adrp", [result.clone(), target.clone()]);
        b.emit("add.X.imm", [result.clone(), result.clone(), target]);
        b.ret();
        b.patch_branch(skip, "b.cond", &[], &[condition(1)]);
    }
    b.emit("brk", [imm(0)]);
    b.finish()
}
