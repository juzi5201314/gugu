//! V128 向量 lowering（SSE2 基线）。
//!
//! lane 由结果类型给出；整数 lane 的比较按**有符号**解释（lane 类型没有无符号性信号，
//! 无符号比较需要 sign-bias，不在本阶段登记），比较结果固定为 SSE 风格的全 1/全 0。
//! 没有基线序列的组合返回 `Unsupported` 并由 `supports_vector` 同步拒绝，vectorizer
//! 必须保留标量循环。

use crate::backend::x64::reg::{Gpr, Xmm};
use crate::backend::x64::table::{Access, OperandKind};
use crate::lir::body::{Condition, Lane, Op, Type, VectorOp};

use super::{Builder, LoweringError, SiteValue, gpr, imm, integer_bits, reg, scalar_type, xmm};

const XMMRM_XMM: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::Xmm];
const XMM_IMM8: &[OperandKind] = &[OperandKind::Xmm, OperandKind::Imm8];
const XMMRM_XMM_IMM8: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::Xmm, OperandKind::Imm8];
const R32_XMM_IMM8: &[OperandKind] = &[OperandKind::R32, OperandKind::Xmm, OperandKind::Imm8];
const RM32_XMM: &[OperandKind] = &[OperandKind::Rm32, OperandKind::Xmm];
const RM64_XMM: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Xmm];
const RM32_R32: &[OperandKind] = &[OperandKind::Rm32, OperandKind::R32];
const RM64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const RM8_R32: &[OperandKind] = &[OperandKind::Rm8, OperandKind::R32];
const RM16_R32: &[OperandKind] = &[OperandKind::Rm16, OperandKind::R32];
const RM32_R64: &[OperandKind] = &[OperandKind::Rm32, OperandKind::R64];

/// 向量 lowering 的临时 XMM 寄存器（全部登记进 clobbers）。
const TEMP: Xmm = Xmm::Xmm15;
const TEMP2: Xmm = Xmm::Xmm14;
const TEMP3: Xmm = Xmm::Xmm13;

pub(super) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let Op::Vector(vector) = op else {
        return Err(LoweringError::InvalidOperands);
    };
    match vector {
        VectorOp::Splat => splat(operands, results, builder),
        VectorOp::Add | VectorOp::Sub => add_sub(*vector, operands, results, builder),
        VectorOp::Mul => multiply(operands, results, builder),
        VectorOp::And | VectorOp::Or | VectorOp::Xor => {
            bitwise(*vector, operands, results, builder)
        }
        VectorOp::Compare(condition) => compare(*condition, operands, results, builder),
        VectorOp::Shuffle(mask) => shuffle(*mask, operands, results, builder),
        VectorOp::Extract(index) => extract(*index, operands, results, builder),
        VectorOp::Insert(index) => insert(*index, operands, results, builder),
        VectorOp::ReduceAdd => reduce_add(operands, results, builder),
    }
}

fn vector_lane(value: &SiteValue) -> Result<Lane, LoweringError> {
    match value.ty.ty {
        Type::V128(lane) => Ok(lane),
        _ => Err(LoweringError::InvalidOperands),
    }
}

fn lane_bytes(lane: Lane) -> u64 {
    match lane {
        Lane::I8 => 1,
        Lane::I16 => 2,
        Lane::I32 | Lane::F32 => 4,
        Lane::I64 | Lane::F64 => 8,
    }
}

/// 该 lane 上的乘法是否有基线序列。
pub(super) fn mul_supported(lane: Lane) -> bool {
    !matches!(lane, Lane::I8 | Lane::I64)
}

/// 该 lane 上的比较是否有基线序列。
pub(super) fn compare_supported(condition: Condition, lane: Lane) -> bool {
    !(lane == Lane::I64 && !matches!(condition, Condition::Eq | Condition::Ne))
}

/// 该 lane 上的归约加法是否有基线序列。
pub(super) fn reduce_supported(lane: Lane) -> bool {
    !matches!(lane, Lane::F32 | Lane::F64)
}

fn splat(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [source] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(result)?;
    let source_kind = match (lane, source.ty.ty) {
        (
            Lane::I8 | Lane::I16 | Lane::I32 | Lane::I64,
            Type::I8 | Type::I16 | Type::I32 | Type::I64,
        ) => source.ty.ty,
        (Lane::F32, Type::F32) | (Lane::F64, Type::F64) => source.ty.ty,
        _ => return Err(LoweringError::InvalidOperands),
    };
    match source_kind {
        Type::F32 | Type::F64 => {
            builder.emit(
                "movaps",
                XMMRM_XMM,
                Access::Read,
                vec![reg(source.reg), reg(result.reg)],
            );
        }
        Type::I64 => {
            builder.emit(
                "movq",
                RM64_XMM,
                Access::Read,
                vec![reg(source.reg), reg(result.reg)],
            );
        }
        Type::I8 | Type::I16 | Type::I32 => {
            builder.emit(
                "movd",
                RM32_XMM,
                Access::Read,
                vec![reg(source.reg), reg(result.reg)],
            );
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    match lane {
        Lane::I8 => {
            builder.emit(
                "punpcklbw",
                XMMRM_XMM,
                Access::Read,
                vec![reg(result.reg), reg(result.reg)],
            );
            builder.emit(
                "punpcklwd",
                XMMRM_XMM,
                Access::Read,
                vec![reg(result.reg), reg(result.reg)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(result.reg), reg(result.reg), imm(0)],
            );
        }
        Lane::I16 => {
            builder.emit(
                "punpcklwd",
                XMMRM_XMM,
                Access::Read,
                vec![reg(result.reg), reg(result.reg)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(result.reg), reg(result.reg), imm(0)],
            );
        }
        Lane::I32 => {
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(result.reg), reg(result.reg), imm(0)],
            );
        }
        Lane::I64 => {
            builder.emit(
                "punpcklqdq",
                XMMRM_XMM,
                Access::Read,
                vec![reg(result.reg), reg(result.reg)],
            );
        }
        Lane::F32 => {
            builder.emit(
                "shufps",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(result.reg), reg(result.reg), imm(0)],
            );
        }
        Lane::F64 => {
            builder.emit(
                "unpcklpd",
                XMMRM_XMM,
                Access::Read,
                vec![reg(result.reg), reg(result.reg)],
            );
        }
    }
    Ok(())
}

fn add_sub(
    vector: VectorOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(result)?;
    let mnemonic = match (lane, vector == VectorOp::Add) {
        (Lane::I8, true) => "paddb",
        (Lane::I8, false) => "psubb",
        (Lane::I16, true) => "paddw",
        (Lane::I16, false) => "psubw",
        (Lane::I32, true) => "paddd",
        (Lane::I32, false) => "psubd",
        (Lane::I64, true) => "paddq",
        (Lane::I64, false) => "psubq",
        (Lane::F32, true) => "addps",
        (Lane::F32, false) => "subps",
        (Lane::F64, true) => "addpd",
        (Lane::F64, false) => "subpd",
    };
    builder.emit(
        "movdqa",
        XMMRM_XMM,
        Access::Read,
        vec![reg(left.reg), reg(result.reg)],
    );
    builder.emit(
        mnemonic,
        XMMRM_XMM,
        Access::Read,
        vec![reg(right.reg), reg(result.reg)],
    );
    Ok(())
}

fn multiply(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(result)?;
    match lane {
        Lane::I16 => {
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), reg(result.reg)],
            );
            builder.emit(
                "pmullw",
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), reg(result.reg)],
            );
        }
        Lane::I32 => {
            // SSE2 的 32 位乘法：偶数 lane 与奇数 lane 分别用 `pmuludq`（64 位结果），
            // 再把每对乘积的低 32 位按 lane 顺序拼回。
            builder.clobber_xmm(TEMP);
            builder.clobber_xmm(TEMP2);
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), reg(result.reg)],
            );
            builder.emit(
                "pmuludq",
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), reg(result.reg)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(result.reg), reg(result.reg), imm(0x08)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), xmm(TEMP)],
            );
            builder.emit(
                "psrlq",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP), imm(32)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), xmm(TEMP2)],
            );
            builder.emit(
                "psrlq",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP2), imm(32)],
            );
            builder.emit(
                "pmuludq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![xmm(TEMP), xmm(TEMP), imm(0x08)],
            );
            builder.emit(
                "punpckldq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP), reg(result.reg)],
            );
        }
        Lane::F32 => {
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), reg(result.reg)],
            );
            builder.emit(
                "mulps",
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), reg(result.reg)],
            );
        }
        Lane::F64 => {
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), reg(result.reg)],
            );
            builder.emit(
                "mulpd",
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), reg(result.reg)],
            );
        }
        Lane::I8 | Lane::I64 => {
            return Err(LoweringError::Unsupported {
                op: "Vector::Mul",
                detail: "SSE2 没有 8/64 位 lane 的乘法序列",
            });
        }
    }
    Ok(())
}

fn bitwise(
    vector: VectorOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let mnemonic = match vector {
        VectorOp::And => "pand",
        VectorOp::Or => "por",
        VectorOp::Xor => "pxor",
        _ => return Err(LoweringError::InvalidOperands),
    };
    builder.emit(
        "movdqa",
        XMMRM_XMM,
        Access::Read,
        vec![reg(left.reg), reg(result.reg)],
    );
    builder.emit(
        mnemonic,
        XMMRM_XMM,
        Access::Read,
        vec![reg(right.reg), reg(result.reg)],
    );
    Ok(())
}

fn compare(
    condition: Condition,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(result)?;
    if !compare_supported(condition, lane) {
        return Err(LoweringError::Unsupported {
            op: "Vector::Compare",
            detail: "SSE2 没有 64 位 lane 的排序比较（`pcmpgtq` 属于 SSE4.2）",
        });
    }
    match lane {
        Lane::F32 | Lane::F64 => {
            let mnemonic = if lane == Lane::F32 { "cmpps" } else { "cmppd" };
            // 谓词：EQ_OQ=0、LT_OS=1、LE_OS=2、NEQ_UQ=4。语言语义是有序比较：NaN 参与时
            // Eq/Lt/Le/Gt/Ge 为假、Ne 为真，因此 Ge/Gt 交换操作数改用 LE/LT，
            // 不用「无序为真」的 NLT/NLE 谓词。
            let (predicate, swap) = match condition {
                Condition::Eq => (0, false),
                Condition::Lt => (1, false),
                Condition::Le => (2, false),
                Condition::Ne => (4, false),
                Condition::Ge => (2, true),
                Condition::Gt => (1, true),
            };
            let (first, second) = if swap { (right, left) } else { (left, right) };
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(first.reg), reg(result.reg)],
            );
            builder.emit(
                mnemonic,
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(second.reg), reg(result.reg), imm(predicate)],
            );
        }
        Lane::I8 | Lane::I16 | Lane::I32 => {
            let (equal, greater) = match lane {
                Lane::I8 => ("pcmpeqb", "pcmpgtb"),
                Lane::I16 => ("pcmpeqw", "pcmpgtw"),
                _ => ("pcmpeqd", "pcmpgtd"),
            };
            match condition {
                Condition::Eq => {
                    builder.emit(
                        "movdqa",
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(left.reg), reg(result.reg)],
                    );
                    builder.emit(
                        equal,
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(right.reg), reg(result.reg)],
                    );
                }
                Condition::Ne => {
                    builder.clobber_xmm(TEMP);
                    builder.emit(
                        "movdqa",
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(left.reg), reg(result.reg)],
                    );
                    builder.emit(
                        equal,
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(right.reg), reg(result.reg)],
                    );
                    builder.emit(
                        "pcmpeqd",
                        XMMRM_XMM,
                        Access::Read,
                        vec![xmm(TEMP), xmm(TEMP)],
                    );
                    builder.emit(
                        "pxor",
                        XMMRM_XMM,
                        Access::Read,
                        vec![xmm(TEMP), reg(result.reg)],
                    );
                }
                Condition::Gt => {
                    builder.emit(
                        "movdqa",
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(left.reg), reg(result.reg)],
                    );
                    builder.emit(
                        greater,
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(right.reg), reg(result.reg)],
                    );
                }
                // `a < b` 是 `b > a`；`a >= b` 是 `a < b` 的取反；`a <= b` 是 `a > b` 的取反。
                Condition::Lt | Condition::Ge => {
                    builder.emit(
                        "movdqa",
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(right.reg), reg(result.reg)],
                    );
                    builder.emit(
                        greater,
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(left.reg), reg(result.reg)],
                    );
                }
                Condition::Le => {
                    builder.emit(
                        "movdqa",
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(left.reg), reg(result.reg)],
                    );
                    builder.emit(
                        greater,
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(right.reg), reg(result.reg)],
                    );
                }
            }
            if matches!(condition, Condition::Ge | Condition::Le) {
                builder.clobber_xmm(TEMP);
                builder.emit(
                    "pcmpeqd",
                    XMMRM_XMM,
                    Access::Read,
                    vec![xmm(TEMP), xmm(TEMP)],
                );
                builder.emit(
                    "pxor",
                    XMMRM_XMM,
                    Access::Read,
                    vec![xmm(TEMP), reg(result.reg)],
                );
            }
        }
        Lane::I64 => {
            // SSE2 没有 `pcmpeqq`：按 32 位比较后把同一 64 位 lane 的两半相与。
            builder.clobber_xmm(TEMP);
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), reg(result.reg)],
            );
            builder.emit(
                "pcmpeqd",
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), reg(result.reg)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(result.reg), xmm(TEMP)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![xmm(TEMP), xmm(TEMP), imm(0xB1)],
            );
            builder.emit(
                "pand",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP), reg(result.reg)],
            );
            if condition == Condition::Ne {
                builder.emit(
                    "pcmpeqd",
                    XMMRM_XMM,
                    Access::Read,
                    vec![xmm(TEMP), xmm(TEMP)],
                );
                builder.emit(
                    "pxor",
                    XMMRM_XMM,
                    Access::Read,
                    vec![xmm(TEMP), reg(result.reg)],
                );
            }
        }
    }
    Ok(())
}

/// Shuffle 合成计划：mask 的每字节给出输出字节来源（0..15 取自 `a`、16..31 取自 `b`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShufflePlan {
    /// 单源 32/64 位粒度置换：`pshufd`/`shufpd` 立即数。
    Single { source: u8, imm: u8 },
    /// 两个源各做一次 32/64 位置换后合并（低半来自 `first`、高半来自 `second`）。
    Pair {
        first: u8,
        first_imm: u8,
        second: u8,
        second_imm: u8,
        wide: bool,
    },
}

pub(super) fn shuffle_supported(mask: [u8; 16]) -> bool {
    shuffle_plan(&mask).is_some()
}

fn shuffle_plan(mask: &[u8; 16]) -> Option<ShufflePlan> {
    // 32 位粒度：每个 32 位 lane 的 4 字节必须连续取自同一源的同一 32 位 lane。
    let aligned32 = mask.chunks_exact(4).all(|chunk| {
        chunk[0].is_multiple_of(4)
            && chunk[1] == chunk[0] + 1
            && chunk[2] == chunk[0] + 2
            && chunk[3] == chunk[0] + 3
    });
    if aligned32 {
        let chunks: [&[u8]; 4] = [&mask[0..4], &mask[4..8], &mask[8..12], &mask[12..16]];
        let sources: [u8; 4] = [
            chunks[0][0] / 16,
            chunks[1][0] / 16,
            chunks[2][0] / 16,
            chunks[3][0] / 16,
        ];
        let lanes: [u8; 4] = [
            (chunks[0][0] % 16) / 4,
            (chunks[1][0] % 16) / 4,
            (chunks[2][0] % 16) / 4,
            (chunks[3][0] % 16) / 4,
        ];
        if sources.iter().all(|source| *source == sources[0]) {
            let imm = lanes[0] | (lanes[1] << 2) | (lanes[2] << 4) | (lanes[3] << 6);
            return Some(ShufflePlan::Single {
                source: sources[0],
                imm,
            });
        }
        if sources[0] == sources[1] && sources[2] == sources[3] && sources[0] != sources[2] {
            return Some(ShufflePlan::Pair {
                first: sources[0],
                first_imm: lanes[0] | (lanes[1] << 2),
                second: sources[2],
                second_imm: lanes[2] | (lanes[3] << 2),
                wide: false,
            });
        }
        return None;
    }
    // 64 位粒度：每个 64 位 lane 的 8 字节连续且 8 对齐。
    let aligned64 = mask.chunks_exact(8).all(|chunk| {
        chunk[0].is_multiple_of(8)
            && chunk.iter().enumerate().all(|(offset, value)| {
                let step = u8::try_from(offset).expect("lane 内偏移适配 u8");
                *value == chunk[0] + step
            })
    });
    if !aligned64 {
        return None;
    }
    let first = mask[0];
    let second = mask[8];
    let first_source = first / 16;
    let second_source = second / 16;
    let first_lane = (first % 16) / 8;
    let second_lane = (second % 16) / 8;
    if first_source == second_source {
        return Some(ShufflePlan::Single {
            source: first_source,
            imm: (first_lane << 1) | (second_lane << 3),
        });
    }
    Some(ShufflePlan::Pair {
        first: first_source,
        first_imm: first_lane << 3,
        second: second_source,
        second_imm: second_lane << 3,
        wide: true,
    })
}

fn shuffle(
    mask: [u8; 16],
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let source = |index: u8| {
        if index == 0 { left.reg } else { right.reg }
    };
    match shuffle_plan(&mask) {
        Some(ShufflePlan::Single {
            source: plan_source,
            imm: plan_imm,
        }) => {
            let mnemonic = if mask[0].is_multiple_of(8) {
                "shufpd"
            } else {
                "pshufd"
            };
            builder.emit(
                mnemonic,
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![
                    reg(source(plan_source)),
                    reg(result.reg),
                    imm(u64::from(plan_imm)),
                ],
            );
        }
        Some(ShufflePlan::Pair {
            first,
            first_imm,
            second,
            second_imm,
            wide,
        }) => {
            builder.clobber_xmm(TEMP);
            let mnemonic = if wide { "shufpd" } else { "pshufd" };
            builder.emit(
                mnemonic,
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![
                    reg(source(first)),
                    reg(result.reg),
                    imm(u64::from(first_imm)),
                ],
            );
            builder.emit(
                mnemonic,
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(source(second)), xmm(TEMP), imm(u64::from(second_imm))],
            );
            if wide {
                builder.emit(
                    "unpcklpd",
                    XMMRM_XMM,
                    Access::Read,
                    vec![xmm(TEMP), reg(result.reg)],
                );
            } else {
                builder.emit(
                    "shufps",
                    XMMRM_XMM_IMM8,
                    Access::Read,
                    vec![xmm(TEMP), reg(result.reg), imm(0x44)],
                );
            }
        }
        None => {
            return Err(LoweringError::Unsupported {
                op: "Vector::Shuffle",
                detail: "mask 不能由基线模板合成，vectorizer 必须保留标量循环",
            });
        }
    }
    Ok(())
}

fn extract(
    index: u8,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [vector] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(vector)?;
    let bytes = lane_bytes(lane);
    match lane {
        Lane::I16 => {
            builder.emit(
                "pextrw",
                R32_XMM_IMM8,
                Access::Write,
                vec![reg(result.reg), reg(vector.reg), imm(u64::from(index))],
            );
        }
        Lane::I32 | Lane::I64 | Lane::F32 | Lane::F64 if index == 0 => {
            // `movd`/`movq` 的 XMM→GPR 方向（0x7E）首操作数是 r/m 目标；`movss`/`movsd`
            // 的首操作数是 XMM 源。
            match lane {
                Lane::I32 => {
                    builder.emit(
                        "movd",
                        RM32_XMM,
                        Access::Write,
                        vec![reg(result.reg), reg(vector.reg)],
                    );
                }
                Lane::I64 => {
                    builder.emit(
                        "movq",
                        RM64_XMM,
                        Access::Write,
                        vec![reg(result.reg), reg(vector.reg)],
                    );
                }
                _ => {
                    let mnemonic = if lane == Lane::F32 { "movss" } else { "movsd" };
                    builder.emit(
                        mnemonic,
                        XMMRM_XMM,
                        Access::Read,
                        vec![reg(vector.reg), reg(result.reg)],
                    );
                }
            }
        }
        _ => {
            builder.clobber_xmm(TEMP);
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP)],
            );
            builder.emit(
                "psrldq",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP), imm(u64::from(index) * bytes)],
            );
            match lane {
                Lane::I32 => {
                    builder.emit(
                        "movd",
                        RM32_XMM,
                        Access::Write,
                        vec![reg(result.reg), xmm(TEMP)],
                    );
                }
                Lane::I64 => {
                    builder.emit(
                        "movq",
                        RM64_XMM,
                        Access::Write,
                        vec![reg(result.reg), xmm(TEMP)],
                    );
                }
                _ => {
                    let mnemonic = if lane == Lane::F32 { "movss" } else { "movsd" };
                    builder.emit(
                        mnemonic,
                        XMMRM_XMM,
                        Access::Read,
                        vec![xmm(TEMP), reg(result.reg)],
                    );
                }
            }
        }
    }
    Ok(())
}

fn insert(
    index: u8,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [vector, value] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(result)?;
    let bytes = lane_bytes(lane);
    if lane == Lane::I16 {
        builder.emit(
            "movdqa",
            XMMRM_XMM,
            Access::Read,
            vec![reg(vector.reg), reg(result.reg)],
        );
        builder.emit(
            "insrw",
            R32_XMM_IMM8,
            Access::Read,
            vec![reg(value.reg), reg(result.reg), imm(u64::from(index))],
        );
        return Ok(());
    }
    builder.clobber_xmm(TEMP2);
    let shift = u64::from(index) * bytes;
    let (mnemonic, kinds, access) = match lane {
        Lane::I32 | Lane::I8 => ("movd", RM32_XMM, Access::Read),
        Lane::I64 => ("movq", RM64_XMM, Access::Read),
        Lane::F32 => ("movss", XMMRM_XMM, Access::Read),
        _ => ("movsd", XMMRM_XMM, Access::Read),
    };
    builder.clobber_xmm(TEMP);
    builder.emit(mnemonic, kinds, access, vec![reg(value.reg), xmm(TEMP)]);
    if lane == Lane::I8 {
        // 源的低 32 位只有最低字节有效，先掩到单字节再移位。
        builder.emit(
            "pcmpeqd",
            XMMRM_XMM,
            Access::Read,
            vec![xmm(TEMP2), xmm(TEMP2)],
        );
        builder.emit(
            "psrlq",
            XMM_IMM8,
            Access::ReadWrite,
            vec![xmm(TEMP2), imm(56)],
        );
        builder.emit("pand", XMMRM_XMM, Access::Read, vec![xmm(TEMP2), xmm(TEMP)]);
    }
    builder.emit(
        "pslldq",
        XMM_IMM8,
        Access::ReadWrite,
        vec![xmm(TEMP), imm(shift)],
    );
    builder.emit(
        "pcmpeqd",
        XMMRM_XMM,
        Access::Read,
        vec![xmm(TEMP2), xmm(TEMP2)],
    );
    // 目标 lane 的掩码：全 1 左移 `16 - bytes` 后再右移 `16 - bytes*(index+1)`，
    // 得到「只有该 lane 为 1」的掩码（左移量与被插入 lane 无关）。
    builder.emit(
        "pslldq",
        XMM_IMM8,
        Access::ReadWrite,
        vec![xmm(TEMP2), imm(16 - bytes)],
    );
    builder.emit(
        "psrldq",
        XMM_IMM8,
        Access::ReadWrite,
        vec![xmm(TEMP2), imm(16 - bytes * (u64::from(index) + 1))],
    );
    // `pandn TEMP2, vector` 得到「清掉目标 lane 的向量」，再与移位后的值相或。
    builder.emit(
        "pandn",
        XMMRM_XMM,
        Access::Read,
        vec![reg(vector.reg), xmm(TEMP2)],
    );
    builder.emit("por", XMMRM_XMM, Access::Read, vec![xmm(TEMP), xmm(TEMP2)]);
    builder.emit(
        "movdqa",
        XMMRM_XMM,
        Access::Read,
        vec![xmm(TEMP2), reg(result.reg)],
    );
    Ok(())
}

/// 把归约和（在 `TEMP` 的 lane0）搬到 GPR 临时寄存器，再按结果宽度收窄或扩展。
fn reduce_narrow(
    source: ReductionSource,
    target: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let bits = integer_bits(scalar_type(target)?)?;
    builder.clobber_gpr(Gpr::R11);
    match source {
        ReductionSource::Word => {
            builder.emit(
                "movd",
                RM32_XMM,
                Access::Write,
                vec![reg(gpr(Gpr::R11)), xmm(TEMP)],
            );
        }
        ReductionSource::Wide => {
            builder.emit(
                "movq",
                RM64_XMM,
                Access::Write,
                vec![reg(gpr(Gpr::R11)), xmm(TEMP)],
            );
        }
    }
    let extend_signed = source == ReductionSource::Word && bits == 64;
    match (bits, extend_signed) {
        (8, _) => {
            builder.emit(
                "movzx",
                RM8_R32,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(target.reg)],
            );
        }
        (16, _) => {
            builder.emit(
                "movzx",
                RM16_R32,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(target.reg)],
            );
        }
        (32, _) => {
            builder.emit(
                "mov",
                RM32_R32,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(target.reg)],
            );
        }
        (_, true) => {
            builder.emit(
                "movsxd",
                RM32_R64,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(target.reg)],
            );
        }
        (_, false) => {
            builder.emit(
                "mov",
                RM64_R64,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(target.reg)],
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReductionSource {
    /// 归约和在 32 位 lane 里（I8/I16/I32 lane，经 `pmaddwd` 或 `paddd`）。
    Word,
    /// 归约和在完整 64 位里（I64 lane）。
    Wide,
}

/// 4 个 32 位 lane 的两轮 `pshufd` + `paddd` 归约，结果落在 `TEMP` 的 lane0。
fn reduce_lanes_32(builder: &mut Builder) {
    builder.clobber_xmm(TEMP2);
    builder.emit(
        "pshufd",
        XMMRM_XMM_IMM8,
        Access::Read,
        vec![xmm(TEMP), xmm(TEMP2), imm(0x4E)],
    );
    builder.emit(
        "paddd",
        XMMRM_XMM,
        Access::Read,
        vec![xmm(TEMP2), xmm(TEMP)],
    );
    builder.emit(
        "pshufd",
        XMMRM_XMM_IMM8,
        Access::Read,
        vec![xmm(TEMP), xmm(TEMP2), imm(0xB1)],
    );
    builder.emit(
        "paddd",
        XMMRM_XMM,
        Access::Read,
        vec![xmm(TEMP2), xmm(TEMP)],
    );
}

fn reduce_add(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [vector] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let lane = vector_lane(vector)?;
    if !reduce_supported(lane) {
        return Err(LoweringError::Unsupported {
            op: "Vector::ReduceAdd",
            detail: "浮点 lane 没有整数归约语义",
        });
    }
    builder.clobber_xmm(TEMP);
    match lane {
        Lane::I8 => {
            // `pmaddwd` 把有符号 16 位 lane 乘 1 并两两相加，乘数现场构造。
            builder.clobber_xmm(TEMP2);
            builder.clobber_xmm(TEMP3);
            builder.emit(
                "pcmpeqd",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP2)],
            );
            // 每个 16 位 lane 取 1：全 1 的逻辑右移 15 位。
            builder.emit(
                "psrlw",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP2), imm(15)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP)],
            );
            builder.emit(
                "punpcklbw",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP), xmm(TEMP)],
            );
            builder.emit(
                "psraw",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP), imm(8)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP3)],
            );
            builder.emit(
                "punpckhbw",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP3), xmm(TEMP3)],
            );
            builder.emit(
                "psraw",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP3), imm(8)],
            );
            builder.emit(
                "paddw",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP3), xmm(TEMP)],
            );
            builder.emit(
                "pmaddwd",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP)],
            );
            reduce_lanes_32(builder);
            reduce_narrow(ReductionSource::Word, result, builder)?;
        }
        Lane::I16 => {
            builder.clobber_xmm(TEMP2);
            builder.emit(
                "pcmpeqd",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP2)],
            );
            // 每个 16 位 lane 取 1：全 1 的逻辑右移 15 位。
            builder.emit(
                "psrlw",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP2), imm(15)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP)],
            );
            builder.emit(
                "pmaddwd",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP)],
            );
            reduce_lanes_32(builder);
            reduce_narrow(ReductionSource::Word, result, builder)?;
        }
        Lane::I32 => {
            // 4 个有符号 32 位 lane 的和最多需要 34 位，必须先符号扩展到 64 位再累加，
            // 否则 32 位累加会在极端取值下丢位。
            builder.clobber_xmm(TEMP2);
            builder.clobber_xmm(TEMP3);
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP2)],
            );
            builder.emit(
                "psrad",
                XMM_IMM8,
                Access::ReadWrite,
                vec![xmm(TEMP2), imm(31)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP)],
            );
            builder.emit(
                "punpckldq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP)],
            );
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP3)],
            );
            builder.emit(
                "punpckhdq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP3)],
            );
            builder.emit(
                "paddq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP3), xmm(TEMP)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![xmm(TEMP), xmm(TEMP3), imm(0x4E)],
            );
            builder.emit(
                "paddq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP3), xmm(TEMP)],
            );
            reduce_narrow(ReductionSource::Wide, result, builder)?;
        }
        Lane::I64 => {
            builder.clobber_xmm(TEMP2);
            builder.emit(
                "movdqa",
                XMMRM_XMM,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP)],
            );
            builder.emit(
                "pshufd",
                XMMRM_XMM_IMM8,
                Access::Read,
                vec![reg(vector.reg), xmm(TEMP2), imm(0x4E)],
            );
            builder.emit(
                "paddq",
                XMMRM_XMM,
                Access::Read,
                vec![xmm(TEMP2), xmm(TEMP)],
            );
            reduce_narrow(ReductionSource::Wide, result, builder)?;
        }
        Lane::F32 | Lane::F64 => {
            return Err(LoweringError::InvalidOperands);
        }
    }
    Ok(())
}
