use super::super::super::ast::{AstRange, ItemKind, PathId};
use super::super::model::{Model, Ty, substitute};
use super::super::traits::{MemberKind, Method, TraitRef};
use super::collect::contains_self_or_impl;
use crate::Diagnostic;
use std::collections::BTreeMap;

impl Model<'_> {
    pub(in super::super) fn form_dyn(
        &self,
        module: usize,
        paths: AstRange<PathId>,
        params: &BTreeMap<String, Ty>,
    ) -> Result<Ty, Diagnostic> {
        let mut interfaces = Vec::new();
        for &path in paths.as_slice(&self.modules[module].arena.path_ids) {
            let interface = self.trait_ref(module, path, params)?;
            self.object_safe(interface.id)?;
            interfaces.push(interface);
        }
        interfaces.sort();
        interfaces.dedup();
        Ok(Ty::Dyn(interfaces))
    }
    pub(in super::super) fn object_safe(&self, id: usize) -> Result<(), Diagnostic> {
        let interface = &self.traits.interfaces[id];
        let error = || {
            self.error(
                interface.definition.map_or(0, |def| def.module),
                format!("trait {} 不满足对象安全条件", interface.name),
            )
        };
        if interface.name == "Fn" && interface.definition.is_none() {
            return Err(error());
        }
        for member in interface.members.values() {
            if !matches!(member.kind, MemberKind::Method { .. }) {
                return Err(error());
            }
            if let Some(definition) = member.definition {
                let arena = &self.modules[definition.module].arena;
                let ItemKind::Function(id) = arena.items[definition.item.0 as usize].kind else {
                    return Err(error());
                };
                let function = &arena.fns[id.0 as usize];
                let parameters = function.params.as_slice(&arena.params);
                if function.generics.len != 0
                    || parameters
                        .first()
                        .is_none_or(|param| !self.is_receiver(definition.module, param))
                    || parameters.iter().skip(1).any(|param| {
                        param
                            .ty
                            .is_some_and(|ty| contains_self_or_impl(self, definition.module, ty))
                    })
                    || function
                        .return_ty
                        .is_some_and(|ty| contains_self_or_impl(self, definition.module, ty))
                {
                    return Err(error());
                }
            } else if let MemberKind::Method {
                signature,
                receiver,
                ..
            } = &member.kind
            {
                let (parameters, result) = signature.signature().expect("语言方法具有签名");
                if !receiver || parameters.iter().skip(1).any(uses_self) || uses_self(result) {
                    return Err(error());
                }
            }
        }
        Ok(())
    }
    pub(in super::super) fn dynamic_method(
        &self,
        module: usize,
        ty: &Ty,
        requested: Option<&TraitRef>,
        name: &str,
    ) -> Result<Option<Method>, Diagnostic> {
        let Ty::Dyn(interfaces) = ty else {
            return Ok(None);
        };
        let mut selected = None;
        for interface in interfaces {
            if requested.is_some_and(|wanted| wanted != interface) {
                continue;
            }
            let definition = &self.traits.interfaces[interface.id];
            if definition.definition.is_some_and(|definition| {
                definition.module != module
                    && self.modules[definition.module].arena.items[definition.item.0 as usize]
                        .visibility
                        != super::super::super::ast::Visibility::Pub
            }) {
                continue;
            }
            let Some(member) = definition.members.get(name) else {
                continue;
            };
            if selected.is_some() {
                return Err(self.error(module, "动态接口方法候选不唯一，需要 UFCS 消歧"));
            }
            let mut bindings: BTreeMap<_, _> = definition
                .parameters
                .iter()
                .cloned()
                .zip(interface.arguments.iter().cloned())
                .collect();
            bindings.insert("Self".into(), ty.clone());
            let MemberKind::Method {
                signature,
                receiver,
                ..
            } = &member.kind
            else {
                return Err(self.error(module, "动态关联项不是方法"));
            };
            selected = Some(Method {
                callable: None,
                arguments: Vec::new(),
                implementation: None,
                dynamic: true,
                unsafety: self.member_is_unsafe(member),
                receiver: *receiver,
                signature: self.normalize(&substitute(signature, &bindings), &[])?,
                interface: Some(interface.clone()),
                member: Some(
                    definition
                        .members
                        .keys()
                        .position(|key| key == name)
                        .expect("已有成员") as u32,
                ),
            });
        }
        Ok(selected)
    }
}
fn uses_self(ty: &Ty) -> bool {
    match ty {
        Ty::Param(name) => name == "Self",
        Ty::Opaque(..) => true,
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => uses_self(t),
        Ty::Tuple(types) | Ty::Named(_, types) => types.iter().any(uses_self),
        Ty::Function(params, ret) => params.iter().any(uses_self) || uses_self(ret),
        Ty::Result(t, e) => uses_self(t) || uses_self(e),
        Ty::Projection(base, _, _) => uses_self(base),
        _ => false,
    }
}
