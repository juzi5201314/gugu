use super::*;
use crate::ast::{GenericArg, GenericParamKind, TyKind};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_const_arguments_do_not_allocate_caller_environment_copies() {
        let sources = crate::SourceMap::new(vec![crate::source::SourceSnapshot::from_str("main.gg", "fn number[comptime N: int]() int { N }\nfn main() { _ = comptime source { let payload = \"abcdefgh\"\nlet value = number::[3]()\nlet invalid = number::[true]()\nlet read = number::[payload]()\nstd.syntax.parse_source(\"3\") } }").unwrap()]).unwrap();
        let cfg = crate::cfg::CfgContext::target_only(crate::TargetName::X86_64Linux);
        let modules =
            crate::modules::parse_modules(&sources, "", &cfg, &Default::default()).unwrap();
        let names = crate::names::analyze_with_prefix(
            "tests/const-arguments@1",
            &Default::default(),
            &modules,
            None,
        )
        .unwrap();
        let paths = Default::default();
        let model = Model::new(&modules, &names, &paths).unwrap();
        let arguments: Vec<_> = modules[0]
            .arena
            .exprs
            .iter()
            .enumerate()
            .filter_map(|(index, expression)| {
                let ExprKind::Call { type_args, .. } = expression.kind else {
                    return None;
                };
                Some((
                    ExprId(index as u32),
                    *type_args.as_slice(&modules[0].arena.generic_args).first()?,
                ))
            })
            .collect();
        let (expression, actual) = arguments[0];
        let mut state = EvalState::new(
            Domain::SOURCE_EXPAND,
            EvalProfile {
                heap_bytes: 16,
                ..Default::default()
            },
        );
        state.frames = Vec::with_capacity(1);
        let span = &modules[0].arena.exprs[expression.0 as usize].span;
        state.enter("caller", span).unwrap();
        state.alloc(16, span).unwrap();
        state.bind(
            "payload".to_owned(),
            ConstantValue::String("abcdefgh".to_owned()),
        );
        assert_eq!(
            model
                .explicit_const_argument(0, expression, actual, &mut state)
                .unwrap(),
            ConstantValue::Int(3)
        );
        assert_eq!(state.heap, 16);
        assert_eq!(
            state.frames.capacity(),
            1,
            "literal arguments must not allocate cloned caller frames outside the heap ledger"
        );
        assert_eq!(state.masked_runtime_base, None);
        for ((expression, actual), code) in arguments[1..].iter().zip([
            DiagnosticCode::InvalidExpression,
            DiagnosticCode::ComptimeBudget,
        ]) {
            let error = model
                .explicit_const_argument(0, *expression, *actual, &mut state)
                .expect_err("invalid arguments must restore the caller mask");
            assert_eq!(error.diagnostic(span).code(), code);
            assert_eq!(state.masked_runtime_base, None);
        }
    }
}

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
        let fixed = parameters
            .iter()
            .filter(|parameter| {
                !matches!(parameter.kind, GenericParamKind::Type { pack: true, .. })
            })
            .count();
        if !actuals.is_empty()
            && (actuals.len() < fixed || (fixed == parameters.len() && actuals.len() != fixed))
        {
            return Err(self.fail(module, expression, "comptime 泛型实参数量不符"));
        }
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
        let base = state.frame_bases.last().copied();
        let previous = std::mem::replace(&mut state.masked_runtime_base, base);
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
        state.masked_runtime_base = previous;
        result
    }
}
