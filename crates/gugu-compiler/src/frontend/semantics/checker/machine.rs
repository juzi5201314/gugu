//! runtime 机器原语按规范路径登记，固定机器边界与 `unsafe` 门禁在同一处检查。
//!
//! 这些原语不是普通函数：它们按规范路径识别，导入别名不改变身份，也不能取地址。全部调用都
//! 必须在 `unsafe` 块内，因为寄存器边界、栈布局与控制转移的前置条件由调用方维护，编译器不能
//! 证明它们成立。实参只验机器形状（原始指针或无符号机器字），点类型由登记机器片段的固定
//! 内部边界维护。

use super::super::model::RuntimeMachineIntrinsic;
use super::super::output::RuntimeMachineOperation;
use super::*;

impl Checker<'_, '_> {
    pub(super) fn runtime_machine_call(
        &mut self,
        callee: ExprId,
        path: PathId,
        type_args: AstRange<GenericArg>,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let segments = self.arena().paths[usize::try_from(path.0).expect("路径下标")]
            .segments
            .as_slice(&self.arena().segments);
        if segments
            .first()
            .is_some_and(|segment| self.state.names.contains_key(&segment.name))
        {
            return None;
        }
        let path = self
            .model
            .external_path(self.module, &self.model.path(self.module, path))?;
        let kind = RuntimeMachineIntrinsic::from_path(&path)?;
        let type_args = if type_args.len != 0 {
            type_args
        } else {
            segments
                .iter()
                .rev()
                .find(|segment| segment.args.len != 0)
                .map_or(AstRange::empty(), |segment| segment.args)
        };
        let span = &self.arena().exprs[usize::try_from(callee.0).expect("表达式下标")].span;
        if type_args.len != 0 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "runtime 机器原语不接受类型实参",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        let expected_arguments = kind.value_arguments();
        if args.len() != expected_arguments {
            self.error(
                DiagnosticCode::InvalidExpression,
                format!("runtime 机器原语 `{kind}` 需要 {expected_arguments} 个值实参"),
                span.clone(),
            );
            return Some(Ty::Error);
        }
        // 指针实参必须是原始指针，机器字实参必须是无符号整数；点类型与前置条件由登记机器
        // 片段的固定内部边界维护，checker 只验机器形状。
        for (index, &argument) in args.iter().enumerate() {
            let ty = self.expression(argument, None);
            let normalized = self.normalized(&ty);
            let pointer = matches!(
                (kind, index),
                (RuntimeMachineIntrinsic::ContextSwitch, 0..=3)
                    | (RuntimeMachineIntrinsic::ContextRestore, 0..=2)
                    | (RuntimeMachineIntrinsic::ReportWrite, 1)
            );
            let acceptable = match normalized {
                Ty::Error | Ty::Var(_) | Ty::Param(_) | Ty::Projection(..) | Ty::Opaque(..) => true,
                Ty::Ptr(_) => pointer,
                Ty::Int { signed: false, .. } => !pointer,
                _ => false,
            };
            if !acceptable {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    if pointer {
                        format!("runtime 机器原语 `{kind}` 的指针实参必须是原始指针")
                    } else {
                        format!("runtime 机器原语 `{kind}` 的机器字实参必须是无符号整数")
                    },
                    span.clone(),
                );
            }
        }
        // 机器原语跨越 runtime 边界并可能转移控制，必须显式承担 unsafe 前置条件。
        self.require_unsafe(span, "runtime 机器原语");
        let ty = self.runtime_machine_result(kind);
        if let Some(expected) = expected {
            self.unify(&ty, expected, span);
        }
        self.require_value_captures(callee);
        for &argument in args {
            self.require_value_captures(argument);
        }
        self.runtime_machine_operations
            .push(RuntimeMachineOperation {
                expression: callee,
                kind,
                ty: ty.clone(),
                arguments: args.to_vec(),
            });
        Some(ty)
    }

    /// 返回原语的返回类型：`current_slot`/`report_write` 返回机器字 `uint`，其余不返回值。
    fn runtime_machine_result(&self, kind: RuntimeMachineIntrinsic) -> Ty {
        if kind.yields_word() {
            Ty::Int {
                signed: false,
                bits: 64,
            }
        } else {
            Ty::Unit
        }
    }
}
