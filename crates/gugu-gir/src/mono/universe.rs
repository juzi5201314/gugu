//! 在实例闭合期间收集类型及布局；冻结后不再访问可变语义环境。
use super::instantiate::WalkEntry;
use super::keys::{MonoContext, StableTypeKey};
use crate::Diagnostic;
use crate::{
    gir::passing::PassingClass,
    hir,
    late::universe::{MetadataShape, Shape, TypeRecord},
    semantics::Ty,
    types::Layouts,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// 实例闭合收集结果：类型记录表与稳定键分配表。
pub type CollectedTypes = (Vec<TypeRecord>, Vec<(u32, StableTypeKey)>);

pub fn collect(context: &MonoContext<'_>, entry: &WalkEntry) -> Result<CollectedTypes, Diagnostic> {
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
        // 静态访问在 GIR 物化时把 `StaticRef` 槽改写为 `*T`：该指针类型同样是具体类型，
        // 必须在冻结前入表。来源有两处：`let` 绑定静态的槽位，与直接解析到静态定义的表达式。
        let static_slot = |id: hir::TypeId, collector: &mut Collector<'_, '_>| {
            let ty = context.type_at(id, &entry.bindings)?;
            let ty = concrete(context, &ty)?;
            if is_concrete(&ty) {
                collector.visit(&Ty::Ptr(Box::new(ty.into_owned())))?;
            }
            Ok::<(), Diagnostic>(())
        };
        for statement in &owner.statements {
            if let hir::StatementKind::Static { local, .. } = &statement.kind {
                static_slot(owner.locals[local.index()].ty, &mut collector)?;
            }
        }
        for (index, expression) in owner.expressions.iter().enumerate() {
            let definition = match &expression.kind {
                hir::ExprKind::Resolved(hir::Res::Def(definition))
                | hir::ExprKind::Resolved(hir::Res::Associated { definition, .. }) => *definition,
                _ => continue,
            };
            if matches!(
                context.module.definitions[definition.index()].kind,
                hir::DefinitionKind::Static | hir::DefinitionKind::LocalStatic
            ) {
                static_slot(owner.expression_types[index], &mut collector)?;
            }
        }
        if !owner.checks.is_empty()
            || owner.expressions.iter().any(|expression| {
                expression.effects.0 & (hir::Effects::PANIC | hir::Effects::SUSPEND) != 0
            })
        {
            let string = context
                .module
                .types
                .iter()
                .position(|ty| matches!(ty, hir::Type::String))
                .expect("HIR 基础类型表包含 String");
            ids.insert(hir::TypeId(
                u32::try_from(string).expect("类型编号不超过 u32"),
            ));
        }
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
        let ty = concrete(context, ty)?;
        collector.visit(&ty)?;
    }
    // 闭包环境与 spawn 参数包是真实分配出去的 managed 对象：它们的 descriptor 必须在冻结前
    // 进入同一张类型表，LIR 之后只按同一份键查表，不再自造 descriptor。
    for record in super::storage::collect(context, entry)? {
        if collector
            .records
            .insert(record.key, record.clone())
            .is_some()
        {
            return Err(Diagnostic::error(
                crate::DiagnosticCode::MonoDivergence,
                "存储类型与已有类型键冲突",
                None,
            ));
        }
    }
    Ok((collector.records.into_values().collect(), bindings))
}

/// 规约隐藏类型与投影；类型已经具体时直接借用原值，不做任何克隆。
pub fn concrete<'t>(context: &MonoContext<'_>, ty: &'t Ty) -> Result<Cow<'t, Ty>, Diagnostic> {
    if already_concrete(ty) {
        return Ok(Cow::Borrowed(ty));
    }
    let mut ty = context
        .model
        .hidden_type(ty, &context.checked.hidden_types)?;
    reduce_children(context, &mut ty)?;
    Ok(Cow::Owned(ty))
}

/// 类型是否已经具体：不含 `Ty::Opaque` 与 `Ty::Projection` 时 `concrete` 原样返回。
///
/// 判定位置必须与 [`reduce_children`] 改写的位置逐一对应，否则会出现漏规约。
fn already_concrete(ty: &Ty) -> bool {
    match ty {
        Ty::Opaque(..) | Ty::Projection(..) => false,
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => already_concrete(t),
        Ty::Tuple(ts) | Ty::Named(_, ts) => ts.iter().all(already_concrete),
        Ty::Function(ts, result) | Ty::Callable(_, ts, result) => {
            ts.iter().all(already_concrete) && already_concrete(result)
        }
        Ty::Result(t, e) => already_concrete(t) && already_concrete(e),
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .all(|interface| interface.arguments.iter().all(already_concrete)),
        _ => true,
    }
}

/// 就地规约子树：只有真的展开过隐藏类型或归一化过投影的子类型才写回。
fn reduce_children(context: &MonoContext<'_>, ty: &mut Ty) -> Result<(), Diagnostic> {
    fn reduce(context: &MonoContext<'_>, ty: &mut Ty) -> Result<(), Diagnostic> {
        match concrete(context, ty)? {
            Cow::Borrowed(_) => Ok(()),
            Cow::Owned(reduced) => {
                *ty = reduced;
                Ok(())
            }
        }
    }
    match ty {
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => reduce(context, t)?,
        Ty::Tuple(ts) | Ty::Named(_, ts) => {
            for t in ts {
                reduce(context, t)?;
            }
        }
        Ty::Function(ts, result) | Ty::Callable(_, ts, result) => {
            for t in ts {
                reduce(context, t)?;
            }
            reduce(context, result)?;
        }
        Ty::Result(t, e) => {
            reduce(context, t)?;
            reduce(context, e)?;
        }
        Ty::Dyn(interfaces) => {
            for interface in interfaces {
                for t in &mut interface.arguments {
                    reduce(context, t)?;
                }
            }
        }
        Ty::Projection(self_ty, interface, _) => {
            reduce(context, self_ty)?;
            for argument in &mut interface.arguments {
                reduce(context, argument)?;
            }
            let normalized = context.model.normalize(ty, &[])?;
            if normalized != *ty {
                *ty = concrete(context, &normalized)?.into_owned();
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn is_concrete(ty: &Ty) -> bool {
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
        let reduced = concrete(self.context, ty)?;
        let ty: &Ty = &reduced;
        let canonical = self.context.encode_type(ty)?;
        let key = self.context.type_key(ty)?;
        if !self.visited.insert(key) {
            return Ok((!matches!(ty, Ty::Never | Ty::MaybeUninit(_))).then_some(key));
        }
        let mut children = BTreeSet::new();
        let context = self.context;
        let mut add = |t: &Ty| -> Result<StableTypeKey, Diagnostic> {
            let child = concrete(self.context, t)?;
            let key = self.context.type_key(&child)?;
            if let Some(child) = self.visit(&child)? {
                children.insert(child);
            }
            Ok(key)
        };
        let shape = match ty {
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
                let variants = context.model.variants(ty).expect("已形成名义类型");
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
                    crate::ast::ItemKind::Struct { .. }
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
        let layout = self.layouts.layout(ty)?.map(|l| (l.size, l.align));
        let metadata = self.metadata(ty)?;
        let passing = self.passing(ty)?;
        let record = TypeRecord {
            key,
            canonical,
            name: self.context.model.describe(ty),
            layout,
            children: children.into_iter().collect(),
            shape,
            metadata,
            passing,
        };
        self.records.insert(key, record);
        Ok(Some(key))
    }

    fn child_key(&mut self, ty: &Ty) -> Result<StableTypeKey, Diagnostic> {
        let child = concrete(self.context, ty)?;
        let key = self.context.type_key(&child)?;
        if let Some(child) = self.visit(&child)? {
            debug_assert_eq!(child, key);
        }
        Ok(key)
    }

    fn metadata(&mut self, ty: &Ty) -> Result<MetadataShape, Diagnostic> {
        Ok(match ty {
            Ty::Ref(inner) | Ty::Chan(inner) | Ty::Join(inner) => {
                MetadataShape::Direct(self.child_key(inner)?)
            }
            Ty::Slice(inner) => MetadataShape::Interior(self.child_key(inner)?),
            Ty::String => MetadataShape::String,
            Ty::Array(inner, count) => MetadataShape::Array {
                element: self.child_key(inner)?,
                count: *count,
            },
            Ty::Tuple(_)
            | Ty::Named(..)
            | Ty::Option(_)
            | Ty::Result(..)
            | Ty::Range
            | Ty::ChanClosed
            | Ty::TrySendErr
            | Ty::TryRecvErr => {
                let aggregate = self.layouts.aggregate_layout(ty)?;
                let variants = aggregate
                    .variants
                    .into_iter()
                    .map(|fields| {
                        fields
                            .into_iter()
                            .map(|(field, offset)| Ok((self.child_key(&field)?, offset)))
                            .collect::<Result<Vec<_>, Diagnostic>>()
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?;
                let tag = aggregate.tag.map(|layout| {
                    let width = u8::try_from(layout.size).expect("tag 宽度适配 u8");
                    (0, width)
                });
                MetadataShape::Aggregate { tag, variants }
            }
            Ty::MaybeUninit(_)
            | Ty::Ptr(_)
            | Ty::Function(..)
            | Ty::Callable(..)
            | Ty::Dyn(_)
            | Ty::Panic
            | Ty::TypeId
            | Ty::Formatter
            | Ty::Unit
            | Ty::Bool
            | Ty::Int { .. }
            | Ty::Float(_)
            | Ty::Char
            | Ty::Never
            | Ty::Error
            | Ty::Var(_)
            | Ty::Param(_)
            | Ty::Projection(..)
            | Ty::Opaque(..) => MetadataShape::None,
        })
    }

    /// 该类型的 `PassingClass` 位集合。
    ///
    /// 冻结类型表早于具体 GIR 建立，因此这里按 `Ty` 结构直接推导类别；具体 GIR
    /// 侧若给出更精确的分类，`frontend/gc.rs` 优先采用后者。位定义只有
    /// `PassingClass` 一处权威，此处不重复书写字面量。
    fn passing(&self, ty: &Ty) -> Result<u8, Diagnostic> {
        let bits = PassingClass::BITS.bits();
        let identity = PassingClass::IDENTITY.bits();
        let cow = PassingClass::COW.bits();
        let resource = PassingClass::RESOURCE.bits();
        Ok(match ty {
            Ty::String => cow,
            Ty::Ref(_) | Ty::Slice(_) | Ty::Chan(_) | Ty::Join(_) | Ty::Dyn(_) => identity,
            Ty::Named(index, _) => {
                let nominal = &self.context.model.nominal[*index];
                match nominal.name.as_str() {
                    "ResourceCell" => resource,
                    "ByteBuffer" | "Bytes" => cow,
                    _ => {
                        let variants = self.context.model.variants(ty).ok_or_else(|| {
                            crate::Diagnostic::error(
                                crate::DiagnosticCode::LateComptime,
                                "名义类型字段不存在",
                                None,
                            )
                        })?;
                        let mut passing = bits;
                        for variant in variants {
                            for field in variant.fields {
                                passing |= self.passing(&field.ty)?;
                            }
                        }
                        passing
                    }
                }
            }
            Ty::Array(inner, _) | Ty::Option(inner) | Ty::MaybeUninit(inner) => {
                self.passing(inner)?
            }
            Ty::Tuple(fields) => {
                let mut passing = bits;
                for field in fields {
                    passing |= self.passing(field)?;
                }
                passing
            }
            Ty::Result(value, error) => self.passing(value)? | self.passing(error)?,
            _ => bits,
        })
    }
}
