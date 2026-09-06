//! C 边界在具体布局形成后检查，不能仅凭机器字数把受管值当成 C 值。
use super::*;
use crate::TargetName;
use crate::frontend::ast::ItemId;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Position {
    Parameter,
    Return,
    Field,
}

impl Layouts<'_, '_> {
    pub(super) fn validate_abi(&mut self, target: TargetName) -> Result<(), Diagnostic> {
        for (module, parsed) in self.model.modules.iter().enumerate() {
            for (index, item) in parsed.arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                let ItemKind::Function(id) = item.kind else {
                    continue;
                };
                let function = &parsed.arena.fns[id.0 as usize];
                if function.extern_abi.is_none() {
                    continue;
                }
                if function.generics.len != 0 {
                    return Err(Diagnostic::error(
                        DiagnosticCode::InvalidType,
                        "C 签名不能声明泛型参数",
                        Some(function.span.clone()),
                    ));
                }
                let signature = self
                    .model
                    .value_type(super::super::semantics::model::DefRef {
                        module,
                        item: ItemId(index as u32),
                    })?;
                let (types, result) = signature.signature().expect("函数声明具有签名");
                let mut types = types.iter();
                for (offset, param) in function
                    .params
                    .as_slice(&parsed.arena.params)
                    .iter()
                    .enumerate()
                {
                    if !parsed
                        .configured
                        .param_active(function.params.start as usize + offset)
                    {
                        continue;
                    }
                    let ty = types.next().expect("签名只包含生效参数");
                    if param.variadic || !self.abi_type(ty, target, Position::Parameter)? {
                        return Err(Diagnostic::error(
                            DiagnosticCode::InvalidType,
                            "该参数类型不能表示为目标 C ABI 值",
                            Some(param.span.clone()),
                        ));
                    }
                }
                if let Some(id) = function.return_ty {
                    if !self.abi_type(result, target, Position::Return)? {
                        return Err(Diagnostic::error(
                            DiagnosticCode::InvalidType,
                            "该返回类型不能表示为目标 C ABI 值",
                            Some(parsed.arena.tys[id.0 as usize].span.clone()),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn abi_type(
        &mut self,
        ty: &Ty,
        target: TargetName,
        position: Position,
    ) -> Result<bool, Diagnostic> {
        Ok(match ty {
            Ty::Unit | Ty::Never => position == Position::Return,
            Ty::Int { bits: 128, .. } => target == TargetName::X86_64Linux,
            Ty::Int { .. } | Ty::Float(_) | Ty::Bool | Ty::Ptr(_) => true,
            Ty::Array(element, _) if position == Position::Field => {
                self.abi_type(element, target, Position::Field)?
            }
            Ty::Opaque(..) => {
                let concrete = self.model.hidden_type(ty, &self.semantics.hidden_types)?;
                self.abi_type(&concrete, target, position)?
            }
            Ty::Named(index, _) => {
                let nominal = &self.model.nominal[*index];
                let repr = nominal.repr;
                let Some(layout) = self.layout(ty)? else {
                    return Ok(false);
                };
                if layout.size == 0 {
                    return Ok(false);
                }
                let variants = self.model.variants(ty).expect("名义类型已形成");
                if nominal.is_enum {
                    if variants.iter().any(|variant| !variant.fields.is_empty()) {
                        return Ok(false);
                    }
                    let Some((signed, bits)) = repr.tag else {
                        return Ok(false);
                    };
                    return self.abi_type(&Ty::Int { signed, bits }, target, position);
                }
                if !repr.c() && !repr.transparent() {
                    return Ok(false);
                }
                for field in &variants[0].fields {
                    if self
                        .layout(&field.ty)?
                        .is_some_and(|layout| layout.size == 0)
                    {
                        continue;
                    }
                    if !self.abi_type(&field.ty, target, Position::Field)? {
                        return Ok(false);
                    }
                }
                true
            }
            _ => false,
        })
    }

    pub(super) fn check_assembly(
        &mut self,
        plan: &super::super::semantics::assembly::AssemblyPlan,
    ) -> Result<(), Diagnostic> {
        use super::super::semantics::assembly::AssemblyContext;
        if plan.context == AssemblyContext::Managed && plan.clobbers & (1 << 4) != 0 {
            return Err(invalid("managed asm 不能声明破坏栈指针"));
        }
        for operand in &plan.operands {
            if self
                .layout(&operand.ty)?
                .is_some_and(|layout| layout.size > u64::from(operand.register.bits) / 8)
            {
                return Err(invalid("汇编值不能装入指定寄存器宽度"));
            }
            if self
                .model
                .has_managed_value(&operand.ty, &self.semantics.hidden_types)
                == Some(true)
            {
                return Err(invalid("汇编寄存器操作数不能绕过 COW 或 resource 管理动作"));
            }
        }
        Ok(())
    }

    pub(super) fn check_native_body(
        &mut self,
        body: &super::super::semantics::CheckedBody,
        target: TargetName,
    ) -> Result<(), Diagnostic> {
        use super::super::semantics::{foreign::ForeignEffect, model::CallableId};
        let definition = body.definition;
        let parsed = &self.model.modules[definition.module];
        let ItemKind::Function(function) = parsed.arena.items[definition.item.0 as usize].kind
        else {
            return Ok(());
        };
        let callable = CallableId {
            module: definition.module,
            function: function.0,
        };
        if self
            .model
            .foreign_definition_at(callable)
            .is_none_or(|definition| {
                !definition.naked() && definition.effect != Some(ForeignEffect::DirtyCpu)
            })
        {
            return Ok(());
        }
        let fail = || {
            Diagnostic::error(
                DiagnosticCode::InvalidType,
                "opaque native definition 只能包含 C ABI bit value/raw pointer，不能携带受管值或调用 Gugu 方法",
                Some(parsed.arena.fns[function.0 as usize].span.clone()),
            )
        };
        for ty in &body.slots {
            if !self.native_value(ty, target)? {
                return Err(fail());
            }
        }
        for (id, ty) in &body.expressions {
            if matches!(ty, Ty::Function(..))
                && body.memory_operations.iter().any(|operation| {
                    operation.expression == *id
                        && matches!(
                            operation.kind,
                            super::super::semantics::model::MemoryIntrinsic::ScalarCast
                                | super::super::semantics::model::MemoryIntrinsic::PointerCast
                        )
                })
            {
                continue;
            }
            if !self.native_value(ty, target)? {
                return Err(fail());
            }
        }
        if body.dispatches.iter().any(|dispatch| {
            dispatch.dynamic
                || dispatch.callable.is_some_and(|callable| {
                    self.model
                        .foreign_definition_at(callable)
                        .and_then(|definition| definition.effect)
                        .is_none_or(|effect| effect == ForeignEffect::Bridge)
                })
        }) {
            return Err(fail());
        }
        Ok(())
    }

    fn native_value(&mut self, ty: &Ty, target: TargetName) -> Result<bool, Diagnostic> {
        if let Ty::Callable(callable, _, _) = ty {
            return Ok(self
                .model
                .foreign_definition_at(*callable)
                .is_some_and(|definition| definition.effect.is_some()));
        }
        if self.layout(ty)?.is_some_and(|layout| layout.size == 0) {
            return Ok(self.model.is_bit_type(ty, &self.semantics.hidden_types) == Some(true));
        }
        self.abi_type(ty, target, Position::Field)
    }
}
