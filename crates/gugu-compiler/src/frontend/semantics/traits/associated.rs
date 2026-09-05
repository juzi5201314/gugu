use super::super::super::ast::{GenericParamKind, ItemKind, PathId};
use super::super::model::{DefRef, Model, Ty, substitute};
use super::select::{match_list, matches};
use super::{Member, MemberKind, Obligation, TraitRef};
use crate::Diagnostic;
use std::collections::BTreeMap;

impl Model<'_> {
    pub(in super::super) fn type_prefix(
        &self,
        module: usize,
        path: PathId,
        params: &BTreeMap<String, Ty>,
    ) -> Result<Option<Ty>, Diagnostic> {
        let parts = self.path(module, path);
        if parts.len() < 2 {
            return Ok(None);
        }
        let prefix = &parts[..parts.len() - 1];
        if prefix.len() == 1 {
            if let Some(ty) = params.get(prefix[0]) {
                return Ok(Some(ty.clone()));
            }
            if let Some(ty) = Ty::primitive(prefix[0]) {
                return Ok(Some(ty));
            }
        }
        let Ok(def) = self.resolve(module, prefix) else {
            return Ok(None);
        };
        let arena = &self.modules[module].arena;
        let segments = arena.paths[path.0 as usize]
            .segments
            .as_slice(&arena.segments);
        let args = segments[segments.len() - 2]
            .args
            .as_slice(&arena.generic_args)
            .iter()
            .map(|arg| self.form_arg(module, *arg, params, &mut Vec::new()))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some((id, nominal)) = self
            .nominal
            .iter()
            .enumerate()
            .find(|(_, n)| n.definition == def)
        {
            if nominal.params.len() != args.len() {
                return Err(self.error(module, "关联路径的类型实参数量不符"));
            }
            return Ok(Some(Ty::Named(id, args)));
        }
        if let ItemKind::TypeAlias {
            ty: Some(ty),
            generics,
        } = self.modules[def.module].arena.items[def.item.0 as usize].kind
        {
            let mut bindings = BTreeMap::new();
            let generics = generics.as_slice(&self.modules[def.module].arena.generic_params);
            if generics.len() != args.len() {
                return Err(self.error(module, "类型别名实参数量不符"));
            }
            for (generic, ty) in generics.iter().zip(args) {
                if let GenericParamKind::Type { name, .. } = generic.kind {
                    bindings.insert(self.name(def.module, name).to_owned(), ty);
                }
            }
            return self
                .form_inner(def.module, ty, &bindings, &mut vec![def])
                .map(Some);
        }
        Ok(None)
    }
    fn association_head(
        &self,
        module: usize,
        path: PathId,
        params: &BTreeMap<String, Ty>,
        assumptions: &[Obligation],
    ) -> Result<Option<(Ty, Option<TraitRef>)>, Diagnostic> {
        if let Some(ty) = self.type_prefix(module, path, params)? {
            return Ok(Some((ty, None)));
        }
        let parts = self.path(module, path);
        let prefix = &parts[..parts.len() - 1];
        let definition = self.resolve(module, prefix).ok();
        let Some((id, interface)) =
            self.traits
                .interfaces
                .iter()
                .enumerate()
                .find(|(_, interface)| match interface.definition {
                    Some(def) => Some(def) == definition,
                    None => prefix.len() == 1 && interface.name == prefix[0],
                })
        else {
            return Ok(None);
        };
        let arena = &self.modules[module].arena;
        let segments = arena.paths[path.0 as usize]
            .segments
            .as_slice(&arena.segments);
        let arguments = segments[segments.len() - 2]
            .args
            .as_slice(&arena.generic_args)
            .iter()
            .map(|arg| self.form_arg(module, *arg, params, &mut Vec::new()))
            .collect::<Result<Vec<_>, _>>()?;
        if arguments.len() != interface.parameters.len() {
            return Err(self.error(module, "关联路径的 trait 实参数量不符"));
        }
        let interface = TraitRef { id, arguments };
        let mut candidates: Vec<Ty> = assumptions
            .iter()
            .filter(|bound| bound.interface == interface)
            .map(|bound| bound.ty.clone())
            .collect();
        for implementation in &self.traits.implementations {
            let Some(pattern) = &implementation.interface else {
                continue;
            };
            let mut bindings = BTreeMap::new();
            if pattern.id != id
                || implementation.negative
                || !match_list(&pattern.arguments, &interface.arguments, &mut bindings)
                || implementation
                    .parameters
                    .keys()
                    .any(|name| !bindings.contains_key(name))
            {
                continue;
            }
            let candidate = substitute(&implementation.self_ty, &bindings);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        candidates.sort();
        candidates.dedup();
        match candidates.len() {
            1 => Ok(Some((candidates.pop().expect("唯一候选"), Some(interface)))),
            _ => Err(self.error(module, "trait 限定关联项不能唯一确定 Self")),
        }
    }
    pub(in super::super) fn projected_type(
        &self,
        module: usize,
        path: PathId,
        params: &BTreeMap<String, Ty>,
        stack: &mut Vec<DefRef>,
    ) -> Result<Option<Ty>, Diagnostic> {
        let arena = &self.modules[module].arena;
        let node = &arena.paths[path.0 as usize];
        let segments = node.segments.as_slice(&arena.segments);
        if segments.len() < 2 || !segments.last().expect("非空路径").colon {
            return Ok(None);
        }
        let name = self.name(module, segments.last().expect("非空路径").name);
        let mut assumptions = Vec::new();
        for function in &arena.fns {
            if function.span.start() <= node.span.start() && function.span.end() >= node.span.end()
            {
                assumptions.extend(self.generic_obligations(module, function.generics, params)?);
            }
        }
        let Some((base, qualified)) = self.association_head(module, path, params, &assumptions)?
        else {
            return Ok(None);
        };
        let mut candidates = Vec::new();
        let mut seen = Vec::new();
        for obligation in &assumptions {
            if obligation.ty == base
                && qualified
                    .as_ref()
                    .is_none_or(|interface| *interface == obligation.interface)
                && self.traits.interfaces[obligation.interface.id]
                    .members
                    .get(name)
                    .is_some_and(|m| matches!(m.kind, MemberKind::Type(_)))
            {
                let projection = Ty::Projection(
                    Box::new(base.clone()),
                    obligation.interface.clone(),
                    name.to_owned(),
                );
                if !candidates.contains(&projection) {
                    candidates.push(projection);
                }
                seen.push(obligation.interface.clone());
            }
        }
        for implementation in &self.traits.implementations {
            let mut bindings = BTreeMap::new();
            if !matches(&implementation.self_ty, &base, &mut bindings) {
                continue;
            }
            let Some(member) = implementation.members.get(name) else {
                continue;
            };
            if !matches!(member.kind, MemberKind::Type(_)) {
                continue;
            }
            if let Some(interface) = &implementation.interface {
                let interface = interface.substitute(&bindings);
                if qualified
                    .as_ref()
                    .is_some_and(|wanted| *wanted != interface)
                {
                    continue;
                }
                if seen.contains(&interface) {
                    continue;
                }
                seen.push(interface.clone());
                let Some((selected, mut matched)) =
                    self.select_impl(&base, &interface, &assumptions)?
                else {
                    continue;
                };
                let member =
                    if let Some(member) = self.traits.implementations[selected].members.get(name) {
                        member
                    } else {
                        let definition = &self.traits.interfaces[interface.id];
                        matched = definition
                            .parameters
                            .iter()
                            .cloned()
                            .zip(interface.arguments.iter().cloned())
                            .collect();
                        &definition.members[name]
                    };
                matched.insert("Self".into(), base.clone());
                let projection = self.resolve_type_member(member, &matched, stack)?;
                candidates.push(projection);
            } else if member.definition.is_some() {
                if qualified.is_some() {
                    continue;
                }
                bindings.insert("Self".into(), base.clone());
                candidates.push(self.resolve_type_member(member, &bindings, stack)?);
            }
        }
        match candidates.len() {
            0 => Ok(None),
            1 => Ok(candidates.pop()),
            _ => Err(self.error(module, "关联类型候选不唯一，需要 trait 约束消歧")),
        }
    }
    fn resolve_type_member(
        &self,
        member: &Member,
        bindings: &BTreeMap<String, Ty>,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        if let MemberKind::Type(Some(ty)) = &member.kind {
            return Ok(substitute(ty, bindings));
        }
        let definition = member
            .definition
            .ok_or_else(|| self.error(0, "关联类型缺少实现"))?;
        if stack.contains(&definition) {
            return Err(self.trait_error(definition, "关联类型形成循环"));
        }
        let ItemKind::TypeAlias { ty: Some(ty), .. } =
            self.modules[definition.module].arena.items[definition.item.0 as usize].kind
        else {
            return Err(self.trait_error(definition, "关联类型缺少具体定义"));
        };
        stack.push(definition);
        let result = self.form_inner(definition.module, ty, bindings, stack);
        stack.pop();
        result
    }
    pub(in super::super) fn constant_member(
        &self,
        module: usize,
        path: PathId,
    ) -> Result<Option<Member>, Diagnostic> {
        let arena = &self.modules[module].arena;
        let node = &arena.paths[path.0 as usize];
        let segments = node.segments.as_slice(&arena.segments);
        if segments.len() < 2 || !segments.last().expect("非空路径").colon {
            return Ok(None);
        }
        let name = self.name(module, segments.last().expect("非空路径").name);
        let params = self.parameters_at(module, &node.span);
        let assumptions = self.assumptions_at(module, &node.span)?;
        let Some((base, qualified)) = self.association_head(module, path, &params, &assumptions)?
        else {
            return Ok(None);
        };
        let mut candidates = Vec::new();
        let mut seen = Vec::new();
        for implementation in &self.traits.implementations {
            let mut bindings = BTreeMap::new();
            if !matches(&implementation.self_ty, &base, &mut bindings) {
                continue;
            }
            let selected = if let Some(interface) = &implementation.interface {
                let interface = interface.substitute(&bindings);
                if qualified
                    .as_ref()
                    .is_some_and(|wanted| *wanted != interface)
                {
                    continue;
                }
                if seen.contains(&interface) {
                    continue;
                }
                seen.push(interface.clone());
                let Some((id, matched)) = self.select_impl(&base, &interface, &assumptions)? else {
                    continue;
                };
                bindings = matched;
                &self.traits.implementations[id]
            } else {
                if qualified.is_some() {
                    continue;
                }
                implementation
            };
            if let Some(member) = selected.members.get(name) {
                if let MemberKind::Const { ty, value } = &member.kind {
                    candidates.push(Member {
                        definition: member.definition,
                        kind: MemberKind::Const {
                            ty: substitute(ty, &bindings),
                            value: value.clone(),
                        },
                    });
                }
            }
        }
        for Obligation { ty, interface, .. } in assumptions {
            if ty == base
                && candidates.is_empty()
                && qualified.as_ref().is_none_or(|wanted| *wanted == interface)
            {
                if let Some(member) = self.traits.interfaces[interface.id].members.get(name) {
                    if matches!(member.kind, MemberKind::Const { .. }) {
                        candidates.push(member.clone());
                    }
                }
            }
        }
        match candidates.len() {
            0 => Ok(None),
            1 => Ok(candidates.pop()),
            _ => Err(self.error(module, "关联常量候选不唯一")),
        }
    }
}
