//! 稳定 GIR dump：按 owner 定义路径、block ID 与 local ID 排序，不含地址或桶序。
use super::GirWorldV1;
use super::body::*;
use crate::frontend::hir::{self, TypeId};
use std::fmt::Write;

pub(crate) fn dump_world(module: &hir::Module, world: &GirWorldV1) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "gir-revision {GIR_REVISION} schema {} bodies {} fingerprint {}",
        world.schema,
        world.bodies.len(),
        hex(&world.fingerprint)
    );
    for body in &world.bodies {
        dump_body(&mut out, module, body);
    }
    dump_placement(&mut out, &world.placement);
    out
}

fn dump_placement(out: &mut String, world: &super::placement::PlacementWorldV1) {
    let _ = writeln!(
        out,
        "placement schema {} records {} allocs {} fingerprint {}",
        world.schema,
        world.records.len(),
        world.allocs.len(),
        hex(&world.fingerprint)
    );
    for record in &world.records {
        let _ = writeln!(
            out,
            "  local body={} local={} {:?} {:?} export={}",
            record.body, record.local, record.kind, record.proof, record.export
        );
    }
    for alloc in &world.allocs {
        let _ = writeln!(
            out,
            "  alloc body={} stmt={} {:?} {:?} export={}",
            alloc.body, alloc.statement, alloc.kind, alloc.proof, alloc.export
        );
    }
}

pub(crate) fn dump_body(out: &mut String, module: &hir::Module, body: &GirBody) {
    let name = module
        .definitions
        .get(body.owner.index())
        .map(|definition| definition.name.as_str())
        .unwrap_or("?");
    let _ = writeln!(
        out,
        "body owner={name} kind={:?} locals={} blocks={} flags={}",
        body.kind,
        body.locals.len(),
        body.blocks.len(),
        body.flags
    );
    for (index, local) in body.locals.iter().enumerate() {
        let _ = writeln!(
            out,
            "  local {index} {:?} {} pinned={}",
            local.kind,
            type_name(module, local.ty),
            u8::from(local.pinned_storage)
        );
    }
    for (index, block) in body.blocks.iter().enumerate() {
        let mark = if body.entry.index() == index {
            " entry"
        } else {
            ""
        };
        let cleanup = if block.cleanup { " cleanup" } else { "" };
        let _ = writeln!(out, "  block {index}{mark}{cleanup}");
        for statement in body.block_statements(BlockId(index as u32)) {
            let _ = writeln!(out, "    {}", statement_text(module, body, statement));
        }
        let _ = writeln!(
            out,
            "    {}",
            terminator_text(module, body, &block.terminator)
        );
    }
    for record in &body.exit_records {
        let dest = record
            .destination
            .map(|block| block.0.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let _ = writeln!(
            out,
            "  exit plan={} chain={:?} entry={} dest={dest}",
            record.plan, record.chain, record.entry.0
        );
    }
}

fn statement_text(module: &hir::Module, body: &GirBody, statement: &Statement) -> String {
    match &statement.kind {
        StatementKind::StorageLive(local) => format!("StorageLive(_{})", local.0),
        StatementKind::StorageDead(local) => format!("StorageDead(_{})", local.0),
        StatementKind::Assign(place, rvalue) => {
            format!(
                "{} = {}",
                place_text(module, body, *place),
                rvalue_text(module, body, rvalue)
            )
        }
        StatementKind::ScopedViewBegin {
            source,
            mode,
            token,
        } => format!(
            "ScopedViewBegin {:?} {} -> _{}",
            mode,
            place_text(module, body, *source),
            token.0
        ),
        StatementKind::ScopedViewEnd { token } => format!("ScopedViewEnd(_{})", token.0),
        StatementKind::NoSafepointBegin(id) => format!("NoSafepointBegin({})", id.0),
        StatementKind::NoSafepointEnd(id) => format!("NoSafepointEnd({})", id.0),
        StatementKind::SafepointPoll(id) => format!("SafepointPoll({})", id.0),
        StatementKind::Nop => "Nop".to_owned(),
        StatementKind::ValueAction {
            action,
            place,
            descriptor,
        } => format!(
            "ValueAction {action:?} {} : {}",
            place_text(module, body, *place),
            type_name(module, *descriptor)
        ),
        StatementKind::ResourceAction {
            action,
            place,
            descriptor,
        } => format!(
            "ResourceAction {action:?} {} : {}",
            place_text(module, body, *place),
            type_name(module, *descriptor)
        ),
        other => format!("{other:?}"),
    }
}

fn terminator_text(module: &hir::Module, body: &GirBody, terminator: &Terminator) -> String {
    match terminator {
        Terminator::Goto { target } => format!("goto {}", target.0),
        Terminator::SwitchInt {
            value,
            targets,
            otherwise,
        } => {
            let arms = targets
                .iter()
                .map(|(disc, block)| format!("{disc} -> {}", block.0))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "switch {} [{arms}] else {}",
                operand_text(module, body, value),
                otherwise.0
            )
        }
        Terminator::Call {
            destination,
            normal,
            unwind,
            call_kind,
            ..
        } => format!(
            "{} = Call {:?} -> {} unwind={}",
            place_text(module, body, *destination),
            call_kind,
            normal.0,
            unwind.map(|block| block.0.to_string()).unwrap_or_default()
        ),
        Terminator::Return => "return".to_owned(),
        Terminator::Panic { unwind, .. } => format!("panic unwind={}", unwind.0),
        Terminator::ResumePanic => "resume".to_owned(),
        Terminator::Abort => "abort".to_owned(),
        Terminator::Unreachable => "unreachable".to_owned(),
        Terminator::Suspend {
            reason,
            resume,
            cancelled,
            ..
        } => format!(
            "suspend {reason:?} resume={} cancelled={}",
            resume.0,
            cancelled
                .map(|block| block.0.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
        Terminator::SelectCommit {
            ready,
            suspend,
            cancelled,
            ..
        } => format!(
            "select ready={} suspend={} cancelled={}",
            ready.0,
            suspend
                .map(|block| block.0.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            cancelled
                .map(|block| block.0.to_string())
                .unwrap_or_else(|| "-".to_owned())
        ),
    }
}

fn rvalue_text(module: &hir::Module, body: &GirBody, rvalue: &Rvalue) -> String {
    match rvalue {
        Rvalue::Use(operand) => operand_text(module, body, operand),
        Rvalue::UnaryOp { op, operand } => {
            format!("{op:?} {}", operand_text(module, body, operand))
        }
        Rvalue::BinaryOp { op, left, right } => format!(
            "{op:?} {}, {}",
            operand_text(module, body, left),
            operand_text(module, body, right)
        ),
        Rvalue::CheckedOp { check, kind, .. } => format!("CheckedOp#{check} {kind:?}"),
        Rvalue::Compare { op, left, right } => format!(
            "{op:?} {}, {}",
            operand_text(module, body, left),
            operand_text(module, body, right)
        ),
        Rvalue::Aggregate { kind, operands } => {
            format!("{kind:?} x{}", operands.len())
        }
        Rvalue::Repeat { count, .. } => format!("repeat {count}"),
        Rvalue::Discriminant(place) => format!("disc {}", place_text(module, body, *place)),
        Rvalue::Len(place) => format!("len {}", place_text(module, body, *place)),
        Rvalue::Ref(place) => format!("&{}", place_text(module, body, *place)),
        Rvalue::RawAddress(place) => format!("addr {}", place_text(module, body, *place)),
        Rvalue::Cast { kind, ty, .. } => format!("cast {kind:?} {}", type_name(module, *ty)),
        Rvalue::Intrinsic { op, .. } => format!("intrinsic {op:?}"),
        Rvalue::ValueCopy(place) => format!("value_copy {}", place_text(module, body, *place)),
        Rvalue::CowSnapshot(place) => format!("cow_snapshot {}", place_text(module, body, *place)),
        Rvalue::DynErase { operand, ty } => format!(
            "dyn_erase {} -> {}",
            operand_text(module, body, operand),
            type_name(module, *ty)
        ),
        other => format!("{other:?}"),
    }
}

fn operand_text(module: &hir::Module, body: &GirBody, operand: &Operand) -> String {
    match operand {
        Operand::Copy(place) => format!("copy {}", place_text(module, body, *place)),
        Operand::MoveInternal(place) => format!("move {}", place_text(module, body, *place)),
        Operand::Constant(id) => match body.constants.get(id.index()) {
            Some(constant) => format!("const {:?}", constant.value),
            None => format!("const#{}", id.0),
        },
        Operand::LateConstRef { expression } => format!("late#{expression}"),
        Operand::Function(candidate) => format!("fn {}", candidate.definition.0),
    }
}

fn place_text(module: &hir::Module, body: &GirBody, place: Place) -> String {
    let mut text = format!("_{}", place.local.0);
    for projection in body.projections_of(place) {
        match projection {
            Projection::Deref => text = format!("(*{text})"),
            Projection::Field { index, .. } => text = format!("{text}.{index}"),
            Projection::TupleField { index, .. } => text = format!("{text}.{index}"),
            Projection::Index(local) => text = format!("{text}[_{}]", local.0),
            Projection::ConstantIndex { offset, from_end } => {
                text = format!("{text}[{offset}{}]", if *from_end { "^" } else { "" });
            }
            Projection::Subslice { from, to, from_end } => {
                text = format!("{text}[{from}..{to}{}]", if *from_end { "^" } else { "" });
            }
            Projection::Downcast(variant) => text = format!("{text} as {variant}"),
            Projection::OpaqueCast(ty) => text = format!("{text} as {}", type_name(module, *ty)),
        }
    }
    text
}

fn type_name(module: &hir::Module, ty: TypeId) -> String {
    match module.types.get(ty.index()) {
        Some(hir::Type::Unit) => "unit".to_owned(),
        Some(hir::Type::Never) => "never".to_owned(),
        Some(hir::Type::Bool) => "bool".to_owned(),
        Some(hir::Type::Int { signed, bits }) => {
            format!("{}{bits}", if *signed { "i" } else { "u" })
        }
        Some(hir::Type::Ptr(inner)) => format!("*{}", type_name(module, *inner)),
        Some(hir::Type::Ref(inner)) => format!("&{}", type_name(module, *inner)),
        Some(other) => format!("{other:?}"),
        None => format!("t{}", ty.0),
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
