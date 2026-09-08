//! 在实例闭合期间收集类型及布局；冻结后不再访问可变语义环境。
use super::instantiate::WalkEntry;
use super::keys::{MonoContext, StableTypeKey, hash_domain};
use crate::Diagnostic;
use crate::frontend::{
    hir,
    late::universe::{Shape, TypeRecord},
    semantics::Ty,
    types::Layouts,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn collect(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
) -> Result<(Vec<TypeRecord>, Vec<(u32, StableTypeKey)>), Diagnostic> {
    let mut collector = Collector {
        context,
        layouts: Layouts::new(context.model, context.checked),
        records: BTreeMap::new(),
        visited: BTreeSet::new(),
    };
    let mut ids = BTreeSet::new();
    if let Some(signature) = context.module.definitions[entry.definition.index()].signature {
        ids.insert(signature);
    }
    if let Some(body) = entry.body {
        let owner = &context.module.owners[body];
        ids.extend(owner.expression_types.iter().copied());
        ids.extend(owner.expression_inputs.iter().copied());
        ids.extend(owner.locals.iter().map(|local| local.ty));
        for expression in &owner.expressions {
            if let hir::ExprKind::Intrinsic { types, .. } = &expression.kind {
                ids.extend(types);
            }
        }
    }
    let mut bindings = Vec::with_capacity(ids.len());
    for id in ids {
        let ty = context.type_at(id, &entry.bindings)?;
        let ty = concrete(context, &ty)?;
        // 泛型 callee 的未实例化函数值类型不是具体类型；它的选定实例独立收集签名。
        if !is_concrete(&ty) {
            continue;
        }
        let key = collector.visit(&ty)?;
        if let Some(key) = key {
            bindings.push((id.0, key));
        }
    }
    for ty in entry.bindings.values() {
        collector.visit(&concrete(context, ty)?)?;
    }
    Ok((collector.records.into_values().collect(), bindings))
}

pub(crate) fn concrete(context: &MonoContext<'_>, ty: &Ty) -> Result<Ty, Diagnostic> {
    let mut ty = context
        .model
        .hidden_type(ty, &context.checked.hidden_types)?;
    let map = |ty: &mut Ty| -> Result<(), Diagnostic> {
        *ty = concrete(context, ty)?;
        Ok(())
    };
    match &mut ty {
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => map(t)?,
        Ty::Tuple(ts) | Ty::Named(_, ts) => {
            for t in ts {
                map(t)?;
            }
        }
        Ty::Function(ts, result) | Ty::Callable(_, ts, result) => {
            for t in ts {
                map(t)?;
            }
            map(result)?;
        }
        Ty::Result(t, e) => {
            map(t)?;
            map(e)?;
        }
        Ty::Dyn(interfaces) => {
            for interface in interfaces {
                for t in &mut interface.arguments {
                    map(t)?;
                }
            }
        }
        _ => {}
    }
    Ok(ty)
}

fn is_concrete(ty: &Ty) -> bool {
    match ty {
        Ty::Error | Ty::Var(_) | Ty::Param(_) | Ty::Projection(..) | Ty::Opaque(..) => false,
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => is_concrete(t),
        Ty::Tuple(ts) | Ty::Named(_, ts) => ts.iter().all(is_concrete),
        Ty::Function(ts, ret) | Ty::Callable(_, ts, ret) => {
            ts.iter().all(is_concrete) && is_concrete(ret)
        }
        Ty::Result(t, e) => is_concrete(t) && is_concrete(e),
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .all(|i| i.arguments.iter().all(is_concrete)),
        _ => true,
    }
}

struct Collector<'a, 'm> {
    context: &'a MonoContext<'m>,
    layouts: Layouts<'a, 'm>,
    records: BTreeMap<StableTypeKey, TypeRecord>,
    visited: BTreeSet<StableTypeKey>,
}

impl Collector<'_, '_> {
    fn visit(&mut self, ty: &Ty) -> Result<Option<StableTypeKey>, Diagnostic> {
        let ty = concrete(self.context, ty)?;
        let canonical = self.context.encode_type(&ty)?;
        let key = hash_domain("gugu-mono-v1", &canonical);
        if !self.visited.insert(key) {
            return Ok((!matches!(ty, Ty::Never | Ty::MaybeUninit(_))).then_some(key));
        }
        let mut children = BTreeSet::new();
        let context = self.context;
        let mut add = |t: &Ty| -> Result<StableTypeKey, Diagnostic> {
            let child = concrete(self.context, t)?;
            let key = hash_domain("gugu-mono-v1", &self.context.encode_type(&child)?);
            if let Some(child) = self.visit(&child)? {
                children.insert(child);
            }
            Ok(key)
        };
        let shape = match &ty {
            Ty::Unit => Shape::Unit,
            Ty::Bool => Shape::Bool,
            Ty::Char => Shape::Char,
            Ty::Int { signed, bits } => Shape::Int {
                signed: *signed,
                bits: *bits,
            },
            Ty::Float(bits) => Shape::Float(*bits),
            Ty::TypeId => Shape::TypeId,
            Ty::Tuple(ts) => Shape::Tuple(ts.iter().map(&mut add).collect::<Result<_, _>>()?),
            Ty::Array(t, n) => Shape::Array(add(t)?, *n),
            Ty::Named(_, arguments) => {
                for t in arguments {
                    add(t)?;
                }
                let variants = context.model.variants(&ty).expect("已形成名义类型");
                let mut fields = Vec::new();
                for variant in &variants {
                    for field in &variant.fields {
                        fields.push((field.name.clone(), add(&field.ty)?));
                    }
                }
                let Ty::Named(index, _) = &ty else {
                    unreachable!()
                };
                let def = self.context.model.nominal[*index].definition;
                if matches!(
                    self.context.model.modules[def.module].arena.items[def.item.0 as usize].kind,
                    crate::frontend::ast::ItemKind::Struct { .. }
                ) {
                    Shape::Struct(fields)
                } else {
                    Shape::Other
                }
            }
            Ty::Function(ts, ret) | Ty::Callable(_, ts, ret) => {
                for t in ts {
                    add(t)?;
                }
                add(ret)?;
                Shape::Other
            }
            Ty::Ref(t)
            | Ty::Ptr(t)
            | Ty::Slice(t)
            | Ty::Option(t)
            | Ty::Chan(t)
            | Ty::Join(t)
            | Ty::MaybeUninit(t) => {
                add(t)?;
                Shape::Other
            }
            Ty::Result(t, e) => {
                add(t)?;
                add(e)?;
                Shape::Other
            }
            Ty::Dyn(interfaces) => {
                for i in interfaces {
                    for t in &i.arguments {
                        add(t)?;
                    }
                }
                Shape::Other
            }
            _ => Shape::Other,
        };
        if matches!(ty, Ty::Never | Ty::MaybeUninit(_)) {
            return Ok(None);
        }
        let layout = self.layouts.layout(&ty)?.map(|l| (l.size, l.align));
        let record = TypeRecord {
            key,
            canonical,
            name: self.context.model.describe(&ty),
            layout,
            children: children.into_iter().collect(),
            shape,
        };
        self.records.insert(key, record);
        Ok(Some(key))
    }
}
