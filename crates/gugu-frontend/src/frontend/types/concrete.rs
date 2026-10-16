//! 具体 GIR 复用布局器的字段推进规则，不在低层重算字段偏移。
use super::{
    AggregateKind, DEFAULT_TAG, Diagnostic, ItemKind, Layout, Layouts, Ty, aggregate_field,
    align_up, invalid,
};

pub(crate) struct AggregateLayout {
    pub(crate) tag: Option<Layout>,
    pub(crate) variants: Vec<Vec<(Ty, u64)>>,
}

impl Layouts<'_, '_> {
    pub(crate) fn aggregate_layout(&mut self, ty: &Ty) -> Result<AggregateLayout, Diagnostic> {
        let (tag, kind, variants) = match ty {
            Ty::Tuple(fields) => (None, AggregateKind::Ordered, vec![fields.clone()]),
            Ty::Range => (
                None,
                AggregateKind::Ordered,
                vec![vec![Ty::int(), Ty::int()]],
            ),
            Ty::Option(inner) => (
                Some(DEFAULT_TAG),
                AggregateKind::Ordered,
                vec![vec![(**inner).clone()], Vec::new()],
            ),
            Ty::Result(ok, error) => (
                Some(DEFAULT_TAG),
                AggregateKind::Ordered,
                vec![vec![(**ok).clone()], vec![(**error).clone()]],
            ),
            Ty::ChanClosed => (None, AggregateKind::Ordered, vec![Vec::new()]),
            Ty::TrySendErr | Ty::TryRecvErr => (
                Some(DEFAULT_TAG),
                AggregateKind::Ordered,
                vec![Vec::new(), Vec::new()],
            ),
            Ty::Named(index, _) => {
                let nominal = &self.model.nominal[*index];
                let repr = nominal.repr;
                let item = &self.model.modules[nominal.definition.module].arena.items
                    [usize::try_from(nominal.definition.item.0).expect("ItemId 适配宿主")];
                let kind = match item.kind {
                    ItemKind::Union { .. } => AggregateKind::Union,
                    _ if repr.packed() => AggregateKind::Packed,
                    _ => AggregateKind::Ordered,
                };
                let tag = nominal.is_enum.then(|| {
                    repr.tag.map_or(DEFAULT_TAG, |(_, bits)| Layout {
                        size: u64::from(bits) / 8,
                        align: u64::from(bits) / 8,
                    })
                });
                let variants = self
                    .model
                    .variants(ty)
                    .expect("已形成名义类型")
                    .into_iter()
                    .map(|variant| variant.fields.into_iter().map(|field| field.ty).collect())
                    .collect();
                (tag, kind, variants)
            }
            _ => return Err(invalid("具体字段布局要求聚合类型")),
        };
        let mut fields = Vec::with_capacity(variants.len());
        let mut payload_align = 1;
        for variant in variants {
            let mut total = Layout { size: 0, align: 1 };
            let mut offsets = Vec::with_capacity(variant.len());
            for ty in variant {
                let layout = self
                    .layout(&ty)?
                    .ok_or_else(|| invalid("具体字段仍有未知布局"))?;
                let offset = aggregate_field(&mut total, layout, kind)?;
                offsets.push((ty, offset));
            }
            payload_align = payload_align.max(total.align);
            fields.push(offsets);
        }
        if let Some(tag) = tag {
            let base = align_up(tag.size, payload_align)?;
            for (_, offset) in fields.iter_mut().flatten() {
                *offset = offset
                    .checked_add(base)
                    .ok_or_else(|| invalid("枚举字段偏移溢出"))?;
            }
        }
        Ok(AggregateLayout {
            tag,
            variants: fields,
        })
    }
}
