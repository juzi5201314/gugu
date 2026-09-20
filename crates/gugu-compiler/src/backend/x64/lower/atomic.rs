//! trap、原子与压缩引用解码 lowering。
//!
//! `DecodeCompressedRef` 的序列与 `runtime::cage::CompressionPlane::decode` 的拒绝分支
//! 一一对应：空字（不计数）、cage id、generation、offset 越界、非 canonical 地址。

use crate::backend::x64::inst::{ColdEdgeKind, Operand, RelocTarget};
use crate::backend::x64::reg::Gpr;
use crate::backend::x64::table::{Access, OperandKind};
use crate::frontend::gir::body::{MemoryOrdering, SourceInfo};
use crate::lir::body::{AtomicOp, Op, Type};
use crate::runtime::cage_control::CAGE_CONTROL_FIELDS;

use super::{
    Builder, LoweringError, SiteValue, access_bits, cold_branch, gpr, imm, move_kinds, reg,
    scalar_type, unary_kinds, wide_kinds,
};

const RM8_R8: &[OperandKind] = &[OperandKind::Rm8, OperandKind::R8];
const RM32_R32: &[OperandKind] = &[OperandKind::Rm32, OperandKind::R32];
const RM64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const RM32_IMM32: &[OperandKind] = &[OperandKind::Rm32, OperandKind::Imm32];
const RM64_IMM8: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Imm8];
const RM64: &[OperandKind] = &[OperandKind::Rm64];
const REL32: &[OperandKind] = &[OperandKind::Rel32];

pub(super) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    source: &SourceInfo,
    builder: &mut Builder,
    site: u32,
) -> Result<(), LoweringError> {
    match op {
        Op::TrapIf => trap_if(operands, source, builder, site),
        Op::Atomic { op, ordering, .. } => atomic(*op, *ordering, operands, results, builder),
        Op::DecodeCompressedRef => decode_compressed_ref(operands, results, source, builder, site),
        _ => Err(LoweringError::InvalidOperands),
    }
}

fn trap_if(
    operands: &[SiteValue],
    source: &SourceInfo,
    builder: &mut Builder,
    site: u32,
) -> Result<(), LoweringError> {
    let [condition] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    builder.emit(
        "test",
        RM8_R8,
        Access::Read,
        vec![reg(condition.reg), reg(condition.reg)],
    );
    builder.emit(
        "jne",
        REL32,
        Access::Read,
        vec![cold_branch(ColdEdgeKind::Trap, source, site)],
    );
    Ok(())
}

fn atomic(
    op: AtomicOp,
    ordering: MemoryOrdering,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match op {
        AtomicOp::Load => atomic_load(operands, results, builder),
        AtomicOp::Store => atomic_store(ordering, operands, builder),
        AtomicOp::Exchange => atomic_exchange(operands, results, builder),
        AtomicOp::Add | AtomicOp::Sub => atomic_add_sub(op, operands, results, builder),
        AtomicOp::CompareExchange => atomic_compare_exchange(operands, results, builder),
        AtomicOp::Fence => atomic_fence(ordering, builder),
        AtomicOp::And | AtomicOp::Or | AtomicOp::Xor => Err(LoweringError::Unsupported {
            op: "Atomic",
            detail: "缺少单指令 fetch 形式，CAS 重试环等出现真实生产者再引入",
        }),
    }
}

fn atomic_load(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [address] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(scalar_type(result)?)?;
    builder.emit(
        "mov",
        wide_kinds(bits),
        Access::Read,
        vec![mem(address), reg(result.reg)],
    );
    Ok(())
}

fn atomic_store(
    ordering: MemoryOrdering,
    operands: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [address, value] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(scalar_type(value)?)?;
    if ordering == MemoryOrdering::SeqCst {
        // `xchg` 会把旧值写回寄存器，先搬到临时寄存器以免破坏值操作数。
        builder.clobber_gpr(Gpr::R11);
        builder.emit(
            "mov",
            move_kinds(bits),
            Access::Write,
            vec![reg(gpr(Gpr::R11)), reg(value.reg)],
        );
        builder.emit(
            "xchg",
            wide_kinds(bits),
            Access::ReadWrite,
            vec![mem(address), reg(gpr(Gpr::R11))],
        );
    } else {
        builder.emit(
            "mov",
            wide_kinds(bits),
            Access::Write,
            vec![mem(address), reg(value.reg)],
        );
    }
    Ok(())
}

fn atomic_exchange(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [address, value] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(scalar_type(result)?)?;
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(result.reg), reg(value.reg)],
    );
    builder.emit(
        "xchg",
        wide_kinds(bits),
        Access::ReadWrite,
        vec![mem(address), reg(result.reg)],
    );
    Ok(())
}

fn atomic_add_sub(
    op: AtomicOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [address, value] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(scalar_type(result)?)?;
    if op == AtomicOp::Sub {
        // `lock xadd` 只能加，减法先取负值。
        builder.emit(
            "mov",
            move_kinds(bits),
            Access::Write,
            vec![reg(result.reg), reg(value.reg)],
        );
        builder.emit(
            "neg",
            unary_kinds(bits),
            Access::ReadWrite,
            vec![reg(result.reg)],
        );
    } else {
        builder.emit(
            "mov",
            move_kinds(bits),
            Access::Write,
            vec![reg(result.reg), reg(value.reg)],
        );
    }
    builder.emit_locked(
        "xadd",
        wide_kinds(bits),
        Access::ReadWrite,
        vec![mem(address), reg(result.reg)],
    );
    Ok(())
}

fn atomic_compare_exchange(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [address, expected, desired] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [old, ok] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(scalar_type(old)?)?;
    builder.clobber_gpr(Gpr::Rax);
    builder.clobber_gpr(Gpr::R11);
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), reg(expected.reg)],
    );
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(gpr(Gpr::R11)), reg(desired.reg)],
    );
    builder.emit_locked(
        "cmpxchg",
        wide_kinds(bits),
        Access::ReadWrite,
        vec![mem(address), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(old.reg), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "sete",
        &[OperandKind::Rm8],
        Access::Write,
        vec![reg(ok.reg)],
    );
    builder.emit(
        "movzx",
        &[OperandKind::Rm8, OperandKind::R32],
        Access::Read,
        vec![reg(ok.reg), reg(ok.reg)],
    );
    Ok(())
}

/// `SeqCst` 用 `mfence`；Acquire/Release/AcqRel 是空序列（顺序由 LIR effect edge 承担）。
fn atomic_fence(ordering: MemoryOrdering, builder: &mut Builder) -> Result<(), LoweringError> {
    match ordering {
        MemoryOrdering::SeqCst => {
            builder.emit("mfence", &[], Access::Read, Vec::new());
            Ok(())
        }
        MemoryOrdering::Acquire | MemoryOrdering::Release | MemoryOrdering::AcqRel => Ok(()),
        MemoryOrdering::Relaxed => Err(LoweringError::Unsupported {
            op: "Atomic",
            detail: "Relaxed fence 由 LIR verifier 拒绝",
        }),
    }
}

fn mem(address: &SiteValue) -> Operand {
    super::mem_base(address.reg, 0)
}

/// 控制记录字段的 RIP 相对操作数；offset 来自 [`CAGE_CONTROL_FIELDS`]（唯一契约）。
fn control_field(name: &str) -> Operand {
    let (_, offset, _) = CAGE_CONTROL_FIELDS
        .iter()
        .find(|(field, _, _)| *field == name)
        .expect("控制记录字段在登记表内");
    Operand::Rip(
        RelocTarget::CageControl,
        i32::try_from(*offset).expect("字段偏移适配 i32"),
    )
}

fn decode_compressed_ref(
    operands: &[SiteValue],
    results: &[SiteValue],
    source: &SourceInfo,
    builder: &mut Builder,
    site: u32,
) -> Result<(), LoweringError> {
    let [word] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [dst] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    // LIR verifier 接受 `Ptr | I32 | I64` 的 64 位字，三种都按寄存器里的 64 位规范形解码；
    // 结果必须是 `GcHeap` 引用（`Type::Ptr`）。
    if !matches!(word.ty.ty, Type::I32 | Type::I64 | Type::Ptr) || dst.ty.ty != Type::Ptr {
        return Err(LoweringError::InvalidOperands);
    }
    let null = builder.label();
    let reject = builder.label();
    let end = builder.label();
    builder.clobber_gpr(Gpr::R11);
    builder.emit(
        "test",
        RM64_R64,
        Access::Read,
        vec![reg(word.reg), reg(word.reg)],
    );
    builder.emit("je", REL32, Access::Read, vec![Operand::Label(null)]);
    // cage id：word >> 56 与记录的 cage_id 比较。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::R11)), reg(word.reg)],
    );
    builder.emit(
        "shr",
        RM64_IMM8,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), imm(56)],
    );
    builder.emit(
        "cmp",
        RM8_R8,
        Access::Read,
        vec![control_field("cage_id"), reg(gpr(Gpr::R11))],
    );
    builder.emit("jne", REL32, Access::Read, vec![Operand::Label(reject)]);
    // generation：word >> 32 取低 24 位。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::R11)), reg(word.reg)],
    );
    builder.emit(
        "shr",
        RM64_IMM8,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), imm(32)],
    );
    builder.emit(
        "and",
        RM32_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), imm(0x00FF_FFFF)],
    );
    builder.emit(
        "cmp",
        RM32_R32,
        Access::Read,
        vec![control_field("generation"), reg(gpr(Gpr::R11))],
    );
    builder.emit("jne", REL32, Access::Read, vec![Operand::Label(reject)]);
    // offset：低 32 位零扩展到 64 位。
    builder.emit(
        "mov",
        RM32_R32,
        Access::Write,
        vec![reg(gpr(Gpr::R11)), reg(word.reg)],
    );
    // 越界条件：`offset >= len`。`cmp [len], offset` 后 `jbe` 覆盖「小于」与「相等」。
    builder.emit(
        "cmp",
        RM64_R64,
        Access::Read,
        vec![control_field("len"), reg(gpr(Gpr::R11))],
    );
    builder.emit("jbe", REL32, Access::Read, vec![Operand::Label(reject)]);
    builder.emit(
        "cmp",
        RM64_R64,
        Access::Read,
        vec![control_field("canonical_headroom"), reg(gpr(Gpr::R11))],
    );
    builder.emit("jbe", REL32, Access::Read, vec![Operand::Label(reject)]);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![control_field("base"), reg(dst.reg)],
    );
    builder.emit(
        "add",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(dst.reg), reg(gpr(Gpr::R11))],
    );
    builder.emit_locked(
        "inc",
        RM64,
        Access::ReadWrite,
        vec![control_field("decodes")],
    );
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(end)]);
    builder.define(reject);
    builder.emit_locked(
        "inc",
        RM64,
        Access::ReadWrite,
        vec![control_field("rejections")],
    );
    builder.emit(
        "jmp",
        REL32,
        Access::Read,
        vec![cold_branch(
            ColdEdgeKind::CompressionDecodeRejected,
            source,
            site,
        )],
    );
    builder.define(null);
    builder.emit(
        "xor",
        RM32_R32,
        Access::ReadWrite,
        vec![reg(dst.reg), reg(dst.reg)],
    );
    builder.define(end);
    Ok(())
}
