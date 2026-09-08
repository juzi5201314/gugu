//! 值传递类别、浅拷/COW/lease 展开计划与 `large_copy` lint。
//!
//! 类别按传递规范组合：位值、身份句柄、COW、ResourceCell。
//! 能力位固定 5 个，热路径用 `u8` 掩码；TypeId 稠密表按类型表长度预分配。
use super::GirWorldV1;
use super::body::LargeCopySite;
use crate::frontend::hir::{self, Location, TypeId};
use crate::frontend::{ParsedModule, attr};
use crate::source::{ExpansionId, SourceFileId, SourceMap};
use crate::{Diagnostic, DiagnosticCode, Severity};
use serde::{Deserialize, Serialize};

/// 位值 / 身份 / COW / 资源 / 未实例化泛型。可按字段并集。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PassingClass(u8);

impl PassingClass {
    pub(crate) const BITS: Self = Self(1);
    pub(crate) const IDENTITY: Self = Self(2);
    pub(crate) const COW: Self = Self(4);
    pub(crate) const RESOURCE: Self = Self(8);
    pub(crate) const UNKNOWN: Self = Self(16);

    pub(crate) fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) fn has_identity(self) -> bool {
        self.0 & Self::IDENTITY.0 != 0
    }

    pub(crate) fn has_cow(self) -> bool {
        self.0 & Self::COW.0 != 0
    }

    pub(crate) fn has_resource(self) -> bool {
        self.0 & Self::RESOURCE.0 != 0
    }

    pub(crate) fn is_unknown(self) -> bool {
        self.0 & Self::UNKNOWN.0 != 0
    }

    pub(crate) fn is_pure_bits(self) -> bool {
        self.0 == Self::BITS.0
    }

    pub(crate) fn is_pure_identity(self) -> bool {
        self.0 == Self::IDENTITY.0
    }

    pub(crate) fn is_pure_cow(self) -> bool {
        self.0 == Self::COW.0
    }

    pub(crate) fn is_pure_resource(self) -> bool {
        self.0 == Self::RESOURCE.0
    }

    pub(crate) fn heap_effect(self) -> bool {
        self.has_identity() || self.has_cow() || self.has_resource() || self.is_unknown()
    }
}

/// 按 TypeId 稠密缓存类别与尺寸；类型表长度即上界。
pub(crate) struct PassingTable {
    class: Vec<PassingClass>,
    size: Vec<Option<u64>>,
}

impl PassingTable {
    pub(crate) fn new(module: &hir::Module) -> Self {
        let n = module.types.len();
        let mut builder = Builder {
            class: vec![PassingClass(0); n],
            size: vec![None; n],
            ready: vec![0; n],
        };
        for index in 0..n {
            builder.ensure(module, TypeId(index as u32));
        }
        Self {
            class: builder.class,
            size: builder.size,
        }
    }

    pub(crate) fn class(&self, ty: TypeId) -> PassingClass {
        self.class
            .get(ty.index())
            .copied()
            .unwrap_or(PassingClass::UNKNOWN)
    }

    pub(crate) fn size(&self, ty: TypeId) -> Option<u64> {
        self.size.get(ty.index()).copied().flatten()
    }
}

struct Builder {
    class: Vec<PassingClass>,
    size: Vec<Option<u64>>,
    ready: Vec<u8>,
}

impl Builder {
    fn ensure(&mut self, module: &hir::Module, ty: TypeId) {
        let index = ty.index();
        if index >= self.ready.len() || self.ready[index] == 2 {
            return;
        }
        if self.ready[index] == 1 {
            self.class[index] = PassingClass::BITS;
            self.size[index] = None;
            self.ready[index] = 2;
            return;
        }
        self.ready[index] = 1;
        let (class, size) = classify_size(self, module, ty);
        self.class[index] = class;
        self.size[index] = size;
        self.ready[index] = 2;
    }

    fn of(&mut self, module: &hir::Module, ty: TypeId) -> (PassingClass, Option<u64>) {
        self.ensure(module, ty);
        (self.class[ty.index()], self.size[ty.index()])
    }
}

fn classify_size(
    table: &mut Builder,
    module: &hir::Module,
    ty: TypeId,
) -> (PassingClass, Option<u64>) {
    let Some(kind) = module.types.get(ty.index()) else {
        return (PassingClass::UNKNOWN, None);
    };
    match kind {
        hir::Type::Never | hir::Type::Unit => (PassingClass::BITS, Some(0)),
        hir::Type::Bool => (PassingClass::BITS, Some(1)),
        hir::Type::Char | hir::Type::TypeId => (PassingClass::BITS, Some(4)),
        hir::Type::Int { bits, .. } | hir::Type::Float(bits) => {
            (PassingClass::BITS, Some(u64::from(*bits) / 8))
        }
        hir::Type::Ptr(_) => (PassingClass::BITS, Some(8)),
        hir::Type::Range => (PassingClass::BITS, Some(16)),
        hir::Type::String => (PassingClass::COW, Some(16)),
        hir::Type::Ref(inner) => ref_class(table, module, *inner),
        hir::Type::Slice(_) => (PassingClass::IDENTITY, Some(16)),
        hir::Type::Chan(_) | hir::Type::Join(_) => (PassingClass::IDENTITY, Some(8)),
        hir::Type::Function { .. } | hir::Type::Dyn(_) => (PassingClass::IDENTITY, Some(16)),
        hir::Type::Callable { .. } => (PassingClass::IDENTITY, Some(8)),
        hir::Type::Array(elem, count) => array_class(table, module, *elem, *count),
        hir::Type::Tuple(fields) => tuple_class(table, module, fields),
        hir::Type::Option(inner) => option_class(table, module, *inner),
        hir::Type::Result(ok, err) => result_class(table, module, *ok, *err),
        hir::Type::MaybeUninit(inner) => table.of(module, *inner),
        hir::Type::Named { definition, .. } => named_class(table, module, *definition),
        hir::Type::Opaque { definition, .. } => opaque_class(table, module, *definition),
        hir::Type::Parameter { .. } | hir::Type::Projection { .. } => (PassingClass::UNKNOWN, None),
    }
}

fn ref_class(
    table: &mut Builder,
    module: &hir::Module,
    inner: TypeId,
) -> (PassingClass, Option<u64>) {
    table.ensure(module, inner);
    let fat = matches!(module.types.get(inner.index()), Some(hir::Type::Slice(_)));
    (PassingClass::IDENTITY, Some(if fat { 16 } else { 8 }))
}

fn array_class(
    table: &mut Builder,
    module: &hir::Module,
    elem: TypeId,
    count: u64,
) -> (PassingClass, Option<u64>) {
    let (class, size) = table.of(module, elem);
    let total = size.and_then(|size| size.checked_mul(count));
    if count == 0 {
        (PassingClass::BITS, Some(0))
    } else {
        (class, total)
    }
}

fn option_class(
    table: &mut Builder,
    module: &hir::Module,
    inner: TypeId,
) -> (PassingClass, Option<u64>) {
    let (class, size) = table.of(module, inner);
    (class.union(PassingClass::BITS), size.map(|size| size + 1))
}

fn result_class(
    table: &mut Builder,
    module: &hir::Module,
    ok: TypeId,
    err: TypeId,
) -> (PassingClass, Option<u64>) {
    let (left, left_size) = table.of(module, ok);
    let (right, right_size) = table.of(module, err);
    let size = match (left_size, right_size) {
        (Some(a), Some(b)) => Some(a.max(b) + 1),
        _ => None,
    };
    (left.union(right).union(PassingClass::BITS), size)
}

fn named_class(
    table: &mut Builder,
    module: &hir::Module,
    definition: hir::DefId,
) -> (PassingClass, Option<u64>) {
    if let Some(class) = lang_item(module, definition) {
        return (class, Some(8));
    }
    let Some(aggregate) = aggregate_of(module, definition) else {
        return (PassingClass::BITS, Some(0));
    };
    if aggregate.representation.flags & 4 != 0 {
        return transparent_class(table, module, aggregate);
    }
    let packed = aggregate.representation.flags & 2 != 0;
    let mut class = PassingClass::BITS;
    let mut size = 0u64;
    let mut known = true;
    let mut align = 1u64;
    for variant in &aggregate.variants {
        let (payload, payload_align, field_known) = variant_payload(table, module, variant, packed);
        class = class.union(variant_class(table, module, variant));
        known &= field_known;
        size = size.max(payload);
        align = align.max(payload_align);
    }
    if aggregate.variants.len() > 1 {
        size = size.saturating_add(1);
        class = class.union(PassingClass::BITS);
    }
    size = align_up(size, align.max(aggregate.representation.align.max(1)));
    (class, known.then_some(size))
}

fn variant_class(
    table: &mut Builder,
    module: &hir::Module,
    variant: &hir::Variant,
) -> PassingClass {
    let mut class = PassingClass::BITS;
    for field in &variant.fields {
        class = class.union(table.of(module, field.ty).0);
    }
    class
}

fn variant_payload(
    table: &mut Builder,
    module: &hir::Module,
    variant: &hir::Variant,
    packed: bool,
) -> (u64, u64, bool) {
    let mut payload = 0u64;
    let mut align = 1u64;
    let mut known = true;
    for field in &variant.fields {
        let field_size = match table.of(module, field.ty).1 {
            Some(size) => size,
            None => {
                known = false;
                continue;
            }
        };
        let field_align = if packed { 1 } else { align_of_size(field_size) };
        payload = align_up(payload, field_align).saturating_add(field_size);
        align = align.max(field_align);
    }
    (align_up(payload, align), align, known)
}

fn transparent_class(
    table: &mut Builder,
    module: &hir::Module,
    aggregate: &hir::Aggregate,
) -> (PassingClass, Option<u64>) {
    let Some(field) = aggregate
        .variants
        .first()
        .and_then(|variant| variant.fields.first())
    else {
        return (PassingClass::BITS, Some(0));
    };
    table.of(module, field.ty)
}

fn opaque_class(
    table: &mut Builder,
    module: &hir::Module,
    definition: hir::DefId,
) -> (PassingClass, Option<u64>) {
    let hidden = module
        .opaques
        .iter()
        .find(|opaque| opaque.definition == definition)
        .and_then(|opaque| opaque.hidden);
    match hidden {
        Some(ty) => table.of(module, ty),
        None => (PassingClass::UNKNOWN, None),
    }
}

fn tuple_class(
    table: &mut Builder,
    module: &hir::Module,
    fields: &[TypeId],
) -> (PassingClass, Option<u64>) {
    let mut class = PassingClass::BITS;
    let mut size = 0u64;
    let mut align = 1u64;
    let mut known = true;
    for &field in fields {
        let (field_class, field_size) = table.of(module, field);
        class = class.union(field_class);
        let Some(field_size) = field_size else {
            known = false;
            continue;
        };
        let field_align = align_of_size(field_size);
        size = align_up(size, field_align).saturating_add(field_size);
        align = align.max(field_align);
    }
    (class, known.then_some(align_up(size, align)))
}

fn lang_item(module: &hir::Module, definition: hir::DefId) -> Option<PassingClass> {
    let name = module.definitions.get(definition.index())?.name.as_str();
    Some(match name {
        "Vec" => PassingClass::IDENTITY,
        "ByteBuffer" | "Bytes" => PassingClass::COW,
        "ResourceCell" => PassingClass::RESOURCE,
        _ => return None,
    })
}

pub(crate) fn aggregate_of(
    module: &hir::Module,
    definition: hir::DefId,
) -> Option<&hir::Aggregate> {
    module
        .aggregates
        .iter()
        .find(|aggregate| aggregate.definition == definition)
}

pub(crate) fn struct_fields<'a>(module: &'a hir::Module, ty: TypeId) -> Option<&'a [hir::Field]> {
    match module.types.get(ty.index())? {
        hir::Type::Named { definition, .. } => {
            if lang_item(module, *definition).is_some() {
                return None;
            }
            let aggregate = aggregate_of(module, *definition)?;
            if aggregate.variants.len() == 1 && aggregate.representation.flags & 4 == 0 {
                Some(aggregate.variants[0].fields.as_slice())
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn tuple_fields(module: &hir::Module, ty: TypeId) -> Option<&[TypeId]> {
    match module.types.get(ty.index())? {
        hir::Type::Tuple(fields) if fields.len() > 1 => Some(fields.as_slice()),
        _ => None,
    }
}

fn align_of_size(size: u64) -> u64 {
    match size {
        0 => 1,
        1 => 1,
        2..=3 => 2,
        4..=7 => 4,
        _ => 8,
    }
}

fn align_up(size: u64, align: u64) -> u64 {
    debug_assert!(align > 0 && align.is_power_of_two());
    size.saturating_add(align - 1) & !(align - 1)
}

/// 超过该字节数的按值位结构体发出 `large_copy`。
pub(crate) const LARGE_COPY_BYTES: u64 = 64;

pub(crate) fn large_copy_lints(
    modules: &[ParsedModule],
    world: &GirWorldV1,
    sources: &SourceMap,
) -> Result<Vec<Diagnostic>, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    for body in &world.bodies {
        for site in &body.large_copies {
            if let Some(diagnostic) = lint_site(modules, sources, site) {
                diagnostics.push(diagnostic);
            }
        }
    }
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity() == Severity::Error)
    {
        Err(diagnostics)
    } else {
        Ok(diagnostics)
    }
}

fn lint_site(
    modules: &[ParsedModule],
    sources: &SourceMap,
    site: &LargeCopySite,
) -> Option<Diagnostic> {
    let file = SourceFileId::new(site.location.source);
    let parsed = modules.iter().find(|module| module.file.source == file)?;
    let snapshot = sources.snapshot(file)?;
    let level = attr::lint_level(
        snapshot.content(),
        parsed.file.inner_attributes,
        &parsed.arena,
        &parsed.tokens,
        &parsed.configured,
        site.location.start,
        attr::LARGE_COPY,
    );
    if matches!(level, attr::LintLevel::Allow) {
        return None;
    }
    let span = sources
        .span(
            file,
            site.location.start as usize,
            site.location.end as usize,
            ExpansionId::new(site.location.expansion),
        )
        .ok();
    let severity = match level {
        attr::LintLevel::Warn => Severity::Warning,
        attr::LintLevel::Deny | attr::LintLevel::Forbid => Severity::Error,
        attr::LintLevel::Allow => return None,
    };
    Some(Diagnostic::new(
        severity,
        DiagnosticCode::LargeCopy,
        format!(
            "按值传递 {} 字节的位结构体，超过 {LARGE_COPY_BYTES} 字节；可改为 `&T` 或 `#[allow(large_copy)]`",
            site.size
        ),
        span,
        u32::MAX,
    ))
}

pub(crate) fn site(location: Location, size: u64, ty: TypeId) -> LargeCopySite {
    LargeCopySite { location, size, ty }
}
