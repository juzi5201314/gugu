use super::{Builder, bits, reg, signature};
use crate::a64::{
    Error,
    inst::{Operand, Shift},
    machine::{self, Access, Item, MReg},
    reg::Width,
};
use crate::lir::body::{Body, Op, Type, ValueId};

pub fn lower(
    body: &Body,
    op: &Op,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    match op {
        Op::StackAddr(slot) => {
            signature(body, args, results, &[], &[Type::Ptr])?;
            if slot.index() >= body.stack_slots.len() {
                return Err(crate::a64::error("unknown stack slot"));
            }
            b.push(Item::StackAddr {
                dest: results[0].0,
                slot: slot.0,
            });
            Ok(())
        }
        Op::PtrOffset => {
            signature(body, args, results, &[Type::Ptr, Type::I64], &[Type::Ptr])?;
            b.emit(
                "add.X.shift",
                vec![
                    machine::write(results[0].0),
                    machine::read(args[0].0),
                    machine::read(args[1].0),
                    machine::fixed(Operand::Shift(Shift::Lsl, 0)),
                ],
            )
        }
        Op::Load(access) | Op::Store(access) => {
            if !access.align.is_power_of_two() {
                return Err(crate::a64::error("invalid memory alignment"));
            }
            let load = matches!(op, Op::Load(_));
            let ty = if load { results.first() } else { args.get(1) }
                .map(|v| body.values[v.index()].kind.ty)
                .ok_or_else(|| crate::a64::error("missing memory value"))?;
            let width = bits(ty)?;
            if load {
                signature(body, args, results, &[Type::Ptr], &[ty])?;
            } else {
                signature(body, args, results, &[Type::Ptr, ty], &[])?;
            }
            let key = match (load, width) {
                (true, 8) => "ldrb.W.Unsigned",
                (true, 16) => "ldrh.W.Unsigned",
                (true, 32) => "ldr.W.Unsigned",
                (true, _) => "ldr.X.Unsigned",
                (false, 8) => "strb.W.Unsigned",
                (false, 16) => "strh.W.Unsigned",
                (false, 32) => "str.W.Unsigned",
                (false, _) => "str.X.Unsigned",
            };
            let id = if load { results[0].0 } else { args[1].0 };
            b.emit(
                key,
                vec![
                    reg(
                        id,
                        if width == 64 { Width::X } else { Width::W },
                        if load { Access::Write } else { Access::Read },
                    ),
                    machine::MOperand::Address(MReg::Virtual(args[0].0)),
                ],
            )
        }
        _ => Err(crate::a64::error(
            "unsupported leaf operation (runtime/call/FP/vector/atomic/symbol)",
        )),
    }
}
