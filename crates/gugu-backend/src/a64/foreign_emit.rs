//! Post-allocation packet expansion; all source lanes are staged before scratch use.
use super::{
    Error,
    alloc::Allocation,
    foreign::{Platform, Slot},
    foreign_packet::{self, Plan},
    frame::Frame,
    inst::{Address, AddressMode, Operand, Shift, Target},
    layout,
    machine::Item,
    reg::{Reg, Width},
    rewrite::{adjust, constant, emit},
    sites::{Emitter as Sites, SiteKind},
};
use crate::{
    gir::body::CallKind,
    lir::body::{Call, CallTarget, Provenance},
    regalloc::{Location, Placement},
    runtime::{
        coroutine::{CoroutineSlot, StackDescriptor},
        foreign_packet as packet,
    },
};

const SP: Reg = Reg::sp(Width::X);

struct Emitter<'a, 'b> {
    plan: Plan,
    call: &'a Call,
    arguments: &'a [u32],
    results: &'a [u32],
    allocation: &'a Allocation,
    frame: &'a Frame,
    sites: &'a mut Sites<'b>,
    out: &'a mut Vec<layout::Item>,
}

pub(crate) fn lower(
    item: &Item,
    allocation: &Allocation,
    frame: &Frame,
    sites: &mut Sites<'_>,
    out: &mut Vec<layout::Item>,
) -> Result<(), Error> {
    let Item::ForeignCall {
        call,
        platform,
        arguments,
        results,
        safepoint,
    } = item
    else {
        return Err(super::error("expected foreign packet item"));
    };
    let plan = foreign_packet::plan(call, *platform)?;
    let dispatcher = sites.foreign_symbol(call.kind == CallKind::ForeignBridgeDirtyCpu)?;
    let mut emitter = Emitter {
        plan,
        call,
        arguments,
        results,
        allocation,
        frame,
        sites,
        out,
    };
    emitter.stage()?;
    emitter.arguments()?;
    emitter.target(*platform)?;
    emitter.store_constant(
        packet::FOREIGN_PACKET_STACK_BYTES as u32,
        u64::from(emitter.plan.abi.stack_bytes),
    )?;
    emitter.high(0)?;
    adjust(emitter.out, Reg::x(16), SP, 0, false)?;
    emitter.subtract(0, 0, 16)?;
    for (reg, value) in [
        (1, emitter.plan.packet.bytes),
        (2, emitter.plan.packet.fixups),
        (3, emitter.plan.fixup_count),
    ] {
        constant(emitter.out, reg, u64::from(value))?;
    }
    emitter.sites.call(
        emitter.out,
        dispatcher,
        SiteKind::ForeignBridge {
            dirty: call.kind == CallKind::ForeignBridgeDirtyCpu,
        },
        Some(safepoint.0),
        Some(safepoint.1),
        None,
    )?;
    emitter.results()
}

fn r(id: u8) -> Operand {
    Operand::Reg(Reg::x(id))
}

impl Emitter<'_, '_> {
    fn memory(
        &mut self,
        load: bool,
        bytes: u8,
        id: u8,
        base: Reg,
        offset: u32,
    ) -> Result<(), Error> {
        let (base, offset) =
            if offset.is_multiple_of(u32::from(bytes)) && offset / u32::from(bytes) <= 4095 {
                (base, offset)
            } else {
                adjust(self.out, Reg::x(17), base, offset, false)?;
                (Reg::x(17), 0)
            };
        let suffix = match bytes {
            1 => "b.W",
            2 => "h.W",
            4 => ".W",
            8 => ".X",
            _ => return Err(super::error("invalid packet memory width")),
        };
        let value = if bytes == 8 { Reg::x(id) } else { Reg::w(id) };
        emit(
            self.out,
            &format!("{}{suffix}.Unsigned", if load { "ldr" } else { "str" }),
            vec![
                Operand::Reg(value),
                Operand::Address(Address {
                    base,
                    mode: AddressMode::Unsigned(i64::from(offset)),
                }),
            ],
        )
    }
    fn store_constant(&mut self, offset: u32, value: u64) -> Result<(), Error> {
        constant(self.out, 16, value)?;
        self.memory(false, 8, 16, SP, offset)
    }
    fn high(&mut self, id: u8) -> Result<(), Error> {
        let offset = (std::mem::offset_of!(CoroutineSlot, stack)
            + std::mem::offset_of!(StackDescriptor, stack_high)) as u32;
        self.memory(true, 8, id, Reg::x(27), offset)
    }
    fn subtract(&mut self, dest: u8, left: u8, right: u8) -> Result<(), Error> {
        emit(
            self.out,
            "sub.X.shift",
            vec![r(dest), r(left), r(right), Operand::Shift(Shift::Lsl, 0)],
        )
    }
    fn branch(&mut self, key: &str, operands: Vec<Operand>, target: u32) -> Result<(), Error> {
        self.out.push(layout::Item::Branch {
            form: super::machine::form(key)?,
            operands,
            target,
        });
        Ok(())
    }
    fn zero(&mut self) -> Result<(), Error> {
        adjust(self.out, Reg::x(16), SP, 0, false)?;
        constant(self.out, 17, u64::from(self.plan.bytes))?;
        let label = self.sites.label()?;
        self.out.push(layout::Item::Label(label));
        emit(
            self.out,
            "str.X.Unsigned",
            vec![
                Operand::Reg(Reg::zr(Width::X)),
                Operand::Address(Address {
                    base: Reg::x(16),
                    mode: AddressMode::Unsigned(0),
                }),
            ],
        )?;
        adjust(self.out, Reg::x(16), Reg::x(16), 8, false)?;
        emit(
            self.out,
            "subs.X.imm",
            vec![
                r(17),
                r(17),
                Operand::AddImm {
                    imm12: 8,
                    shift12: false,
                },
            ],
        )?;
        self.branch("b.cond", vec![Operand::Condition(1)], label)
    }
    fn location(&self, value: u32) -> Result<Location, Error> {
        match self
            .allocation
            .values
            .get(value as usize)
            .map(|value| value.placement)
        {
            Some(Placement::Location(location)) => Ok(location),
            _ => Err(super::error("foreign value has no allocation")),
        }
    }
    fn stage(&mut self) -> Result<(), Error> {
        self.zero()?;
        for (index, value) in self.arguments.iter().copied().enumerate() {
            let width = super::fp::width(self.allocation.types[value as usize]);
            let id = match self.location(value)? {
                Location::Reg(reg) => reg.0,
                Location::Slot(slot) => {
                    let id = if width == Width::X { 16 } else { 63 };
                    super::fp::stack(self.out, true, id, width, self.frame.spills[slot as usize])?;
                    id
                }
            };
            super::fp::stack(
                self.out,
                false,
                id,
                width,
                self.plan.bank + index as u32 * 8,
            )?;
        }
        for index in 0..self.plan.groups.len() {
            let group = self.plan.groups[index].clone();
            if group.by_reference {
                self.memory(true, 8, 9, SP, self.plan.bank + group.first * 8)?;
            } else {
                adjust(
                    self.out,
                    Reg::x(9),
                    SP,
                    self.plan.bank + group.first * 8,
                    false,
                )?;
            }
            adjust(self.out, Reg::x(10), SP, group.data, false)?;
            self.copy(group.source_bytes)?;
        }
        if let Some((index, _, _)) = self.call.sret {
            self.memory(true, 8, 16, SP, self.plan.bank + index * 8)?;
            self.high(9)?;
            self.subtract(16, 9, 16)?;
            self.memory(false, 8, 16, SP, self.plan.sret_depth)?;
        }
        Ok(())
    }
    fn copy(&mut self, bytes: u32) -> Result<(), Error> {
        if bytes == 0 {
            return Ok(());
        }
        constant(self.out, 11, u64::from(bytes))?;
        let label = self.sites.label()?;
        self.out.push(layout::Item::Label(label));
        self.memory(true, 1, 12, Reg::x(9), 0)?;
        self.memory(false, 1, 12, Reg::x(10), 0)?;
        adjust(self.out, Reg::x(9), Reg::x(9), 1, false)?;
        adjust(self.out, Reg::x(10), Reg::x(10), 1, false)?;
        emit(
            self.out,
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
        self.branch("b.cond", vec![Operand::Condition(1)], label)
    }
    fn copy_range(&mut self, source: u32, dest: u32, bytes: u32) -> Result<(), Error> {
        adjust(self.out, Reg::x(9), SP, source, false)?;
        adjust(self.out, Reg::x(10), SP, dest, false)?;
        self.copy(bytes)
    }
    fn fixup(
        &mut self,
        index: u32,
        slot: u32,
        source: u32,
        bytes: u32,
        kind: u64,
    ) -> Result<(), Error> {
        let record = self.plan.packet.fixups + index * 32;
        self.store_constant(record, u64::from(slot))?;
        if kind == packet::FIXUP_POINTER {
            self.memory(true, 8, 16, SP, source)?;
        } else {
            adjust(self.out, Reg::x(16), SP, source, false)?;
            self.high(9)?;
            self.subtract(16, 9, 16)?;
        }
        self.memory(false, 8, 16, SP, record + 8)?;
        self.store_constant(record + 16, u64::from(bytes))?;
        self.store_constant(record + 24, kind)
    }
    fn arguments(&mut self) -> Result<(), Error> {
        let mut group_index = 0;
        let mut fixup = 0;
        for index in 0..self.plan.abi.arguments.len() {
            let argument = self.plan.abi.arguments[index].clone();
            if argument.pieces.is_empty() {
                continue;
            }
            let group = self.plan.groups[group_index].clone();
            group_index += 1;
            if argument.promote_float {
                super::fp::stack(self.out, true, 63, Width::S, group.data)?;
                emit(
                    self.out,
                    "fcvt.D.S",
                    vec![
                        Operand::Reg(Reg::v(31, Width::D)),
                        Operand::Reg(Reg::v(31, Width::S)),
                    ],
                )?;
                super::fp::stack(self.out, false, 63, Width::D, group.data)?;
            }
            if let Some((bits, signed)) = argument.extend {
                self.memory(true, 4, 16, SP, group.data)?;
                emit(
                    self.out,
                    if signed { "sbfm.W" } else { "ubfm.W" },
                    vec![
                        Operand::Reg(Reg::w(16)),
                        Operand::Reg(Reg::w(16)),
                        Operand::Imm(0),
                        Operand::Imm(i64::from(bits - 1)),
                    ],
                )?;
                self.memory(false, 8, 16, SP, group.data)?;
            }
            if argument.indirect {
                self.fixup(
                    fixup,
                    foreign_packet::lane(argument.pieces[0].slot),
                    group.data,
                    group.bytes,
                    packet::FIXUP_INPUT,
                )?;
                fixup += 1;
            } else {
                for piece in argument.pieces {
                    let bytes = if matches!(piece.slot, Slot::Stack(_)) {
                        u32::from(piece.bytes)
                    } else {
                        8
                    };
                    self.copy_range(
                        group.data + piece.offset,
                        foreign_packet::lane(piece.slot),
                        bytes,
                    )?;
                }
                if self.call.parameters[group.first as usize].provenance == Some(Provenance::Raw) {
                    let slot = foreign_packet::lane(self.plan.abi.arguments[index].pieces[0].slot);
                    self.fixup(fixup, slot, group.data, 0, packet::FIXUP_POINTER)?;
                    fixup += 1;
                }
            }
        }
        if self.plan.abi.result.indirect {
            self.fixup(
                fixup,
                packet::A64_PACKET_SRET as u32,
                self.plan.result,
                self.call.foreign_signature.as_ref().unwrap().result.bytes as u32,
                packet::FIXUP_OUTPUT,
            )?;
            fixup += 1;
        }
        if self.plan.abi.result.pieces.len() > 2 {
            self.fixup(
                fixup,
                packet::A64_PACKET_EXTRA_RESULTS as u32,
                self.plan.extra,
                16,
                packet::FIXUP_OUTPUT,
            )?;
            fixup += 1;
        }
        if fixup != self.plan.fixup_count {
            return Err(super::error("A64 foreign fixup count mismatch"));
        }
        Ok(())
    }
    fn target(&mut self, platform: Platform) -> Result<(), Error> {
        let symbol = match &self.call.target {
            CallTarget::External { name, .. } => Some(platform.symbol(name)),
            CallTarget::Instance(key) => Some(super::abi::symbol(key)),
            CallTarget::Indirect => None,
            _ => return Err(super::error("unsupported foreign target")),
        };
        if let Some(name) = symbol {
            let target = Operand::Target(Target::Symbol { name, addend: 0 });
            emit(self.out, "adrp", vec![r(16), target.clone()])?;
            emit(self.out, "add.X.imm", vec![r(16), r(16), target])?;
        } else {
            let index = self
                .call
                .parameters
                .iter()
                .position(|p| p.provenance == Some(Provenance::Code))
                .ok_or_else(|| super::error("missing foreign Code callee"))?;
            self.memory(true, 8, 16, SP, self.plan.bank + index as u32 * 8)?;
        }
        self.memory(false, 8, 16, SP, 0)
    }
    fn results(&mut self) -> Result<(), Error> {
        for piece in self.plan.abi.result.pieces.clone() {
            let source = match piece.slot {
                Slot::Integer(index) => 128 + u32::from(index) * 8,
                Slot::Float(index) if index < 2 => 144 + u32::from(index) * 8,
                Slot::Float(index) => self.plan.extra + u32::from(index - 2) * 8,
                _ => return Err(super::error("invalid C result slot")),
            };
            self.copy_range(
                source,
                self.plan.result + piece.offset,
                u32::from(piece.bytes),
            )?;
        }
        if self.call.sret.is_some() {
            self.memory(true, 8, 16, SP, self.plan.sret_depth)?;
            self.high(9)?;
            self.subtract(10, 9, 16)?;
            adjust(self.out, Reg::x(9), SP, self.plan.result, false)?;
            return self.copy(self.call.foreign_signature.as_ref().unwrap().result.bytes as u32);
        }
        for (index, value) in self.results.iter().copied().enumerate() {
            let width = super::fp::width(self.allocation.types[value as usize]);
            match self.location(value)? {
                Location::Reg(reg) => super::fp::stack(
                    self.out,
                    true,
                    reg.0,
                    width,
                    self.plan.result + index as u32 * 8,
                )?,
                Location::Slot(slot) => {
                    let id = if width == Width::X { 16 } else { 63 };
                    super::fp::stack(
                        self.out,
                        true,
                        id,
                        width,
                        self.plan.result + index as u32 * 8,
                    )?;
                    super::fp::stack(self.out, false, id, width, self.frame.spills[slot as usize])?;
                }
            }
        }
        Ok(())
    }
}
