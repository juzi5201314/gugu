//! 布局只消费语义类型，禁止将未解析路径伪装成零大小值。
use super::{
    ast::ItemKind,
    semantics::{
        CheckedSemantics,
        model::{Model, Ty},
    },
};
use crate::{Diagnostic, DiagnosticCode};
use std::collections::BTreeMap;
mod borrow;
mod foreign;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Layout {
    pub(crate) size: u64,
    pub(crate) align: u64,
}

#[derive(Clone, Copy)]
enum AggregateKind {
    Ordered,
    Packed,
    Union,
}

const DEFAULT_TAG: Layout = Layout { size: 1, align: 1 };

pub(crate) fn form_and_layout(
    model: &Model<'_>,
    semantics: &CheckedSemantics,
    target: crate::TargetName,
) -> Result<Vec<Layout>, Vec<Diagnostic>> {
    model
        .validate_linkage(&semantics.linkage, target)
        .map_err(|error| vec![error])?;
    let mut arena = Layouts::new(model, semantics);
    for body in &semantics.bodies {
        for ty in body
            .slots
            .iter()
            .chain(body.expressions.iter().map(|(_, ty)| ty))
        {
            arena.layout(ty).map_err(|error| vec![error])?;
        }
        for operation in &body.memory_operations {
            arena.check_memory(operation).map_err(|error| vec![error])?;
        }
        for plan in &body.assembly {
            arena.check_assembly(plan).map_err(|error| vec![error])?;
        }
        for check in &body.borrow_checks {
            arena.check_borrow(check).map_err(|error| {
                vec![Diagnostic::error(
                    error.code(),
                    error.message(),
                    Some(
                        model.modules[body.definition.module].arena.exprs
                            [check.expression.0 as usize]
                            .span
                            .clone(),
                    ),
                )]
            })?;
        }
        arena
            .check_native_body(body, target)
            .map_err(|error| vec![error])?;
    }
    for (index, nominal) in model.nominal.iter().enumerate() {
        if !nominal.params.is_empty() {
            continue;
        }
        arena
            .layout(&Ty::Named(index, Vec::new()))
            .map_err(|error| vec![error])?;
        for variant in &nominal.variants {
            for field in &variant.fields {
                arena.layout(&field.ty).map_err(|error| vec![error])?;
            }
        }
    }
    arena.validate_abi(target).map_err(|error| vec![error])?;
    Ok(arena.complete.into_values().collect())
}

impl<'m, 'a> Layouts<'m, 'a> {
    pub(crate) fn new(model: &'m Model<'a>, semantics: &'m CheckedSemantics) -> Self {
        Self {
            model,
            semantics,
            capturing: capturing_functions(model, semantics),
            complete: BTreeMap::new(),
            active: Vec::new(),
        }
    }
}

fn capturing_functions(model: &Model<'_>, semantics: &CheckedSemantics) -> Vec<Vec<bool>> {
    // 模块和 FnDecl 都是稠密编号；Vec<bool> 为每个现有函数保留一位。
    let mut capturing: Vec<_> = model
        .modules
        .iter()
        .map(|module| vec![false; module.arena.fns.len()])
        .collect();
    for plan in semantics.bodies.iter().flat_map(|body| &body.captures) {
        if let Some(id) = plan.function {
            debug_assert!(
                id.module < capturing.len() && (id.function as usize) < capturing[id.module].len()
            );
            capturing[id.module][id.function as usize] |= !plan.captures.is_empty();
        }
    }
    capturing
}

pub(crate) struct Layouts<'m, 'a> {
    model: &'m Model<'a>,
    semantics: &'m CheckedSemantics,
    capturing: Vec<Vec<bool>>,
    complete: BTreeMap<Ty, Layout>,
    active: Vec<Ty>,
}
impl Layouts<'_, '_> {
    fn check_memory(
        &mut self,
        operation: &super::semantics::MemoryOperation,
    ) -> Result<(), Diagnostic> {
        use super::semantics::model::MemoryIntrinsic;
        let source = self.layout(&operation.value)?;
        let target = self.layout(&operation.result)?;
        let hidden = &self.semantics.hidden_types;
        match operation.kind {
            MemoryIntrinsic::Transmute => {
                if source
                    .zip(target)
                    .is_some_and(|(source, target)| source.size != target.size)
                {
                    return Err(invalid("transmute 的源类型与目标类型大小必须相同"));
                }
                if self.model.has_managed_value(&operation.value, hidden) == Some(true)
                    || self.model.has_managed_value(&operation.result, hidden) == Some(true)
                {
                    return Err(invalid("transmute 不能绕过 COW 或 resource 管理动作"));
                }
            }
            MemoryIntrinsic::PtrRead
            | MemoryIntrinsic::PtrWrite
            | MemoryIntrinsic::VolatileLoad
            | MemoryIntrinsic::VolatileStore => {
                if self.model.has_managed_value(&operation.value, hidden) == Some(true) {
                    return Err(invalid("按位指针访问不能绕过 COW 或 resource 管理动作"));
                }
            }
            MemoryIntrinsic::ReadUnaligned | MemoryIntrinsic::WriteUnaligned => {
                if self.model.is_bit_type(&operation.value, hidden) == Some(false) {
                    return Err(invalid("未对齐访问只允许位类型"));
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) fn layout(&mut self, ty: &Ty) -> Result<Option<Layout>, Diagnostic> {
        if let Some(layout) = self.complete.get(ty) {
            return Ok(Some(*layout));
        }
        if self.active.contains(ty) {
            return Err(Diagnostic::error(
                DiagnosticCode::RecursiveType,
                "类型形成无限大小递归",
                None,
            ));
        }
        self.active.push(ty.clone());
        let result = self.compute(ty);
        self.active.pop();
        let result = result?;
        if let Some(layout) = result {
            self.complete.insert(ty.clone(), layout);
        }
        Ok(result)
    }
    fn compute(&mut self, ty: &Ty) -> Result<Option<Layout>, Diagnostic> {
        let word = Layout { size: 8, align: 8 };
        Ok(Some(match ty {
            Ty::Error | Ty::Var(_) => return Err(invalid("布局类型尚未收敛")),
            Ty::Param(_) | Ty::Projection(..) => return Ok(None),
            Ty::Opaque(id, _) => {
                if !self.model.opaque_requires_definition(*id) {
                    return Ok(None);
                }
                return self.layout(&self.model.hidden_type(ty, &self.semantics.hidden_types)?);
            }
            Ty::MaybeUninit(inner) => return self.layout(inner),
            Ty::Unit | Ty::Never => Layout { size: 0, align: 1 },
            Ty::Bool => Layout { size: 1, align: 1 },
            Ty::Char | Ty::TypeId => Layout { size: 4, align: 4 },
            Ty::Int { bits, .. } | Ty::Float(bits) => Layout {
                size: u64::from(*bits) / 8,
                align: u64::from(*bits) / 8,
            },
            Ty::Ptr(_) | Ty::Chan(_) | Ty::Join(_) => word,
            Ty::Ref(t) => {
                if matches!(**t, Ty::Slice(_)) {
                    Layout { size: 16, align: 8 }
                } else {
                    word
                }
            }
            Ty::Slice(_) => return Ok(None),
            Ty::String | Ty::Function(..) | Ty::Dyn(_) | Ty::Range => Layout { size: 16, align: 8 },
            Ty::Callable(id, _, _) => {
                if !self.capturing[id.module][id.function as usize] {
                    Layout { size: 0, align: 1 }
                } else {
                    Layout { size: 8, align: 8 }
                }
            }
            Ty::Array(elem, n) => {
                let Some(elem) = self.layout(elem)? else {
                    return Ok(None);
                };
                Layout {
                    size: elem
                        .size
                        .checked_mul(*n)
                        .ok_or_else(|| invalid("数组布局溢出"))?,
                    align: elem.align,
                }
            }
            Ty::Tuple(ts) => {
                let Some(layout) = self.aggregate(ts.iter(), AggregateKind::Ordered)? else {
                    return Ok(None);
                };
                layout
            }
            Ty::Option(t) => {
                let Some(payload) = self.layout(t)? else {
                    return Ok(None);
                };
                tagged(payload, DEFAULT_TAG)?
            }
            Ty::Result(t, e) => {
                let (Some(t), Some(e)) = (self.layout(t)?, self.layout(e)?) else {
                    return Ok(None);
                };
                tagged(
                    Layout {
                        size: t.size.max(e.size),
                        align: t.align.max(e.align),
                    },
                    DEFAULT_TAG,
                )?
            }
            Ty::Named(index, _) => return self.nominal_layout(ty, *index),
        }))
    }
    fn nominal_layout(&mut self, ty: &Ty, index: usize) -> Result<Option<Layout>, Diagnostic> {
        let nominal = &self.model.nominal[index];
        let repr = nominal.repr;
        let item = &self.model.modules[nominal.definition.module].arena.items
            [nominal.definition.item.0 as usize];
        let variants = self.model.variants(ty).expect("名义类型字段");
        if repr.tag.is_some() && !nominal.is_enum {
            return Err(invalid("整数 repr 只允许用于枚举"));
        }
        if repr.transparent() {
            if !matches!(item.kind, ItemKind::Struct { .. }) {
                return Err(invalid("transparent 只允许结构体或 newtype"));
            }
            let mut nonzero = None;
            for field in &variants[0].fields {
                let Some(layout) = self.layout(&field.ty)? else {
                    return Ok(None);
                };
                if layout.size != 0 && nonzero.replace(layout).is_some() {
                    return Err(invalid("transparent 必须恰好有一个非 ZST 字段"));
                }
            }
            let Some(layout) = nonzero else {
                return Err(invalid("transparent 必须恰好有一个非 ZST 字段"));
            };
            if repr.align > layout.align {
                return Err(invalid("transparent 不能改变唯一非 ZST 字段的对齐"));
            }
            return Ok(Some(layout));
        }
        let kind = match item.kind {
            ItemKind::Union { .. } => AggregateKind::Union,
            ItemKind::Struct { .. } if repr.packed() => AggregateKind::Packed,
            ItemKind::Struct { .. } | ItemKind::Enum { .. } => AggregateKind::Ordered,
            _ => return Err(invalid("布局要求名义类型声明")),
        };
        if nominal.is_enum && repr.packed() {
            return Err(invalid("packed 不能用于枚举"));
        }
        let mut layout = Layout { size: 0, align: 1 };
        for variant in &variants {
            let Some(payload) =
                self.aggregate(variant.fields.iter().map(|field| &field.ty), kind)?
            else {
                return Ok(None);
            };
            layout.size = layout.size.max(payload.size);
            layout.align = layout.align.max(payload.align);
        }
        if nominal.is_enum && !variants.is_empty() {
            let tag = repr.tag.map_or(DEFAULT_TAG, |(_, bits)| Layout {
                size: u64::from(bits) / 8,
                align: u64::from(bits) / 8,
            });
            layout = tagged(layout, tag)?;
        }
        layout.align = layout.align.max(repr.align);
        layout.size = align_up(layout.size, layout.align)?;
        Ok(Some(layout))
    }
    fn aggregate<'t>(
        &mut self,
        fields: impl Iterator<Item = &'t Ty>,
        kind: AggregateKind,
    ) -> Result<Option<Layout>, Diagnostic> {
        let mut total = Layout { size: 0, align: 1 };
        for ty in fields {
            let Some(layout) = self.layout(ty)? else {
                return Ok(None);
            };
            if !matches!(kind, AggregateKind::Ordered)
                && self.model.is_bit_type(ty, &self.semantics.hidden_types) == Some(false)
            {
                return Err(invalid("union 与 packed 字段必须是位类型"));
            }
            aggregate_field(&mut total, layout, kind)?;
        }
        total.size = align_up(total.size, total.align)?;
        Ok(Some(total))
    }
}
fn aggregate_field(
    total: &mut Layout,
    field: Layout,
    kind: AggregateKind,
) -> Result<u64, Diagnostic> {
    let align = if matches!(kind, AggregateKind::Packed) {
        1
    } else {
        field.align
    };
    let offset = if matches!(kind, AggregateKind::Union) {
        0
    } else {
        align_up(total.size, align)?
    };
    total.size = total.size.max(
        offset
            .checked_add(field.size)
            .ok_or_else(|| invalid("聚合布局溢出"))?,
    );
    total.align = total.align.max(align);
    Ok(offset)
}

fn tagged(payload: Layout, tag: Layout) -> Result<Layout, Diagnostic> {
    let align = payload.align.max(tag.align);
    let size = align_up(tag.size, payload.align)?
        .checked_add(payload.size)
        .ok_or_else(|| invalid("枚举布局溢出"))?;
    Ok(Layout {
        size: align_up(size, align)?,
        align,
    })
}
fn align_up(size: u64, align: u64) -> Result<u64, Diagnostic> {
    debug_assert!(align.is_power_of_two());
    size.checked_add(align - 1)
        .map(|s| s & !(align - 1))
        .ok_or_else(|| invalid("布局对齐溢出"))
}
fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
