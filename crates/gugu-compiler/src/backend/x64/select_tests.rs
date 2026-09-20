//! 内部 ABI、mangling、布局与选指站点回归。

use super::abi::{self, AbiSlot};
use super::encode::assemble;
use super::inst::{Inst, LabelDefinition, LabelId, Operand, Sequence};
use super::layout;
use super::lower::{self, LoweringError, SiteValue};
use super::mangle;
use super::reg::{Gpr, Reg, Xmm};
use super::table::{self, OperandKind};
use super::verify::verify_inst;
use crate::frontend::gir::body::{CallKind, MemoryOrdering};
use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{AtomicOp, Call, CallTarget, Op, Signature, Type, ValueType};
use crate::target::{CpuBaseline, TargetName};

fn empty_universe() -> TypeUniverse {
    TypeUniverse::default()
}

fn signature(parameters: Vec<ValueType>, results: Vec<ValueType>) -> Signature {
    Signature {
        parameters,
        results,
        sret: None,
        by_value: Vec::new(),
    }
}

#[test]
fn integer_arg_uses_rax_and_f64_uses_xmm0() {
    let universe = empty_universe();
    let layout = abi::classify_signature(
        &signature(
            vec![ValueType::scalar(Type::I64), ValueType::scalar(Type::F64)],
            vec![ValueType::scalar(Type::I64)],
        ),
        &universe,
    )
    .expect("内部 ABI 可分类");
    assert_eq!(layout.arguments[0].slots, vec![AbiSlot::Integer(Gpr::Rax)]);
    assert_eq!(layout.arguments[1].slots, vec![AbiSlot::Float(Xmm::Xmm0)]);
    assert_eq!(layout.results[0].slots, vec![AbiSlot::Integer(Gpr::Rax)]);
    assert!(!layout.sret);
}

#[test]
fn sret_occupies_first_integer_slot() {
    let universe = empty_universe();
    let key = [1_u8; 32];
    let layout = abi::classify_signature(
        &Signature {
            parameters: vec![ValueType::scalar(Type::I64)],
            results: Vec::new(),
            sret: Some((24, 0, key)),
            by_value: Vec::new(),
        },
        &universe,
    )
    .expect("sret 可分类");
    assert!(layout.sret);
    assert_eq!(layout.arguments[0].slots, vec![AbiSlot::Integer(Gpr::Rax)]);
    assert_eq!(layout.arguments[1].slots, vec![AbiSlot::Integer(Gpr::Rbx)]);
}

#[test]
fn aggregate_over_16_bytes_is_indirect() {
    let universe = empty_universe();
    let key = [2_u8; 32];
    let layout = abi::classify_signature(
        &Signature {
            parameters: vec![ValueType::scalar(Type::Ptr)],
            results: vec![ValueType::scalar(Type::I64)],
            sret: None,
            by_value: vec![(0, key, 24)],
        },
        &universe,
    )
    .expect("大聚合可分类");
    assert!(layout.arguments[0].indirect);
    assert_eq!(layout.arguments[0].slots, vec![AbiSlot::Integer(Gpr::Rax)]);
}

#[test]
fn ordinary_sequence_must_not_write_r14() {
    let form =
        table::form_id("mov", &[OperandKind::Rm64, OperandKind::R64]).expect("mov r/m64, r64");
    let bad = Inst {
        form,
        operands: vec![
            Operand::Reg(Reg::Gpr(Gpr::R14)),
            Operand::Reg(Reg::Gpr(Gpr::Rax)),
        ],
        lock: false,
    };
    assert!(verify_inst(&bad, CpuBaseline::X86_64V1).is_err());
}

#[test]
fn mangling_is_gugu_fn_plus_64_hex() {
    let name = mangle::mangle_runtime("gc_alloc_slow");
    assert!(name.starts_with("__gugu_runtime_"));
    assert_eq!(name.len(), "__gugu_runtime_".len() + 64);
    let glue = mangle::mangle_glue("memcpy");
    assert!(glue.starts_with("__gugu_glue_"));
    assert_eq!(glue.len(), "__gugu_glue_".len() + 64);
}

#[test]
fn nosafepoint_begin_encodes_zero_bytes() {
    let lowered =
        lower::lower(&Op::NoSafepointBegin(0), &[], &[], &lower::probe_source()).expect("空序列");
    assert!(lowered.sequence.instructions.is_empty());
    let assembled = assemble(&lowered.sequence).expect("空序列可编码");
    assert!(assembled.bytes.is_empty());
}

#[test]
fn atomic_relaxed_load_is_mov_without_mfence() {
    let lowered = lower::lower(
        &Op::Atomic {
            op: AtomicOp::Load,
            ordering: MemoryOrdering::Relaxed,
            failure: None,
            align: 8,
        },
        &[SiteValue {
            ty: ValueType::scalar(Type::Ptr),
            reg: Reg::Virtual(0),
        }],
        &[SiteValue {
            ty: ValueType::scalar(Type::I64),
            reg: Reg::Virtual(1),
        }],
        &lower::probe_source(),
    )
    .expect("Relaxed load");
    let mnemonics: Vec<_> = lowered
        .sequence
        .instructions
        .iter()
        .map(|inst| table::form(inst.form).mnemonic)
        .collect();
    assert!(mnemonics.iter().any(|name| *name == "mov"), "{mnemonics:?}");
    assert!(
        mnemonics.iter().all(|name| *name != "mfence"),
        "{mnemonics:?}"
    );
}

#[test]
fn acqrel_cas_has_lock_cmpxchg() {
    let cas = lower::lower(
        &Op::Atomic {
            op: AtomicOp::CompareExchange,
            ordering: MemoryOrdering::AcqRel,
            failure: Some(MemoryOrdering::Relaxed),
            align: 8,
        },
        &[
            SiteValue {
                ty: ValueType::scalar(Type::Ptr),
                reg: Reg::Virtual(0),
            },
            SiteValue {
                ty: ValueType::scalar(Type::I64),
                reg: Reg::Virtual(1),
            },
            SiteValue {
                ty: ValueType::scalar(Type::I64),
                reg: Reg::Virtual(2),
            },
        ],
        &[
            SiteValue {
                ty: ValueType::scalar(Type::I64),
                reg: Reg::Virtual(3),
            },
            SiteValue {
                ty: ValueType::scalar(Type::I8),
                reg: Reg::Virtual(4),
            },
        ],
        &lower::probe_source(),
    )
    .expect("CAS");
    assert!(cas.sequence.instructions.iter().any(|inst| inst.lock));
    let mnemonics: Vec<_> = cas
        .sequence
        .instructions
        .iter()
        .map(|inst| table::form(inst.form).mnemonic)
        .collect();
    assert!(
        mnemonics.iter().any(|name| *name == "cmpxchg"),
        "{mnemonics:?}"
    );
}

#[test]
fn rel8_fits_short_backedge() {
    let mut seq = Sequence {
        instructions: vec![Inst {
            form: table::form_id("jmp", &[OperandKind::Rel32]).expect("jmp rel32"),
            operands: vec![Operand::Label(LabelId(0))],
            lock: false,
        }],
        labels: vec![LabelDefinition {
            label: LabelId(0),
            at: 0,
        }],
    };
    let count = layout::relax(&mut seq).expect("可收缩");
    assert_eq!(
        table::form(seq.instructions[0].form).operands,
        [OperandKind::Rel8]
    );
    assert!(count >= 1);
}

#[test]
fn classify_call_managed_uses_internal_integer_order() {
    let universe = empty_universe();
    let call = Call {
        target: CallTarget::Instance([0; 32]),
        kind: CallKind::Managed,
        parameters: vec![ValueType::scalar(Type::I64)],
        results: vec![ValueType::scalar(Type::I64)],
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: Vec::new(),
        sret: None,
        poll_free_leaf: false,
    };
    let linux = abi::classify_call(&call, TargetName::X86_64Linux, &universe).expect("call");
    let windows = abi::classify_call(&call, TargetName::X86_64Windows, &universe).expect("win");
    assert_eq!(linux.arguments[0].slots, vec![AbiSlot::Integer(Gpr::Rax)]);
    assert_eq!(windows.arguments[0].slots, vec![AbiSlot::Integer(Gpr::Rax)]);
}

#[test]
fn lower_error_display_for_invalid() {
    assert_eq!(
        LoweringError::InvalidOperands.to_string(),
        "lowering 的操作数不符合 op 语义"
    );
}
