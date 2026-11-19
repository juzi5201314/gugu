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
        Op::CoverageCounter(index) => {
            signature(body, args, results, &[], &[])?;
            if b.symbols.is_none() {
                return Err(crate::a64::error("A64 coverage requires image storage"));
            }
            let address = b.temp()?;
            let value = b.temp()?;
            b.push(Item::SymbolAddr {
                dest: address,
                symbol: crate::a64::world::coverage_symbol(*index),
            });
            b.emit(
                "ldr.W.Unsigned",
                vec![
                    reg(value, Width::W, Access::Write),
                    machine::MOperand::Address(MReg::Virtual(address)),
                ],
            )?;
            b.emit(
                "add.W.imm",
                vec![
                    reg(value, Width::W, Access::Write),
                    reg(value, Width::W, Access::Read),
                    machine::fixed(Operand::AddImm {
                        imm12: 1,
                        shift12: false,
                    }),
                ],
            )?;
            b.emit(
                "str.W.Unsigned",
                vec![
                    reg(value, Width::W, Access::Read),
                    machine::MOperand::Address(MReg::Virtual(address)),
                ],
            )
        }
        Op::Memcpy { bytes } | Op::Memmove { bytes } | Op::Memset { bytes } => {
            signature(
                body,
                args,
                results,
                &[
                    Type::Ptr,
                    if matches!(op, Op::Memset { .. }) {
                        Type::I8
                    } else {
                        Type::Ptr
                    },
                ],
                &[],
            )?;
            let count = b.constant(*bytes)?;
            b.push(Item::Copies(vec![
                (MReg::Virtual(args[0].0), MReg::Physical(9)),
                (MReg::Virtual(args[1].0), MReg::Physical(10)),
                (MReg::Virtual(count), MReg::Physical(11)),
            ]));
            b.push(Item::Memory(match op {
                Op::Memcpy { .. } => crate::a64::memory::Kind::Copy,
                Op::Memmove { .. } => crate::a64::memory::Kind::Move,
                _ => crate::a64::memory::Kind::Set,
            }));
            Ok(())
        }
        Op::TrapIf => {
            signature(body, args, results, &[Type::I8], &[])?;
            b.push(Item::TrapIf {
                condition: args[0].0,
            });
            Ok(())
        }
        Op::SymbolAddr(symbol) => {
            signature(body, args, results, &[], &[Type::Ptr])?;
            let bound = b.symbols.and_then(|symbols| {
                symbols
                    .iter()
                    .find(|(key, _)| key == symbol)
                    .map(|(_, name)| name.clone())
            });
            let symbol = match (bound, symbol) {
                (Some(name), _) => name,
                (_, crate::lir::body::Symbol::Instance(key)) => crate::a64::abi::symbol(key),
                (_, crate::lir::body::Symbol::External { name, .. }) => {
                    if body.values[results[0].index()].kind.provenance
                        == Some(crate::lir::body::Provenance::Code)
                    {
                        crate::a64::foreign_packet::platform(body)?.symbol(name)
                    } else {
                        name.clone()
                    }
                }
                _ => {
                    return Err(crate::a64::error(
                        "A64 adapter/global/descriptor/blob symbols require image/runtime binding",
                    ));
                }
            };
            b.push(Item::SymbolAddr {
                dest: results[0].0,
                symbol,
            });
            if b.symbols.is_some()
                && matches!(
                    op,
                    Op::SymbolAddr(crate::lir::body::Symbol::External { .. })
                )
                && body.values[results[0].index()].kind.provenance
                    != Some(crate::lir::body::Provenance::Code)
                && crate::a64::foreign_packet::platform(body)?
                    == crate::a64::foreign::Platform::Darwin
            {
                b.emit(
                    "ldr.X.Unsigned",
                    vec![
                        machine::write(results[0].0),
                        machine::MOperand::Address(MReg::Virtual(results[0].0)),
                    ],
                )?;
            }
            Ok(())
        }
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
            let fp = crate::a64::abi::class(ty) == crate::a64::abi::FPR;
            let width = if fp {
                (ty.bytes().unwrap() * 8) as u32
            } else {
                bits(ty)?
            };
            if load {
                signature(body, args, results, &[Type::Ptr], &[ty])?;
            } else {
                signature(body, args, results, &[Type::Ptr, ty], &[])?;
            }
            let fp_key;
            let key = if fp {
                fp_key = format!(
                    "{}.{}.Unsigned",
                    if load { "ldr" } else { "str" },
                    match ty {
                        Type::F32 => "S",
                        Type::F64 => "D",
                        _ => "Q",
                    }
                );
                fp_key.as_str()
            } else {
                match (load, width) {
                    (true, 8) => "ldrb.W.Unsigned",
                    (true, 16) => "ldrh.W.Unsigned",
                    (true, 32) => "ldr.W.Unsigned",
                    (true, _) => "ldr.X.Unsigned",
                    (false, 8) => "strb.W.Unsigned",
                    (false, 16) => "strh.W.Unsigned",
                    (false, 32) => "str.W.Unsigned",
                    (false, _) => "str.X.Unsigned",
                }
            };
            let id = if load { results[0].0 } else { args[1].0 };
            b.emit(
                key,
                vec![
                    reg(
                        id,
                        if fp {
                            crate::a64::fp::width(ty)
                        } else if width == 64 {
                            Width::X
                        } else {
                            Width::W
                        },
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
