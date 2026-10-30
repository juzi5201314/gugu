use super::super::assembly::{
    AssemblyContext, AssemblyPlan, Direction, FLAGS, MEMORY, Operand, Register, RegisterClass,
    managed_stack,
};
use super::super::model::ConstantValue;
use super::*;

impl Checker<'_, '_> {
    pub(super) fn global_asm(&mut self, template: ExprId) {
        if let Some(template_text) = self.assembly_template(template) {
            self.assembly.push(AssemblyPlan {
                expression: template,
                context: AssemblyContext::Global,
                template: template_text,
                operands: Vec::new(),
                clobbers: 0,
                stack_reserve: None,
            });
        }
    }

    pub(super) fn inline_asm(
        &mut self,
        expression: ExprId,
        template: ExprId,
        operands: AstRange<AsmOperand>,
    ) -> Ty {
        let context = self
            .native_definition()
            .map_or(AssemblyContext::Managed, |definition| {
                if definition.naked() {
                    AssemblyContext::Naked
                } else {
                    AssemblyContext::Native
                }
            });
        let span = &self.arena().exprs[expression.0 as usize].span;
        if self.unsafe_depth == 0 && context != AssemblyContext::Naked {
            self.error(
                DiagnosticCode::InvalidExpression,
                "asm 必须处于 unsafe 块中",
                span.clone(),
            );
        }
        let Some(template) = self.assembly_template(template) else {
            return Ty::Error;
        };
        let stack_reserve = if context == AssemblyContext::Managed {
            match managed_stack(&template) {
                Ok(stack) => Some(stack),
                Err(message) => {
                    self.error(DiagnosticCode::InvalidExpression, format!("{message}；请拆分 asm 并调用 std.runtime.safepoint_poll()，或使用 #[ffi(dirty_cpu)] native definition"), span.clone());
                    None
                }
            }
        } else {
            None
        };
        let mut plan = AssemblyPlan {
            expression,
            context,
            template,
            operands: Vec::with_capacity(operands.len as usize),
            clobbers: 0,
            stack_reserve,
        };
        let mut inputs = 0_u64;
        let mut outputs = 0_u64;
        let mut early_outputs = 0_u64;
        for operand in operands.as_slice(&self.arena().asm_operands) {
            let (direction, reg, expression) = match operand.kind {
                AsmOperandKind::In { reg, expr } => (Direction::In, reg, expr),
                AsmOperandKind::Out { reg, place } => (Direction::Out, reg, place),
                AsmOperandKind::Lateout { reg, place } => (Direction::Lateout, reg, place),
                AsmOperandKind::Clobber { regs } => {
                    for &reg in regs.as_slice(&self.arena().symbols) {
                        let name = super::super::super::string::decode_string(
                            self.model.name(self.module, reg),
                        );
                        let mask = match name.as_ref() {
                            "memory" => Some(MEMORY),
                            "cc" => Some(FLAGS),
                            _ => Register::parse(&name).map(Register::mask),
                        };
                        if let Some(mask) = mask {
                            plan.clobbers |= mask;
                        } else {
                            self.error(
                                DiagnosticCode::InvalidExpression,
                                "未知汇编 clobber",
                                operand.span.clone(),
                            );
                        }
                    }
                    continue;
                }
            };
            let name =
                super::super::super::string::decode_string(self.model.name(self.module, reg));
            let Some(register) = Register::parse(&name) else {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "寄存器不属于 x86-64-v1",
                    operand.span.clone(),
                );
                continue;
            };
            if context == AssemblyContext::Managed
                && register.class == RegisterClass::Gpr
                && register.index == 4
            {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "managed asm 不能把栈指针作为输入输出绑定",
                    operand.span.clone(),
                );
            }
            let mask = register.mask();
            let conflict = if direction == Direction::In {
                inputs & mask != 0 || early_outputs & mask != 0
            } else {
                outputs & mask != 0 || direction == Direction::Out && inputs & mask != 0
            };
            if conflict {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "汇编输入输出占用了冲突的物理寄存器",
                    operand.span.clone(),
                );
            }
            let ty = if direction == Direction::In {
                inputs |= mask;
                let hint = (register.class != RegisterClass::Xmm
                    && matches!(
                        self.arena().exprs[expression.0 as usize].kind,
                        ExprKind::Literal(LitKind::Int { .. })
                    ))
                .then_some(Ty::Int {
                    signed: true,
                    bits: register.bits,
                });
                self.expression(expression, hint.as_ref())
            } else {
                outputs |= mask;
                if direction == Direction::Out {
                    early_outputs |= mask;
                }
                plan.clobbers |= mask;
                self.place(expression, false)
            };
            plan.operands.push(Operand {
                direction,
                register,
                expression,
                ty,
            });
        }
        for operand in &plan.operands {
            if operand.direction != Direction::In
                && let Some(slot) = self.place_root(operand.expression)
            {
                self.initialize(slot, true);
                self.state.callables[slot].clear();
            }
        }
        self.assembly.push(plan);
        if context == AssemblyContext::Naked {
            Ty::Never
        } else {
            Ty::Unit
        }
    }

    fn assembly_template(&mut self, expression: ExprId) -> Option<String> {
        match self
            .model
            .constant_value(self.module, expression, &Ty::String)
        {
            Ok(ConstantValue::String(value)) => Some(value),
            Ok(_) => {
                self.error(
                    DiagnosticCode::InvalidType,
                    "汇编模板必须是编译期 string",
                    self.arena().exprs[expression.0 as usize].span.clone(),
                );
                None
            }
            Err(error) => {
                self.errors.push(error);
                None
            }
        }
    }
}
