//! Physical rewrite using reserved x16/x17 only; never borrow a live allocated register.
use super::{
    Error,
    alloc::Allocation,
    frame::Frame,
    inst::{Address, AddressMode, Extend, Inst, Operand, Shift},
    layout,
    machine::{self, Access, Function, Item, MOperand, MReg},
    reg::{Reg, Width},
};
use crate::regalloc::{
    Location, PhysReg, Placement,
    parallel::{self, Copy, CopyPolicy, CycleBreak},
};

pub fn rewrite(
    function: &Function,
    allocation: &Allocation,
    frame: &Frame,
) -> Result<Vec<layout::Item>, Error> {
    Ok(rewrite_runtime(function, allocation, frame, None)?.0)
}

pub(crate) fn rewrite_runtime(
    function: &Function,
    allocation: &Allocation,
    frame: &Frame,
    bindings: Option<&super::glue::RuntimeBindings>,
) -> Result<(Vec<layout::Item>, Vec<super::sites::Anchor>), Error> {
    let mut out = Vec::new();
    let mut runtime = super::sites::Emitter::new(bindings, function.blocks.len())?;
    if bindings.is_some()
        && !frame.unchecked_leaf
        && (function.checked_entry || frame.required_frame != 0)
    {
        runtime.entry(&mut out, frame.required_frame)?;
    }
    prologue(&mut out, frame)?;
    if function.entry != 0 {
        out.push(layout::Item::Branch {
            form: machine::form("b")?,
            operands: vec![],
            target: function.entry as u32,
        });
    }
    for (index, block) in function.blocks.iter().enumerate() {
        out.push(layout::Item::Label(index as u32));
        for site in &block.items {
            if let Item::Poll { block, instruction } = site.item {
                runtime
                    .poll(&mut out, block, instruction)
                    .map_err(|e| Error(format!("{}: {e}", site.context)))?;
                continue;
            }
            rewrite_item(&mut out, &site.item, allocation, frame, &mut runtime)
                .map_err(|e| Error(format!("{}: {e}", site.context)))?;
        }
    }
    Ok((out, runtime.anchors))
}

pub(super) fn emit(
    out: &mut Vec<layout::Item>,
    key: &str,
    operands: Vec<Operand>,
) -> Result<(), Error> {
    out.push(layout::Item::Inst(machine::inst(key, operands)?));
    Ok(())
}

pub fn constant(out: &mut Vec<layout::Item>, dest: u8, bits: u64) -> Result<(), Error> {
    let halves = (0..4)
        .map(|i| ((bits >> (i * 16)) & 0xffff) as u16)
        .collect::<Vec<_>>();
    let invert = halves.iter().filter(|h| **h != 0xffff).count()
        < halves.iter().filter(|h| **h != 0).count();
    let fill = if invert { 0xffff } else { 0 };
    let first = halves.iter().position(|h| *h != fill).unwrap_or(0);
    emit(
        out,
        if invert { "movn.X" } else { "movz.X" },
        vec![
            Operand::Reg(Reg::x(dest)),
            Operand::Wide {
                imm16: if invert {
                    !halves[first]
                } else {
                    halves[first]
                },
                shift: first as u8 * 16,
            },
        ],
    )?;
    for (index, half) in halves.iter().enumerate() {
        if index != first && *half != fill {
            emit(
                out,
                "movk.X",
                vec![
                    Operand::Reg(Reg::x(dest)),
                    Operand::Wide {
                        imm16: *half,
                        shift: index as u8 * 16,
                    },
                ],
            )?;
        }
    }
    Ok(())
}

pub(super) fn adjust(
    out: &mut Vec<layout::Item>,
    dest: Reg,
    source: Reg,
    bytes: u32,
    subtract: bool,
) -> Result<(), Error> {
    let high = bytes >> 12;
    let low = bytes & 4095;
    let key = if subtract { "sub.X.imm" } else { "add.X.imm" };
    if high > 4095 {
        let scratch = match dest.kind {
            super::reg::RegKind::Gpr(id) if dest != source => id,
            _ => {
                if source == Reg::x(17) {
                    16
                } else {
                    17
                }
            }
        };
        constant(out, scratch, u64::from(bytes))?;
        return emit(
            out,
            if subtract {
                "sub.X.extend"
            } else {
                "add.X.extend"
            },
            vec![
                Operand::Reg(dest),
                Operand::Reg(source),
                Operand::Reg(Reg::x(scratch)),
                Operand::Extend(Extend::Uxtx, 0),
            ],
        );
    }
    let mut base = source;
    if high != 0 {
        emit(
            out,
            key,
            vec![
                Operand::Reg(dest),
                Operand::Reg(base),
                Operand::AddImm {
                    imm12: high as u16,
                    shift12: true,
                },
            ],
        )?;
        base = dest;
    }
    if low != 0 || high == 0 {
        emit(
            out,
            key,
            vec![
                Operand::Reg(dest),
                Operand::Reg(base),
                Operand::AddImm {
                    imm12: low as u16,
                    shift12: false,
                },
            ],
        )?;
    }
    Ok(())
}

fn stack(out: &mut Vec<layout::Item>, load: bool, reg: u8, offset: u32) -> Result<(), Error> {
    let (base, offset) = if offset.is_multiple_of(8) && offset <= 32760 {
        (Reg::sp(Width::X), offset)
    } else {
        let address = if load {
            reg
        } else if reg == 17 {
            16
        } else {
            17
        };
        adjust(out, Reg::x(address), Reg::sp(Width::X), offset, false)?;
        (Reg::x(address), 0)
    };
    emit(
        out,
        if load {
            "ldr.X.Unsigned"
        } else {
            "str.X.Unsigned"
        },
        vec![
            Operand::Reg(Reg::x(reg)),
            Operand::Address(Address {
                base,
                mode: AddressMode::Unsigned(i64::from(offset)),
            }),
        ],
    )
}

fn prologue(out: &mut Vec<layout::Item>, frame: &Frame) -> Result<(), Error> {
    if frame.size == 0 {
        return Ok(());
    }
    adjust(out, Reg::sp(Width::X), Reg::sp(Width::X), frame.size, true)?;
    stack(out, false, 29, frame.record)?;
    stack(out, false, 30, frame.record + 8)?;
    for (reg, offset) in &frame.saves {
        stack(out, false, *reg, *offset)?;
    }
    adjust(out, Reg::x(29), Reg::sp(Width::X), frame.record, false)
}

fn restore(out: &mut Vec<layout::Item>, frame: &Frame) -> Result<(), Error> {
    if frame.size != 0 {
        for (reg, offset) in &frame.saves {
            stack(out, true, *reg, *offset)?;
        }
        stack(out, true, 29, frame.record)?;
        stack(out, true, 30, frame.record + 8)?;
        adjust(out, Reg::sp(Width::X), Reg::sp(Width::X), frame.size, false)?;
    }
    Ok(())
}

fn location(reg: MReg, allocation: &Allocation) -> Result<Option<Location>, Error> {
    match reg {
        MReg::Physical(id) if id < 31 => Ok(Some(Location::Reg(PhysReg(id)))),
        MReg::Physical(_) => Err(super::error("invalid physical GPR")),
        MReg::Virtual(id) => match allocation.values.get(id as usize).map(|v| v.placement) {
            Some(Placement::Location(loc)) => Ok(Some(loc)),
            Some(Placement::Dead) => Ok(None),
            _ => Err(super::error(
                "missing allocation or unexpected rematerialization",
            )),
        },
    }
}

fn offset(frame: &Frame, slot: u32) -> Result<u32, Error> {
    if slot == u32::MAX {
        return Ok(frame.copy_scratch);
    }
    frame
        .spills
        .get(slot as usize)
        .copied()
        .ok_or_else(|| super::error("unknown spill slot"))
}

fn move_loc(
    out: &mut Vec<layout::Item>,
    src: Location,
    dst: Location,
    frame: &Frame,
) -> Result<(), Error> {
    match (src, dst) {
        (Location::Reg(src), Location::Reg(dst)) => {
            if src != dst {
                emit(
                    out,
                    "orr.X.shift",
                    vec![
                        Operand::Reg(Reg::x(dst.0)),
                        Operand::Reg(Reg::zr(Width::X)),
                        Operand::Reg(Reg::x(src.0)),
                        Operand::Shift(Shift::Lsl, 0),
                    ],
                )?;
            }
            Ok(())
        }
        (Location::Slot(src), Location::Reg(dst)) => stack(out, true, dst.0, offset(frame, src)?),
        (Location::Reg(src), Location::Slot(dst)) => stack(out, false, src.0, offset(frame, dst)?),
        (Location::Slot(src), Location::Slot(dst)) => {
            stack(out, true, 17, offset(frame, src)?)?;
            stack(out, false, 17, offset(frame, dst)?)
        }
    }
}

struct Policy<'a>(&'a Frame);
impl CopyPolicy<Location, ()> for Policy<'_> {
    fn break_cycle(
        &self,
        saved: Location,
        _: (),
        _: &[Copy<Location, ()>],
    ) -> CycleBreak<Location, ()> {
        use crate::regalloc::RegisterFile;
        let holder = if self.0.size > 32752 {
            Location::Slot(u32::MAX)
        } else {
            Location::Reg(
                super::abi::A64Registers
                    .copy_scratch(super::abi::GPR)
                    .unwrap_or(PhysReg(16)),
            )
        };
        CycleBreak {
            holder,
            saves: vec![Copy {
                src: saved,
                dst: holder,
                ty: (),
            }],
            uses_scratch: false,
        }
    }
}

fn rewrite_item(
    out: &mut Vec<layout::Item>,
    item: &Item,
    allocation: &Allocation,
    frame: &Frame,
    runtime: &mut super::sites::Emitter<'_>,
) -> Result<(), Error> {
    match item {
        Item::Poll { .. } => Err(super::error("poll requires runtime bindings")),
        Item::Constant { dest, .. } | Item::StackAddr { dest, .. } => {
            let Some(loc) = location(MReg::Virtual(*dest), allocation)? else {
                return Ok(());
            };
            let reg = match loc {
                Location::Reg(reg) => reg.0,
                Location::Slot(_) => 16,
            };
            if let Item::Constant { bits, .. } = item {
                constant(out, reg, *bits)?;
            } else if let Item::StackAddr { slot, .. } = item {
                let offset = *frame
                    .locals
                    .get(*slot as usize)
                    .ok_or_else(|| super::error("unknown local frame slot"))?;
                adjust(out, Reg::x(reg), Reg::sp(Width::X), offset, false)?;
            }
            if let Location::Slot(slot) = loc {
                stack(out, false, reg, offset(frame, slot)?)?;
            }
            Ok(())
        }
        Item::Incoming { dest, index } => {
            let Some(loc) = location(MReg::Virtual(*dest), allocation)? else {
                return Ok(());
            };
            let reg = match loc {
                Location::Reg(reg) => reg.0,
                Location::Slot(_) => 16,
            };
            let at = index
                .checked_mul(8)
                .and_then(|n| n.checked_add(frame.size))
                .ok_or_else(|| super::error("incoming stack argument offset overflow"))?;
            stack(out, true, reg, at)?;
            if let Location::Slot(slot) = loc {
                stack(out, false, reg, offset(frame, slot)?)?;
            }
            Ok(())
        }
        Item::Copies(pairs) => {
            let mut moves = Vec::new();
            for (src, dst) in pairs {
                if let Some(dst) = location(*dst, allocation)? {
                    let src = location(*src, allocation)?
                        .ok_or_else(|| super::error("copy reads dead value"))?;
                    moves.push(Copy { src, dst, ty: () });
                }
            }
            let plan = parallel::resolve_copies(&moves, &Policy(frame), true)
                .map_err(super::alloc::shared)?;
            for mov in plan.moves {
                move_loc(out, mov.src, mov.dst, frame)?;
            }
            Ok(())
        }
        Item::Call {
            target,
            arguments,
            results,
            safepoint,
        } => {
            arguments_to_abi(out, arguments, allocation, frame, false, runtime)?;
            if let Some((block, instruction)) = safepoint {
                runtime.call(
                    out,
                    target.clone(),
                    super::sites::SiteKind::CallReturn,
                    Some(*block),
                    Some(*instruction),
                )?;
            } else {
                out.push(layout::Item::SymbolBranch {
                    target: target.clone(),
                    link: true,
                });
            }
            rewrite_item(
                out,
                &Item::Copies(
                    results
                        .iter()
                        .enumerate()
                        .map(|(index, value)| (MReg::Physical(index as u8), MReg::Virtual(*value)))
                        .collect(),
                ),
                allocation,
                frame,
                runtime,
            )
        }
        Item::TailCall { target, arguments } => {
            arguments_to_abi(out, arguments, allocation, frame, true, runtime)?;
            restore(out, frame)?;
            out.push(layout::Item::SymbolBranch {
                target: target.clone(),
                link: false,
            });
            Ok(())
        }
        Item::Return => {
            restore(out, frame)?;
            emit(out, "ret", vec![Operand::Reg(Reg::x(30))])
        }
        Item::Inst(inst) => {
            let operands = rewrite_operands(out, &inst.operands, allocation, frame)?;
            out.push(layout::Item::Inst(Inst::new(inst.form, operands.operands)));
            for (reg, slot) in operands.stores {
                stack(out, false, reg, offset(frame, slot)?)?;
            }
            Ok(())
        }
        Item::Branch {
            form,
            operands,
            target,
        } => {
            let operands = rewrite_operands(out, operands, allocation, frame)?;
            if !operands.stores.is_empty() {
                return Err(super::error("branch cannot define a virtual register"));
            }
            out.push(layout::Item::Branch {
                form: *form,
                operands: operands.operands,
                target: *target,
            });
            Ok(())
        }
    }
}

fn arguments_to_abi(
    out: &mut Vec<layout::Item>,
    arguments: &[u32],
    allocation: &Allocation,
    frame: &Frame,
    tail: bool,
    runtime: &mut super::sites::Emitter<'_>,
) -> Result<(), Error> {
    for (index, value) in arguments.iter().enumerate().skip(8) {
        let src = location(MReg::Virtual(*value), allocation)?
            .ok_or_else(|| super::error("call reads dead argument"))?;
        let reg = match src {
            Location::Reg(reg) => reg.0,
            Location::Slot(slot) => {
                stack(out, true, 16, offset(frame, slot)?)?;
                16
            }
        };
        let at = u32::try_from(index - 8)
            .ok()
            .and_then(|index| index.checked_mul(8))
            .and_then(|offset| offset.checked_add(if tail { frame.size } else { 0 }))
            .ok_or_else(|| super::error("outgoing argument offset overflow"))?;
        stack(out, false, reg, at)?;
    }
    rewrite_item(
        out,
        &Item::Copies(
            arguments
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, value)| (MReg::Virtual(*value), MReg::Physical(index as u8)))
                .collect(),
        ),
        allocation,
        frame,
        runtime,
    )
}

struct Operands {
    operands: Vec<Operand>,
    stores: Vec<(u8, u32)>,
}

fn rewrite_operands(
    out: &mut Vec<layout::Item>,
    operands: &[MOperand],
    allocation: &Allocation,
    frame: &Frame,
) -> Result<Operands, Error> {
    let mut loaded = Vec::<(MReg, u8)>::new();
    for operand in operands {
        let source = match operand {
            MOperand::Reg(reg, _, Access::Read | Access::ReadWrite) | MOperand::Address(reg) => {
                Some(*reg)
            }
            _ => None,
        };
        if let Some(source) = source
            && !loaded.iter().any(|(id, _)| *id == source)
            && let Some(Location::Slot(slot)) = location(source, allocation)?
        {
            if loaded.len() >= 2 {
                return Err(super::error(
                    "instruction exceeds two reserved spill-read temporaries",
                ));
            }
            let reg = 16 + loaded.len() as u8;
            stack(out, true, reg, offset(frame, slot)?)?;
            loaded.push((source, reg));
        }
    }
    let mut stores = Vec::new();
    let mut final_operands = Vec::new();
    for operand in operands {
        let (reg, width, write) = match operand {
            MOperand::Fixed(operand) => {
                final_operands.push(operand.clone());
                continue;
            }
            MOperand::Reg(reg, width, access) => (*reg, *width, !matches!(access, Access::Read)),
            MOperand::Address(reg) => (*reg, Width::X, false),
        };
        let physical = match location(reg, allocation)?
            .ok_or_else(|| super::error("instruction uses dead value"))?
        {
            Location::Reg(reg) => reg.0,
            Location::Slot(slot) => {
                let physical = loaded
                    .iter()
                    .find(|(source, _)| *source == reg)
                    .map_or(16, |(_, reg)| *reg);
                if write {
                    stores.push((physical, slot));
                }
                physical
            }
        };
        let physical = Reg {
            kind: super::reg::RegKind::Gpr(physical),
            width,
        };
        final_operands.push(if matches!(operand, MOperand::Address(_)) {
            Operand::Address(Address {
                base: physical,
                mode: AddressMode::Unsigned(0),
            })
        } else {
            Operand::Reg(physical)
        });
    }
    Ok(Operands {
        operands: final_operands,
        stores,
    })
}
