use super::{Builder, bits, signature};
use crate::{
    a64::{
        Error,
        atomic::Atomic,
        inst::Operand,
        machine::{self, Item, MReg},
    },
    gir::body::MemoryOrdering,
    lir::body::{AtomicOp, Body, Instruction, Op, Provenance, Type, ValueType},
};

pub fn lower(body: &Body, inst: &Instruction, b: &mut Builder<'_>) -> Result<(), Error> {
    use MemoryOrdering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
    let Op::Atomic {
        op,
        ordering,
        failure,
        align,
    } = inst.op
    else {
        unreachable!()
    };
    let args = body.args(&inst.arguments);
    let results_vec: Vec<crate::lir::body::ValueId> = body.results(inst).collect();
    let results = results_vec.as_slice();
    let valid = match op {
        AtomicOp::Load => matches!(ordering, Relaxed | Acquire | SeqCst),
        AtomicOp::Store => matches!(ordering, Relaxed | Release | SeqCst),
        AtomicOp::Fence => ordering != Relaxed,
        _ => true,
    };
    let valid_failure = if op == AtomicOp::CompareExchange {
        match failure {
            Some(Relaxed) => true,
            Some(Acquire) => matches!(ordering, Acquire | AcqRel | SeqCst),
            Some(SeqCst) => ordering == SeqCst,
            _ => false,
        }
    } else {
        failure.is_none()
    };
    if !valid || !valid_failure || inst.memory.is_none() {
        return Err(crate::a64::error(
            "invalid atomic ordering or missing sequencing token",
        ));
    }
    if op == AtomicOp::Fence {
        signature(body, args, results, &[], &[])?;
        return b.emit(
            "dmb",
            vec![machine::fixed(Operand::Imm(if ordering == Acquire {
                9
            } else {
                11
            }))],
        );
    }
    let value = if op == AtomicOp::Load {
        results.first()
    } else {
        args.get(1)
    }
    .ok_or_else(|| crate::a64::error("atomic operation lacks value"))?;
    let kind = body.values[value.index()].kind;
    if !(kind.ty.integer() && kind.provenance.is_none()
        || kind.ty == Type::Ptr && kind.provenance == Some(Provenance::Raw))
    {
        return Err(crate::a64::error(
            "atomic value must be integer or raw pointer",
        ));
    }
    let width = bits(kind.ty)?;
    if !align.is_power_of_two() || align < width / 8 {
        return Err(crate::a64::error("atomic value requires natural alignment"));
    }
    let (inputs, outputs) = match op {
        AtomicOp::Load => (vec![Type::Ptr], vec![kind.ty]),
        AtomicOp::Store => (vec![Type::Ptr, kind.ty], vec![]),
        AtomicOp::CompareExchange => (vec![Type::Ptr, kind.ty, kind.ty], vec![kind.ty, Type::I8]),
        _ => (vec![Type::Ptr, kind.ty], vec![kind.ty]),
    };
    signature(body, args, results, &inputs, &outputs)?;
    if args
        .iter()
        .skip(1)
        .chain(results.iter().take(1))
        .any(|id| body.values[id.index()].kind != kind)
    {
        return Err(crate::a64::error("atomic value provenance mismatch"));
    }
    if op == AtomicOp::CompareExchange
        && body.values[results[1].index()].kind != ValueType::scalar(Type::I8)
    {
        return Err(crate::a64::error("atomic CAS flag must be scalar I8"));
    }
    b.push(Item::Copies(
        args.iter()
            .enumerate()
            .map(|(i, id)| (MReg::Virtual(id.0), MReg::Physical(9 + i as u8)))
            .collect(),
    ));
    b.push(Item::Atomic(Atomic {
        op,
        ordering,
        bits: width,
    }));
    b.push(Item::Copies(
        results
            .iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    MReg::Physical(if i == 0 { 12 } else { 15 }),
                    MReg::Virtual(id.0),
                )
            })
            .collect(),
    ));
    Ok(())
}
