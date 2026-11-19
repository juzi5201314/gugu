//! Runtime fragment call sites; serialized stack maps/unwind follow in 4c.2.
use super::{
    Error,
    frame::Frame,
    glue::RuntimeBindings,
    inst::{Address, AddressMode, Operand},
    layout, machine,
    reg::{Reg, Width},
    rewrite::{adjust, emit},
};
use crate::lir::body::{Body, Op, Terminator};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SiteKind {
    MorestackEntry,
    PollResume,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallerLr {
    Register(u8),
    StackOffset(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSite {
    pub pc_offset: u32,
    pub kind: SiteKind,
    pub block: Option<u32>,
    pub instruction: Option<u32>,
    pub cfa_offset: u32,
    pub caller_lr: CallerLr,
    pub empty_roots: [Vec<u64>; 5],
}

pub fn lookup(sites: &[RuntimeSite], return_pc: u32) -> Option<&RuntimeSite> {
    sites
        .binary_search_by_key(&return_pc, |site| site.pc_offset)
        .ok()
        .map(|index| &sites[index])
}

pub(crate) struct Anchor {
    label: u32,
    kind: SiteKind,
    block: Option<u32>,
    instruction: Option<u32>,
}

pub(crate) fn resolve(
    anchors: &[Anchor],
    plan: &layout::Plan,
    frame: &Frame,
) -> Result<Vec<RuntimeSite>, Error> {
    let mut sites = Vec::new();
    for anchor in anchors {
        let pc = *plan
            .labels
            .get(&anchor.label)
            .ok_or_else(|| super::error("missing runtime return label"))?;
        if pc == 0 || pc >= plan.size || pc % 4 != 0 {
            return Err(super::error("runtime return PC is not inside code"));
        }
        let entry = anchor.kind == SiteKind::MorestackEntry;
        sites.push(RuntimeSite {
            pc_offset: u32::try_from(pc).map_err(|_| super::error("runtime PC exceeds u32"))?,
            kind: anchor.kind,
            block: anchor.block,
            instruction: anchor.instruction,
            cfa_offset: if entry { 0 } else { frame.size },
            caller_lr: if entry {
                CallerLr::Register(10)
            } else {
                CallerLr::StackOffset(frame.record + 8)
            },
            empty_roots: crate::metadata::empty_bitmaps(if entry { 0 } else { frame.size / 8 }),
        });
    }
    sites.sort_by_key(|site| site.pc_offset);
    Ok(sites)
}

pub(crate) struct Emitter<'a> {
    bindings: Option<&'a RuntimeBindings>,
    next: u32,
    pub anchors: Vec<Anchor>,
}

impl<'a> Emitter<'a> {
    pub fn new(bindings: Option<&'a RuntimeBindings>, labels: usize) -> Result<Self, Error> {
        Ok(Self {
            bindings,
            next: u32::try_from(labels).map_err(|_| super::error("too many CFG labels"))?,
            anchors: vec![],
        })
    }
    fn label(&mut self) -> Result<u32, Error> {
        let label = self.next;
        self.next = label
            .checked_add(1)
            .ok_or_else(|| super::error("runtime label overflow"))?;
        Ok(label)
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
    fn call(
        &mut self,
        out: &mut Vec<layout::Item>,
        symbol: String,
        kind: SiteKind,
        block: Option<u32>,
        instruction: Option<u32>,
    ) -> Result<(), Error> {
        out.push(layout::Item::SymbolBranch {
            target: symbol,
            link: true,
        });
        let label = self.label()?;
        out.push(layout::Item::Label(label));
        self.anchors.push(Anchor {
            label,
            kind,
            block,
            instruction,
        });
        Ok(())
    }
    pub fn entry(&mut self, out: &mut Vec<layout::Item>, required: u32) -> Result<(), Error> {
        let symbol = self
            .bindings
            .ok_or_else(|| super::error("missing morestack binding"))?
            .morestack_or_poll
            .clone();
        let retry = self.label()?;
        let ready = self.label()?;
        out.push(layout::Item::Label(retry));
        adjust(out, Reg::x(11), Reg::sp(Width::X), required, true)?;
        adjust(
            out,
            Reg::x(9),
            Reg::x(27),
            crate::runtime::coroutine_schema::stack_check_offset(),
            false,
        )?;
        emit(
            out,
            "ldar.X",
            vec![Operand::Reg(Reg::x(9)), memory(Reg::x(9))],
        )?;
        emit(
            out,
            "subs.X.shift",
            vec![
                Operand::Reg(Reg::zr(Width::X)),
                Operand::Reg(Reg::x(11)),
                Operand::Reg(Reg::x(9)),
                Operand::Shift(super::inst::Shift::Lsl, 0),
            ],
        )?;
        Self::branch(out, "b.cond", vec![Operand::Condition(10)], ready)?;
        adjust(out, Reg::x(10), Reg::x(30), 0, false)?;
        self.call(out, symbol, SiteKind::MorestackEntry, None, None)?;
        adjust(out, Reg::x(30), Reg::x(10), 0, false)?;
        Self::branch(out, "b", vec![], retry)?;
        out.push(layout::Item::Label(ready));
        Ok(())
    }
    pub fn poll(
        &mut self,
        out: &mut Vec<layout::Item>,
        block: u32,
        instruction: u32,
    ) -> Result<(), Error> {
        let symbol = self
            .bindings
            .ok_or_else(|| super::error("missing poll binding"))?
            .safepoint_slow
            .clone();
        let done = self.label()?;
        adjust(
            out,
            Reg::x(16),
            Reg::x(28),
            crate::runtime::processor::poll_flags_offset(),
            false,
        )?;
        emit(
            out,
            "ldar.W",
            vec![Operand::Reg(Reg::w(17)), memory(Reg::x(16))],
        )?;
        Self::branch(out, "cbz.W", vec![Operand::Reg(Reg::w(17))], done)?;
        self.call(
            out,
            symbol,
            SiteKind::PollResume,
            Some(block),
            Some(instruction),
        )?;
        out.push(layout::Item::Label(done));
        Ok(())
    }
}

fn memory(base: Reg) -> Operand {
    Operand::Address(Address {
        base,
        mode: AddressMode::Base,
    })
}

pub(crate) fn validate(body: &Body) -> Result<(), Error> {
    if body.signature.sret.is_some()
        || !body.signature.by_value.is_empty()
        || !body.root_inits.is_empty()
        || body
            .values
            .iter()
            .any(|v| v.kind.provenance.is_some_and(|p| p.root_kind().is_some()))
        || body
            .signature
            .parameters
            .iter()
            .chain(&body.signature.results)
            .any(|v| v.provenance.is_some_and(|p| p.root_kind().is_some()))
        || body.stack_slots.iter().any(|slot| !slot.roots.is_empty())
    {
        return Err(super::error("roots/sret require stack maps (4c.2)"));
    }
    let mut checks = 0;
    for (index, inst) in body.instructions.iter().enumerate() {
        let block = body
            .blocks
            .iter()
            .position(|b| b.instructions.contains(&(index as u32)));
        let fail = |reason: &str| {
            super::error(&format!(
                "block {block:?} site {index} {:?}: {reason}",
                inst.op
            ))
        };
        if matches!(inst.op, Op::StackCheck | Op::SafepointPoll { .. }) {
            if block.is_none()
                || body
                    .blocks
                    .iter()
                    .filter(|b| b.instructions.contains(&(index as u32)))
                    .count()
                    != 1
            {
                return Err(fail("runtime site must belong to one block"));
            }
            if !inst.arguments.is_empty() || !inst.results.is_empty() || inst.memory.is_none() {
                return Err(fail(
                    "runtime marker requires memory sequencing and no value operands",
                ));
            }
        }
        if matches!(inst.op, Op::StackCheck) {
            checks += 1;
            if block != Some(body.entry.index())
                || body
                    .blocks
                    .get(body.entry.index())
                    .is_none_or(|b| b.instructions.start != index as u32)
                || checks > 1
            {
                return Err(fail("StackCheck must be the single entry marker"));
            }
        }
        if matches!(inst.op, Op::Call(_) | Op::ForeignCall(_) | Op::StackAddr(_)) {
            return Err(fail("calls/stack addresses require stack maps (4c.2)"));
        }
        if inst.safepoint.is_some() && !matches!(inst.op, Op::StackCheck | Op::SafepointPoll { .. })
        {
            return Err(fail("safepoint requires stack maps (4c.2)"));
        }
        if let Some(id) = inst.safepoint {
            let expected = if matches!(inst.op, Op::StackCheck) {
                crate::lir::body::SafepointKind::StackCheck
            } else {
                crate::lir::body::SafepointKind::Poll
            };
            if body.safepoints.get(id.index()).is_none_or(|point| {
                point.kind != expected
                    || Some(point.block.index()) != block
                    || point.instruction.is_none_or(|site| site.index() != index)
            }) {
                return Err(fail("invalid runtime safepoint table reference"));
            }
        }
    }
    if body.blocks.iter().any(|b| {
        matches!(
            b.terminator,
            Terminator::Invoke { .. } | Terminator::TailCall { .. }
        )
    }) {
        return Err(super::error("call terminator requires stack maps (4c.2)"));
    }
    for (index, point) in body.safepoints.iter().enumerate() {
        if !matches!(
            point.kind,
            crate::lir::body::SafepointKind::StackCheck | crate::lir::body::SafepointKind::Poll
        ) {
            return Err(super::error("safepoint table requires stack maps (4c.2)"));
        }
        if point
            .instruction
            .and_then(|id| body.instructions.get(id.index()))
            .is_none_or(|inst| inst.safepoint.is_none_or(|id| id.index() != index))
        {
            return Err(super::error("unreferenced runtime safepoint table entry"));
        }
    }
    Ok(())
}
