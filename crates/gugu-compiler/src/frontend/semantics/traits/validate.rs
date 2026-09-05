use super::super::super::ast::{BoundKind, FnBody, GenericParamKind, ItemKind, TyKind, Visibility};
use super::super::model::{Model, Ty, substitute};
use super::select::covers;
use super::{Implementation, Member, MemberKind, Obligation};
use crate::Diagnostic;
use std::collections::BTreeMap;

impl Model<'_> {
    pub(in super::super) fn validate_traits(&self) -> Result<(), Diagnostic> {
        for implementation in &self.traits.implementations {
            self.validate_implementation(implementation)?;
        }
        for (index, a) in self.traits.implementations.iter().enumerate() {
            for b in &self.traits.implementations[index + 1..] {
                if a.interface.as_ref().map(|t| t.id) != b.interface.as_ref().map(|t| t.id)
                    || !overlap(a, b)
                {
                    continue;
                }
                if a.interface.is_none() {
                    if a.members.keys().any(|name| b.members.contains_key(name)) {
                        return Err(
                            self.trait_error(b.definition, "重叠固有 impl 不能声明同名关联项")
                        );
                    }
                    continue;
                }
                let ab = covers(a, b);
                let ba = covers(b, a);
                let (general, specific, bindings) = match (ab, ba) {
                    (Some(bindings), None) => (a, b, bindings),
                    (None, Some(bindings)) => (b, a, bindings),
                    _ => {
                        return Err(
                            self.trait_error(b.definition, "impl 重叠无法形成严格具体性顺序")
                        );
                    }
                };
                if general.negative && !specific.negative {
                    return Err(self.trait_error(
                        specific.definition,
                        "肯定 impl 不能覆盖否定 impl 的禁止范围",
                    ));
                }
                if specific.negative {
                    continue;
                }
                for (name, member) in &general.members {
                    if let Some(other) = specific.members.get(name) {
                        let bindings = self.method_bindings(member, other, &bindings)?;
                        self.compare_member_kinds(
                            &member.kind,
                            &other.kind,
                            &bindings,
                            &specific.obligations,
                        )
                        .map_err(|_| {
                            self.trait_error(
                                specific.definition,
                                format!("特化不能改变 `{name}` 的签名、关联类型或关联常量"),
                            )
                        })?;
                    }
                }
            }
        }
        Ok(())
    }
    fn validate_implementation(&self, implementation: &Implementation) -> Result<(), Diagnostic> {
        let def = implementation.definition;
        let item = &self.modules[def.module].arena.items[def.item.0 as usize];
        let ItemKind::Impl { unsafety, .. } = item.kind else {
            unreachable!()
        };
        if implementation.negative && !implementation.members.is_empty() {
            return Err(self.trait_error(def, "否定 impl 的体必须为空"));
        }
        let Some(interface) = &implementation.interface else {
            if implementation.negative
                || self.def_module(&implementation.self_ty) != Some(def.module)
            {
                return Err(
                    self.trait_error(def, "固有 impl 必须位于类型定义模块，且不能是否定实现")
                );
            }
            for member in implementation.members.values() {
                if let MemberKind::Method { default: false, .. } = member.kind {
                    return Err(self.trait_error(def, "固有方法必须有函数体"));
                }
            }
            return Ok(());
        };
        let definition = &self.traits.interfaces[interface.id];
        if definition.definition.is_none() {
            if self.builtin_trait(&implementation.self_ty, interface) {
                return Err(self.trait_error(def, "用户 impl 不能覆盖编译器提供的语言实现"));
            }
            if matches!(definition.name.as_str(), "Any" | "Fn") {
                return Err(self.trait_error(def, "用户不能手写 Any 或 Fn 的肯定或否定 impl"));
            }
            if definition.name == "Clone"
                && matches!(implementation.self_ty, Ty::Chan(_) | Ty::Join(_))
                && !implementation.negative
            {
                return Err(self.trait_error(def, "语言否定 impl 禁止 chan 和 Join 实现 Clone"));
            }
        }
        if !implementation.negative && unsafety != definition.unsafety {
            return Err(self.trait_error(def, "unsafe trait 与 unsafe impl 必须一致"));
        }
        if implementation.negative {
            return Ok(());
        }
        let mut bindings: BTreeMap<_, _> = definition
            .parameters
            .iter()
            .cloned()
            .zip(interface.arguments.iter().cloned())
            .collect();
        bindings.insert("Self".into(), implementation.self_ty.clone());
        for (name, member) in &definition.members {
            if let Some(actual) = implementation.members.get(name) {
                if actual.definition.is_some_and(|d| {
                    self.modules[d.module].arena.items[d.item.0 as usize].visibility
                        != Visibility::Private
                }) {
                    return Err(self.trait_error(def, "trait impl 不能改写关联项可见性"));
                }
                let bindings = self.method_bindings(member, actual, &bindings)?;
                match (&member.kind, &actual.kind) {
                    (MemberKind::Type(None), MemberKind::Type(Some(_))) => {}
                    (
                        MemberKind::Const { ty, .. },
                        MemberKind::Const {
                            ty: actual,
                            value: Some(_),
                        },
                    ) if self
                        .normalize(&substitute(ty, &bindings), &implementation.obligations)?
                        == *actual => {}
                    _ => self
                        .compare_member_kinds(
                            &member.kind,
                            &actual.kind,
                            &bindings,
                            &implementation.obligations,
                        )
                        .map_err(|_| {
                            self.trait_error(def, format!("关联项 `{name}` 不满足 trait 签名"))
                        })?,
                }
                if let Some(d) = actual.definition {
                    if let ItemKind::Function(id) =
                        self.modules[d.module].arena.items[d.item.0 as usize].kind
                    {
                        if self.modules[d.module].arena.fns[id.0 as usize].body == FnBody::None {
                            return Err(self.trait_error(d, "impl 方法必须提供函数体"));
                        }
                    }
                }
            } else if !matches!(
                member.kind,
                MemberKind::Method { default: true, .. }
                    | MemberKind::Type(Some(_))
                    | MemberKind::Const { value: Some(_), .. }
            ) {
                return Err(self.trait_error(def, format!("impl 缺少必需关联项 `{name}`")));
            }
        }
        if let Some(name) = implementation
            .members
            .keys()
            .find(|name| !definition.members.contains_key(*name))
        {
            return Err(self.trait_error(def, format!("trait 未声明关联项 `{name}`")));
        }
        if definition.definition.is_none() && definition.name == "IntoIter" {
            let iterator = self.normalize(
                &Ty::Projection(
                    Box::new(implementation.self_ty.clone()),
                    interface.clone(),
                    "Iter".into(),
                ),
                &implementation.obligations,
            )?;
            let iter_trait = super::TraitRef {
                id: self
                    .traits
                    .interfaces
                    .iter()
                    .position(|interface| {
                        interface.definition.is_none() && interface.name == "Iter"
                    })
                    .expect("Iter 已登记"),
                arguments: Vec::new(),
            };
            self.require_trait(&iterator, &iter_trait, &implementation.obligations)?;
            let item = self.normalize(
                &Ty::Projection(Box::new(iterator), iter_trait, "Item".into()),
                &implementation.obligations,
            )?;
            let declared = self.normalize(
                &Ty::Projection(
                    Box::new(implementation.self_ty.clone()),
                    interface.clone(),
                    "Item".into(),
                ),
                &implementation.obligations,
            )?;
            if item != declared {
                return Err(self.trait_error(def, "IntoIter::Item 必须与 Iter::Item 相同"));
            }
        }
        Ok(())
    }
    fn compare_member_kinds(
        &self,
        a: &MemberKind,
        b: &MemberKind,
        bindings: &BTreeMap<String, Ty>,
        assumptions: &[Obligation],
    ) -> Result<(), Diagnostic> {
        let same_type = |a: &Ty, b: &Ty| -> Result<bool, Diagnostic> {
            Ok(self.normalize(&substitute(a, bindings), assumptions)?
                == self.normalize(b, assumptions)?)
        };
        let valid = match (a, b) {
            (
                MemberKind::Method {
                    signature: a,
                    receiver: ar,
                    ..
                },
                MemberKind::Method {
                    signature: b,
                    receiver: br,
                    ..
                },
            ) => {
                ar == br
                    && self.same_opaque_signature(
                        &self.normalize(&substitute(a, bindings), assumptions)?,
                        &self.normalize(b, assumptions)?,
                        assumptions,
                    )?
            }
            (MemberKind::Type(Some(a)), MemberKind::Type(Some(b))) => same_type(a, b)?,
            (MemberKind::Const { ty: a, value: av }, MemberKind::Const { ty: b, value: bv }) => {
                av == bv && same_type(a, b)?
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(self.error(0, "关联项契约不一致"))
        }
    }
    fn method_bindings(
        &self,
        expected: &Member,
        actual: &Member,
        outer: &BTreeMap<String, Ty>,
    ) -> Result<BTreeMap<String, Ty>, Diagnostic> {
        let mut bindings = outer.clone();
        let (Some(a), Some(b)) = (expected.definition, actual.definition) else {
            return Ok(bindings);
        };
        let ma = &self.modules[a.module];
        let mb = &self.modules[b.module];
        let (ItemKind::Function(af), ItemKind::Function(bf)) = (
            ma.arena.items[a.item.0 as usize].kind.clone(),
            mb.arena.items[b.item.0 as usize].kind.clone(),
        ) else {
            return Ok(bindings);
        };
        let a_apits: Vec<_> = self
            .apits(super::super::model::CallableId {
                module: a.module,
                function: af.0,
            })
            .collect();
        let b_apits: Vec<_> = self
            .apits(super::super::model::CallableId {
                module: b.module,
                function: bf.0,
            })
            .collect();
        let af = &ma.arena.fns[af.0 as usize];
        let bf = &mb.arena.fns[bf.0 as usize];
        let ag = af.generics.as_slice(&ma.arena.generic_params);
        let bg = bf.generics.as_slice(&mb.arena.generic_params);
        let fail = || self.trait_error(b, "方法的泛型约束、unsafe 或参数传递契约与 trait 不一致");
        if a_apits.len() != b_apits.len() {
            return Err(fail());
        }
        for (&a, &b) in a_apits.iter().zip(&b_apits) {
            bindings.insert(Self::apit_name(a), Ty::Param(Self::apit_name(b)));
        }
        if ag.len() != bg.len() || af.unsafety != bf.unsafety || af.extern_abi != bf.extern_abi {
            return Err(fail());
        }
        let ap: Vec<_> = af
            .params
            .as_slice(&ma.arena.params)
            .iter()
            .enumerate()
            .filter(|(i, _)| ma.configured.param_active(af.params.start as usize + i))
            .map(|(_, p)| (p.comptime, p.variadic))
            .collect();
        let bp: Vec<_> = bf
            .params
            .as_slice(&mb.arena.params)
            .iter()
            .enumerate()
            .filter(|(i, _)| mb.configured.param_active(bf.params.start as usize + i))
            .map(|(_, p)| (p.comptime, p.variadic))
            .collect();
        if ap != bp {
            return Err(fail());
        }
        for (a_param, b_param) in ag.iter().zip(bg) {
            match (&a_param.kind, &b_param.kind) {
                (
                    GenericParamKind::Type {
                        name: an, pack: ap, ..
                    },
                    GenericParamKind::Type {
                        name: bn, pack: bp, ..
                    },
                ) if ap == bp => {
                    bindings.insert(
                        self.name(a.module, *an).into(),
                        Ty::Param(self.name(b.module, *bn).into()),
                    );
                }
                (
                    GenericParamKind::Comptime {
                        name: an, ty: at, ..
                    },
                    GenericParamKind::Comptime {
                        name: bn, ty: bt, ..
                    },
                ) => {
                    if substitute(&self.form(a.module, *at)?, &bindings)
                        != self.form(b.module, *bt)?
                    {
                        return Err(fail());
                    }
                    bindings.insert(
                        self.name(a.module, *an).into(),
                        Ty::Param(self.name(b.module, *bn).into()),
                    );
                }
                _ => return Err(fail()),
            }
        }
        let mut expected_params = self.parameters_at(a.module, &af.span);
        expected_params.extend(bindings.clone());
        let actual_params = self.parameters_at(b.module, &bf.span);
        let assumptions = self.assumptions_at(b.module, &bf.span)?;
        for (a_id, b_id) in a_apits.into_iter().zip(b_apits) {
            let a_bound = self.form_bounds(
                a.module,
                self.opaques.definitions[a_id as usize].bounds,
                &expected_params,
                &Ty::Param("$parameter".into()),
            )?;
            let b_bound = self.form_bounds(
                b.module,
                self.opaques.definitions[b_id as usize].bounds,
                &actual_params,
                &Ty::Param("$parameter".into()),
            )?;
            if self.bound_contract(a_bound, &bindings, &assumptions)?
                != self.bound_contract(b_bound, &BTreeMap::new(), &assumptions)?
            {
                return Err(fail());
            }
        }
        for (a_param, b_param) in ag.iter().zip(bg) {
            if let (
                GenericParamKind::Type { bounds: ab, .. },
                GenericParamKind::Type { bounds: bb, .. },
            ) = (&a_param.kind, &b_param.kind)
            {
                let form_bounds = |module: usize,
                                   bounds: &[super::super::super::ast::Bound],
                                   params: &BTreeMap<String, Ty>|
                 -> Result<
                    Vec<(Option<super::TraitRef>, Option<Ty>)>,
                    Diagnostic,
                > {
                    let mut result = Vec::new();
                    for bound in bounds {
                        result.push(match bound.kind {
                            BoundKind::Path(path) => {
                                (Some(self.trait_ref(module, path, params)?), None)
                            }
                            BoundKind::Fn { params: ps, ret } => (
                                None,
                                Some(self.form_kind(
                                    module,
                                    TyKind::Fn { params: ps, ret },
                                    params,
                                    &mut Vec::new(),
                                )?),
                            ),
                        });
                    }
                    result.sort();
                    result.dedup();
                    Ok(result)
                };
                if form_bounds(a.module, ab.as_slice(&ma.arena.bounds), &expected_params)?
                    != form_bounds(b.module, bb.as_slice(&mb.arena.bounds), &actual_params)?
                {
                    return Err(fail());
                }
            }
        }
        Ok(bindings)
    }
}
fn overlap(a: &Implementation, b: &Implementation) -> bool {
    let renamed: BTreeMap<_, _> = b
        .parameters
        .keys()
        .map(|name| (name.clone(), Ty::Param(format!("$rhs:{name}"))))
        .collect();
    let mut bindings = BTreeMap::new();
    if !intersect(&a.self_ty, &substitute(&b.self_ty, &renamed), &mut bindings) {
        return false;
    }
    if let (Some(a), Some(b)) = (&a.interface, &b.interface) {
        a.arguments.len() == b.arguments.len()
            && a.arguments
                .iter()
                .zip(&b.arguments)
                .all(|(a, b)| intersect(a, &substitute(b, &renamed), &mut bindings))
    } else {
        true
    }
}
fn intersect(a: &Ty, b: &Ty, bindings: &mut BTreeMap<String, Ty>) -> bool {
    let resolve = |ty: &Ty| {
        let mut value = ty.clone();
        while let Ty::Param(name) = &value {
            let Some(next) = bindings.get(name) else {
                break;
            };
            value = next.clone();
        }
        value
    };
    let a = resolve(a);
    let b = resolve(b);
    if a == b {
        return true;
    }
    if let (Ty::Param(name), ty) | (ty, Ty::Param(name)) = (&a, &b) {
        if occurs(name, ty, bindings) {
            return false;
        }
        bindings.insert(name.clone(), ty.clone());
        return true;
    }
    match (&a, &b) {
        (Ty::Ref(a), Ty::Ref(b))
        | (Ty::Ptr(a), Ty::Ptr(b))
        | (Ty::Slice(a), Ty::Slice(b))
        | (Ty::Option(a), Ty::Option(b))
        | (Ty::Chan(a), Ty::Chan(b))
        | (Ty::Join(a), Ty::Join(b)) => intersect(a, b, bindings),
        (Ty::Array(a, n), Ty::Array(b, m)) => n == m && intersect(a, b, bindings),
        (Ty::Tuple(a), Ty::Tuple(b)) => intersect_list(a, b, bindings),
        (Ty::Named(i, a), Ty::Named(j, b)) => i == j && intersect_list(a, b, bindings),
        (Ty::Result(a, e), Ty::Result(b, f)) => {
            intersect(a, b, bindings) && intersect(e, f, bindings)
        }
        (Ty::Function(a, r), Ty::Function(b, s)) => {
            intersect_list(a, b, bindings) && intersect(r, s, bindings)
        }
        _ => false,
    }
}
fn intersect_list(a: &[Ty], b: &[Ty], bindings: &mut BTreeMap<String, Ty>) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| intersect(a, b, bindings))
}
fn occurs(name: &str, ty: &Ty, bindings: &BTreeMap<String, Ty>) -> bool {
    match ty {
        Ty::Param(other) => {
            name == other
                || bindings
                    .get(other)
                    .is_some_and(|ty| occurs(name, ty, bindings))
        }
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t) => occurs(name, t, bindings),
        Ty::Tuple(ts) | Ty::Named(_, ts) => ts.iter().any(|t| occurs(name, t, bindings)),
        Ty::Result(t, e) => occurs(name, t, bindings) || occurs(name, e, bindings),
        Ty::Function(ts, ret) => {
            ts.iter().any(|t| occurs(name, t, bindings)) || occurs(name, ret, bindings)
        }
        _ => false,
    }
}
