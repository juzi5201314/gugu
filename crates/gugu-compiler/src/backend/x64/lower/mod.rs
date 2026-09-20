//! x86_64 lowering：把 LIR 数值、向量、原子与压缩解码指令分解成基线机器序列。
//!
//! # 寄存器表示不变量
//!
//! - `I8`/`I16`/`I32` 的值在寄存器里零扩展到 64 位（`I8`：bit 63..8 = 0；`I16`：
//!   bit 63..16 = 0；`I32`：bit 63..32 = 0）；`I64` 是完整 64 位补码；`bool` 是 0/1；
//!   `F32`/`F64` 在 XMM 低 lane；`Ptr` 是 64 位地址。
//! - 由不变量直接得到：`ZeroExtend` 恒为一次 `mov`；窄宽度算术结果保持规范形（8/16 位
//!   ALU 写只改低位，输入的高位零保持不动）。
//! - 两地址形式一律先 `mov dst, lhs` 再 `op dst, rhs`（分配阶段做 copy coalescing），
//!   lowering 不隐式破坏输入。
//! - lowering 内部的临时寄存器是固定的物理 scratch（`r11`、`rax`/`rcx`/`rdx`、`xmm15`），
//!   全部登记进 [`Lowered::clobbers`]；LIR 值对应的虚拟寄存器直接透传。

pub(crate) mod alloc;
pub(crate) mod asm;
pub(crate) mod atomic;
pub(crate) mod call;
pub(crate) mod ctrl;
pub(crate) mod memory;
pub(crate) mod numeric;
#[cfg(test)]
mod tests;
pub(crate) mod vector;

use std::fmt;

use crate::frontend::gir::body::SourceInfo;
use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{Body, Lane, Op, Type, ValueType};
use crate::runtime::RuntimeRawContractV1;
use crate::target::TargetName;

use super::inst::{
    ColdEdge, ColdEdgeKind, Inst, LabelDefinition, LabelId, Mem, Operand, RelocKind, RelocTarget,
    Scale, Sequence,
};
use super::reg::{Clobbers, Gpr, Reg, Xmm};
use super::table::{self, Access, FormId, OperandKind};

/// 站点操作数或结果：值类型与虚拟（或物理）寄存器。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SiteValue {
    pub(crate) ty: ValueType,
    pub(crate) reg: Reg,
}

/// lowering 结果：机器序列与破坏的物理寄存器集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Lowered {
    pub(crate) sequence: Sequence,
    pub(crate) clobbers: Clobbers,
}

/// lowering 失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoweringError {
    /// 该 op 的类型组合没有基线机器序列；vectorizer 必须保留标量路径。
    Unsupported {
        op: &'static str,
        detail: &'static str,
    },
    /// 操作数或结果的数量、种类与 op 的语义不符。
    InvalidOperands,
}

/// 需要宇宙/契约的站点 lowering 上下文；探针与 legalize 可不带。
#[derive(Clone, Copy)]
pub(crate) struct LowerCtx<'a> {
    pub target: TargetName,
    pub body: Option<&'a Body>,
    pub universe: Option<&'a TypeUniverse>,
    pub raw: Option<&'a RuntimeRawContractV1>,
    pub site: u32,
}

impl LowerCtx<'static> {
    /// 探针/legalize：只走不依赖宇宙的序列。
    pub(crate) fn probe(target: TargetName) -> Self {
        Self {
            target,
            body: None,
            universe: None,
            raw: None,
            site: 0,
        }
    }
}

impl fmt::Display for LoweringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { op, detail } => {
                write!(formatter, "{op} 没有基线机器序列：{detail}")
            }
            Self::InvalidOperands => formatter.write_str("lowering 的操作数不符合 op 语义"),
        }
    }
}

impl std::error::Error for LoweringError {}

/// 返回封闭 LIR opcode 的稳定域名；穷尽 match，未知组合编译期失败。
pub(crate) fn domain(op: &Op) -> &'static str {
    match op {
        Op::IConst(_) => "IConst",
        Op::FConst(_) => "FConst",
        Op::SymbolAddr(_) => "SymbolAddr",
        Op::StackAddr(_) => "StackAddr",
        Op::PtrOffset => "PtrOffset",
        Op::Integer(_) => "Integer",
        Op::Float(_) => "Float",
        Op::Compare { .. } => "Compare",
        Op::Convert(_) => "Convert",
        Op::Vector(_) => "Vector",
        Op::Select => "Select",
        Op::TrapIf => "TrapIf",
        Op::Load(_) => "Load",
        Op::Store(_) => "Store",
        Op::Memcpy { .. } => "Memcpy",
        Op::Memmove { .. } => "Memmove",
        Op::Memset { .. } => "Memset",
        Op::Atomic { .. } => "Atomic",
        Op::GcAlloc { .. } => "GcAlloc",
        Op::RegionAlloc { .. } => "RegionAlloc",
        Op::PlatformCall(_) => "PlatformCall",
        Op::RegionPublish { .. } => "RegionPublish",
        Op::RegionReset { .. } => "RegionReset",
        Op::PromoteManaged { .. } => "PromoteManaged",
        Op::RegionTransfer { .. } => "RegionTransfer",
        Op::MarkTicketBatch => "MarkTicketBatch",
        Op::EdgeDeltaBatch => "EdgeDeltaBatch",
        Op::ResolveSharedHandle => "ResolveSharedHandle",
        Op::SharedAccessBegin { .. } => "SharedAccessBegin",
        Op::SharedAccessEnd { .. } => "SharedAccessEnd",
        Op::SharedFieldBarrier { .. } => "SharedFieldBarrier",
        Op::SharedFieldBarrierReserved { .. } => "SharedFieldBarrierReserved",
        Op::ForwardSharedHandle => "ForwardSharedHandle",
        Op::DecodeCompressedRef => "DecodeCompressedRef",
        Op::BarrierReserve(_) => "BarrierReserve",
        Op::GcWriteBarrier { .. } => "GcWriteBarrier",
        Op::GcWriteBarrierReserved { .. } => "GcWriteBarrierReserved",
        Op::ScopedViewBegin { .. } => "ScopedViewBegin",
        Op::ScopedViewEnd { .. } => "ScopedViewEnd",
        Op::SafepointPoll { .. } => "SafepointPoll",
        Op::StackCheck => "StackCheck",
        Op::NoSafepointBegin(_) => "NoSafepointBegin",
        Op::NoSafepointEnd(_) => "NoSafepointEnd",
        Op::CoroutineSwitch => "CoroutineSwitch",
        Op::Park => "Park",
        Op::Ready => "Ready",
        Op::Call(_) => "Call",
        Op::ForeignCall(_) => "ForeignCall",
        Op::InlineAsm(_) => "InlineAsm",
        Op::CoverageCounter(_) => "CoverageCounter",
    }
}

/// 把一条 LIR 指令 lower 成机器序列。
pub(crate) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    source: &SourceInfo,
) -> Result<Lowered, LoweringError> {
    lower_with(
        op,
        operands,
        results,
        source,
        LowerCtx::probe(TargetName::X86_64Linux),
    )
}

/// 带宇宙/契约的站点 lowering。
pub(crate) fn lower_with(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    source: &SourceInfo,
    ctx: LowerCtx<'_>,
) -> Result<Lowered, LoweringError> {
    let mut builder = Builder::new();
    match op {
        Op::IConst(_)
        | Op::FConst(_)
        | Op::Integer(_)
        | Op::Float(_)
        | Op::Compare { .. }
        | Op::Convert(_)
        | Op::Select => numeric::lower(op, operands, results, &mut builder)?,
        Op::Vector(_) => vector::lower(op, operands, results, &mut builder)?,
        Op::TrapIf | Op::Atomic { .. } | Op::DecodeCompressedRef => {
            atomic::lower(op, operands, results, source, &mut builder, ctx.site)?;
        }
        Op::SymbolAddr(_)
        | Op::StackAddr(_)
        | Op::PtrOffset
        | Op::Load(_)
        | Op::Store(_)
        | Op::Memcpy { .. }
        | Op::Memmove { .. }
        | Op::Memset { .. }
        | Op::CoverageCounter(_) => memory::lower(op, operands, results, &mut builder)?,
        Op::GcAlloc { .. } | Op::RegionAlloc { .. } | Op::SafepointPoll { .. } | Op::StackCheck => {
            alloc::lower(op, operands, results, source, ctx, &mut builder)?
        }
        Op::PlatformCall(_)
        | Op::RegionPublish { .. }
        | Op::RegionReset { .. }
        | Op::PromoteManaged { .. }
        | Op::RegionTransfer { .. }
        | Op::MarkTicketBatch
        | Op::EdgeDeltaBatch
        | Op::ResolveSharedHandle
        | Op::SharedAccessBegin { .. }
        | Op::SharedAccessEnd { .. }
        | Op::SharedFieldBarrier { .. }
        | Op::SharedFieldBarrierReserved { .. }
        | Op::ForwardSharedHandle
        | Op::BarrierReserve(_)
        | Op::GcWriteBarrier { .. }
        | Op::GcWriteBarrierReserved { .. }
        | Op::ScopedViewBegin { .. }
        | Op::ScopedViewEnd { .. }
        | Op::NoSafepointBegin(_)
        | Op::NoSafepointEnd(_)
        | Op::Park
        | Op::Ready
        | Op::Call(_)
        | Op::ForeignCall(_) => call::lower(op, operands, results, ctx, &mut builder)?,
        Op::CoroutineSwitch => ctrl::coroutine_switch(&mut builder)?,
        Op::InlineAsm(_) => asm::lower(op, operands, results, ctx, &mut builder)?,
    }
    Ok(builder.finish())
}

/// 规范操作数下 lowering 后 form `poll_cost` 的饱和和。
///
/// 权重取机器 form 表的 `poll_cost`（当前每条指令 1）；探针类型取该 op 成本最大的
/// 类型变体（最宽整数、窄宽度掩码路径、无符号 u64 转换、浮点比较、最贵 lane），
/// 保证用于 poll 插点的成本是不低于真实站点的上界。
pub(crate) fn poll_cost(op: &Op) -> u32 {
    let Some((operand_types, result_types)) = probe_types(op) else {
        return 1;
    };
    let operands = probe_values(&operand_types, 0);
    let results = probe_values(
        &result_types,
        u32::try_from(operand_types.len()).unwrap_or(0),
    );
    let source = probe_source();
    match lower(op, &operands, &results, &source) {
        Ok(lowered) => lowered
            .sequence
            .instructions
            .iter()
            .map(|inst| u32::from(table::form(inst.form).poll_cost.get()))
            .fold(0_u32, u32::saturating_add),
        // 没有基线序列的 op 组合在 codegen 会被拒绝；成本取 1 保持保守。
        Err(_) => 1,
    }
}

/// 判定向量 op 在给定 lane 上是否有基线序列。
///
/// 判定与 lowering 的分派同源：`Shuffle` 走同一套模板合成规则，`Mul`/`Compare`/`ReduceAdd`
/// 按 lane 与条件拒绝没有基线序列的组合。
pub(crate) fn supports_vector(vector: crate::lir::body::VectorOp, lane: Lane) -> bool {
    use crate::lir::body::VectorOp;
    match vector {
        VectorOp::Mul => vector::mul_supported(lane),
        VectorOp::Compare(condition) => vector::compare_supported(condition, lane),
        VectorOp::ReduceAdd => vector::reduce_supported(lane),
        VectorOp::Shuffle(mask) => vector::shuffle_supported(mask),
        // 标量源由 lowering 校验；`Select` 没有 V128 基线序列。
        VectorOp::Splat
        | VectorOp::Add
        | VectorOp::Sub
        | VectorOp::And
        | VectorOp::Or
        | VectorOp::Xor
        | VectorOp::Extract(_)
        | VectorOp::Insert(_) => true,
    }
}

/// 各 op 的规范探针类型：`(操作数类型, 结果类型)`。
fn probe_types(op: &Op) -> Option<(Vec<Type>, Vec<Type>)> {
    use crate::lir::body::{Conversion, FloatOp, IntOp, VectorOp};
    let probe = match op {
        Op::IConst(_) => (vec![], vec![Type::I64]),
        Op::FConst(_) => (vec![], vec![Type::F64]),
        Op::Integer(inner) => match inner {
            IntOp::Add
            | IntOp::Sub
            | IntOp::And
            | IntOp::Or
            | IntOp::Xor
            | IntOp::Neg
            | IntOp::Not => (vec![Type::I64; 2], vec![Type::I64]),
            // 8 位乘法要补零扩展，窄宽度移位要补掩码：是最贵的宽度路径。
            IntOp::Mul | IntOp::Shl | IntOp::ShrSigned | IntOp::ShrUnsigned => {
                (vec![Type::I8; 2], vec![Type::I8])
            }
            IntOp::DivSigned | IntOp::DivUnsigned | IntOp::RemSigned | IntOp::RemUnsigned => {
                (vec![Type::I64; 2], vec![Type::I64])
            }
            IntOp::AddCarry | IntOp::SubBorrow => (vec![Type::I64; 3], vec![Type::I64; 2]),
            IntOp::MulWide => (vec![Type::I64; 2], vec![Type::I64; 2]),
        },
        Op::Float(inner) => match inner {
            FloatOp::Add | FloatOp::Sub | FloatOp::Mul | FloatOp::Div => {
                (vec![Type::F64; 2], vec![Type::F64])
            }
            FloatOp::Neg => (vec![Type::F64], vec![Type::F64]),
        },
        // 浮点比较要补 parity 分支，是成本上界。
        Op::Compare { .. } => (vec![Type::F64; 2], vec![Type::I8]),
        Op::Convert(conversion) => match conversion {
            Conversion::ZeroExtend | Conversion::SignExtend => (vec![Type::I8], vec![Type::I64]),
            Conversion::Truncate => (vec![Type::I64], vec![Type::I8]),
            Conversion::IntToFloat { .. } => (vec![Type::I64], vec![Type::F64]),
            Conversion::FloatToInt { .. } => (vec![Type::F64], vec![Type::I64]),
            Conversion::FloatResize => (vec![Type::F64], vec![Type::F32]),
            Conversion::Bitcast => (vec![Type::I64], vec![Type::F64]),
            Conversion::PointerToInt
            | Conversion::IntToPointer
            | Conversion::PointerCast
            | Conversion::RawToReference => (vec![Type::Ptr], vec![Type::Ptr]),
        },
        Op::Vector(inner) => match inner {
            // I8 lane 的广播要三次腾挪，是 Splat 的成本上界。
            VectorOp::Splat => (vec![Type::I8], vec![Type::V128(Lane::I8)]),
            VectorOp::Add
            | VectorOp::Sub
            | VectorOp::And
            | VectorOp::Or
            | VectorOp::Xor
            | VectorOp::Compare(_) => (vec![Type::V128(Lane::I32); 2], vec![Type::V128(Lane::I32)]),
            // I32 lane 乘法是基线可合成路径里最贵的；I64 lane 没有序列。
            VectorOp::Mul => (vec![Type::V128(Lane::I32); 2], vec![Type::V128(Lane::I32)]),
            VectorOp::Shuffle(mask) => {
                if vector::shuffle_supported(*mask) {
                    (vec![Type::V128(Lane::I32); 2], vec![Type::V128(Lane::I32)])
                } else {
                    return None;
                }
            }
            VectorOp::Extract(_) => (vec![Type::V128(Lane::I32)], vec![Type::I32]),
            VectorOp::Insert(_) => (
                vec![Type::V128(Lane::I8), Type::I8],
                vec![Type::V128(Lane::I8)],
            ),
            VectorOp::ReduceAdd => (vec![Type::V128(Lane::I8)], vec![Type::I64]),
        },
        Op::Select => (vec![Type::I8, Type::F64, Type::F64], vec![Type::F64]),
        Op::TrapIf => (vec![Type::I8], vec![]),
        Op::Atomic { op, .. } => match op {
            crate::lir::body::AtomicOp::Load => (vec![Type::Ptr], vec![Type::I64]),
            crate::lir::body::AtomicOp::Store => (vec![Type::Ptr, Type::I64], vec![]),
            crate::lir::body::AtomicOp::Exchange
            | crate::lir::body::AtomicOp::Add
            | crate::lir::body::AtomicOp::Sub => (vec![Type::Ptr, Type::I64], vec![Type::I64]),
            crate::lir::body::AtomicOp::CompareExchange => (
                vec![Type::Ptr, Type::I64, Type::I64],
                vec![Type::I64, Type::I8],
            ),
            crate::lir::body::AtomicOp::Fence => (vec![], vec![]),
            crate::lir::body::AtomicOp::And
            | crate::lir::body::AtomicOp::Or
            | crate::lir::body::AtomicOp::Xor => return None,
        },
        Op::DecodeCompressedRef => (vec![Type::I64], vec![Type::Ptr]),
        _ => return None,
    };
    Some(probe)
}

fn probe_values(types: &[Type], offset: u32) -> Vec<SiteValue> {
    types
        .iter()
        .enumerate()
        .map(|(index, ty)| SiteValue {
            ty: ValueType::scalar(*ty),
            reg: Reg::Virtual(offset + u32::try_from(index).expect("探针类型数适配 u32")),
        })
        .collect()
}

/// 探针序列使用的占位源码位置：probe 与 harness 只关心类型，不关心位置。
pub(crate) fn probe_source() -> SourceInfo {
    SourceInfo {
        location: crate::frontend::hir::Location {
            source: 0,
            start: 0,
            end: 0,
            expansion: u32::MAX,
        },
        scope: crate::frontend::gir::body::ScopeId(0),
    }
}

/// lowering 序列构造器：指令、标签定义与 clobber 登记。
pub(crate) struct Builder {
    sequence: Sequence,
    clobbers: Clobbers,
    next_label: u32,
}

impl Builder {
    pub(crate) fn new() -> Self {
        Self {
            sequence: Sequence::new(),
            clobbers: Clobbers::NONE,
            next_label: 0,
        }
    }

    pub(crate) fn finish(self) -> Lowered {
        Lowered {
            sequence: self.sequence,
            clobbers: self.clobbers,
        }
    }

    /// 追加一条已解析 form 的指令。
    pub(crate) fn push_inst(&mut self, inst: Inst) {
        self.sequence.instructions.push(inst);
    }

    /// 追加一条指令；`first` 用于区分同形不同方向的编码（如 `mov` 的 89/8B）。
    pub(crate) fn emit(
        &mut self,
        mnemonic: &'static str,
        kinds: &'static [OperandKind],
        first: Access,
        operands: Vec<Operand>,
    ) -> &mut Self {
        let form = find_form(mnemonic, kinds, first);
        self.sequence.instructions.push(Inst {
            form,
            operands,
            lock: false,
        });
        self
    }

    /// 追加一条带显式 `lock` 前缀的指令。
    pub(crate) fn emit_locked(
        &mut self,
        mnemonic: &'static str,
        kinds: &'static [OperandKind],
        first: Access,
        operands: Vec<Operand>,
    ) -> &mut Self {
        let form = find_form(mnemonic, kinds, first);
        self.sequence.instructions.push(Inst {
            form,
            operands,
            lock: true,
        });
        self
    }

    /// 分配一个新标签编号。
    pub(crate) fn label(&mut self) -> LabelId {
        let label = LabelId(self.next_label);
        self.next_label += 1;
        label
    }

    /// 在当前指令位置定义标签。
    pub(crate) fn define(&mut self, label: LabelId) {
        let at = u32::try_from(self.sequence.instructions.len()).expect("站点指令数适配 u32");
        self.sequence.labels.push(LabelDefinition { label, at });
    }

    pub(crate) fn has_label(&self, label: LabelId) -> bool {
        self.sequence
            .labels
            .iter()
            .any(|definition| definition.label == label)
    }

    /// 登记一个被破坏的 GPR。
    pub(crate) fn clobber_gpr(&mut self, gpr: Gpr) -> &mut Self {
        self.clobbers = self.clobbers.union(Clobbers::gpr(gpr));
        self
    }

    /// 登记一个被破坏的 XMM。
    pub(crate) fn clobber_xmm(&mut self, xmm: Xmm) -> &mut Self {
        self.clobbers = self.clobbers.union(Clobbers::xmm(xmm));
        self
    }
}

fn find_form(mnemonic: &'static str, kinds: &'static [OperandKind], first: Access) -> FormId {
    let index = table::FORMS
        .iter()
        .position(|form| {
            form.mnemonic == mnemonic
                && form.operands == kinds
                // 无操作数形式（`cqo` 等）没有访问表可比；其余用首操作数区分同形不同方向。
                && (kinds.is_empty() || form.access.first() == Some(&first))
        })
        .unwrap_or_else(|| panic!("表内缺少 {mnemonic} 的 {kinds:?} 形式（首操作数 {first:?}）"));
    FormId(u16::try_from(index).expect("form 数量适配 u16"))
}

/// 操作数构造：寄存器。
pub(crate) fn reg(reg: Reg) -> Operand {
    Operand::Reg(reg)
}

/// 寄存器构造：物理 GPR。
pub(crate) fn gpr(gpr: Gpr) -> Reg {
    Reg::Gpr(gpr)
}

/// 操作数构造：物理 XMM 寄存器。
pub(crate) fn xmm(xmm: Xmm) -> Operand {
    Operand::Reg(Reg::Xmm(xmm))
}

/// 操作数构造：立即数。
pub(crate) fn imm(value: u64) -> Operand {
    Operand::Imm(value)
}

/// 操作数构造：`[base + disp]`。
pub(crate) fn mem_base(base: Reg, disp: i32) -> Operand {
    Operand::Mem(Mem {
        base: Some(base),
        index: None,
        scale: Scale::One,
        disp,
    })
}

/// 操作数构造：冷边分支目标。
pub(crate) fn cold_branch(kind: ColdEdgeKind, source: &SourceInfo, site: u32) -> Operand {
    Operand::Reloc(
        RelocTarget::Cold(ColdEdge {
            kind,
            source: source.clone(),
            site,
        }),
        RelocKind::PcRel32,
    )
}

/// 标量结果的机器类型；非标量时报错。
pub(crate) fn scalar_type(value: &SiteValue) -> Result<Type, LoweringError> {
    match value.ty.ty {
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::F32 | Type::F64 | Type::Ptr => {
            Ok(value.ty.ty)
        }
        _ => Err(LoweringError::InvalidOperands),
    }
}

/// 整数宽度（位）；非整数时报错。
pub(crate) fn integer_bits(ty: Type) -> Result<u32, LoweringError> {
    match ty {
        Type::I8 => Ok(8),
        Type::I16 => Ok(16),
        Type::I32 => Ok(32),
        Type::I64 | Type::Ptr => Ok(64),
        _ => Err(LoweringError::InvalidOperands),
    }
}

/// 算术与比较的宽度形式（8/16/32/64）。
pub(crate) fn wide_kinds(bits: u32) -> &'static [OperandKind] {
    match bits {
        8 => &[OperandKind::Rm8, OperandKind::R8],
        16 => &[OperandKind::Rm16, OperandKind::R16],
        32 => &[OperandKind::Rm32, OperandKind::R32],
        _ => &[OperandKind::Rm64, OperandKind::R64],
    }
}

/// 值搬运（`mov`）的目标宽度：≤32 位值走 32 位形式以维持零扩展。
pub(crate) fn move_kinds(bits: u32) -> &'static [OperandKind] {
    if bits <= 32 {
        &[OperandKind::Rm32, OperandKind::R32]
    } else {
        &[OperandKind::Rm64, OperandKind::R64]
    }
}

/// 单操作数 r/m 形式（`neg`/`not`）。
pub(crate) fn unary_kinds(bits: u32) -> &'static [OperandKind] {
    match bits {
        8 => &[OperandKind::Rm8],
        16 => &[OperandKind::Rm16],
        32 => &[OperandKind::Rm32],
        _ => &[OperandKind::Rm64],
    }
}

/// 内存访问的字节数 → 位宽。
pub(crate) fn access_bits(ty: Type) -> Result<u32, LoweringError> {
    match ty.bytes() {
        Some(1) => Ok(8),
        Some(2) => Ok(16),
        Some(4) => Ok(32),
        Some(8) => Ok(64),
        _ => Err(LoweringError::InvalidOperands),
    }
}
