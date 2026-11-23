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
    CallReturn,
    SuspendResume,
    ForeignBridge { dirty: bool },
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
    // invoke 落点：(landing_pc, cleanup 链)，由 Anchor.landing 经 plan.labels 解析
    pub landing: Option<(u32, u32)>,
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
    // invoke 落点：(目标 block 标签, cleanup 链)
    landing: Option<(u32, u32)>,
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
        let landing = match anchor.landing {
            Some((label, cleanup)) => {
                let target = *plan
                    .labels
                    .get(&label)
                    .ok_or_else(|| super::error("missing invoke landing target label"))?;
                if target >= plan.size || target % 4 != 0 {
                    return Err(super::error("invoke landing target is not inside code"));
                }
                Some((
                    u32::try_from(target)
                        .map_err(|_| super::error("invoke landing PC exceeds u32"))?,
                    cleanup,
                ))
            }
            None => None,
        };
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
            landing,
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
    pub(super) fn foreign_symbol(&self, dirty: bool) -> Result<String, Error> {
        let bindings = self
            .bindings
            .and_then(|bindings| bindings.foreign.as_ref())
            .ok_or_else(|| {
                super::error("A64 foreign bridge requires explicit Gugu dispatcher bindings")
            })?;
        Ok(if dirty {
            &bindings.dirty_dispatch
        } else {
            &bindings.ordinary_dispatch
        }
        .clone())
    }
    pub fn new(bindings: Option<&'a RuntimeBindings>, labels: usize) -> Result<Self, Error> {
        Ok(Self {
            bindings,
            next: u32::try_from(labels).map_err(|_| super::error("too many CFG labels"))?,
            anchors: vec![],
        })
    }
    pub(super) fn label(&mut self) -> Result<u32, Error> {
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
    pub(super) fn call(
        &mut self,
        out: &mut Vec<layout::Item>,
        symbol: String,
        kind: SiteKind,
        block: Option<u32>,
        instruction: Option<u32>,
        landing: Option<(u32, u32)>,
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
            landing,
        });
        Ok(())
    }
    /// 间接调用目标（Code 参数物化后的物理寄存器）；锚点与符号调用同记录。
    pub(super) fn call_register(
        &mut self,
        out: &mut Vec<layout::Item>,
        register: u8,
        kind: SiteKind,
        block: Option<u32>,
        instruction: Option<u32>,
        landing: Option<(u32, u32)>,
    ) -> Result<(), Error> {
        emit(out, "blr", vec![Operand::Reg(Reg::x(register))])?;
        let label = self.label()?;
        out.push(layout::Item::Label(label));
        self.anchors.push(Anchor {
            label,
            kind,
            block,
            instruction,
            landing,
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
        self.call(out, symbol, SiteKind::MorestackEntry, None, None, None)?;
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
            None,
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
    // 聚合签名（by_value/sret）只占用 Ptr 车道，无额外后端集成义务。
    super::locals::validate(body)?;
    let entry = body
        .blocks
        .get(body.entry.index())
        .ok_or_else(|| super::error("invalid runtime entry"))?;
    let parameters = body
        .parameters
        .get(entry.parameters.start as usize..entry.parameters.end as usize)
        .ok_or_else(|| super::error("invalid entry parameter arena"))?;
    let params = parameters
        .iter()
        .map(|p| body.values.get(p.value.index()).map(|v| v.kind))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| super::error("invalid entry parameter value"))?;
    let params = params
        .into_iter()
        .filter(|p| p.ty != crate::lir::body::Type::Mem)
        .collect::<Vec<_>>();
    if params != body.signature.parameters {
        return Err(super::error(
            "entry parameter provenance/signature mismatch (4c.2)",
        ));
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
        if let Op::StackAddr(slot) = inst.op
            && (body.stack_slots.get(slot.index()).is_none()
                || body
                    .values
                    .get(inst.results.start as usize..inst.results.end as usize)
                    .is_none_or(|results| {
                        results.len() != 1
                            || results[0].kind
                                != crate::lir::body::ValueType::pointer(
                                    crate::lir::body::Provenance::Stack,
                                )
                    }))
        {
            return Err(fail(
                "runtime StackAddr requires a valid slot and Stack provenance",
            ));
        }
        if let Op::Call(ref call) | Op::ForeignCall(ref call) = inst.op {
            if (call.sret.is_some() && call.kind == crate::gir::body::CallKind::Managed)
                || (call.poll_free_leaf
                    && (call.may_allocate || call.may_unwind || call.may_suspend))
            {
                return Err(fail("sret or inconsistent poll-free proof is unsupported"));
            }
            if !call.poll_free_leaf && inst.safepoint.is_none() {
                return Err(fail(
                    "managed call requires an authoritative CallReturn/Suspend map",
                ));
            }
        }
        if inst.safepoint.is_some()
            && !matches!(
                inst.op,
                Op::StackCheck | Op::SafepointPoll { .. } | Op::Call(_) | Op::ForeignCall(_)
            )
            && !super::lower::runtime::handles(&inst.op)
        {
            return Err(fail("safepoint requires stack maps (4c.2)"));
        }
        if let Some(id) = inst.safepoint {
            let expected = inst
                .op
                .safepoint_kind()
                .ok_or_else(|| fail("operation has no canonical safepoint kind"))?;
            if body.safepoints.get(id.index()).is_none_or(|point| {
                let call_return=point.kind==crate::lir::body::SafepointKind::CallReturn && expected==crate::lir::body::SafepointKind::Allocation && matches!(&inst.op,Op::Call(call) if matches!(call.target,crate::lir::body::CallTarget::Instance(_)));
                (point.kind != expected && !call_return)
                    || Some(point.block.index()) != block
                    || point.instruction.is_none_or(|site| site.index() != index)
            }) {
                return Err(fail("invalid runtime safepoint table reference"));
            }
        }
        if super::runtime::implicit_call(&inst.op)
            && inst.op.safepoint_kind().is_some()
            && inst.safepoint.is_none()
        {
            return Err(fail(
                "runtime operation requires an authoritative safepoint",
            ));
        }
    }
    for block in body.blocks.iter() {
        match &block.terminator {
            Terminator::Invoke {
                call,
                safepoint,
                normal,
                unwind,
                ..
            } => {
                if call.poll_free_leaf || safepoint.is_none() {
                    return Err(super::error(
                        "invoke requires an authoritative CallReturn map",
                    ));
                }
                if body.edges.get(normal.index()).is_none_or(|e| e.unwind)
                    || body.edges.get(unwind.index()).is_none_or(|e| !e.unwind)
                {
                    return Err(super::error("invoke requires normal and unwind edges"));
                }
            }
            Terminator::TailCall { call, .. } if super::lower::call::requires_maps(call) => {
                return Err(super::error("tail call requires stack maps (4c.2)"));
            }
            _ => {}
        }
    }
    for (index, point) in body.safepoints.iter().enumerate() {
        if !matches!(
            point.kind,
            crate::lir::body::SafepointKind::StackCheck
                | crate::lir::body::SafepointKind::Poll
                | crate::lir::body::SafepointKind::CallReturn
                | crate::lir::body::SafepointKind::Suspend
                | crate::lir::body::SafepointKind::ForeignBridge
                | crate::lir::body::SafepointKind::DirtyCpuBridge
                | crate::lir::body::SafepointKind::Allocation
                | crate::lir::body::SafepointKind::Barrier
                | crate::lir::body::SafepointKind::Select
        ) {
            return Err(super::error("safepoint table requires stack maps (4c.2)"));
        }
        let referenced = point
            .instruction
            .and_then(|id| body.instructions.get(id.index()))
            .is_some_and(|inst| inst.safepoint.is_some_and(|id| id.index() == index))
            || (point.instruction.is_none()
                && body.blocks.get(point.block.index()).is_some_and(|b| {
                    matches!(&b.terminator, Terminator::Invoke { safepoint, .. } if safepoint.is_some_and(|id| id.index() == index))
                }));
        if !referenced {
            return Err(super::error("unreferenced runtime safepoint table entry"));
        }
    }
    Ok(())
}
