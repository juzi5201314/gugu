//! x86_64 对 LIR [`TargetOracle`] 的实现：poll 成本与 legalization 门禁都取自 lowering 表与
//! instruction verifier，与 codegen 同源。

use super::lower::{self, SiteValue};
use super::reg::Reg;
use super::verify;
use crate::frontend::gir::body::SourceInfo;
use crate::lir::body::{Lane, Op, Type, ValueId, ValueType};
use crate::lir::target::{LegalizeError, TargetOracle};
use crate::target::CpuBaseline;

/// 按 CPU 基线回答 LIR 机器事实的 x86_64 后端。
#[derive(Clone, Copy, Debug)]
pub(crate) struct X64Oracle {
    baseline: CpuBaseline,
}

impl X64Oracle {
    pub(crate) const fn new(baseline: CpuBaseline) -> Self {
        Self { baseline }
    }
}

impl TargetOracle for X64Oracle {
    fn poll_cost(&self, op: &Op) -> u32 {
        lower::poll_cost(op)
    }

    fn legalize(
        &self,
        op: &Op,
        operands: &[ValueId],
        results: &[ValueId],
        kind: &dyn Fn(ValueId) -> ValueType,
        source: &SourceInfo,
    ) -> Result<(), LegalizeError> {
        if let Op::Vector(vector) = op
            && let Some(lane) = vector_lane(results, kind)
            && !lower::supports_vector(*vector, lane)
        {
            return Err(LegalizeError::UnsupportedVector {
                vector: *vector,
                lane,
            });
        }
        let name = lower::domain(op);
        let lowered = lower::lower(
            op,
            &site_values(operands, kind),
            &site_values(results, kind),
            source,
        )
        .map_err(|error| LegalizeError::MissingSequence {
            op: name,
            detail: error.to_string(),
        })?;
        verify::verify_sequence(&lowered.sequence, self.baseline).map_err(|error| {
            LegalizeError::IllegalSequence {
                op: name,
                detail: error.to_string(),
            }
        })?;
        Ok(())
    }
}

/// 站点的操作数表示：虚拟寄存器编号 = 值编号。
fn site_values(values: &[ValueId], kind: &dyn Fn(ValueId) -> ValueType) -> Vec<SiteValue> {
    values
        .iter()
        .map(|value| SiteValue {
            ty: kind(*value),
            reg: Reg::Virtual(value.0),
        })
        .collect()
}

/// 向量结果类型的 lane；非 `V128` 结果返回 `None`。
fn vector_lane(results: &[ValueId], kind: &dyn Fn(ValueId) -> ValueType) -> Option<Lane> {
    let [result] = results else {
        return None;
    };
    match kind(*result).ty {
        Type::V128(lane) => Some(lane),
        _ => None,
    }
}
