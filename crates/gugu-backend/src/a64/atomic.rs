//! Spill-free ARMv8.0 exclusive sequences, expanded after allocation.
use super::{
    Error,
    inst::{Address, AddressMode, Operand, Shift},
    layout, machine,
    reg::{Reg, Width},
    rewrite::{constant, emit},
    sites::Emitter,
};
use crate::{gir::body::MemoryOrdering, lir::body::AtomicOp};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Atomic {
    pub op: AtomicOp,
    pub ordering: MemoryOrdering,
    pub bits: u32,
}

fn barrier(out: &mut Vec<layout::Item>) -> Result<(), Error> {
    emit(out, "dmb", vec![Operand::Imm(11)])
}

fn branch(
    out: &mut Vec<layout::Item>,
    key: &str,
    operands: Vec<Operand>,
    target: u32,
) -> Result<(), Error> {
    out.push(layout::Item::Branch {
        form: machine::form(key)?,
        operands,
        target,
    });
    Ok(())
}

impl Atomic {
    fn reg(&self, id: u8) -> Operand {
        Operand::Reg(if self.bits == 64 {
            Reg::x(id)
        } else {
            Reg::w(id)
        })
    }

    fn lane(&self) -> &'static str {
        match self.bits {
            8 => "B",
            16 => "H",
            32 => "W",
            _ => "X",
        }
    }

    fn address(ordered: bool) -> Operand {
        Operand::Address(Address {
            base: Reg::x(9),
            mode: if ordered {
                AddressMode::Base
            } else {
                AddressMode::Unsigned(0)
            },
        })
    }

    pub(super) fn emit(
        &self,
        out: &mut Vec<layout::Item>,
        labels: &mut Emitter<'_>,
    ) -> Result<(), Error> {
        if self.ordering == MemoryOrdering::SeqCst {
            barrier(out)?;
        }
        if matches!(self.op, AtomicOp::Load | AtomicOp::Store) {
            self.access(out)?;
        } else {
            self.exclusive(out, labels)?;
        }
        if self.ordering == MemoryOrdering::SeqCst {
            barrier(out)?;
        }
        Ok(())
    }

    fn access(&self, out: &mut Vec<layout::Item>) -> Result<(), Error> {
        let load = self.op == AtomicOp::Load;
        let ordered = self.ordering != MemoryOrdering::Relaxed;
        let key = if ordered {
            format!("{}.{}", if load { "ldar" } else { "stlr" }, self.lane())
        } else {
            match (load, self.bits) {
                (true, 8) => "ldrb.W.Unsigned",
                (true, 16) => "ldrh.W.Unsigned",
                (true, 32) => "ldr.W.Unsigned",
                (true, _) => "ldr.X.Unsigned",
                (false, 8) => "strb.W.Unsigned",
                (false, 16) => "strh.W.Unsigned",
                (false, 32) => "str.W.Unsigned",
                _ => "str.X.Unsigned",
            }
            .into()
        };
        emit(
            out,
            &key,
            vec![self.reg(if load { 12 } else { 10 }), Self::address(ordered)],
        )
    }

    fn compare(&self, out: &mut Vec<layout::Item>, mismatch: u32) -> Result<(), Error> {
        emit(
            out,
            "subs.X.shift",
            vec![
                Operand::Reg(Reg::zr(Width::X)),
                Operand::Reg(Reg::x(12)),
                Operand::Reg(Reg::x(10)),
                Operand::Shift(Shift::Lsl, 0),
            ],
        )?;
        branch(out, "b.cond", vec![Operand::Condition(1)], mismatch)
    }

    fn combine(&self, out: &mut Vec<layout::Item>) -> Result<(), Error> {
        if self.op == AtomicOp::Exchange {
            return Ok(());
        }
        let key = match self.op {
            AtomicOp::Add => "add.X.shift",
            AtomicOp::Sub => "sub.X.shift",
            AtomicOp::And => "and.X.shift",
            AtomicOp::Or => "orr.X.shift",
            AtomicOp::Xor => "eor.X.shift",
            _ => return Err(super::error("invalid exclusive operation")),
        };
        emit(
            out,
            key,
            vec![
                Operand::Reg(Reg::x(13)),
                Operand::Reg(Reg::x(12)),
                Operand::Reg(Reg::x(10)),
                Operand::Shift(Shift::Lsl, 0),
            ],
        )
    }

    fn exclusive(
        &self,
        out: &mut Vec<layout::Item>,
        labels: &mut Emitter<'_>,
    ) -> Result<(), Error> {
        use MemoryOrdering::{AcqRel, Acquire, Release, SeqCst};
        let retry = labels.label()?;
        let mismatch = labels.label()?;
        let done = labels.label()?;
        let acquire = matches!(self.ordering, Acquire | AcqRel | SeqCst);
        let release = matches!(self.ordering, Release | AcqRel | SeqCst);
        let cas = self.op == AtomicOp::CompareExchange;
        if cas {
            constant(out, 15, 0)?;
        }
        out.push(layout::Item::Label(retry));
        emit(
            out,
            &format!("{}.{}", if acquire { "ldaxr" } else { "ldxr" }, self.lane()),
            vec![self.reg(12), Self::address(true)],
        )?;
        if cas {
            self.compare(out, mismatch)?;
        } else {
            self.combine(out)?;
        }
        let value = if cas {
            11
        } else if self.op == AtomicOp::Exchange {
            10
        } else {
            13
        };
        emit(
            out,
            &format!("{}.{}", if release { "stlxr" } else { "stxr" }, self.lane()),
            vec![
                Operand::Reg(Reg::w(14)),
                self.reg(value),
                Self::address(true),
            ],
        )?;
        branch(out, "cbnz.W", vec![Operand::Reg(Reg::w(14))], retry)?;
        if cas {
            constant(out, 15, 1)?;
            branch(out, "b", vec![], done)?;
            out.push(layout::Item::Label(mismatch));
            emit(out, "clrex", vec![Operand::Imm(15)])?;
        }
        out.push(layout::Item::Label(done));
        Ok(())
    }
}
