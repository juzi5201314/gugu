//! Scalar root placement over authoritative LIR anchors and shared allocations.
use super::{
    Error,
    alloc::Allocation,
    frame::Frame,
    sites::{RuntimeSite, SiteKind},
};
use crate::{
    lir::{
        self,
        body::{Body, Definition, Op},
        stackmap::{LogicalRoot, LogicalSafepoint},
    },
    regalloc::{Location, Placement},
    runtime::{
        self,
        stackmap_codec::{self, a64_unwind},
        stackmap_schema::StackMapArchitecture,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub stackmaps: Vec<u8>,
    pub unwind: Vec<u8>,
}

fn fail(error: impl std::fmt::Display) -> Error {
    super::error(&error.to_string())
}

pub(crate) fn logical(body: &Body) -> Result<Vec<LogicalSafepoint>, Error> {
    let needed = body.stack_slots.iter().any(|slot| !slot.roots.is_empty())
        || body
            .values
            .iter()
            .any(|v| v.kind.provenance.is_some_and(|p| p.root_kind().is_some()))
        || body
            .instructions
            .iter()
            .any(|inst| matches!(&inst.op, Op::Call(call) | Op::ForeignCall(call) if !call.poll_free_leaf));
    if !needed {
        return Ok(vec![]);
    }
    for value in &body.values {
        let valid = match value.definition {
            Definition::Parameter { block, .. } => body.blocks.get(block.index()).is_some(),
            Definition::Instruction { instruction, .. } => {
                body.instructions.get(instruction.index()).is_some()
            }
            Definition::Invoke { .. } => false,
        };
        if !valid {
            return Err(super::error("invalid value definition for logical roots"));
        }
        if value
            .kind
            .provenance
            .is_some_and(|p| p.root_kind().is_some())
            && super::lower::bits(value.kind.ty)? != 64
        {
            return Err(super::error(
                "scalar roots require full-width pointer lanes",
            ));
        }
    }
    let mut derived = body.clone();
    lir::uses::rebuild(&mut derived);
    lir::stackmap::anchors(&derived).map_err(|error| super::error(error.message()))
}

pub(crate) fn build(
    body: &Body,
    allocation: &Allocation,
    frame: &Frame,
    size: u64,
    sites: &[RuntimeSite],
    logical: &[LogicalSafepoint],
) -> Result<Metadata, Error> {
    let mut points = Vec::new();
    let graph = lir::Graph::new(body).map_err(|error| super::error(error.message()))?;
    let init = lir::root_init::InitSlots::new(body, &graph);
    let init = body
        .edges
        .iter()
        .all(|edge| edge.to != body.entry)
        .then_some(&init);
    for site in sites {
        let entry = site.kind == SiteKind::MorestackEntry;
        let kind = match site.kind {
            SiteKind::MorestackEntry => 4,
            SiteKind::PollResume => 1,
            SiteKind::CallReturn => 0,
            SiteKind::SuspendResume => 2,
            SiteKind::ForeignBridge { .. } => 3,
        };
        let anchor = logical.iter().find(|a| {
            a.kind == kind
                && (entry || Some(a.block) == site.block && Some(a.instruction) == site.instruction)
        });
        let mut map = runtime::SafepointLayout::<u32> {
            pc_offset: site.pc_offset,
            kind,
            dirty: false,
            copy_allowed: !entry,
            scan_allowed: true,
            slots: crate::metadata::empty_bitmaps(if entry { 0 } else { frame.size / 8 }),
            registers: [0; 5],
            typed: vec![],
            function: 0,
            slot_count: if entry { 0 } else { frame.size / 8 },
        };
        if entry {
            let (locations, _) =
                super::abi::arguments(body.signature.parameters.iter().map(|p| p.ty))?;
            for (parameter, location) in body.signature.parameters.iter().zip(locations) {
                if let (Some(class), super::abi::Argument::Register(id)) =
                    (parameter.provenance.and_then(|p| p.root_kind()), location)
                {
                    map.registers[class as usize] |= 1 << id;
                }
            }
        } else if let Some(anchor) = anchor {
            map.copy_allowed = anchor.flags & 1 != 0;
            map.scan_allowed = anchor.flags & 2 != 0;
            if let SiteKind::ForeignBridge { dirty } = site.kind {
                if dirty != anchor.dirty {
                    return Err(super::error("foreign bridge dirty-map mismatch"));
                }
                map.dirty = dirty;
            }
            let position = allocation
                .site_positions
                .get(&(anchor.block, anchor.instruction))
                .copied()
                .ok_or_else(|| super::error("missing selected safepoint position"))?;
            let groups = [
                &anchor.roots.direct,
                &anchor.roots.interior,
                &anchor.roots.handle,
                &anchor.roots.compressed,
                &anchor.roots.stack,
            ];
            for (class, roots) in groups.into_iter().enumerate() {
                for root in roots {
                    match root {
                        LogicalRoot::Value { instance, value } => {
                            if *instance != body.instance {
                                return Err(super::error("root belongs to another function"));
                            }
                            let definition = body
                                .values
                                .get(*value as usize)
                                .ok_or_else(|| super::error("unknown logical root value"))?
                                .definition;
                            if matches!(definition, Definition::Instruction { instruction, .. } if instruction.0 == anchor.instruction)
                            {
                                continue;
                            }
                            place_value(allocation, frame, &mut map, class, *value, position)?;
                            if site.kind == SiteKind::SuspendResume {
                                let inst = &body.instructions[anchor.instruction as usize];
                                for (index, argument) in
                                    body.args(&inst.arguments).iter().enumerate()
                                {
                                    if argument.0 == *value
                                        && let Some(offset) = argument_offset(body, inst, index)?
                                    {
                                        set_slot(&mut map, class, offset)?;
                                    }
                                }
                            }
                        }
                        LogicalRoot::Argument { index } => {
                            let inst = &body.instructions[anchor.instruction as usize];
                            let value = body
                                .args(&inst.arguments)
                                .get(*index as usize)
                                .ok_or_else(|| super::error("unknown call root argument"))?;
                            place_value(allocation, frame, &mut map, class, value.0, position)?;
                            if let Some(offset) = argument_offset(body, inst, *index as usize)? {
                                set_slot(&mut map, class, offset)?;
                            }
                        }
                        LogicalRoot::Slot { slot, offset } => {
                            let local = *frame
                                .locals
                                .get(*slot as usize)
                                .ok_or_else(|| super::error("rooted local has no frame offset"))?;
                            let (word, gate) = crate::metadata::local_root(
                                body,
                                anchor,
                                class,
                                *slot,
                                *offset,
                                local,
                                frame.init_bases.get(*slot as usize).copied().flatten(),
                                init,
                            )
                            .map_err(super::error)?;
                            if let Some(init_byte) = gate {
                                map.typed.push(runtime::TypedRoot {
                                    word,
                                    root_kind: class as u32,
                                    init_byte,
                                });
                            } else {
                                set_slot(&mut map, class, word * 8)?;
                            }
                        }
                    }
                }
            }
        } else if !logical.is_empty()
            || matches!(
                site.kind,
                SiteKind::CallReturn | SiteKind::SuspendResume | SiteKind::ForeignBridge { .. }
            )
            || body
                .values
                .iter()
                .any(|v| v.kind.provenance.is_some_and(|p| p.root_kind().is_some()))
        {
            return Err(super::error(
                "mapped runtime site requires an authoritative logical root anchor",
            ));
        }
        points.push((0, map));
    }
    let code = runtime::CodeLayout {
        code_rva: 0,
        code_size: u32::try_from(size).map_err(fail)?,
        frame_size: frame.size,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: points.iter().any(|(_, map)| {
            map.registers[4] != 0
                || map.slots[4].iter().any(|word| *word != 0)
                || map.typed.iter().any(|root| root.root_kind == 4)
        }),
    };
    let stackmaps = stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &[code], &points)
        .map_err(fail)?;
    let unwind_sites = sites
        .iter()
        .map(|site| a64_unwind::Site {
            pc_offset: site.pc_offset,
            entry: site.kind == SiteKind::MorestackEntry,
        })
        .collect::<Vec<_>>();
    let unwind = a64_unwind::encode(code, frame.record, &unwind_sites).map_err(fail)?;
    a64_unwind::decode(&unwind)
        .and_then(|table| table.verify_joint(&runtime::decode_tables(&stackmaps)?))
        .map_err(fail)?;
    Ok(Metadata { stackmaps, unwind })
}

fn place_value(
    allocation: &Allocation,
    frame: &Frame,
    map: &mut runtime::SafepointLayout<u32>,
    class: usize,
    value: u32,
    position: u32,
) -> Result<(), Error> {
    let allocated = allocation
        .values
        .get(value as usize)
        .ok_or_else(|| super::error("logical root has no allocation"))?;
    if allocated.range.0 > position || allocated.range.1 < position {
        return Err(super::error(
            "logical root does not cover its selected safepoint",
        ));
    }
    let Placement::Location(Location::Slot(slot)) = allocated.placement else {
        return Err(super::error(
            "live root was not spilled by shared allocation",
        ));
    };
    set_slot(
        map,
        class,
        *frame
            .spills
            .get(slot as usize)
            .ok_or_else(|| super::error("missing root spill frame offset"))?,
    )
}

fn set_slot(
    map: &mut runtime::SafepointLayout<u32>,
    class: usize,
    offset: u32,
) -> Result<(), Error> {
    if !offset.is_multiple_of(8) || offset / 8 >= map.slot_count {
        return Err(super::error("root offset is outside frame"));
    }
    let word = offset / 8;
    map.slots[class][word as usize / 64] |= 1 << (word % 64);
    Ok(())
}

fn argument_offset(
    body: &crate::lir::body::Body,
    inst: &crate::lir::body::Instruction,
    index: usize,
) -> Result<Option<u32>, Error> {
    if matches!(&inst.op,Op::Call(call) | Op::ForeignCall(call) if call.kind != crate::gir::body::CallKind::Managed)
    {
        return Ok(None);
    }
    let (locations, _) = super::abi::arguments(
        body.args(&inst.arguments)
            .iter()
            .map(|id| body.values[id.index()].kind.ty),
    )?;
    Ok(match locations.get(index) {
        Some(super::abi::Argument::Stack(offset)) => Some(*offset),
        _ => None,
    })
}
