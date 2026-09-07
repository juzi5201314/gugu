//! 不透明声明保留源位置身份；隐藏类型只交给布局与单态化，不参与对外名称解析。
use super::super::ast::{AstRange, Bound, BoundKind, FnBody, ItemKind, TyId, TyKind};
use super::model::{CallableId, DefRef, Model, Ty, substitute};
use super::traits::{Obligation, TraitRef};
use crate::{Diagnostic, DiagnosticCode, Span};
use std::collections::BTreeMap;
mod collect;
mod dynamic;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Origin {
    Parameter(CallableId),
    Return(CallableId),
    Alias(DefRef),
}
#[derive(Clone, Debug)]
pub(super) struct Definition {
    pub(super) module: usize,
    pub(super) ty: TyId,
    pub(super) origin: Origin,
    pub(super) bounds: AstRange<Bound>,
}
#[derive(Default)]
pub(super) struct Opaques {
    // TyId 在模块内稠密；表中只存紧凑的声明索引，完整记录仅为 impl Trait 分配。
    pub(super) by_type: Vec<Vec<Option<u32>>>,
    pub(super) definitions: Vec<Definition>,
}
#[derive(Clone)]
pub(super) struct Bounds {
    pub(super) traits: Vec<Obligation>,
    pub(super) functions: Vec<Ty>,
}
impl Model<'_> {
    pub(crate) fn apit_name(id: u32) -> String {
        format!("$apit:{id}")
    }
    pub(crate) fn apits(&self, owner: CallableId) -> impl Iterator<Item = u32> + '_ {
        self.opaques
            .definitions
            .iter()
            .enumerate()
            .filter_map(move |(index, definition)| {
                (definition.origin == Origin::Parameter(owner)).then_some(index as u32)
            })
    }
    pub(super) fn validate_opaque_bounds(&self) -> Result<(), Diagnostic> {
        for (id, definition) in self.opaques.definitions.iter().enumerate() {
            let params = self.opaque_environment(id as u32);
            let bounds = self.form_bounds(
                definition.module,
                definition.bounds,
                &params,
                &Ty::Param("$opaque".into()),
            )?;
            if bounds.functions.windows(2).any(|pair| pair[0] != pair[1]) {
                return Err(Diagnostic::error(
                    DiagnosticCode::InvalidType,
                    "impl Trait 的 Fn 约束必须具有同一调用签名",
                    Some(self.opaque_span(id as u32).clone()),
                ));
            }
        }
        Ok(())
    }
    pub(super) fn opaque_span(&self, id: u32) -> &Span {
        let definition = &self.opaques.definitions[id as usize];
        &self.modules[definition.module].arena.tys[definition.ty.0 as usize].span
    }
    fn opaque_environment(&self, id: u32) -> BTreeMap<String, Ty> {
        let definition = &self.opaques.definitions[id as usize];
        let mut context = self.parameters_at(definition.module, self.opaque_span(id));
        if let Origin::Alias(owner) = definition.origin {
            if let ItemKind::TypeAlias { generics, .. } =
                self.modules[owner.module].arena.items[owner.item.0 as usize].kind
            {
                context.extend(self.generic_parameters(owner.module, generics));
            }
        }
        context
    }
    pub(super) fn opaque_context(&self, id: u32) -> BTreeMap<String, Ty> {
        let mut context = self.opaque_environment(id);
        // Self::关联项是推导出的路径缓存，不是隐藏类型的独立参数。
        context.retain(|name, _| !name.starts_with("Self::"));
        context
    }
    pub(super) fn form_opaque(
        &self,
        module: usize,
        ty: TyId,
        params: &BTreeMap<String, Ty>,
    ) -> Result<Ty, Diagnostic> {
        let id = self.opaques.by_type[module][ty.0 as usize].ok_or_else(|| Diagnostic::error(DiagnosticCode::InvalidType,
            "impl Trait 只允许用于函数参数、返回类型或 type 别名，且不能进入外部 ABI、union 或句柄实参", Some(self.modules[module].arena.tys[ty.0 as usize].span.clone())))?;
        let definition = &self.opaques.definitions[id as usize];
        if matches!(definition.origin, Origin::Parameter(_)) {
            let name = Self::apit_name(id);
            return Ok(params
                .get(&name)
                .cloned()
                .unwrap_or_else(|| Ty::Param(name)));
        }
        let arguments = self
            .opaque_context(id)
            .into_iter()
            .map(|(name, formal)| params.get(&name).cloned().unwrap_or(formal))
            .collect();
        Ok(Ty::Opaque(id, arguments))
    }
    pub(super) fn opaque_bindings(&self, id: u32, arguments: &[Ty]) -> BTreeMap<String, Ty> {
        let mut context = self.opaque_environment(id);
        let keys: Vec<_> = context
            .keys()
            .filter(|name| !name.starts_with("Self::"))
            .cloned()
            .collect();
        debug_assert_eq!(keys.len(), arguments.len(), "隐藏类型保留全部声明环境参数");
        let bindings = keys.into_iter().zip(arguments.iter().cloned()).collect();
        for ty in context.values_mut() {
            *ty = substitute(ty, &bindings);
        }
        context
    }
    pub(super) fn opaque_bounds(
        &self,
        id: u32,
        arguments: &[Ty],
        self_ty: &Ty,
    ) -> Result<Bounds, Diagnostic> {
        let definition = &self.opaques.definitions[id as usize];
        let params = self.opaque_bindings(id, arguments);
        self.form_bounds(definition.module, definition.bounds, &params, self_ty)
    }
    pub(super) fn form_bounds(
        &self,
        module: usize,
        range: AstRange<Bound>,
        params: &BTreeMap<String, Ty>,
        self_ty: &Ty,
    ) -> Result<Bounds, Diagnostic> {
        let mut traits = Vec::new();
        let mut functions = Vec::new();
        for bound in range.as_slice(&self.modules[module].arena.bounds) {
            match bound.kind {
                BoundKind::Path(path) => traits.push(Obligation {
                    ty: self_ty.clone(),
                    interface: self.trait_ref(module, path, params)?,
                    span: bound.span.clone(),
                }),
                BoundKind::Fn {
                    params: inputs,
                    ret,
                } => functions.push(self.form_kind(
                    module,
                    TyKind::Fn {
                        params: inputs,
                        ret,
                    },
                    params,
                    &mut Vec::new(),
                )?),
            }
        }
        Ok(Bounds { traits, functions })
    }
    pub(super) fn opaque_function(&self, ty: &Ty) -> Result<Option<Ty>, Diagnostic> {
        let Ty::Opaque(id, arguments) = ty else {
            return Ok(None);
        };
        let mut signatures = self.opaque_bounds(*id, arguments, ty)?.functions;
        signatures.sort();
        signatures.dedup();
        match signatures.len() {
            0 => Ok(None),
            1 => Ok(signatures.pop()),
            _ => Err(self.error(
                self.opaques.definitions[*id as usize].module,
                "不透明类型的 Fn 约束签名不一致",
            )),
        }
    }
    pub(super) fn can_bind_opaque(&self, id: u32, module: usize, span: &Span) -> bool {
        let definition = &self.opaques.definitions[id as usize];
        if definition.module != module {
            return false;
        }
        match definition.origin {
            Origin::Alias(_) => true,
            Origin::Parameter(_) => false,
            Origin::Return(owner) => {
                let function = &self.modules[module].arena.fns[owner.function as usize];
                function.span.start() <= span.start() && function.span.end() >= span.end()
            }
        }
    }
    pub(crate) fn opaque_requires_definition(&self, id: u32) -> bool {
        match self.opaques.definitions[id as usize].origin {
            Origin::Parameter(_) => false,
            Origin::Alias(_) => true,
            Origin::Return(owner) => {
                self.modules[owner.module].arena.fns[owner.function as usize].body != FnBody::None
            }
        }
    }
    pub(crate) fn hidden_type(&self, ty: &Ty, hidden: &[Option<Ty>]) -> Result<Ty, Diagnostic> {
        let mut result = ty.clone();
        let mut seen = Vec::new();
        while let Ty::Opaque(id, ref arguments) = result {
            if seen.contains(&id) {
                return Err(self.error(
                    self.opaques.definitions[id as usize].module,
                    "隐藏类型形成循环",
                ));
            }
            seen.push(id);
            let definition = hidden
                .get(id as usize)
                .and_then(Option::as_ref)
                .ok_or_else(|| {
                    self.error(
                        self.opaques.definitions[id as usize].module,
                        "隐藏类型尚未唯一形成",
                    )
                })?;
            result = substitute(definition, &self.opaque_bindings(id, arguments));
        }
        Ok(result)
    }
    pub(super) fn declared_trait_bounds(&self, ty: &Ty) -> Result<Vec<TraitRef>, Diagnostic> {
        if let Ty::Dyn(bounds) = ty {
            return Ok(bounds.clone());
        }
        let mut base = ty;
        while let Ty::Projection(parent, _, _) = base {
            base = parent;
        }
        let Ty::Opaque(id, arguments) = base else {
            return Ok(Vec::new());
        };
        let mut obligations = self.opaque_bounds(*id, arguments, base)?.traits;
        self.expand_requirements(&mut obligations);
        Ok(obligations
            .into_iter()
            .filter(|bound| bound.ty == *ty)
            .map(|bound| bound.interface)
            .collect())
    }
    pub(super) fn same_opaque_signature(
        &self,
        expected: &Ty,
        actual: &Ty,
        assumptions: &[Obligation],
    ) -> Result<bool, Diagnostic> {
        if expected == actual {
            return Ok(true);
        }
        match (expected, actual) {
            (Ty::Function(a, ar), Ty::Function(b, br)) => {
                Ok(a == b && self.same_opaque_signature(ar, br, assumptions)?)
            }
            (Ty::Opaque(a, aa), Ty::Opaque(b, ba)) => {
                if !matches!(
                    self.opaques.definitions[*a as usize].origin,
                    Origin::Return(_)
                ) || !matches!(
                    self.opaques.definitions[*b as usize].origin,
                    Origin::Return(_)
                ) {
                    return Ok(false);
                }
                let canonical = |id, args: &[Ty]| -> Result<(Vec<TraitRef>, Vec<Ty>), Diagnostic> {
                    let bounds = self.opaque_bounds(id, args, &Ty::Param("$return".into()))?;
                    self.bound_contract(bounds, &BTreeMap::new(), assumptions)
                };
                Ok(canonical(*a, aa)? == canonical(*b, ba)?)
            }
            (Ty::Ref(a), Ty::Ref(b))
            | (Ty::Ptr(a), Ty::Ptr(b))
            | (Ty::Slice(a), Ty::Slice(b))
            | (Ty::Option(a), Ty::Option(b)) => self.same_opaque_signature(a, b, assumptions),
            (Ty::Array(a, an), Ty::Array(b, bn)) => {
                Ok(an == bn && self.same_opaque_signature(a, b, assumptions)?)
            }
            (Ty::Result(a, ae), Ty::Result(b, be)) => {
                Ok(self.same_opaque_signature(a, b, assumptions)?
                    && self.same_opaque_signature(ae, be, assumptions)?)
            }
            (Ty::Tuple(a), Ty::Tuple(b)) | (Ty::Named(_, a), Ty::Named(_, b)) => {
                if a.len() != b.len()
                    || matches!((expected, actual), (Ty::Named(i, _), Ty::Named(j, _)) if i != j)
                {
                    return Ok(false);
                }
                for (a, b) in a.iter().zip(b) {
                    if !self.same_opaque_signature(a, b, assumptions)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
    pub(super) fn bound_contract(
        &self,
        bounds: Bounds,
        bindings: &BTreeMap<String, Ty>,
        assumptions: &[Obligation],
    ) -> Result<(Vec<TraitRef>, Vec<Ty>), Diagnostic> {
        let mut traits = bounds
            .traits
            .into_iter()
            .map(|bound| {
                Ok(TraitRef {
                    id: bound.interface.id,
                    arguments: bound
                        .interface
                        .arguments
                        .iter()
                        .map(|ty| self.normalize(&substitute(ty, bindings), assumptions))
                        .collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;
        let mut functions = bounds
            .functions
            .iter()
            .map(|ty| self.normalize(&substitute(ty, bindings), assumptions))
            .collect::<Result<Vec<_>, _>>()?;
        traits.sort();
        traits.dedup();
        functions.sort();
        functions.dedup();
        Ok((traits, functions))
    }
}
pub(super) fn has_unbound(ty: &Ty, parameters: &BTreeMap<String, Ty>) -> bool {
    match ty {
        Ty::Param(name) => !parameters.contains_key(name),
        Ty::Var(_) => true,
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => has_unbound(t, parameters),
        Ty::Tuple(types) | Ty::Named(_, types) | Ty::Opaque(_, types) => {
            types.iter().any(|ty| has_unbound(ty, parameters))
        }
        Ty::Function(params, ret) | Ty::Callable(_, params, ret) => {
            params.iter().any(|ty| has_unbound(ty, parameters)) || has_unbound(ret, parameters)
        }
        Ty::Result(value, error) => {
            has_unbound(value, parameters) || has_unbound(error, parameters)
        }
        Ty::Projection(base, interface, _) => {
            has_unbound(base, parameters)
                || interface
                    .arguments
                    .iter()
                    .any(|ty| has_unbound(ty, parameters))
        }
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .flat_map(|interface| &interface.arguments)
            .any(|ty| has_unbound(ty, parameters)),
        _ => false,
    }
}
