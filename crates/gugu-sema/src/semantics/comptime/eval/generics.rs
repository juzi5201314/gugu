use super::*;
use crate::ast::{GenericArg, GenericParamKind, TyKind};
use crate::semantics::model::ArrayLength;

impl Model<'_> {
    pub(super) fn call_const_arguments(
        &self,
        module: usize,
        expression: ExprId,
        definition: DefRef,
        arguments: &[ConstantValue],
        state: &mut EvalState,
    ) -> EvalResult<BTreeMap<String, ConstantValue>> {
        let ItemKind::Function(function) =
            self.modules[definition.module].arena.items[definition.item.0 as usize].kind
        else {
            unreachable!()
        };
        let parsed = &self.modules[definition.module];
        let declaration = &parsed.arena.fns[function.0 as usize];
        let parameters = declaration.generics.as_slice(&parsed.arena.generic_params);
        if !parameters
            .iter()
            .any(|parameter| matches!(parameter.kind, GenericParamKind::Comptime { .. }))
        {
            return Ok(BTreeMap::new());
        }
        let arena = &self.modules[module].arena;
        let ExprKind::Call { type_args, .. } = arena.exprs[expression.0 as usize].kind else {
            unreachable!()
        };
        let actuals = type_args.as_slice(&arena.generic_args);
        if !actuals.is_empty() && actuals.len() != parameters.len() {
            return Err(self.fail(module, expression, "comptime 泛型实参数量不符"));
        }
        let mut values = BTreeMap::new();
        for (parameter, actual) in parameters.iter().zip(actuals) {
            let GenericParamKind::Comptime { name, .. } = parameter.kind else {
                continue;
            };
            let value = match *actual {
                GenericArg::Expr(expr) => self.value(module, expr, state)?,
                GenericArg::Type(ty) => {
                    let TyKind::Path(path) = arena.tys[ty.0 as usize].kind else {
                        return Err(self.fail(module, expression, "comptime 泛型实参需要整数常量"));
                    };
                    self.path_value(module, expression, path, state)?
                }
            };
            if !matches!(value, ConstantValue::Int(value) if value >= 0 && value <= i128::from(i64::MAX))
            {
                return Err(self.fail(module, expression, "comptime 泛型实参需要非负 int 常量"));
            }
            values.insert(self.name(definition.module, name).to_owned(), value);
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
                self.infer_const_values(&self.form(definition.module, ty)?, value, &mut values)
                    .map_err(|message| self.fail(module, expression, message))?;
            }
        }
        for parameter in parameters {
            if let GenericParamKind::Comptime { name, .. } = parameter.kind
                && !values.contains_key(self.name(definition.module, name))
            {
                return Err(self.fail(module, expression, "无法推断 comptime 泛型实参"));
            }
        }
        Ok(values)
    }

    fn infer_const_values(
        &self,
        ty: &Ty,
        value: &ConstantValue,
        values: &mut BTreeMap<String, ConstantValue>,
    ) -> Result<(), &'static str> {
        match (ty, value) {
            (Ty::Array(element, length), ConstantValue::Array(items)) => {
                let actual = ConstantValue::Int(items.len() as i128);
                match length {
                    ArrayLength::Param(name) => {
                        if values.get(name).is_some_and(|previous| previous != &actual) {
                            return Err("comptime 泛型数组长度不一致");
                        }
                        values.insert(name.clone(), actual);
                    }
                    ArrayLength::Known(count) if *count != items.len() as u64 => {
                        return Err("comptime 数组长度不一致");
                    }
                    _ => {}
                }
                for item in items {
                    self.infer_const_values(element, item, values)?;
                }
            }
            (Ty::Tuple(types), ConstantValue::Tuple(items)) => {
                for (ty, value) in types.iter().zip(items) {
                    self.infer_const_values(ty, value, values)?;
                }
            }
            (Ty::Named(..), ConstantValue::Struct(fields)) => {
                if let Some(variants) = self.variants(ty) {
                    for field in variants.iter().flat_map(|variant| &variant.fields) {
                        if let Some(value) = fields.get(&field.name) {
                            self.infer_const_values(&field.ty, value, values)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
