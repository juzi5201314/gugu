use super::super::super::ast::{ItemKind, Visibility};
use super::super::model::{CallableId, Model, Ty, substitute};
use super::{Implementation, Member, MemberKind, Method, Obligation, TraitRef};
use crate::Diagnostic;
use std::collections::BTreeMap;

pub(in super::super) fn matches(
    pattern: &Ty,
    concrete: &Ty,
    bindings: &mut BTreeMap<String, Ty>,
) -> bool {
    if let Ty::Param(name) = pattern {
        if let Some(previous) = bindings.get(name) {
            return previous == concrete;
        }
        bindings.insert(name.clone(), concrete.clone());
        return true;
    }
    match (pattern, concrete) {
        (Ty::Ref(a), Ty::Ref(b))
        | (Ty::Ptr(a), Ty::Ptr(b))
        | (Ty::Slice(a), Ty::Slice(b))
        | (Ty::Option(a), Ty::Option(b))
        | (Ty::Chan(a), Ty::Chan(b))
        | (Ty::Join(a), Ty::Join(b)) => matches(a, b, bindings),
        (Ty::Array(a, n), Ty::Array(b, m)) => n == m && matches(a, b, bindings),
        (Ty::Tuple(a), Ty::Tuple(b)) => match_list(a, b, bindings),
        (Ty::Named(i, a), Ty::Named(j, b)) => i == j && match_list(a, b, bindings),
        (Ty::Opaque(i, a), Ty::Opaque(j, b)) => i == j && match_list(a, b, bindings),
        (Ty::Dyn(a), Ty::Dyn(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| a.id == b.id && match_list(&a.arguments, &b.arguments, bindings))
        }
        (Ty::Result(a, e), Ty::Result(b, f)) => matches(a, b, bindings) && matches(e, f, bindings),
        (Ty::Function(a, r), Ty::Function(b, s)) => {
            match_list(a, b, bindings) && matches(r, s, bindings)
        }
        (Ty::Projection(a, t, n), Ty::Projection(b, u, m)) => {
            t.id == u.id
                && n == m
                && matches(a, b, bindings)
                && match_list(&t.arguments, &u.arguments, bindings)
        }
        _ => pattern == concrete,
    }
}
pub(super) fn match_list(a: &[Ty], b: &[Ty], bindings: &mut BTreeMap<String, Ty>) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| matches(a, b, bindings))
}
pub(super) fn covers(
    general: &Implementation,
    specific: &Implementation,
) -> Option<BTreeMap<String, Ty>> {
    let mut bindings = BTreeMap::new();
    if !matches(&general.self_ty, &specific.self_ty, &mut bindings) {
        return None;
    }
    match (&general.interface, &specific.interface) {
        (Some(a), Some(b))
            if a.id == b.id && match_list(&a.arguments, &b.arguments, &mut bindings) =>
        {
            Some(bindings)
        }
        (None, None) => Some(bindings),
        _ => None,
    }
}
impl Model<'_> {
    pub(in super::super) fn require_trait(
        &self,
        ty: &Ty,
        interface: &TraitRef,
        assumptions: &[Obligation],
    ) -> Result<(), Diagnostic> {
        self.require_trait_inner(ty, interface, assumptions, &mut Vec::new())
    }
    fn require_trait_inner(
        &self,
        ty: &Ty,
        interface: &TraitRef,
        assumptions: &[Obligation],
        stack: &mut Vec<(Ty, TraitRef)>,
    ) -> Result<(), Diagnostic> {
        if assumptions
            .iter()
            .any(|a| a.ty == *ty && a.interface == *interface)
        {
            return Ok(());
        }
        if self.declared_trait_bounds(ty)?.contains(interface) {
            return Ok(());
        }
        if stack.iter().any(|(t, i)| t == ty && i == interface) {
            return Err(self.error(0, "trait 约束形成循环证明"));
        }
        stack.push((ty.clone(), interface.clone()));
        let selected = self.select_impl_inner(ty, interface, assumptions, stack);
        stack.pop();
        if selected?.is_some() || self.builtin_trait(ty, interface) {
            return Ok(());
        }
        Err(self.error(
            0,
            format!(
                "{} 不满足 trait {}",
                self.describe(ty),
                self.traits.interfaces[interface.id].name
            ),
        ))
    }
    pub(in super::super) fn select_impl(
        &self,
        ty: &Ty,
        interface: &TraitRef,
        assumptions: &[Obligation],
    ) -> Result<Option<(usize, BTreeMap<String, Ty>)>, Diagnostic> {
        self.select_impl_inner(ty, interface, assumptions, &mut Vec::new())
    }
    fn select_impl_inner(
        &self,
        ty: &Ty,
        interface: &TraitRef,
        assumptions: &[Obligation],
        stack: &mut Vec<(Ty, TraitRef)>,
    ) -> Result<Option<(usize, BTreeMap<String, Ty>)>, Diagnostic> {
        let mut selected: Option<(usize, BTreeMap<String, Ty>)> = None;
        for (id, implementation) in self.traits.implementations.iter().enumerate() {
            let Some(candidate) = &implementation.interface else {
                continue;
            };
            if candidate.id != interface.id {
                continue;
            }
            let mut bindings = BTreeMap::new();
            if !matches(&implementation.self_ty, ty, &mut bindings)
                || !match_list(&candidate.arguments, &interface.arguments, &mut bindings)
            {
                continue;
            }
            if implementation.obligations.iter().any(|bound| {
                self.require_trait_inner(
                    &substitute(&bound.ty, &bindings),
                    &bound.interface.substitute(&bindings),
                    assumptions,
                    stack,
                )
                .is_err()
            }) {
                continue;
            }
            if let Some((previous, _)) = &selected {
                let old = &self.traits.implementations[*previous];
                if covers(old, implementation).is_some() && covers(implementation, old).is_none() {
                    selected = Some((id, bindings));
                } else if covers(implementation, old).is_none() {
                    return Err(self.trait_error(
                        implementation.definition,
                        "trait 实现选择存在不可比较的重叠",
                    ));
                }
            } else {
                selected = Some((id, bindings));
            }
        }
        if let Some((id, _)) = selected.as_ref() {
            let implementation = &self.traits.implementations[*id];
            if implementation.negative {
                return Err(self.trait_error(
                    implementation.definition,
                    format!(
                        "否定 impl 禁止 {} 实现 {}",
                        self.describe(ty),
                        self.traits.interfaces[interface.id].name
                    ),
                ));
            }
        }
        Ok(selected)
    }
    pub(super) fn builtin_trait(&self, ty: &Ty, interface: &TraitRef) -> bool {
        let definition = &self.traits.interfaces[interface.id];
        if definition.definition.is_some() {
            return false;
        }
        let name = definition.name.as_str();
        if name == "Any" {
            return !matches!(ty, Ty::Never | Ty::Var(_) | Ty::Error | Ty::Param(_));
        }
        if matches!(name, "Clone" | "Eq" | "Ord" | "StableOrd" | "StableHash") {
            return match ty {
                Ty::Unit | Ty::Bool | Ty::Int { .. } | Ty::Char | Ty::String | Ty::TypeId => true,
                Ty::Float(_) => name == "Clone",
                Ty::Array(t, _) | Ty::Option(t) => self.builtin_trait(t, interface),
                Ty::Tuple(ts) => ts.iter().all(|t| self.builtin_trait(t, interface)),
                Ty::Result(t, e) => {
                    self.builtin_trait(t, interface) && self.builtin_trait(e, interface)
                }
                _ => false,
            };
        }
        if name == "Try" {
            return matches!(ty, Ty::Option(_) | Ty::Result(_, _));
        }
        if !matches!(interface.arguments.as_slice(), [rhs] if rhs == ty) {
            return false;
        }
        match ty {
            Ty::Int { .. } => matches!(
                name,
                "Add"
                    | "Sub"
                    | "Mul"
                    | "Div"
                    | "Rem"
                    | "BitAnd"
                    | "BitOr"
                    | "BitXor"
                    | "Shl"
                    | "Shr"
            ),
            Ty::Float(_) => matches!(name, "Add" | "Sub" | "Mul" | "Div" | "Rem"),
            Ty::String => name == "Add",
            _ => false,
        }
    }
    pub(in super::super) fn normalize(
        &self,
        ty: &Ty,
        assumptions: &[Obligation],
    ) -> Result<Ty, Diagnostic> {
        self.normalize_inner(ty, assumptions, &mut Vec::new())
    }
    fn normalize_inner(
        &self,
        ty: &Ty,
        assumptions: &[Obligation],
        stack: &mut Vec<Ty>,
    ) -> Result<Ty, Diagnostic> {
        let recur = |ty: &Ty, stack: &mut Vec<Ty>| self.normalize_inner(ty, assumptions, stack);
        Ok(match ty {
            Ty::Projection(base, interface, name) => {
                for (pattern, value) in &self.traits.projection_equalities {
                    let mut bindings = BTreeMap::new();
                    if matches(pattern, ty, &mut bindings) {
                        return recur(&substitute(value, &bindings), stack);
                    }
                }
                if stack.contains(ty) {
                    return Err(self.error(0, "关联类型形成循环"));
                }
                stack.push(ty.clone());
                let result = if let Some((id, bindings)) =
                    self.select_impl(base, interface, assumptions)?
                {
                    let Some(Member {
                        kind: MemberKind::Type(Some(value)),
                        ..
                    }) = self.traits.implementations[id].members.get(name)
                    else {
                        return Err(self.error(0, "impl 缺少关联类型"));
                    };
                    recur(&substitute(value, &bindings), stack)?
                } else if self.builtin_trait(base, interface) {
                    match (&**base, name.as_str()) {
                        (_, "Output") => (**base).clone(),
                        (Ty::Option(value) | Ty::Result(value, _), "Value") => (**value).clone(),
                        (Ty::Option(_), "Error") => Ty::Unit,
                        (Ty::Result(_, error), "Error") => (**error).clone(),
                        _ => return Err(self.error(0, "语言 trait 没有此关联类型")),
                    }
                } else {
                    ty.clone()
                };
                stack.pop();
                result
            }
            Ty::Ref(t) => Ty::Ref(Box::new(recur(t, stack)?)),
            Ty::Ptr(t) => Ty::Ptr(Box::new(recur(t, stack)?)),
            Ty::Slice(t) => Ty::Slice(Box::new(recur(t, stack)?)),
            Ty::Array(t, n) => Ty::Array(Box::new(recur(t, stack)?), *n),
            Ty::Option(t) => Ty::Option(Box::new(recur(t, stack)?)),
            Ty::Chan(t) => Ty::Chan(Box::new(recur(t, stack)?)),
            Ty::Join(t) => Ty::Join(Box::new(recur(t, stack)?)),
            Ty::Result(t, e) => Ty::Result(Box::new(recur(t, stack)?), Box::new(recur(e, stack)?)),
            Ty::Tuple(ts) => Ty::Tuple(
                ts.iter()
                    .map(|t| recur(t, stack))
                    .collect::<Result<_, _>>()?,
            ),
            Ty::Named(id, ts) => Ty::Named(
                *id,
                ts.iter()
                    .map(|t| recur(t, stack))
                    .collect::<Result<_, _>>()?,
            ),
            Ty::Function(ts, ret) => Ty::Function(
                ts.iter()
                    .map(|t| recur(t, stack))
                    .collect::<Result<_, _>>()?,
                Box::new(recur(ret, stack)?),
            ),
            Ty::Callable(id, arguments, signature) => Ty::Callable(
                *id,
                arguments
                    .iter()
                    .map(|ty| recur(ty, stack))
                    .collect::<Result<_, _>>()?,
                Box::new(recur(signature, stack)?),
            ),
            Ty::Opaque(id, arguments) => Ty::Opaque(
                *id,
                arguments
                    .iter()
                    .map(|ty| recur(ty, stack))
                    .collect::<Result<_, _>>()?,
            ),
            Ty::Dyn(interfaces) => Ty::Dyn(
                interfaces
                    .iter()
                    .map(|interface| {
                        Ok(TraitRef {
                            id: interface.id,
                            arguments: interface
                                .arguments
                                .iter()
                                .map(|ty| recur(ty, stack))
                                .collect::<Result<_, Diagnostic>>()?,
                        })
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            _ => ty.clone(),
        })
    }
    pub(in super::super) fn method(
        &self,
        module: usize,
        ty: &Ty,
        interface: Option<&TraitRef>,
        name: &str,
        assumptions: &[Obligation],
    ) -> Result<Option<Method>, Diagnostic> {
        if let Some(method) = self.dynamic_method(module, ty, interface, name)? {
            return Ok(Some(method));
        }
        if interface.is_none() {
            let mut inherent = Vec::new();
            for implementation in &self.traits.implementations {
                if implementation.interface.is_some() {
                    continue;
                }
                let mut bindings = BTreeMap::new();
                if matches(&implementation.self_ty, ty, &mut bindings) {
                    if implementation.obligations.iter().any(|bound| {
                        self.require_trait(
                            &substitute(&bound.ty, &bindings),
                            &bound.interface.substitute(&bindings),
                            assumptions,
                        )
                        .is_err()
                    }) {
                        continue;
                    }
                    if let Some(member) = implementation.members.get(name) {
                        inherent.push(self.resolved_method(
                            module,
                            member,
                            &bindings,
                            assumptions,
                            false,
                        )?);
                    }
                }
            }
            if inherent.len() == 1 {
                return Ok(inherent.pop());
            }
            if inherent.len() > 1 {
                return Err(self.error(module, "固有方法候选不唯一"));
            }
        }
        let mut interfaces = Vec::new();
        let declared = self.declared_trait_bounds(ty)?;
        if let Some(interface) = interface {
            interfaces.push(interface.clone());
        } else {
            interfaces.extend(declared.iter().cloned());
            for (id, definition) in
                self.traits
                    .interfaces
                    .iter()
                    .enumerate()
                    .filter(|(_, definition)| {
                        definition.definition.is_none() && definition.members.contains_key(name)
                    })
            {
                let arguments = if definition.parameters.is_empty() {
                    Vec::new()
                } else {
                    vec![ty.clone()]
                };
                let interface = TraitRef { id, arguments };
                if self.builtin_trait(ty, &interface) {
                    interfaces.push(interface);
                }
            }
            for bound in assumptions.iter().filter(|bound| bound.ty == *ty) {
                interfaces.push(bound.interface.clone());
            }
            for implementation in &self.traits.implementations {
                if let Some(interface) = &implementation.interface {
                    let mut bindings = BTreeMap::new();
                    if matches(&implementation.self_ty, ty, &mut bindings) {
                        interfaces.push(interface.substitute(&bindings));
                    }
                }
            }
        }
        interfaces.sort();
        interfaces.dedup();
        let mut methods = Vec::new();
        let mut rejected = None;
        for interface in interfaces {
            let previous = methods.len();
            let definition = &self.traits.interfaces[interface.id];
            if definition.definition.is_some_and(|def| {
                def.module != module
                    && self.modules[def.module].arena.items[def.item.0 as usize].visibility
                        != Visibility::Pub
            }) {
                continue;
            }
            let Some(member) = definition.members.get(name) else {
                continue;
            };
            let mut bindings: BTreeMap<_, _> = definition
                .parameters
                .iter()
                .cloned()
                .zip(interface.arguments.iter().cloned())
                .collect();
            bindings.insert("Self".into(), ty.clone());
            match self.select_impl(ty, &interface, assumptions) {
                Ok(Some((id, matched))) => {
                    if let Some(member) = self.traits.implementations[id].members.get(name) {
                        methods.push(self.resolved_method(
                            module,
                            member,
                            &matched,
                            assumptions,
                            true,
                        )?);
                    } else {
                        methods.push(self.resolved_method(
                            module,
                            member,
                            &bindings,
                            assumptions,
                            true,
                        )?);
                    }
                    methods.last_mut().expect("刚刚插入方法").implementation =
                        Some(self.traits.implementations[id].definition);
                }
                Ok(None)
                    if assumptions
                        .iter()
                        .any(|b| b.ty == *ty && b.interface == interface)
                        || declared.contains(&interface)
                        || self.builtin_trait(ty, &interface) =>
                {
                    methods.push(self.resolved_method(
                        module,
                        member,
                        &bindings,
                        assumptions,
                        true,
                    )?);
                }
                Err(error) => rejected = Some(error),
                Ok(None) => {}
            }
            if methods.len() != previous {
                let method = methods.last_mut().expect("刚刚插入方法");
                method.interface = Some(interface.clone());
                method.member = Some(
                    definition
                        .members
                        .keys()
                        .position(|key| key == name)
                        .expect("已有成员") as u32,
                );
            }
        }
        match methods.len() {
            1 => Ok(methods.pop()),
            0 => rejected.map_or(Ok(None), Err),
            _ => Err(self.error(module, "trait 方法候选不唯一，需要 UFCS 消歧")),
        }
    }
    pub(in super::super) fn resolved_method(
        &self,
        module: usize,
        member: &Member,
        bindings: &BTreeMap<String, Ty>,
        assumptions: &[Obligation],
        trait_member: bool,
    ) -> Result<Method, Diagnostic> {
        let MemberKind::Method {
            signature,
            receiver,
            ..
        } = &member.kind
        else {
            return Err(self.error(module, "关联项不是函数"));
        };
        let callable = member.definition.and_then(|def| {
            if let ItemKind::Function(id) =
                self.modules[def.module].arena.items[def.item.0 as usize].kind
            {
                Some(CallableId {
                    module: def.module,
                    function: id.0,
                })
            } else {
                None
            }
        });
        if !trait_member
            && member.definition.is_some_and(|def| {
                def.module != module
                    && self.modules[def.module].arena.items[def.item.0 as usize].visibility
                        != Visibility::Pub
            })
        {
            return Err(self.error(module, "不能访问私有固有方法"));
        }
        let implementation = member.definition.and_then(|def| {
            match self.traits.owners[def.module][def.item.0 as usize] {
                Some(super::Owner::Implementation(id)) => {
                    Some(self.traits.implementations[id].definition)
                }
                _ => None,
            }
        });
        let arguments = callable
            .map(|id| {
                self.callable_context(id)
                    .into_values()
                    .map(|ty| substitute(&ty, bindings))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Method {
            callable,
            arguments,
            signature: self.normalize(&substitute(signature, bindings), assumptions)?,
            receiver: *receiver,
            implementation,
            interface: None,
            member: None,
            dynamic: false,
        })
    }
}
