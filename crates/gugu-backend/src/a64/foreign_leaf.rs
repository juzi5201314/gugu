//! `#[ffi(leaf)]` 直调发射：当前协程栈上按 AAPCS64 落位实参、`bl`/`blr` 进入 C 目标、
//! 回收寄存器结果。无 packet、无 bridge、无安全点记录；leaf 语义下调用不能 unwind，
//! invoke 的落点边是死边。
use super::{
    Error,
    alloc::Allocation,
    foreign::{Platform, Slot},
    foreign_packet::{self, Plan},
    frame::Frame,
    inst::{Address, AddressMode, Operand, Shift},
    layout,
    machine::{Item, MReg},
    reg::{Reg, Width},
    rewrite::{adjust, emit, location, move_loc, offset, rewrite_item, stack},
    sites::Emitter as Sites,
};
use crate::{
    lir::body::{Call, CallTarget, Provenance},
    regalloc::{Location, PhysReg},
};

const SP: Reg = Reg::sp(Width::X);

/// 实参落位的暂存划分：C 栈实参区之后依次是结果映像与间接 by_value 私有副本。
struct Staging {
    copies: Vec<Option<u32>>,
    total: u32,
}

fn staging(plan: &Plan, call: &Call) -> Result<Staging, Error> {
    let signature = call
        .foreign_signature
        .as_ref()
        .ok_or_else(|| super::error("missing foreign signature"))?;
    let image = if plan.abi.result.indirect || signature.result.bytes == 0 {
        0
    } else {
        u32::try_from(signature.result.bytes)
            .map_err(|_| super::error("leaf result image overflow"))?
            .div_ceil(16)
            * 16
    };
    let mut cursor = plan
        .abi
        .stack_bytes
        .checked_add(image)
        .ok_or_else(|| super::error("leaf outgoing overflow"))?;
    let mut copies = vec![None; plan.groups.len()];
    let mut group = 0;
    for (ty, argument) in signature.arguments.iter().zip(&plan.abi.arguments) {
        let index = group;
        group += usize::from(ty.bytes != 0);
        if ty.bytes != 0 && argument.indirect {
            copies[index] = Some(cursor);
            cursor = cursor
                .checked_add(plan.groups[index].bytes.div_ceil(16) * 16)
                .ok_or_else(|| super::error("leaf by_value copy overflow"))?;
        }
    }
    Ok(Staging {
        copies,
        total: cursor,
    })
}

/// `outgoing` 区需求：C 栈实参 + 结果映像 + 间接 by_value 副本区。
pub(crate) fn outgoing(call: &Call, platform: Platform) -> Result<u32, Error> {
    let plan = foreign_packet::plan(call, platform)?;
    Ok(staging(&plan, call)?.total)
}

pub(crate) fn lower(
    item: &Item,
    allocation: &Allocation,
    frame: &Frame,
    sites: &mut Sites<'_>,
    out: &mut Vec<layout::Item>,
) -> Result<(), Error> {
    let Item::ForeignLeaf {
        call,
        platform,
        arguments,
        results,
    } = item
    else {
        return Err(super::error("expected foreign leaf item"));
    };
    let plan = foreign_packet::plan(call, *platform)?;
    let staging = staging(&plan, call)?;
    let mut emitter = LeafEmitter {
        plan,
        call,
        arguments,
        allocation,
        frame,
        out,
    };
    emitter.write_memory(&staging)?;
    emitter.fill_registers(&staging, sites)?;
    emitter.target()?;
    emitter.results(results, &staging)
}

struct LeafEmitter<'a> {
    plan: Plan,
    call: &'a Call,
    arguments: &'a [u32],
    allocation: &'a Allocation,
    frame: &'a Frame,
    out: &'a mut Vec<layout::Item>,
}

impl LeafEmitter<'_> {
    /// C 实参序遍历：克隆 ABI 分类以避免借用冲突，返回 (分类, packet 组下标)。
    fn arguments(&self) -> Result<Vec<(super::foreign::Argument, usize)>, Error> {
        let signature = self
            .call
            .foreign_signature
            .as_ref()
            .ok_or_else(|| super::error("missing foreign signature"))?;
        if signature.arguments.len() != self.plan.abi.arguments.len() {
            return Err(super::error("leaf argument classification mismatch"));
        }
        let mut group = 0;
        signature
            .arguments
            .iter()
            .zip(&self.plan.abi.arguments)
            .map(|(ty, argument)| {
                let index = group;
                group += usize::from(ty.bytes != 0);
                Ok((argument.clone(), index))
            })
            .collect()
    }

    fn value(&self, index: u32) -> Result<u32, Error> {
        self.arguments
            .get(index as usize)
            .copied()
            .ok_or_else(|| super::error("leaf argument lane out of range"))
    }

    fn lane(&self, index: u32) -> Result<Location, Error> {
        location(MReg::Virtual(self.value(index)?), self.allocation)?
            .ok_or_else(|| super::error("leaf call reads dead argument"))
    }

    /// 协程栈深度 → 实际地址（stack_high - depth），结果放在 `dest` 物理寄存器。
    fn real_address(&mut self, depth: u32, dest: u8) -> Result<(), Error> {
        let lane = self.lane(depth)?;
        move_loc(
            self.out,
            lane,
            Location::Reg(PhysReg(16)),
            self.frame,
            super::abi::GPR,
        )?;
        let high = (std::mem::offset_of!(crate::runtime::coroutine::CoroutineSlot, stack)
            + std::mem::offset_of!(crate::runtime::coroutine::StackDescriptor, stack_high))
            as u32;
        emit(
            self.out,
            "ldr.X.Unsigned",
            vec![
                Operand::Reg(Reg::x(17)),
                Operand::Address(Address {
                    base: Reg::x(super::abi::COROUTINE),
                    mode: AddressMode::Unsigned(i64::from(high)),
                }),
            ],
        )?;
        emit(
            self.out,
            "sub.X.shift",
            vec![
                Operand::Reg(Reg::x(dest)),
                Operand::Reg(Reg::x(17)),
                Operand::Reg(Reg::x(16)),
                Operand::Shift(Shift::Lsl, 0),
            ],
        )
    }

    fn memory(&mut self, load: bool, bytes: u8, reg: u8, base: Reg, at: u32) -> Result<(), Error> {
        let (value, suffix) = match (bytes, reg >= 32) {
            (1, false) => (Reg::w(reg), "b.W"),
            (2, false) => (Reg::w(reg), "h.W"),
            (4, false) => (Reg::w(reg), ".W"),
            (8, false) => (Reg::x(reg), ".X"),
            (4, true) => (super::fp::reg(reg, Width::S), ".S"),
            (8, true) => (super::fp::reg(reg, Width::D), ".D"),
            _ => return Err(super::error("invalid leaf memory width")),
        };
        let (base, at) = if at.is_multiple_of(u32::from(bytes)) && at / u32::from(bytes) <= 4095 {
            (base, at)
        } else {
            let scratch = if base == Reg::x(16) { 17 } else { 16 };
            adjust(self.out, Reg::x(scratch), base, at, false)?;
            (Reg::x(scratch), 0)
        };
        emit(
            self.out,
            &format!("{}{suffix}.Unsigned", if load { "ldr" } else { "str" }),
            vec![
                Operand::Reg(value),
                Operand::Address(Address {
                    base,
                    mode: AddressMode::Unsigned(i64::from(at)),
                }),
            ],
        )
    }

    /// 整块副本：源 `src` 起始、`bytes` 字节 → `[SP + dst]`，按 8/4/2/1 分块。
    fn copy_block(&mut self, src: Reg, dst: u32, bytes: u32) -> Result<(), Error> {
        let mut done = 0_u32;
        while done < bytes {
            let width = if bytes - done >= 8 {
                8
            } else if bytes - done >= 4 {
                4
            } else if bytes - done >= 2 {
                2
            } else {
                1
            };
            self.memory(true, width, 17, src, done)?;
            self.memory(false, width, 17, SP, dst + done)?;
            done += u32::from(width);
        }
        Ok(())
    }

    /// 阶段 A：全部内存写——C 栈实参加载/拷贝与间接 by_value 私有副本。
    /// lane 源值此刻都还没被落位覆盖，必须先行完成。
    fn write_memory(&mut self, staging: &Staging) -> Result<(), Error> {
        for (argument, group_index) in self.arguments()? {
            let group = self.plan.groups[group_index].clone();
            if group.by_reference {
                self.real_address(self.value(group.first)?, 16)?;
                if let Some(base) = staging.copies[group_index] {
                    self.copy_block(Reg::x(16), base, group.bytes)?;
                } else {
                    for piece in &argument.pieces {
                        if let Slot::Stack(address) = piece.slot {
                            self.memory(true, piece.bytes, 17, Reg::x(16), piece.offset)?;
                            self.memory(false, piece.bytes, 17, SP, address)?;
                        }
                    }
                }
                continue;
            }
            for piece in &argument.pieces {
                let Slot::Stack(address) = piece.slot else {
                    continue;
                };
                let reg = match self.lane(group.first + piece.offset / 8)? {
                    Location::Reg(reg) => reg.0,
                    Location::Slot(slot) => {
                        stack(self.out, true, 17, offset(self.frame, slot)?)?;
                        17
                    }
                };
                self.memory(false, piece.bytes, reg, SP, address)?;
            }
        }
        Ok(())
    }

    /// 阶段 B/C：lane 源实参走并行拷贝组落位；内存源、sret 与目标在其后（此刻
    /// lane 已消费完，x16/x17 重新可用作暂存）。
    fn fill_registers(&mut self, staging: &Staging, sites: &mut Sites<'_>) -> Result<(), Error> {
        let mut copies = Vec::new();
        for (argument, group_index) in self.arguments()? {
            if self.plan.groups[group_index].by_reference {
                continue;
            }
            let first = self.plan.groups[group_index].first;
            for piece in &argument.pieces {
                let dest = match piece.slot {
                    Slot::Integer(id) => id,
                    Slot::Float(id) => 32 + id,
                    Slot::Stack(_) => continue,
                };
                copies.push((
                    MReg::Virtual(self.value(first + piece.offset / 8)?),
                    MReg::Physical(dest),
                ));
            }
        }
        rewrite_item(
            self.out,
            &Item::Copies(copies),
            self.allocation,
            self.frame,
            sites,
        )?;
        for (argument, group_index) in self.arguments()? {
            let group = &self.plan.groups[group_index];
            if group.by_reference && !argument.indirect {
                self.real_address(self.value(group.first)?, 16)?;
                for piece in &argument.pieces {
                    let dest = match piece.slot {
                        Slot::Integer(id) => id,
                        Slot::Float(id) => 32 + id,
                        Slot::Stack(_) => continue,
                    };
                    self.memory(true, piece.bytes, dest, Reg::x(16), piece.offset)?;
                }
            }
            if argument.indirect {
                let base = staging.copies[group_index]
                    .ok_or_else(|| super::error("missing leaf by_value copy base"))?;
                for piece in &argument.pieces {
                    if let Slot::Integer(id) = piece.slot {
                        adjust(self.out, Reg::x(id), SP, base, false)?;
                    }
                }
            }
            if let Some((bits, signed)) = argument.extend {
                for piece in &argument.pieces {
                    if let Slot::Integer(id) = piece.slot {
                        emit(
                            self.out,
                            if signed { "sbfm.W" } else { "ubfm.W" },
                            vec![
                                Operand::Reg(Reg::w(id)),
                                Operand::Reg(Reg::w(id)),
                                Operand::Imm(0),
                                Operand::Imm(i64::from(bits - 1)),
                            ],
                        )?;
                    }
                }
            }
            if argument.promote_float {
                for piece in &argument.pieces {
                    if let Slot::Float(id) = piece.slot {
                        emit(
                            self.out,
                            "fcvt.D.S",
                            vec![
                                Operand::Reg(super::fp::reg(id, Width::D)),
                                Operand::Reg(super::fp::reg(id, Width::S)),
                            ],
                        )?;
                    }
                }
            }
        }
        if let Some((index, _, _)) = self.call.sret {
            // AAPCS64 间接结果指针经 x8 传递。
            self.real_address(self.value(index)?, 8)?;
        }
        if matches!(self.call.target, CallTarget::Indirect) {
            let index = self
                .call
                .parameters
                .iter()
                .position(|p| p.provenance == Some(Provenance::Code))
                .ok_or_else(|| super::error("missing leaf Code callee"))?;
            move_loc(
                self.out,
                self.lane(index as u32)?,
                Location::Reg(PhysReg(16)),
                self.frame,
                super::abi::GPR,
            )?;
        }
        Ok(())
    }

    /// 阶段 D：目标发射——符号 `bl` 或 `blr x16`。leaf 无安全点/落点记录。
    fn target(&mut self) -> Result<(), Error> {
        match &self.call.target {
            CallTarget::External { name, .. } => self.out.push(layout::Item::SymbolBranch {
                target: self.plan.platform.symbol(name),
                link: true,
            }),
            CallTarget::Instance(key) => self.out.push(layout::Item::SymbolBranch {
                target: super::abi::symbol(key),
                link: true,
            }),
            CallTarget::Indirect => emit(self.out, "blr", vec![Operand::Reg(Reg::x(16))])?,
            _ => return Err(super::error("unsupported leaf call target")),
        }
        Ok(())
    }

    /// 阶段 E：结果回收——piece 写进结果映像（C 栈实参区复用），lane 按 8B 粒度读回。
    fn results(&mut self, results: &[u32], _staging: &Staging) -> Result<(), Error> {
        let image = self.plan.abi.stack_bytes;
        let pieces = self.plan.abi.result.pieces.clone();
        for piece in pieces {
            match piece.slot {
                Slot::Integer(id) => {
                    self.memory(false, 8, id, SP, image + piece.offset)?;
                }
                Slot::Float(id) => {
                    self.memory(false, piece.bytes, 32 + id, SP, image + piece.offset)?;
                }
                Slot::Stack(_) => return Err(super::error("invalid leaf result slot")),
            }
        }
        for (index, value) in results.iter().copied().enumerate() {
            let lane = location(MReg::Virtual(value), self.allocation)?
                .ok_or_else(|| super::error("leaf result has no allocation"))?;
            let width = super::fp::width(self.allocation.types[value as usize]);
            let reg = match lane {
                Location::Reg(reg) => reg.0,
                Location::Slot(_) => {
                    if super::abi::class(self.allocation.types[value as usize]) == super::abi::FPR {
                        62
                    } else {
                        16
                    }
                }
            };
            super::fp::stack(self.out, true, reg, width, image + index as u32 * 8)?;
            if let Location::Slot(slot) = lane {
                stack(self.out, false, reg, offset(self.frame, slot)?)?;
            }
        }
        Ok(())
    }
}
