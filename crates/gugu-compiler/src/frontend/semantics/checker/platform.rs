//! 平台范围原语按规范路径登记，固定签名与 `unsafe` 门禁在同一处检查。
//!
//! 这些原语不是普通函数：它们按规范路径识别，导入别名不改变身份，也不能取地址。全部调用都
//! 必须在 `unsafe` 块内，因为对齐、长度、lease 与 grace 的前置条件由调用方维护，编译器不能
//! 证明它们成立。

use super::super::model::PlatformIntrinsic;
use super::super::output::PlatformOperation;
use super::*;

impl Checker<'_, '_> {
    pub(super) fn platform_call(
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
        let kind = PlatformIntrinsic::from_path(&path)?;
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
                "平台范围原语不接受类型实参",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        let expected_arguments = kind.value_arguments();
        if args.len() != expected_arguments {
            self.error(
                DiagnosticCode::InvalidExpression,
                format!("平台范围原语 `{kind}` 需要 {expected_arguments} 个值实参"),
                span.clone(),
            );
            return Some(Ty::Error);
        }
        // 全部参数都是平台无关的整数或布尔标量；调用方负责给出满足前置条件的值。
        for (index, &argument) in args.iter().enumerate() {
            let ty = self.expression(argument, None);
            let normalized = self.normalized(&ty);
            let acceptable = match (kind, index) {
                (PlatformIntrinsic::SetDumpPolicy, 1) => {
                    matches!(normalized, Ty::Bool | Ty::Error | Ty::Var(_) | Ty::Param(_))
                }
                // 整数实参必须是无符号的：字节数、对齐、range 编号与字数都不接受有符号量。
                _ => matches!(
                    normalized,
                    Ty::Int { signed: false, .. }
                        | Ty::Error
                        | Ty::Var(_)
                        | Ty::Param(_)
                        | Ty::Projection(..)
                        | Ty::Opaque(..)
                ),
            };
            if !acceptable {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    format!("平台范围原语 `{kind}` 的实参必须是整数或布尔标量"),
                    span.clone(),
                );
            }
        }
        // 平台调用会触碰映射状态，也可能睡眠，因此必须显式承担 unsafe 前置条件。
        self.require_unsafe(span, "平台范围原语");
        let ty = self.platform_result(kind);
        if let Some(expected) = expected {
            self.unify(&ty, expected, span);
        }
        self.require_value_captures(callee);
        for &argument in args {
            self.require_value_captures(argument);
        }
        self.platform_operations.push(PlatformOperation {
            expression: callee,
            kind,
            ty: ty.clone(),
            arguments: args.to_vec(),
        });
        Some(ty)
    }

    /// 返回原语的返回类型：状态操作返回 `()`，`reserve_aligned`/`wake` 返回无符号整数，
    /// `wait`/`low_memory_hint` 返回 `bool`，`entropy` 返回平台所有的 `*byte`。
    fn platform_result(&self, kind: PlatformIntrinsic) -> Ty {
        if kind.yields_raw_bytes() {
            Ty::Ptr(Box::new(Ty::Int {
                signed: false,
                bits: 8,
            }))
        } else if kind.yields_bool() {
            Ty::Bool
        } else if kind.yields_range() || kind.yields_count() {
            Ty::Int {
                signed: false,
                bits: 64,
            }
        } else {
            Ty::Unit
        }
    }

    /// 检查调用点是否位于 `unsafe` 块内；不在时报告固定诊断。
    ///
    /// 与原始指针解引用、union 字段访问共用 `E0041`：它们都是「不安全操作必须处于
    /// `unsafe` 块中」这一条规则的实例。
    fn require_unsafe(&mut self, span: &Span, what: &str) {
        if self.unsafe_depth == 0 {
            self.error(
                DiagnosticCode::InvalidExpression,
                format!("{what}只能在 `unsafe` 块内调用"),
                span.clone(),
            );
        }
    }
}
