use super::*;

impl Checker<'_, '_> {
    pub(super) fn pack_parameter(&self, iter: ExprId, ty: &Ty) -> Option<String> {
        let Ty::Param(name) = ty else { return None };
        let ExprKind::Path(path) = self.arena().exprs[iter.0 as usize].kind else {
            return None;
        };
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        let [segment] = segments else { return None };
        let slot = &self.slots[*self.state.names.get(&segment.name)?];
        if self
            .current_function
            .is_some_and(|function| self.is_parameter_pack(function, ty))
        {
            return Some(name.clone());
        }
        self.arena()
            .fns
            .iter()
            .enumerate()
            .find_map(|(index, function)| {
                (self.is_parameter_pack(FnId(index as u32), ty)
                    && function
                        .params
                        .as_slice(&self.arena().params)
                        .iter()
                        .any(|parameter| {
                            parameter.variadic
                                && parameter.variadic_name == Some(segment.name)
                                && slot.range == [parameter.span.start(), parameter.span.end()]
                        }))
                .then(|| name.clone())
            })
    }

    pub(super) fn assumptions(
        &self,
        span: &Span,
    ) -> Result<Vec<super::super::traits::Obligation>, Diagnostic> {
        let mut assumptions = self.model.assumptions_at(self.module, span)?;
        let original = assumptions.clone();
        for (element, pack) in &self.pack_elements {
            let bindings = BTreeMap::from([(pack.clone(), Ty::Param(element.clone()))]);
            assumptions.extend(original.iter().map(|obligation| {
                super::super::traits::Obligation {
                    ty: super::super::model::substitute(&obligation.ty, &bindings),
                    interface: obligation.interface.substitute(&bindings),
                    span: obligation.span.clone(),
                }
            }));
        }
        Ok(assumptions)
    }
}
