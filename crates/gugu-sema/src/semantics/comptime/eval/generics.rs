use super::*;
use crate::ast::{GenericArg, GenericParamKind, TyKind};

impl Model<'_> {
    pub(super) fn call_const_arguments(
        &self,
        module: usize,
        expression: ExprId,
        definition: DefRef,
        state: &mut EvalState,
    ) -> EvalResult<BTreeMap<String, Option<ConstantValue>>> {
        let parsed = &self.modules[definition.module];
        let ItemKind::Function(function) = parsed.arena.items[definition.item.0 as usize].kind
        else {
            unreachable!()
        };
        let parameters = parsed.arena.fns[function.0 as usize]
            .generics
            .as_slice(&parsed.arena.generic_params);
        let arena = &self.modules[module].arena;
        let ExprKind::Call { type_args, .. } = arena.exprs[expression.0 as usize].kind else {
            unreachable!()
        };
        let actuals = type_args.as_slice(&arena.generic_args);
        let mut index = Some(0);
        let mut values = BTreeMap::new();
        for parameter in parameters {
            match parameter.kind {
                GenericParamKind::Type { pack: true, .. } => index = None,
                GenericParamKind::Type { .. } => index = index.map(|index| index + 1),
                GenericParamKind::Comptime { name, .. } => {
                    let value = match index.and_then(|index| actuals.get(index)) {
                        Some(actual) => {
                            Some(self.explicit_const_argument(module, expression, *actual, state)?)
                        }
                        None => None,
                    };
                    values.insert(self.name(definition.module, name).to_owned(), value);
                    index = index.map(|index| index + 1);
                }
            }
        }
        Ok(values)
    }

    fn explicit_const_argument(
        &self,
        module: usize,
        expression: ExprId,
        argument: GenericArg,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let bindings = state.const_arguments.last().cloned().unwrap_or_default();
        state.frame_bases.push(state.frames.len());
        state.frames.push(Vec::new());
        for (name, value) in bindings {
            if let Some(value) = value {
                state.bind(name, value);
            }
        }
        let result = (|| {
            let value = match argument {
                GenericArg::Expr(expr) => self.value(module, expr, state)?,
                GenericArg::Type(ty) => {
                    let TyKind::Path(path) = self.modules[module].arena.tys[ty.0 as usize].kind
                    else {
                        return Err(self.fail(module, expression, "comptime 泛型实参需要整数常量"));
                    };
                    self.path_value(module, expression, path, state)?
                }
            };
            if matches!(value, ConstantValue::Int(value) if value >= 0 && value <= i128::from(i64::MAX))
            {
                Ok(value)
            } else {
                Err(self.fail(module, expression, "comptime 泛型实参需要非负 int 常量"))
            }
        })();
        state.frames.pop();
        state.frame_bases.pop();
        result
    }
}
