use super::*;
use crate::expand::derive;
use crate::semantics::traits::{Obligation, Owner};

impl Model<'_> {
    pub(super) fn collect_derive_obligations(&mut self) -> Result<(), Diagnostic> {
        for (module_index, module) in self.modules.iter().enumerate() {
            for &(implementation, declaration) in &module.derived_impls {
                let Some(Owner::Implementation(index)) =
                    self.traits.owners[module_index][implementation.0 as usize]
                else {
                    unreachable!("派生声明已经收集为 impl");
                };
                let item = &module.arena.items[declaration.0 as usize];
                let params = self.generic_parameters(module_index, derive::generics(item));
                let interface = self.traits.implementations[index]
                    .interface
                    .clone()
                    .expect("派生 trait");
                let mut obligations = Vec::new();
                for shape in derive::shapes(module, item) {
                    for field in shape.fields {
                        obligations.push(Obligation {
                            ty: self.form_inner(
                                module_index,
                                field.ty,
                                &params,
                                &mut Vec::new(),
                            )?,
                            interface: interface.clone(),
                            span: field.span.clone(),
                        });
                    }
                }
                // 直接保留字段义务，现有 trait 选择自然处理 Option[T]:Print 等复合条件。
                // 方法体 assumptions 与单态化都从这个 impl 读取同一组义务。
                self.traits.implementations[index]
                    .obligations
                    .extend(obligations);
            }
        }
        Ok(())
    }

    pub(super) fn validate_derives(&self) -> Result<(), Diagnostic> {
        for (module_index, module) in self.modules.iter().enumerate() {
            for &(implementation, declaration) in &module.derived_impls {
                let Some(Owner::Implementation(index)) =
                    self.traits.owners[module_index][implementation.0 as usize]
                else {
                    unreachable!("派生声明已经收集为 impl");
                };
                let implementation = &self.traits.implementations[index];
                let interface = implementation.interface.as_ref().expect("派生 trait");
                let name = &self.traits.interfaces[interface.id].name;
                let item = &module.arena.items[declaration.0 as usize];
                let params = self.generic_parameters(module_index, derive::generics(item));
                for shape in derive::shapes(module, item) {
                    for (index, field) in shape.fields.iter().enumerate() {
                        let ty =
                            self.form_inner(module_index, field.ty, &params, &mut Vec::new())?;
                        if !has_parameters(&ty) && self.require_trait(&ty, interface, &[]).is_err()
                        {
                            let field_name = field
                                .name
                                .map(|symbol| self.name(module_index, symbol).to_owned())
                                .unwrap_or_else(|| index.to_string());
                            return Err(self.error_at(
                                &field.span,
                                format!(
                                    "字段 `{field_name}` 未实现 {name}，无法派生 {} 的 {name}",
                                    self.name(module_index, item.name.expect("名义类型"))
                                ),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn has_parameters(ty: &Ty) -> bool {
    match ty {
        Ty::Param(_) | Ty::Projection(..) => true,
        Ty::Ref(inner)
        | Ty::Ptr(inner)
        | Ty::Slice(inner)
        | Ty::Array(inner, _)
        | Ty::Option(inner)
        | Ty::Chan(inner)
        | Ty::Join(inner)
        | Ty::MaybeUninit(inner) => has_parameters(inner),
        Ty::Tuple(types) | Ty::Named(_, types) | Ty::Opaque(_, types) => {
            types.iter().any(has_parameters)
        }
        Ty::Result(value, error) => has_parameters(value) || has_parameters(error),
        Ty::Function(parameters, result) => {
            parameters.iter().any(has_parameters) || has_parameters(result)
        }
        Ty::Callable(_, arguments, signature) => {
            arguments.iter().any(has_parameters) || has_parameters(signature)
        }
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .any(|interface| interface.arguments.iter().any(has_parameters)),
        _ => false,
    }
}
