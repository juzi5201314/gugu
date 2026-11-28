//! 闭世界接口表：声明身份、实现模式与关联项在同一张表上解析。
use super::super::ast::{
    AstRange, BinOp, BoundKind, GenericParam, GenericParamKind, Param, PatKind, PathId,
};
use super::model::{CallableId, ConstantValue, DefRef, Model, Ty, substitute};
use crate::{Diagnostic, DiagnosticCode, Span};
use std::collections::BTreeMap;
mod associated;
mod collect;
pub mod select;
mod validate;

pub(super) fn binary_operator_trait(op: BinOp) -> Option<(&'static str, &'static str, bool)> {
    Some(match op {
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
        BinOp::And | BinOp::Or => return None,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize)]
pub struct TraitRef {
    pub id: usize,
    pub arguments: Vec<Ty>,
}
impl TraitRef {
    pub fn substitute(&self, bindings: &BTreeMap<String, Ty>) -> Self {
        Self {
            id: self.id,
            arguments: self
                .arguments
                .iter()
                .map(|t| substitute(t, bindings))
                .collect(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct Obligation {
    pub ty: Ty,
    pub interface: TraitRef,
    pub span: Span,
}
#[derive(Clone)]
pub(super) enum MemberKind {
    Method {
        signature: Ty,
        receiver: bool,
        default: bool,
    },
    Type(Option<Ty>),
    Const {
        ty: Ty,
        value: Option<ConstantValue>,
    },
}
#[derive(Clone)]
pub(super) struct Member {
    pub(super) definition: Option<DefRef>,
    pub(super) kind: MemberKind,
}
pub struct Interface {
    pub(super) definition: Option<DefRef>,
    pub name: String,
    pub(super) parameters: Vec<String>,
    pub(super) members: BTreeMap<String, Member>,
    pub(super) unsafety: bool,
    pub(super) requirements: Vec<(Ty, TraitRef)>,
}
pub struct Implementation {
    pub definition: DefRef,
    pub(super) self_ty: Ty,
    pub(super) interface: Option<TraitRef>,
    pub(super) parameters: BTreeMap<String, Ty>,
    pub(super) obligations: Vec<Obligation>,
    pub(super) members: BTreeMap<String, Member>,
    pub(super) negative: bool,
}
#[derive(Clone, Copy)]
pub(super) enum Owner {
    Interface(usize),
    Implementation(usize),
}
#[derive(Default)]
pub struct Traits {
    pub interfaces: Vec<Interface>,
    pub implementations: Vec<Implementation>,
    // ItemId 在模块内稠密；索引只访问已有 AST 项。
    pub(super) owners: Vec<Vec<Option<Owner>>>,
    pub(super) projection_equalities: Vec<(Ty, Ty)>,
}
#[derive(Clone)]
pub struct Method {
    pub callable: Option<CallableId>,
    pub signature: Ty,
    pub receiver: bool,
    pub dynamic: bool,
    pub unsafety: bool,
    pub arguments: Vec<Ty>,
    pub implementation: Option<DefRef>,
    pub interface: Option<TraitRef>,
    pub member: Option<u32>,
    pub(super) obligations: Vec<Obligation>,
}

impl Model<'_> {
    /// 按接口成员下标取成员名；成员表为有序 map，下标即键序。
    pub fn interface_member_name(&self, interface: usize, member: u32) -> Option<String> {
        self.traits
            .interfaces
            .get(interface)?
            .members
            .keys()
            .nth(member as usize)
            .cloned()
    }

    /// 列出接口的全部方法成员名，供 dyn 擦除点物化 vtable 方法集。
    pub fn interface_method_members(&self, interface: usize) -> Vec<String> {
        self.traits
            .interfaces
            .get(interface)
            .map(|definition| {
                definition
                    .members
                    .iter()
                    .filter(|(_, member)| matches!(member.kind, MemberKind::Method { .. }))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn trait_error(&self, definition: DefRef, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(
            DiagnosticCode::InvalidDeclaration,
            message,
            Some(
                self.modules[definition.module].arena.items[definition.item.0 as usize]
                    .span
                    .clone(),
            ),
        )
    }
    pub(super) fn trait_ref(
        &self,
        module: usize,
        path: PathId,
        params: &BTreeMap<String, Ty>,
    ) -> Result<TraitRef, Diagnostic> {
        let parts = self.path(module, path);
        let span = &self.modules[module].arena.paths[path.0 as usize].span;
        let resolved = self.resolve(module, &parts).ok();
        let id = self
            .traits
            .interfaces
            .iter()
            .position(|t| match t.definition {
                Some(def) => Some(def) == resolved,
                None => parts.len() == 1 && t.name == parts[0],
            })
            .ok_or_else(|| self.error_at(span, format!("`{}` 不是 trait", parts.join("."))))?;
        let arena = &self.modules[module].arena;
        let segment = arena.paths[path.0 as usize]
            .segments
            .as_slice(&arena.segments)
            .last()
            .expect("非空路径");
        let arguments = segment
            .args
            .as_slice(&arena.generic_args)
            .iter()
            .map(|arg| self.form_arg(module, *arg, params, &mut Vec::new()))
            .collect::<Result<Vec<_>, _>>()?;
        if arguments.len() != self.traits.interfaces[id].parameters.len() {
            return Err(self.error_at(span, "trait 实参数量不符"));
        }
        Ok(TraitRef { id, arguments })
    }
    pub(super) fn generic_parameters(
        &self,
        module: usize,
        generics: AstRange<GenericParam>,
    ) -> BTreeMap<String, Ty> {
        generics
            .as_slice(&self.modules[module].arena.generic_params)
            .iter()
            .map(|g| self.generic_binding(module, &g.kind))
            .collect()
    }
    pub(super) fn generic_obligations(
        &self,
        module: usize,
        generics: AstRange<GenericParam>,
        params: &BTreeMap<String, Ty>,
    ) -> Result<Vec<Obligation>, Diagnostic> {
        let arena = &self.modules[module].arena;
        let mut obligations = Vec::new();
        for generic in generics.as_slice(&arena.generic_params) {
            if let GenericParamKind::Type { name, bounds, .. } = generic.kind {
                for bound in bounds.as_slice(&arena.bounds) {
                    if let BoundKind::Path(path) = bound.kind {
                        obligations.push(Obligation {
                            ty: params[self.name(module, name)].clone(),
                            interface: self.trait_ref(module, path, params)?,
                            span: bound.span.clone(),
                        });
                    }
                }
            }
        }
        self.expand_requirements(&mut obligations);
        Ok(obligations)
    }
    pub(super) fn expand_requirements(&self, obligations: &mut Vec<Obligation>) {
        let mut index = 0;
        while index < obligations.len() {
            let bound = &obligations[index];
            let definition = &self.traits.interfaces[bound.interface.id];
            let mut bindings: BTreeMap<_, _> = definition
                .parameters
                .iter()
                .cloned()
                .zip(bound.interface.arguments.iter().cloned())
                .collect();
            bindings.insert("Self".into(), bound.ty.clone());
            let span = bound.span.clone();
            for (ty, interface) in &definition.requirements {
                let ty = substitute(ty, &bindings);
                let interface = interface.substitute(&bindings);
                if !obligations
                    .iter()
                    .any(|bound| bound.ty == ty && bound.interface == interface)
                {
                    obligations.push(Obligation {
                        ty,
                        interface,
                        span: span.clone(),
                    });
                }
            }
            index += 1;
        }
    }
    pub(super) fn associated_scope(&self, module: usize, span: &Span) -> BTreeMap<String, Ty> {
        let arena = &self.modules[module].arena;
        let owner = arena
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.span.start() <= span.start() && item.span.end() >= span.end())
            .find_map(|(index, _)| {
                self.traits
                    .owners
                    .get(module)?
                    .get(index)
                    .copied()
                    .flatten()
            });
        match owner {
            Some(Owner::Interface(id)) => {
                let interface = &self.traits.interfaces[id];
                let mut params: BTreeMap<_, _> = interface
                    .parameters
                    .iter()
                    .map(|name| (name.clone(), Ty::Param(name.clone())))
                    .collect();
                params.insert("Self".into(), Ty::Param("Self".into()));
                for (name, member) in &interface.members {
                    if matches!(member.kind, MemberKind::Type(_)) {
                        params.insert(
                            format!("Self::{name}"),
                            Ty::Projection(
                                Box::new(Ty::Param("Self".into())),
                                TraitRef {
                                    id,
                                    arguments: interface
                                        .parameters
                                        .iter()
                                        .map(|name| Ty::Param(name.clone()))
                                        .collect(),
                                },
                                name.clone(),
                            ),
                        );
                    }
                }
                params
            }
            Some(Owner::Implementation(id)) => {
                let implementation = &self.traits.implementations[id];
                let mut params = implementation.parameters.clone();
                params.insert("Self".into(), implementation.self_ty.clone());
                for (name, member) in &implementation.members {
                    if let MemberKind::Type(Some(ty)) = &member.kind {
                        params.insert(format!("Self::{name}"), ty.clone());
                    }
                }
                params
            }
            None => BTreeMap::new(),
        }
    }
    pub(super) fn assumptions_at(
        &self,
        module: usize,
        span: &Span,
    ) -> Result<Vec<Obligation>, Diagnostic> {
        let arena = &self.modules[module].arena;
        let mut result = Vec::new();
        let params = self.parameters_at(module, span);
        for (index, item) in arena.items.iter().enumerate() {
            if item.span.start() > span.start() || item.span.end() < span.end() {
                continue;
            }
            match self.traits.owners[module][index] {
                Some(Owner::Interface(id)) => {
                    let obligation = Obligation {
                        ty: Ty::Param("Self".into()),
                        interface: TraitRef {
                            id,
                            arguments: self.traits.interfaces[id]
                                .parameters
                                .iter()
                                .map(|n| Ty::Param(n.clone()))
                                .collect(),
                        },
                        span: item.span.clone(),
                    };
                    if !result.iter().any(|o: &Obligation| {
                        o.ty == obligation.ty && o.interface == obligation.interface
                    }) {
                        result.push(obligation);
                    }
                }
                Some(Owner::Implementation(id)) => {
                    let implementation = &self.traits.implementations[id];
                    result.extend(implementation.obligations.iter().cloned());
                    if let Some(interface) = &implementation.interface {
                        result.push(Obligation {
                            ty: implementation.self_ty.clone(),
                            interface: interface.clone(),
                            span: item.span.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        for (index, function) in arena.fns.iter().enumerate() {
            if function.span.start() <= span.start() && function.span.end() >= span.end() {
                result.extend(self.generic_obligations(module, function.generics, &params)?);
                for id in self.apits(CallableId {
                    module,
                    function: index as u32,
                }) {
                    let definition = &self.opaques.definitions[id as usize];
                    result.extend(
                        self.form_bounds(
                            module,
                            definition.bounds,
                            &params,
                            &Ty::Param(Self::apit_name(id)),
                        )?
                        .traits,
                    );
                }
            }
        }
        self.expand_requirements(&mut result);
        Ok(result)
    }
    pub(super) fn is_receiver(&self, module: usize, param: &Param) -> bool {
        param.pat.is_some_and(
            |id| match self.modules[module].arena.pats[id.0 as usize].kind {
                PatKind::Ident(name) => self.name(module, name) == "self",
                _ => false,
            },
        )
    }
}
