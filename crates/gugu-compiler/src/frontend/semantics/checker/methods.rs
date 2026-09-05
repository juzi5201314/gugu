use super::super::super::ast::{AstRange, BinOp, ExprId, ExprKind, GenericArg, PathId};
use super::super::model::Ty;
use super::super::traits::{MemberKind, TraitRef};
use super::Checker;
use crate::{DiagnosticCode, Span};

impl Checker<'_, '_> {
    pub(super) fn method_call(
        &mut self,
        callee: ExprId,
        type_args: AstRange<GenericArg>,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let expression = &self.arena().exprs[callee.0 as usize];
        let (ty, interface, name, receiver) = match expression.kind {
            ExprKind::Field { base, name } => {
                let ty = self.expression(base, None);
                (
                    ty,
                    None,
                    self.model.name(self.module, name).to_owned(),
                    true,
                )
            }
            ExprKind::Path(path) => {
                let segments = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments);
                if segments.len() < 2 {
                    return None;
                }
                let last = segments.last().expect("非空路径");
                let name = self.model.name(self.module, last.name).to_owned();
                if !last.colon {
                    let mut ty = self.local(segments[0].name, true, &expression.span)?;
                    for segment in &segments[1..segments.len() - 1] {
                        ty = self.field(
                            &ty,
                            self.model.name(self.module, segment.name),
                            &segment.span,
                        );
                    }
                    (ty, None, name, true)
                } else {
                    let parts = self.model.path(self.module, path);
                    let prefix = &parts[..parts.len() - 1];
                    let definition = self.model.resolve(self.module, prefix).ok();
                    let interface =
                        self.model
                            .traits
                            .interfaces
                            .iter()
                            .enumerate()
                            .find(|(_, t)| match t.definition {
                                Some(def) => Some(def) == definition,
                                None => prefix.len() == 1 && t.name == prefix[0],
                            });
                    if let Some((id, _)) = interface {
                        let arguments = segments[segments.len() - 2]
                            .args
                            .as_slice(&self.arena().generic_args)
                            .iter()
                            .map(|arg| self.model.form_argument(self.module, *arg))
                            .collect::<Result<Vec<_>, _>>();
                        let arguments = match arguments {
                            Ok(args) => args,
                            Err(error) => {
                                self.errors.push(error);
                                return Some(Ty::Error);
                            }
                        };
                        if arguments.len() != self.model.traits.interfaces[id].parameters.len() {
                            self.error(
                                DiagnosticCode::InvalidExpression,
                                "UFCS trait 实参数量不符",
                                expression.span.clone(),
                            );
                            return Some(Ty::Error);
                        }
                        let interface = TraitRef { id, arguments };
                        let receiver = self.model.traits.interfaces[id]
                            .members
                            .get(&name)
                            .is_some_and(|member| {
                                matches!(member.kind, MemberKind::Method { receiver: true, .. })
                            });
                        let self_ty = if receiver {
                            args.first().map(|&first| self.expression(first, None))
                        } else {
                            expected.cloned().or_else(|| {
                                let mut candidates =
                                    self.model.traits.implementations.iter().filter(
                                        |implementation| {
                                            implementation.interface.as_ref() == Some(&interface)
                                                && implementation.parameters.is_empty()
                                                && !implementation.negative
                                        },
                                    );
                                let first = candidates.next()?;
                                candidates.next().is_none().then(|| first.self_ty.clone())
                            })
                        };
                        let Some(self_ty) = self_ty else {
                            self.error(
                                DiagnosticCode::InvalidExpression,
                                "trait 关联调用缺少可唯一推断的 Self",
                                expression.span.clone(),
                            );
                            return Some(Ty::Error);
                        };
                        return Some(self.invoke_method(
                            callee,
                            self_ty,
                            Some(interface),
                            &name,
                            type_args,
                            args,
                            receiver,
                            expected,
                        ));
                    }
                    let ty = self.type_head(path, &expression.span)?;
                    (ty, None, name, false)
                }
            }
            _ => return None,
        };
        Some(self.invoke_method(
            callee, ty, interface, &name, type_args, args, receiver, expected,
        ))
    }
    fn type_head(&mut self, path: PathId, span: &Span) -> Option<Ty> {
        let parts = self.model.path(self.module, path);
        let prefix = &parts[..parts.len() - 1];
        if prefix == ["Self"] {
            return self
                .model
                .parameters_at(self.module, span)
                .get("Self")
                .cloned();
        }
        if prefix.len() == 1
            && let Some(ty) = Ty::primitive(prefix[0])
        {
            return Some(ty);
        }
        let def = self.model.resolve(self.module, prefix).ok()?;
        let id = self
            .model
            .nominal
            .iter()
            .position(|n| n.definition == def)?;
        let arena = self.arena();
        let segments = arena.paths[path.0 as usize]
            .segments
            .as_slice(&arena.segments);
        let arguments = segments[segments.len() - 2]
            .args
            .as_slice(&arena.generic_args);
        let count = self.model.nominal[id].params.len();
        let mut types = Vec::with_capacity(count);
        if arguments.is_empty() {
            for _ in 0..count {
                types.push(self.fresh());
            }
        } else {
            for argument in arguments {
                match self.model.form_argument(self.module, *argument) {
                    Ok(ty) => types.push(ty),
                    Err(error) => {
                        self.errors.push(error);
                        types.push(Ty::Error);
                    }
                }
            }
            if types.len() != count {
                self.error(
                    DiagnosticCode::InvalidType,
                    "类型实参数量不符",
                    span.clone(),
                );
            }
        }
        Some(Ty::Named(id, types))
    }
    fn invoke_method(
        &mut self,
        callee: ExprId,
        self_ty: Ty,
        interface: Option<TraitRef>,
        name: &str,
        type_args: AstRange<GenericArg>,
        args: &[ExprId],
        receiver: bool,
        expected: Option<&Ty>,
    ) -> Ty {
        if interface.is_none()
            && receiver
            && self.model.fields(&self_ty).is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field.name == name && field.ty.signature().is_some())
            })
        {
            let ty = self.field(&self_ty, name, &self.arena().exprs[callee.0 as usize].span);
            return self.invoke(callee, ty, args.to_vec(), None, expected);
        }
        let span = &self.arena().exprs[callee.0 as usize].span;
        let assumptions = match self.model.assumptions_at(self.module, span) {
            Ok(assumptions) => assumptions,
            Err(error) => {
                self.errors.push(error);
                return Ty::Error;
            }
        };
        let self_ty = self.resolve(&self_ty);
        let mut target = &self_ty;
        let mut selected =
            self.model
                .method(self.module, target, interface.as_ref(), name, &assumptions);
        let mut dereferences = 0;
        while matches!(selected, Ok(None)) {
            let Ty::Ref(inner) = target else {
                break;
            };
            target = inner;
            dereferences += 1;
            selected =
                self.model
                    .method(self.module, target, interface.as_ref(), name, &assumptions);
        }
        let method = match selected {
            Ok(Some(method)) => method,
            Ok(None) => {
                self.error(
                    DiagnosticCode::InvalidType,
                    format!("{} 没有适用的方法 `{name}`", self.model.describe(target)),
                    span.clone(),
                );
                return Ty::Error;
            }
            Err(error) => {
                self.error(error.code(), error.message(), span.clone());
                return Ty::Error;
            }
        };
        if receiver && !method.receiver {
            self.error(
                DiagnosticCode::InvalidExpression,
                "无 self 的关联函数只能通过类型或 trait 的 :: 调用",
                span.clone(),
            );
            return Ty::Error;
        }
        let callable = match method.callable {
            Some(id) => self.instantiate_callable(
                Ty::Callable(id, Vec::new(), Box::new(method.signature)),
                type_args,
                span,
            ),
            None => method.signature,
        };
        let (parameters, _) = callable.signature().expect("方法具有签名");
        let actual = if receiver {
            let parameter = parameters.first().expect("接收者方法有首参");
            Some(match parameter {
                Ty::Ref(_) if !matches!(target, Ty::Ref(_)) => Ty::Ref(Box::new(target.clone())),
                _ => target.clone(),
            })
        } else {
            None
        };
        let trait_ufcs = interface.is_some()
            && matches!(
                self.arena().exprs[callee.0 as usize].kind,
                ExprKind::Path(_)
            );
        if actual.as_ref().is_some_and(|ty| matches!(ty, Ty::Ref(_))) {
            let receiver_expression = if trait_ufcs {
                args.first().copied()
            } else {
                match self.arena().exprs[callee.0 as usize].kind {
                    ExprKind::Field { base, .. } => Some(base),
                    _ => Some(callee),
                }
            };
            if let Some(expression) = receiver_expression {
                self.address_taken(expression);
            }
        }
        let args = if trait_ufcs && receiver {
            &args[1..]
        } else {
            args
        };
        self.expressions.push((callee, callable.clone()));
        let signature = callable
            .signature()
            .map(|(params, ret)| Ty::Function(params.to_vec(), Box::new(ret.clone())))
            .expect("方法签名");
        self.dispatches.push(super::super::output::Dispatch {
            expression: callee,
            callable: method.callable,
            implementation: method.implementation,
            interface: method.interface,
            member: method.member,
            self_ty: target.clone(),
            signature,
            dereferences,
            borrow: actual.as_ref().is_some_and(|ty| matches!(ty, Ty::Ref(_)))
                && !matches!(target, Ty::Ref(_)),
            implicit_receiver: receiver,
        });
        self.invoke(callee, callable, args.to_vec(), actual, expected)
    }
    pub(super) fn associated_constant(&mut self, path: PathId) -> Option<Ty> {
        match self.model.constant_member(self.module, path) {
            Ok(Some(member)) => match member.kind {
                MemberKind::Const { ty, .. } => Some(ty),
                _ => None,
            },
            Ok(None) => None,
            Err(error) => {
                self.errors.push(error);
                Some(Ty::Error)
            }
        }
    }
    pub(super) fn trait_operator(
        &mut self,
        id: ExprId,
        op: BinOp,
        left: &Ty,
        right: &Ty,
        span: &Span,
    ) -> Ty {
        let (interface, method, comparison) = match op {
            BinOp::Add => ("Add", "add", false),
            BinOp::Sub => ("Sub", "sub", false),
            BinOp::Mul => ("Mul", "mul", false),
            BinOp::Div => ("Div", "div", false),
            BinOp::Rem => ("Rem", "rem", false),
            BinOp::BitAnd => ("BitAnd", "bitand", false),
            BinOp::BitOr => ("BitOr", "bitor", false),
            BinOp::BitXor => ("BitXor", "bitxor", false),
            BinOp::Shl => ("Shl", "shl", false),
            BinOp::Shr => ("Shr", "shr", false),
            BinOp::Eq | BinOp::Ne => ("Eq", "eq", true),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => ("Ord", "cmp", true),
            _ => unreachable!("短路操作不进入 trait 分发"),
        };
        let interface_id = self
            .model
            .traits
            .interfaces
            .iter()
            .position(|t| t.definition.is_none() && t.name == interface)
            .expect("语言操作符 trait 已注册");
        let right = self.pattern_type(right);
        let interface = TraitRef {
            id: interface_id,
            arguments: if comparison {
                Vec::new()
            } else {
                vec![right.clone()]
            },
        };
        let assumptions = match self.model.assumptions_at(self.module, span) {
            Ok(assumptions) => assumptions,
            Err(error) => {
                self.errors.push(error);
                return Ty::Error;
            }
        };
        let selected = match self.model.method(
            self.module,
            &self.resolve(left),
            Some(&interface),
            method,
            &assumptions,
        ) {
            Ok(Some(method)) => method,
            Ok(None) => {
                self.error(
                    DiagnosticCode::InvalidType,
                    "操作符没有匹配的 trait 实现",
                    span.clone(),
                );
                return Ty::Error;
            }
            Err(error) => {
                self.error(error.code(), error.message(), span.clone());
                return Ty::Error;
            }
        };
        let (params, ret) = selected.signature.signature().expect("操作符方法签名");
        for (actual, expected) in [left, &right].into_iter().zip(params) {
            let actual = if matches!(expected, Ty::Ref(_)) {
                Ty::Ref(Box::new(actual.clone()))
            } else {
                actual.clone()
            };
            self.unify(&actual, expected, span);
        }
        if let Some(callable) = selected.callable {
            if let Some(definition) = self.model.function_definition(callable) {
                if !self.dependencies.contains(&definition) {
                    self.dependencies.push(definition);
                }
            }
            self.dispatches.push(super::super::output::Dispatch {
                expression: id,
                callable: Some(callable),
                implementation: selected.implementation,
                interface: selected.interface.clone(),
                member: selected.member,
                self_ty: self.resolve(left),
                signature: selected.signature.clone(),
                dereferences: 0,
                borrow: matches!(params.first(), Some(Ty::Ref(_))),
                implicit_receiver: true,
            });
        }
        if comparison { Ty::Bool } else { ret.clone() }
    }
}
