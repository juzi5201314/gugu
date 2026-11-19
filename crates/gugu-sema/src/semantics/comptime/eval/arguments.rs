use super::*;
use crate::ast::{TyId, TyKind};

impl Model<'_> {
    pub(super) fn check_const_array_arguments(
        &self,
        definition: DefRef,
        arguments: &[ConstantValue],
        bindings: &BTreeMap<String, Option<ConstantValue>>,
        span: &crate::Span,
    ) -> Result<(), Diagnostic> {
        let parsed = &self.modules[definition.module];
        let ItemKind::Function(function) = parsed.arena.items[definition.item.0 as usize].kind
        else {
            unreachable!()
        };
        let declaration = &parsed.arena.fns[function.0 as usize];
        let mut bindings = bindings.clone();
        for (parameter, value) in declaration
            .params
            .as_slice(&parsed.arena.params)
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                parsed
                    .configured
                    .param_active(declaration.params.start as usize + index)
            })
            .map(|(_, parameter)| parameter)
            .zip(arguments)
        {
            if parameter.comptime
                && let Some(pattern) = parameter.pat
                && let PatKind::Ident(name) = parsed.arena.pats[pattern.0 as usize].kind
            {
                bindings.insert(
                    self.name(definition.module, name).to_owned(),
                    Some(value.clone()),
                );
            }
        }
        for (parameter, value) in declaration
            .params
            .as_slice(&parsed.arena.params)
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                parsed
                    .configured
                    .param_active(declaration.params.start as usize + index)
            })
            .map(|(_, parameter)| parameter)
            .zip(arguments)
        {
            if let Some(ty) = parameter.ty {
                self.check_const_array_value(definition.module, ty, value, &bindings, span)?;
            }
        }
        Ok(())
    }

    fn check_const_array_value(
        &self,
        module: usize,
        ty: TyId,
        value: &ConstantValue,
        bindings: &BTreeMap<String, Option<ConstantValue>>,
        span: &crate::Span,
    ) -> Result<(), Diagnostic> {
        let arena = &self.modules[module].arena;
        match (arena.tys[ty.0 as usize].kind, value) {
            (TyKind::Array { elem, len }, ConstantValue::Array(items)) => {
                let len = self.unparenthesized_expression(module, len);
                let expected = match arena.exprs[len.0 as usize].kind {
                    ExprKind::Literal(literal) => self.literal_value(module, literal),
                    ExprKind::Path(path) => match self.path(module, path).as_slice() {
                        [name] => bindings.get(*name).cloned().flatten(),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(ConstantValue::Int(count)) = expected
                    && count != items.len() as i128
                {
                    return Err(Diagnostic::error(
                        DiagnosticCode::InvalidType,
                        format!(
                            "comptime 数组实参长度与参数长度不一致：需要 {count}，实际 {}",
                            items.len()
                        ),
                        Some(span.clone()),
                    ));
                }
                for item in items {
                    self.check_const_array_value(module, elem, item, bindings, span)?;
                }
            }
            (TyKind::Tuple(types), ConstantValue::Tuple(items)) => {
                for (&ty, item) in types.as_slice(&arena.ty_ids).iter().zip(items) {
                    self.check_const_array_value(module, ty, item, bindings, span)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
