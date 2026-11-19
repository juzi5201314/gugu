use crate::a64::{
    inst::{AddressMode, Operand},
    reg::{Reg, Width},
};
use crate::gir::body::MemoryOrdering;
use crate::lir::body::AtomicOp;

use super::{Builder, Fragment, condition, imm, mem, movz, next, reg, unshifted};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicWidth {
    Byte,
    Half,
    Word,
    Doubleword,
}

impl AtomicWidth {
    fn suffix(self) -> &'static str {
        match self {
            Self::Byte => "B",
            Self::Half => "H",
            Self::Word => "W",
            Self::Doubleword => "X",
        }
    }

    fn register(self, number: u8) -> Reg {
        match self {
            Self::Doubleword => Reg::x(number),
            _ => Reg::w(number),
        }
    }

    fn ordinary_form(self, operation: &str) -> String {
        let size = match self {
            Self::Byte => "b.W",
            Self::Half => "h.W",
            Self::Word => ".W",
            Self::Doubleword => ".X",
        };
        format!("{operation}{size}.Unsigned")
    }
}

/// x0=地址，x1=写入值或 CAS 期望值，x2=CAS 新值。
/// load/RMW 在 x0 返回原值（窄值零扩展），CAS 另在 w1 返回是否交换。
/// CAS 以成功内存序选择独占读写；合法的失败序不比该读更强。
pub fn atomic(
    op: AtomicOp,
    width: AtomicWidth,
    ordering: MemoryOrdering,
    failure: Option<MemoryOrdering>,
) -> Option<Fragment> {
    use MemoryOrdering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
    if op == AtomicOp::CompareExchange {
        match failure {
            Some(Relaxed) => {}
            Some(Acquire) if matches!(ordering, Acquire | AcqRel | SeqCst) => {}
            Some(SeqCst) if ordering == SeqCst => {}
            _ => return None,
        }
    } else if failure.is_some() {
        return None;
    }
    let acquire = matches!(ordering, Acquire | AcqRel | SeqCst);
    let release = matches!(ordering, Release | AcqRel | SeqCst);
    let mut b = Builder::default();
    let address = Reg::x(0);
    let access = mem(address, AddressMode::Base);
    match op {
        AtomicOp::Load if matches!(ordering, Relaxed | Acquire | SeqCst) => {
            let result = width.register(0);
            if acquire {
                b.emit(&format!("ldar.{}", width.suffix()), [reg(result), access]);
            } else {
                b.memory(&width.ordinary_form("ldr"), result, address, 0);
            }
        }
        AtomicOp::Store if matches!(ordering, Relaxed | Release | SeqCst) => {
            let value = width.register(1);
            if release {
                b.emit(&format!("stlr.{}", width.suffix()), [reg(value), access]);
            } else {
                b.memory(&width.ordinary_form("str"), value, address, 0);
            }
        }
        AtomicOp::Fence if ordering != Relaxed => {
            b.emit("dmb", [imm(if ordering == Acquire { 9 } else { 11 })]);
        }
        AtomicOp::Load | AtomicOp::Store | AtomicOp::Fence => return None,
        _ => {
            emit_rmw(&mut b, op, width, acquire, release);
            return Some(b.finish());
        }
    }
    b.ret();
    Some(b.finish())
}

fn emit_rmw(b: &mut Builder, op: AtomicOp, width: AtomicWidth, acquire: bool, release: bool) {
    let suffix = width.suffix();
    let load = if acquire { "ldaxr" } else { "ldxr" };
    let store = if release { "stlxr" } else { "stxr" };
    let address = mem(Reg::x(0), AddressMode::Base);
    let old = width.register(4);
    let status = Reg::w(5);
    let compare = op == AtomicOp::CompareExchange;
    if compare {
        narrow_expected(b, width);
    }
    let retry = b.offset();
    b.emit(&format!("{load}.{suffix}"), [reg(old), address.clone()]);
    let (new, mismatch) = match op {
        AtomicOp::CompareExchange => {
            let expected = width.register(1);
            let mode = if old.width == Width::X { "X" } else { "W" };
            b.emit(
                &format!("subs.{mode}.shift"),
                [
                    reg(Reg::zr(old.width)),
                    reg(old),
                    reg(expected),
                    unshifted(),
                ],
            );
            let mismatch = b.offset();
            b.emit("b.cond", [next(), condition(1)]);
            (width.register(2), Some(mismatch))
        }
        AtomicOp::Exchange => (width.register(1), None),
        _ => (compute_rmw(b, op, width, old), None),
    };
    b.emit(
        &format!("{store}.{suffix}"),
        [reg(status), reg(new), address],
    );
    b.branch("cbnz.W", &[reg(status)], retry);
    b.add(Reg::x(0), Reg::x(4), 0);
    if let Some(mismatch) = mismatch {
        b.emit("movz.W", [reg(Reg::w(1)), movz(1)]);
        b.ret();
        b.patch_branch(mismatch, "b.cond", &[], &[condition(1)]);
        b.emit("clrex", [imm(15)]);
        b.add(Reg::x(0), Reg::x(4), 0);
        b.emit("movz.W", [reg(Reg::w(1)), movz(0)]);
    }
    b.ret();
}

fn narrow_expected(b: &mut Builder, width: AtomicWidth) {
    let mask = match width {
        AtomicWidth::Byte => 0xff,
        AtomicWidth::Half => 0xffff,
        _ => return,
    };
    let expected = reg(Reg::w(1));
    b.emit(
        "and.W.imm",
        [expected.clone(), expected, Operand::Bits(mask)],
    );
}

fn compute_rmw(b: &mut Builder, op: AtomicOp, width: AtomicWidth, old: Reg) -> Reg {
    let mnemonic = match op {
        AtomicOp::Add => "add",
        AtomicOp::Sub => "sub",
        AtomicOp::And => "and",
        AtomicOp::Or => "orr",
        AtomicOp::Xor => "eor",
        _ => unreachable!("非算术 RMW"),
    };
    let mode = if old.width == Width::X { "X" } else { "W" };
    let new = width.register(6);
    b.emit(
        &format!("{mnemonic}.{mode}.shift"),
        [reg(new), reg(old), reg(width.register(1)), unshifted()],
    );
    new
}
