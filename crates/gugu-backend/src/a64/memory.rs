//! Fixed-register byte-copy regions; no calls, spills or safepoints inside.
use super::{
    Error,
    inst::{Address, AddressMode, Operand, Shift},
    layout, machine,
    reg::{Reg, Width},
    rewrite::{adjust, emit},
    sites::Emitter,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Copy,
    Move,
    Set,
}

fn r(id: u8) -> Operand {
    Operand::Reg(Reg::x(id))
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

pub(crate) fn lower(
    kind: Kind,
    out: &mut Vec<layout::Item>,
    sites: &mut Emitter<'_>,
) -> Result<(), Error> {
    let done = sites.label()?;
    branch(out, "cbz.X", vec![r(11)], done)?;
    if kind == Kind::Move {
        let forward = sites.label()?;
        emit(
            out,
            "subs.X.shift",
            vec![
                Operand::Reg(Reg::zr(Width::X)),
                r(9),
                r(10),
                Operand::Shift(Shift::Lsl, 0),
            ],
        )?;
        branch(out, "b.cond", vec![Operand::Condition(9)], forward)?;
        for id in [9, 10] {
            emit(
                out,
                "add.X.shift",
                vec![r(id), r(id), r(11), Operand::Shift(Shift::Lsl, 0)],
            )?;
        }
        loop_bytes(kind, true, out, sites)?;
        branch(out, "b", vec![], done)?;
        out.push(layout::Item::Label(forward));
    }
    loop_bytes(kind, false, out, sites)?;
    out.push(layout::Item::Label(done));
    Ok(())
}

fn loop_bytes(
    kind: Kind,
    backward: bool,
    out: &mut Vec<layout::Item>,
    sites: &mut Emitter<'_>,
) -> Result<(), Error> {
    let start = sites.label()?;
    out.push(layout::Item::Label(start));
    if backward {
        for id in [9, 10] {
            adjust(out, Reg::x(id), Reg::x(id), 1, true)?;
        }
    }
    if kind != Kind::Set {
        emit(
            out,
            "ldrb.W.Unsigned",
            vec![
                Operand::Reg(Reg::w(12)),
                Operand::Address(Address {
                    base: Reg::x(10),
                    mode: AddressMode::Unsigned(0),
                }),
            ],
        )?;
    }
    emit(
        out,
        "strb.W.Unsigned",
        vec![
            Operand::Reg(Reg::w(if kind == Kind::Set { 10 } else { 12 })),
            Operand::Address(Address {
                base: Reg::x(9),
                mode: AddressMode::Unsigned(0),
            }),
        ],
    )?;
    if !backward {
        adjust(out, Reg::x(9), Reg::x(9), 1, false)?;
        if kind != Kind::Set {
            adjust(out, Reg::x(10), Reg::x(10), 1, false)?;
        }
    }
    emit(
        out,
        "subs.X.imm",
        vec![
            r(11),
            r(11),
            Operand::AddImm {
                imm12: 1,
                shift12: false,
            },
        ],
    )?;
    branch(out, "b.cond", vec![Operand::Condition(1)], start)
}
