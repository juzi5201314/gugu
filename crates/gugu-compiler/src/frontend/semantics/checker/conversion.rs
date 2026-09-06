use super::super::model::MemoryIntrinsic;
use super::super::output::MemoryOperation;
use super::*;

impl Checker<'_, '_> {
    pub(super) fn conversion(&mut self, callee: ExprId, args: &[ExprId]) -> Option<Ty> {
        let target = match self.conversion_target(callee)? {
            Ok(target) => target,
            Err(error) => {
                self.errors.push(error);
                return Some(Ty::Error);
            }
        };
        let span = &self.arena().exprs[callee.0 as usize].span;
        let [value] = args else {
            self.error(
                DiagnosticCode::InvalidExpression,
                "类型转换需要一个实参",
                span.clone(),
            );
            return Some(Ty::Error);
        };
        let actual = self.expression(*value, None);
        let actual = self.resolve(&actual);
        let pointer = matches!(target, Ty::Ptr(_) | Ty::Ref(_)) || matches!(actual, Ty::Ptr(_));
        if pointer {
            if !self.pointer_conversion(&actual, &target, span) {
                return Some(Ty::Error);
            }
            self.memory_operations.push(MemoryOperation {
                expression: callee,
                kind: MemoryIntrinsic::PointerCast,
                value: self.resolve(&actual),
                result: target.clone(),
                arguments: args.to_vec(),
            });
        } else if !(self.is_number(&actual) || matches!(actual, Ty::Char))
            || !matches!(target, Ty::Int { .. } | Ty::Float(_) | Ty::Char)
        {
            self.error(
                DiagnosticCode::InvalidType,
                "只支持数值标量之间的显式转换",
                span.clone(),
            );
        } else if self.number_kind(&actual) == inference::NumberKind::Float
            && let Ty::Int { signed, bits } = target
        {
            self.record_check(
                callee,
                super::super::output::CheckKind::FloatToInt {
                    signed,
                    bits,
                    value: *value,
                },
            );
        } else if target == Ty::Char {
            self.record_check(
                callee,
                super::super::output::CheckKind::UnicodeScalar { value: *value },
            );
        }
        if !pointer {
            self.memory_operations.push(MemoryOperation {
                expression: callee,
                kind: MemoryIntrinsic::ScalarCast,
                value: actual.clone(),
                result: target.clone(),
                arguments: args.to_vec(),
            });
        }
        self.expressions
            .push((callee, Ty::Function(vec![actual], Box::new(target.clone()))));
        Some(target)
    }

    fn conversion_target(&self, expression: ExprId) -> Option<Result<Ty, Diagnostic>> {
        match self.arena().exprs[expression.0 as usize].kind {
            ExprKind::Paren(inner) => self.conversion_target(inner),
            ExprKind::TypeCallee(ty) if !self.type_is_value(ty) => {
                Some(self.model.form(self.module, ty))
            }
            ExprKind::Path(path) => {
                let parts = self.model.path(self.module, path);
                if parts.len() == 1
                    && let Some(ty) = Ty::primitive(parts[0])
                {
                    return Some(Ok(ty));
                }
                let first = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments)[0]
                    .name;
                if self.state.names.contains_key(&first) {
                    return None;
                }
                let definition = self.model.resolve(self.module, &parts).ok()?;
                if !matches!(
                    self.model.modules[definition.module].arena.items[definition.item.0 as usize]
                        .kind,
                    ItemKind::TypeAlias { .. }
                ) {
                    return None;
                }
                let ty = self
                    .model
                    .form_argument(self.module, GenericArg::Expr(expression));
                match &ty {
                    Ok(Ty::Int { .. } | Ty::Float(_) | Ty::Char | Ty::Ptr(_) | Ty::Ref(_))
                    | Err(_) => Some(ty),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn pointer_conversion(&mut self, actual: &Ty, target: &Ty, span: &Span) -> bool {
        if let (Ty::Ptr(from) | Ty::Ref(from), Ty::Ref(to)) = (actual, target)
            && let (Ty::Array(element, _), Ty::Slice(wanted)) = (&**from, &**to)
        {
            self.unify(element, wanted, span);
            if matches!(actual, Ty::Ptr(_)) && self.unsafe_depth == 0 {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "从数组裸指针构造切片引用必须处于 unsafe 块中",
                    span.clone(),
                );
            }
            return true;
        }
        let valid = match (actual, target) {
            (Ty::Ptr(_) | Ty::Ref(_), Ty::Ptr(_)) => true,
            (integer, Ty::Ptr(_)) if self.is_integer(integer) => {
                self.unify(
                    integer,
                    &Ty::Int {
                        signed: false,
                        bits: 64,
                    },
                    span,
                );
                true
            }
            (
                Ty::Ptr(_),
                Ty::Int {
                    signed: false,
                    bits: 64,
                },
            ) => true,
            (Ty::Ptr(from) | Ty::Ref(from), Ty::Ref(to)) => {
                if matches!(**to, Ty::Slice(_))
                    && !(matches!(**from, Ty::Array(..))
                        || matches!(actual, Ty::Ref(inner) if matches!(**inner, Ty::Slice(_))))
                {
                    self.error(
                        DiagnosticCode::InvalidType,
                        "构造切片引用需要已有长度或固定数组长度，裸地址不提供长度",
                        span.clone(),
                    );
                    return false;
                }
                let safe = matches!(actual, Ty::Ref(_)) && from == to;
                if !safe && self.unsafe_depth == 0 {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "从裸指针或不同类型视图构造引用必须处于 unsafe 块中",
                        span.clone(),
                    );
                }
                true
            }
            _ => false,
        };
        if !valid {
            self.error(
                DiagnosticCode::InvalidType,
                "指针转换只接受引用、裸指针或 uint 地址",
                span.clone(),
            );
        }
        valid
    }

    pub(super) fn type_is_value(&self, ty: TyId) -> bool {
        match self.arena().tys[ty.0 as usize].kind {
            TyKind::Ptr(inner) | TyKind::Ref(inner) => self.type_is_value(inner),
            TyKind::Path(path) => {
                let first = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments)[0]
                    .name;
                self.state.names.contains_key(&first)
                    || self
                        .model
                        .resolve(self.module, &self.model.path(self.module, path))
                        .is_ok_and(|definition| {
                            matches!(
                                self.model.modules[definition.module].arena.items
                                    [definition.item.0 as usize]
                                    .kind,
                                ItemKind::Function(_)
                                    | ItemKind::Static { .. }
                                    | ItemKind::Const { .. }
                            )
                        })
            }
            _ => false,
        }
    }

    pub(super) fn type_as_value(&mut self, expression: ExprId, ty: TyId) -> Ty {
        let span = &self.arena().tys[ty.0 as usize].span;
        match self.arena().tys[ty.0 as usize].kind {
            TyKind::Path(path) => self.path_value(expression, path, true, None, AstRange::empty()),
            TyKind::Ptr(inner) => {
                let value = self.type_as_value(expression, inner);
                self.dereference(value, span)
            }
            TyKind::Ref(inner) => {
                let value = match self.arena().tys[inner.0 as usize].kind {
                    TyKind::Path(path) => {
                        let value = self.path_place(expression, path, true);
                        self.address_taken_path(path);
                        self.borrow_path_check(expression, path, &value);
                        value
                    }
                    TyKind::Ptr(_) => self.type_as_value(expression, inner),
                    _ => {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "取引用需要可赋值位置",
                            span.clone(),
                        );
                        Ty::Error
                    }
                };
                Ty::Ref(Box::new(value))
            }
            _ => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "类型表达式不能作为运行时值",
                    span.clone(),
                );
                Ty::Error
            }
        }
    }
    pub(super) fn type_value_path(&self, ty: TyId) -> Option<PathId> {
        match self.arena().tys[ty.0 as usize].kind {
            TyKind::Path(path) => Some(path),
            TyKind::Ptr(inner) | TyKind::Ref(inner) => self.type_value_path(inner),
            _ => None,
        }
    }
}
