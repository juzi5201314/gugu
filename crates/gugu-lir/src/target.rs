//! LIR 向目标后端询问机器事实的唯一接口。
//!
//! LIR 不依赖具体后端：poll 成本模型与目标 legalization 需要的机器序列由后端实现本 trait，
//! 编译 driver 按目标描述符构造实现并注入 [`super::build`]。

use std::fmt;

use super::body::{Lane, Op, ValueId, ValueType, VectorOp};
use crate::gir::body::SourceInfo;

/// 目标后端为 LIR 提供的机器事实。
pub trait TargetOracle {
    /// 机器 lowering 域内 op 的 poll 成本：实际机器序列权重之和，作为该 op 所有站点的上界。
    fn poll_cost(&self, op: &Op) -> u32;

    /// 用虚拟寄存器试 lower 一条指令并跑目标 instruction verifier；`kind` 给出值的类型。
    fn legalize(
        &self,
        op: &Op,
        operands: &[ValueId],
        results: &[ValueId],
        kind: &dyn Fn(ValueId) -> ValueType,
        source: &SourceInfo,
    ) -> Result<(), LegalizeError>;
}

/// 目标 legalization 失败。
///
/// LIR 不能命名后端的错误类型，`detail` 是后端 lowering/verifier 给出的原因文本；
/// 它只进入 legalization 诊断的消息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegalizeError {
    /// 向量 op 在该 lane 上没有基线机器序列。
    UnsupportedVector { vector: VectorOp, lane: Lane },
    /// op 的类型组合没有基线机器序列。
    MissingSequence { op: &'static str, detail: String },
    /// 机器序列没有通过目标 instruction verifier。
    IllegalSequence { op: &'static str, detail: String },
}

impl fmt::Display for LegalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVector { vector, lane } => write!(
                formatter,
                "目标 legalization 缺少机器序列: Vector::{vector:?}（lane {lane:?}）"
            ),
            Self::MissingSequence { op, detail } => {
                write!(
                    formatter,
                    "目标 legalization 缺少机器序列: {op}（{detail}）"
                )
            }
            Self::IllegalSequence { op, detail } => {
                write!(formatter, "目标 legalization 的 {op} 序列非法: {detail}")
            }
        }
    }
}
