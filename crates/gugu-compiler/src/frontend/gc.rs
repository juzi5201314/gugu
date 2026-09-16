//! GC 元数据推导：从冻结类型表、具体 GIR layout 和真实根位置生成 metadata。

use std::collections::BTreeMap;

use crate::Diagnostic;
use crate::frontend::{
    gir,
    gir::passing::PassingClass,
    hir,
    late::universe::{MetadataShape, TypeUniverse},
};
use crate::runtime::gc_metadata_schema::{
    GcAllocSiteV1, GcArenaLayoutV1, GcGlueEntryV1, GcMetadataWorldV1, GcRootKindV1,
    GcRootLocationV1, GcRootRangeV1, GcSourceEntryV1, GcTypeEntryV1, GcVtableEntryV1,
    TRACE_BITMAP_MAX_WORDS, TraceKind, TraceOp, ValueOp, boot_verify, encode_uleb,
};

/// 编译期 GC metadata 两个镜像 section；world 是两段的唯一逻辑来源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GcMetadataBundle {
    pub(crate) world: GcMetadataWorldV1,
    pub(crate) type_section: Vec<u8>,
    pub(crate) metadata_section: Vec<u8>,
}

/// 从冻结类型表、具体 GIR 与 HIR source table 推导真实 GC metadata。
pub(crate) fn derive(
    universe: &TypeUniverse,
    gir: &gir::GirWorldV1,
    module: &hir::Module,
) -> Result<GcMetadataBundle, Diagnostic> {
    let mut trace_program = Vec::new();
    let mut value_program = Vec::new();
    let mut types = Vec::with_capacity(universe.records.len());
    let mut glue = Vec::new();
    let concrete_passing = gir
        .concrete
        .iter()
        .flat_map(|body| body.types.iter())
        .map(|layout| (layout.key, layout.passing.bits()))
        .collect::<BTreeMap<_, _>>();

    let mut encoder = ValueEncoder::new(universe);
    for record in &universe.records {
        let trace_offset = u32::try_from(trace_program.len()).expect("trace program 适配 u32");
        let trace = trace_for(record, universe)?;
        let trace_len = u32::try_from(trace.len()).expect("trace program 长度适配 u32");
        trace_program.extend_from_slice(&trace);
        let passing = concrete_passing
            .get(&record.key)
            .copied()
            .unwrap_or(record.passing);
        let value = encoder.program(record)?;
        let has_value_actions = passing & PassingClass::COW.bits() != 0
            || passing & PassingClass::RESOURCE.bits() != 0
            || value.len() > 1;
        let (value_offset, value_len) = if has_value_actions {
            let offset = u32::try_from(value_program.len()).expect("value program 适配 u32");
            let length = u32::try_from(value.len()).expect("value program 长度适配 u32");
            value_program.extend_from_slice(&value);
            (offset, length)
        } else {
            (0, 0)
        };
        let (has_direct, has_interior) = trace_flags(&record.metadata, universe)?;
        let mut flags = 0u8;
        if has_direct {
            flags |= 1;
        }
        if has_interior {
            flags |= 1 << 1;
        }
        if has_value_actions {
            flags |= 1 << 2;
            glue.push(GcGlueEntryV1 {
                type_key: record.key,
                copy_rva: 0,
                release_rva: 0,
            });
        }
        if passing & PassingClass::RESOURCE.bits() != 0 {
            flags |= 1 << 3;
            flags |= 1 << 4;
        }
        if record.layout.is_none() {
            flags |= 1 << 5;
        }
        if matches!(&record.metadata, MetadataShape::String) {
            flags |= 1 << 6;
        }
        types.push(GcTypeEntryV1 {
            type_key: record.key,
            canonical: record.canonical.clone(),
            name: record.name.clone(),
            layout: record.layout,
            children: record.children.clone(),
            flags,
            trace_offset,
            value_offset,
            trace_len,
            value_len,
        });
    }

    let vtables = universe
        .vtables
        .iter()
        .map(|vtable| {
            let concrete_type = universe
                .records
                .get(vtable.concrete as usize)
                .map(|record| record.key)
                .ok_or_else(|| invalid("vtable 具体类型引用越界"))?;
            Ok(GcVtableEntryV1 {
                interface: vtable.interface,
                concrete_type,
            })
        })
        .collect::<Result<Vec<_>, Diagnostic>>()?;
    let (roots, sources) = roots(universe, gir, module)?;
    let alloc_sites = alloc_sites(universe, gir, module)?;
    let world = GcMetadataWorldV1 {
        types,
        vtables,
        trace_program,
        value_program,
        glue,
        roots,
        sources,
        alloc_sites,
        arena: GcArenaLayoutV1 {
            arena_bytes: 2 * 1024 * 1024,
            block_bytes: 32 * 1024,
            line_bytes: 128,
        },
        schema: GcMetadataWorldV1::SCHEMA,
    };
    boot_verify(&world).map_err(|error| invalid(error.message()))?;
    let (type_section, metadata_section) =
        crate::runtime::gc_metadata_section::encode_sections(&world)
            .map_err(|error| invalid(error.message()))?;
    Ok(GcMetadataBundle {
        world,
        type_section,
        metadata_section,
    })
}

/// 生成一个类型的 trace descriptor：kind 字节加表示负载，并在等长时优先 Bitmap。
fn trace_for(
    record: &crate::frontend::late::universe::TypeRecord,
    universe: &TypeUniverse,
) -> Result<Vec<u8>, Diagnostic> {
    let program = trace_program_for(&record.metadata, 0, universe)?;
    let flat = flat_trace(record, universe)?;
    match flat {
        FlatTrace::NoPointers => Ok(vec![TraceKind::None as u8]),
        FlatTrace::Words {
            word_count,
            direct,
            interior,
        } => {
            let bitmap = encode_trace_bitmap(word_count, &direct, &interior);
            let encoded_program = encode_trace_program(&program);
            // 编码等长时优先 Bitmap，因此这里用 `<=` 而不是 `<`。
            if bitmap.len() <= encoded_program.len() {
                Ok(bitmap)
            } else {
                Ok(encoded_program)
            }
        }
        FlatTrace::NotFlat => Ok(encode_trace_program(&program)),
    }
}

/// 扁平 word 位图可行性；`NoPointers` 表示类型没有 tracked managed pointer。
enum FlatTrace {
    NoPointers,
    NotFlat,
    Words {
        word_count: u32,
        /// 直接指针所在的 payload word 下标。
        direct: Vec<u32>,
        /// interior 指针所在的 payload word 下标。
        interior: Vec<u32>,
    },
}

/// 尝试把类型的 trace 压成扁平 word 位图；带判别值、动态长度或超过 256 word 时返回 `NotFlat`。
fn flat_trace(
    record: &crate::frontend::late::universe::TypeRecord,
    universe: &TypeUniverse,
) -> Result<FlatTrace, Diagnostic> {
    let Some((size, _)) = record.layout else {
        return Ok(FlatTrace::NotFlat);
    };
    let word_count = size.div_ceil(8);
    if word_count == 0 || word_count > u64::from(TRACE_BITMAP_MAX_WORDS) {
        return Ok(FlatTrace::NotFlat);
    }
    let mut words = FlatWords {
        word_count: u32::try_from(word_count).expect("bitmap word 数适配 u32"),
        direct: Vec::new(),
        interior: Vec::new(),
    };
    if !collect_flat_words(&record.metadata, 0, universe, &mut words)? {
        return Ok(FlatTrace::NotFlat);
    }
    if words.direct.is_empty() && words.interior.is_empty() {
        return Ok(FlatTrace::NoPointers);
    }
    Ok(FlatTrace::Words {
        word_count: words.word_count,
        direct: words.direct,
        interior: words.interior,
    })
}

struct FlatWords {
    word_count: u32,
    direct: Vec<u32>,
    interior: Vec<u32>,
}

impl FlatWords {
    fn push(&mut self, word: u64, interior: bool) -> Result<(), Diagnostic> {
        let word = u32::try_from(word).map_err(|_| invalid("trace word 下标溢出"))?;
        if word >= self.word_count {
            return Err(invalid("trace word 下标超出 payload"));
        }
        let target = if interior {
            &mut self.interior
        } else {
            &mut self.direct
        };
        if !target.contains(&word) {
            target.push(word);
        }
        Ok(())
    }
}

/// 收集定长对象的 pointer word；返回 `false` 表示无法扁平表示。
fn collect_flat_words(
    shape: &MetadataShape,
    base: u64,
    universe: &TypeUniverse,
    words: &mut FlatWords,
) -> Result<bool, Diagnostic> {
    match shape {
        MetadataShape::None => Ok(true),
        MetadataShape::Direct(_) => {
            words.push(base / 8, false)?;
            Ok(true)
        }
        MetadataShape::Interior(_) | MetadataShape::String => {
            words.push(base / 8, true)?;
            Ok(true)
        }
        MetadataShape::Array { element, count } => {
            let child = universe.record(element)?;
            let Some((stride, _)) = child.layout else {
                return Ok(false);
            };
            for index in 0..*count {
                if !collect_flat_words(
                    &child.metadata,
                    base.saturating_add(stride.saturating_mul(index)),
                    universe,
                    words,
                )? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        MetadataShape::Aggregate { tag, variants } => {
            if tag.is_some() {
                return Ok(false);
            }
            for variant in variants {
                for (child, offset) in variant {
                    let record = universe.record(child)?;
                    if !collect_flat_words(
                        &record.metadata,
                        base.saturating_add(*offset),
                        universe,
                        words,
                    )? {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        }
    }
}

/// 编码 Bitmap 表示：`kind, reserved[3], word_count u32, direct, interior, padding-to-4`。
fn encode_trace_bitmap(word_count: u32, direct: &[u32], interior: &[u32]) -> Vec<u8> {
    let bitmap_bytes = usize::try_from(word_count.div_ceil(8)).expect("bitmap 字节数适配宿主");
    let body = 8 + bitmap_bytes * 2;
    let length = body.next_multiple_of(4);
    let mut out = vec![0u8; length];
    out[0] = TraceKind::Bitmap as u8;
    out[4..8].copy_from_slice(&word_count.to_le_bytes());
    for word in direct {
        out[8 + (word / 8) as usize] |= 1 << (word % 8);
    }
    for word in interior {
        out[8 + bitmap_bytes + (word / 8) as usize] |= 1 << (word % 8);
    }
    out
}

/// 编码 Program 表示：`kind, u32 program_len, program`。
fn encode_trace_program(program: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(program.len() + 5);
    out.push(TraceKind::Program as u8);
    out.extend_from_slice(
        &u32::try_from(program.len())
            .expect("trace program 长度适配 u32")
            .to_le_bytes(),
    );
    out.extend_from_slice(program);
    out
}

fn trace_flags(shape: &MetadataShape, universe: &TypeUniverse) -> Result<(bool, bool), Diagnostic> {
    Ok(match shape {
        MetadataShape::None => (false, false),
        MetadataShape::Direct(_) => (true, false),
        MetadataShape::Interior(_) | MetadataShape::String => (false, true),
        MetadataShape::Array { element, .. } => {
            trace_flags(&universe.record(element)?.metadata, universe)?
        }
        MetadataShape::Aggregate { variants, .. } => {
            let mut flags = (false, false);
            for variant in variants {
                for (child, _) in variant {
                    let child_flags = trace_flags(&universe.record(child)?.metadata, universe)?;
                    flags.0 |= child_flags.0;
                    flags.1 |= child_flags.1;
                }
            }
            flags
        }
    })
}

/// 生成一条 trace program（不含 kind 与长度前缀）。
fn trace_program_for(
    shape: &MetadataShape,
    base: u64,
    universe: &TypeUniverse,
) -> Result<Vec<u8>, Diagnostic> {
    let mut out = Vec::new();
    emit_trace(shape, base, universe, &mut out)?;
    out.push(TraceOp::End as u8);
    Ok(out)
}

fn emit_trace(
    shape: &MetadataShape,
    base: u64,
    universe: &TypeUniverse,
    out: &mut Vec<u8>,
) -> Result<(), Diagnostic> {
    match shape {
        MetadataShape::None => {}
        MetadataShape::Direct(_) => {
            out.push(TraceOp::Direct as u8);
            encode_uleb(out, base / 8);
            encode_uleb(out, 1);
        }
        MetadataShape::Interior(_) | MetadataShape::String => {
            out.push(TraceOp::Interior as u8);
            encode_uleb(out, base / 8);
            encode_uleb(out, 1);
        }
        MetadataShape::Array { element, count } => {
            let child = universe.record(element)?;
            let body = trace_program_for(&child.metadata, 0, universe)?;
            if body.len() > 1 && *count > 0 {
                out.push(TraceOp::Repeat as u8);
                encode_uleb(out, base / 8);
                encode_uleb(out, *count);
                encode_uleb(out, child.layout.map_or(0, |layout| layout.0 / 8));
                out.extend_from_slice(
                    &u32::try_from(body.len())
                        .expect("trace body 适配 u32")
                        .to_le_bytes(),
                );
                out.extend_from_slice(&body);
            }
        }
        MetadataShape::Aggregate {
            tag: None,
            variants,
        } => {
            for variant in variants {
                for (child, offset) in variant {
                    let record = universe.record(child)?;
                    emit_trace(&record.metadata, base + *offset, universe, out)?;
                }
            }
        }
        MetadataShape::Aggregate {
            tag: Some((tag_offset, width)),
            variants,
        } => {
            let mut bodies = Vec::with_capacity(variants.len());
            for variant in variants {
                let body = trace_program_for_variant(variant, base, universe)?;
                bodies.push(body);
            }
            out.push(TraceOp::Switch as u8);
            // tag 操作数以 payload 字节计，判别值按 8 字节小端记录。
            encode_uleb(out, base + *tag_offset);
            encode_uleb(out, u64::from(*width));
            encode_uleb(
                out,
                u64::try_from(bodies.len()).expect("trace case 数量适配 uleb"),
            );
            for (index, body) in bodies.iter().enumerate() {
                out.extend_from_slice(&(index as u64).to_le_bytes());
                out.extend_from_slice(
                    &u32::try_from(body.len())
                        .expect("trace case 适配 u32")
                        .to_le_bytes(),
                );
                out.extend_from_slice(body);
            }
            // 前端只产生稠密判别值；未命中任何 case 的 tag 属于违反类型系统，因此 default
            // 为空动作而不是复述 variant 0，避免在非法 tag 上扫描无关字节。
            out.extend_from_slice(&1u32.to_le_bytes());
            out.push(TraceOp::End as u8);
        }
    }
    Ok(())
}

fn trace_program_for_variant(
    variant: &[([u8; 32], u64)],
    base: u64,
    universe: &TypeUniverse,
) -> Result<Vec<u8>, Diagnostic> {
    let mut body = Vec::new();
    for (child, offset) in variant {
        let record = universe.record(child)?;
        emit_trace(&record.metadata, base + *offset, universe, &mut body)?;
    }
    body.push(TraceOp::End as u8);
    Ok(body)
}

/// value program 生成器：按类型记忆化，避免共享子类型重复展开。
struct ValueEncoder<'a> {
    universe: &'a TypeUniverse,
    cache: BTreeMap<[u8; 32], Vec<u8>>,
}

impl<'a> ValueEncoder<'a> {
    fn new(universe: &'a TypeUniverse) -> Self {
        Self {
            universe,
            cache: BTreeMap::new(),
        }
    }

    /// 返回类型自身的 value program：正向类动作在前、逆向类动作在后。
    fn program(
        &mut self,
        record: &crate::frontend::late::universe::TypeRecord,
    ) -> Result<Vec<u8>, Diagnostic> {
        if let Some(program) = self.cache.get(&record.key) {
            return Ok(program.clone());
        }
        let mut out = Vec::new();
        self.emit(&record.metadata, 0, ValueDirection::Forward, &mut out)?;
        self.emit(&record.metadata, 0, ValueDirection::Backward, &mut out)?;
        out.push(ValueOp::End as u8);
        self.cache.insert(record.key, out.clone());
        Ok(out)
    }

    /// 类型是否需要语义 copy/drop：COW/resource 类别或 program 含结构动作。
    fn has_actions(
        &mut self,
        record: &crate::frontend::late::universe::TypeRecord,
    ) -> Result<bool, Diagnostic> {
        if record.passing & PassingClass::COW.bits() != 0
            || record.passing & PassingClass::RESOURCE.bits() != 0
        {
            return Ok(true);
        }
        Ok(self.program(record)?.len() > 1)
    }

    fn emit(
        &mut self,
        shape: &MetadataShape,
        base: u64,
        direction: ValueDirection,
        out: &mut Vec<u8>,
    ) -> Result<(), Diagnostic> {
        match shape {
            // 指针与句柄类型自身的类别动作由引用它的字段指令表达，这里没有结构动作。
            MetadataShape::None
            | MetadataShape::Direct(_)
            | MetadataShape::Interior(_)
            | MetadataShape::String => Ok(()),
            MetadataShape::Array { element, count } => {
                let child = self.universe.record(element)?;
                let mut body = Vec::new();
                self.field_action(child, 0, direction, &mut body)?;
                body.push(ValueOp::End as u8);
                if body.len() > 1 {
                    out.push(ValueOp::RepeatValue as u8);
                    encode_uleb(out, base / 8);
                    encode_uleb(out, *count);
                    encode_uleb(out, child.layout.map_or(0, |layout| layout.0 / 8));
                    out.extend_from_slice(
                        &u32::try_from(body.len())
                            .expect("value body 适配 u32")
                            .to_le_bytes(),
                    );
                    out.extend_from_slice(&body);
                }
                Ok(())
            }
            MetadataShape::Aggregate {
                tag: None,
                variants,
            } => {
                for variant in variants {
                    let mut ordered = variant.iter().collect::<Vec<_>>();
                    if direction == ValueDirection::Backward {
                        ordered.reverse();
                    }
                    for (child, offset) in ordered {
                        let record = self.universe.record(child)?;
                        self.field_action(record, base + *offset, direction, out)?;
                    }
                }
                Ok(())
            }
            MetadataShape::Aggregate {
                tag: Some((tag_offset, width)),
                variants,
            } => {
                let mut bodies = Vec::with_capacity(variants.len());
                for variant in variants {
                    let mut body = Vec::new();
                    let mut ordered = variant.iter().collect::<Vec<_>>();
                    if direction == ValueDirection::Backward {
                        ordered.reverse();
                    }
                    for (child, offset) in ordered {
                        let record = self.universe.record(child)?;
                        self.field_action(record, base + *offset, direction, &mut body)?;
                    }
                    body.push(ValueOp::End as u8);
                    bodies.push(body);
                }
                if bodies.iter().any(|body| body.len() > 1) {
                    out.push(ValueOp::SwitchValue as u8);
                    encode_uleb(out, base + *tag_offset);
                    encode_uleb(out, u64::from(*width));
                    encode_uleb(
                        out,
                        u64::try_from(bodies.len()).expect("value case 数量适配 uleb"),
                    );
                    for (index, body) in bodies.iter().enumerate() {
                        out.extend_from_slice(&(index as u64).to_le_bytes());
                        out.extend_from_slice(
                            &u32::try_from(body.len())
                                .expect("value case 适配 u32")
                                .to_le_bytes(),
                        );
                        out.extend_from_slice(body);
                    }
                    out.extend_from_slice(&1u32.to_le_bytes());
                    out.push(ValueOp::End as u8);
                }
                Ok(())
            }
        }
    }

    /// 发出一个字段的类别动作：resource 走 acquire/release，COW 先 publish，其余按 copy/drop。
    fn field_action(
        &mut self,
        child: &crate::frontend::late::universe::TypeRecord,
        offset: u64,
        direction: ValueDirection,
        out: &mut Vec<u8>,
    ) -> Result<(), Diagnostic> {
        let index = self
            .universe
            .type_id(&child.key)
            .ok_or_else(|| invalid("字段类型不在冻结 GC 类型表"))?;
        if child.passing & PassingClass::RESOURCE.bits() != 0 {
            out.push(match direction {
                ValueDirection::Forward => ValueOp::AcquireResource as u8,
                ValueDirection::Backward => ValueOp::ReleaseResource as u8,
            });
            encode_uleb(out, offset);
            encode_uleb(out, u64::from(index));
            return Ok(());
        }
        if !self.has_actions(child)? {
            return Ok(());
        }
        match direction {
            ValueDirection::Forward => {
                if child.passing & PassingClass::COW.bits() != 0 {
                    out.push(ValueOp::PublishField as u8);
                    encode_uleb(out, offset);
                    encode_uleb(out, u64::from(index));
                }
                out.push(ValueOp::CopyField as u8);
                encode_uleb(out, offset);
                encode_uleb(out, u64::from(index));
            }
            ValueDirection::Backward => {
                out.push(ValueOp::DropField as u8);
                encode_uleb(out, offset);
                encode_uleb(out, u64::from(index));
            }
        }
        Ok(())
    }
}

/// value 动作方向；wrapper body 内必须同类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueDirection {
    Forward,
    Backward,
}

fn roots(
    universe: &TypeUniverse,
    gir: &gir::GirWorldV1,
    module: &hir::Module,
) -> Result<(Vec<GcRootRangeV1>, Vec<GcSourceEntryV1>), Diagnostic> {
    // 每个真正承载 managed 值的具体 local 都是一个 coroutine-local root；
    // 同一类型的多个 local 各自保留独立 word 范围，不做按类型的去重。
    let mut candidates = Vec::<(u32, u64, [u8; 32], GcSourceEntryV1)>::new();
    for body in &gir.concrete {
        for local in &body.body.locals {
            let layout = body
                .types
                .get(local.ty.index())
                .ok_or_else(|| invalid("具体 local 的 GC 类型索引越界"))?;
            // `Never`/`MaybeUninit` 等类型不进入冻结 GC 类型表，也没有 TypeId；
            // 它们既不会出现在 type entry 中，也不可能是 GC root。
            let Some(type_id) = universe.type_id(&layout.key) else {
                continue;
            };
            let record = &universe.records[type_id as usize];
            let passing = layout.passing.bits();
            if passing == PassingClass::BITS.bits()
                && matches!(&record.metadata, MetadataShape::None)
            {
                continue;
            }
            let size = record.layout.map_or(8, |layout| layout.0.max(8));
            let location = body
                .body
                .source_scopes
                .get(local.source_scope.index())
                .ok_or_else(|| invalid("具体 local 的来源作用域越界"))?
                .location
                .clone();
            let path = module
                .sources
                .get(location.source as usize)
                .map_or_else(String::new, |source| source.path.clone());
            candidates.push((
                type_id,
                size,
                layout.key,
                GcSourceEntryV1 {
                    type_key: layout.key,
                    source_path: path,
                    byte_offset: u64::from(location.start),
                },
            ));
        }
    }
    candidates.sort_by_key(|(type_id, _, key, _)| (*type_id, *key));
    let mut roots = Vec::with_capacity(candidates.len());
    let mut sources = Vec::with_capacity(candidates.len());
    // coroutine-local layout 的字节偏移：按 `(TypeId, size)` 顺序紧密排布，
    // 与 stack map 的 slot 编号同源，二者都只描述逻辑世界。
    let mut word = 0u32;
    for (type_id, size, _, source) in candidates {
        let words = u32::try_from(size.div_ceil(8)).expect("root words 适配 u32");
        roots.push(GcRootRangeV1 {
            kind: GcRootKindV1::CoroutineFrame,
            location: GcRootLocationV1::Aggregate {
                offset_bytes: u64::from(word) * 8,
            },
            type_range: (type_id, type_id + 1),
            word_range: (word, word + words),
        });
        word = word
            .checked_add(words)
            .ok_or_else(|| invalid("root word 范围溢出"))?;
        sources.push(source);
    }
    sources.sort_by(|left, right| {
        (left.type_key, left.source_path.as_bytes(), left.byte_offset).cmp(&(
            right.type_key,
            right.source_path.as_bytes(),
            right.byte_offset,
        ))
    });
    sources.dedup();
    Ok((roots, sources))
}

fn alloc_sites(
    universe: &TypeUniverse,
    gir: &gir::GirWorldV1,
    module: &hir::Module,
) -> Result<Vec<GcAllocSiteV1>, Diagnostic> {
    let mut sites = BTreeMap::new();
    for body in &gir.concrete {
        for statement in &body.body.statements {
            let (type_id, location) = match &statement.kind {
                gir::body::StatementKind::Assign(_, gir::body::Rvalue::AllocObject { ty, .. }) => {
                    (*ty, statement.source.location.clone())
                }
                gir::body::StatementKind::Assign(
                    _,
                    gir::body::Rvalue::AllocArray { element, .. },
                ) => (*element, statement.source.location.clone()),
                _ => continue,
            };
            let layout = body
                .types
                .get(type_id.index())
                .ok_or_else(|| invalid("分配站点类型索引越界"))?;
            let key = layout.key;
            let path = module
                .sources
                .get(location.source as usize)
                .map_or_else(String::new, |source| source.path.clone());
            sites.insert(
                (key, path.clone(), u64::from(location.start)),
                GcAllocSiteV1 {
                    type_key: key,
                    location: GcSourceEntryV1 {
                        type_key: key,
                        source_path: path,
                        byte_offset: u64::from(location.start),
                    },
                },
            );
            let _ = universe
                .type_id(&key)
                .ok_or_else(|| invalid("分配站点类型不在冻结 GC 类型表"))?;
        }
    }
    Ok(sites.into_values().collect())
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(crate::DiagnosticCode::LateComptime, message, None)
}

#[cfg(test)]
mod tests {
    //! GC metadata 推导的确定性回归：program 字节、section 自洽与根保真。
    //!
    //! 只消费进程内的冻结前端产物，不读镜像、不启动子进程。

    use super::*;
    use crate::frontend::gir::tests::compile_frontend;

    fn derive_source(source: &str) -> GcMetadataBundle {
        let output = compile_frontend(source);
        derive(&output.mono.universe, &output.gir, output.hir.module()).expect("GC metadata 可推导")
    }

    /// 取某类型 trace descriptor 中第一条 SWITCH 的 `(tag_byte_offset, tag_width)`。
    fn switch_tag(entry: &GcTypeEntryV1, world: &GcMetadataWorldV1) -> (u64, u64) {
        let descriptor = &world.trace_program
            [entry.trace_offset as usize..(entry.trace_offset + entry.trace_len) as usize];
        assert_eq!(
            descriptor[0],
            TraceKind::Program as u8,
            "含判别值的聚合必须是 Program 表示"
        );
        let program = &descriptor[5..];
        let switch = program
            .iter()
            .position(|byte| *byte == TraceOp::Switch as u8)
            .expect("带判别值的聚合必须发出 SWITCH");
        let mut index = switch + 1;
        let tag_byte_offset = crate::runtime::gc_metadata_schema::decode_uleb(program, &mut index)
            .expect("tag 字节偏移可解码");
        let tag_width = crate::runtime::gc_metadata_schema::decode_uleb(program, &mut index)
            .expect("tag width 可解码");
        (tag_byte_offset, tag_width)
    }

    #[test]
    fn trace_program_uses_byte_units_for_switch_tag() {
        // `Holder` 的 `Option[int]` 字段位于字节偏移 8。SWITCH 的 tag 操作数按 payload
        // 字节计，因此必须是 8；若误用 word 单位就会得到 1，runtime 将按错误基准读判别值。
        let bundle = derive_source(
            "struct Holder { head: uint, body: Option[int] }\nfn main() {\n let value = Holder { head: 1, body: Option.Some(2) }\n _ = value\n }",
        );
        let holder = bundle
            .world
            .types
            .iter()
            .find(|entry| entry.name == "Holder")
            .expect("Holder 必须进入冻结类型表");
        let (tag_byte_offset, tag_width) = switch_tag(holder, &bundle.world);
        assert_eq!(
            tag_byte_offset, 8,
            "SWITCH tag 必须以 payload 字节计（字节偏移 8）"
        );
        assert_eq!(tag_width, 1, "Option 判别值宽度为 1 字节");

        // 顶层聚合（基址 0）与嵌套聚合共用同一单位规则。
        let option = bundle
            .world
            .types
            .iter()
            .find(|entry| entry.name == "Option[int]")
            .expect("Option[int] 必须进入冻结类型表");
        assert_eq!(
            switch_tag(option, &bundle.world).0,
            0,
            "顶层 tag 位于字节 0"
        );
    }

    #[test]
    fn derived_world_round_trips_through_sections() {
        let bundle = derive_source(
            "struct ResourceCell { id: uint }\nfn main() {\n let value = ResourceCell { id: 1 }\n _ = value\n }",
        );
        assert!(!bundle.world.types.is_empty());
        // 真实 program 不是单字节占位：类型数之外还必须有指令字节。
        assert!(
            bundle.world.trace_program.len() > bundle.world.types.len(),
            "trace program 必须含逐类型指令而非单字节 END"
        );
        crate::runtime::gc_metadata_section::verify_sections(
            &bundle.type_section,
            &bundle.metadata_section,
        )
        .expect("推导出的 section 必须自洽");
        assert_eq!(&bundle.type_section[..8], b"GUGUTY01");
        assert_eq!(&bundle.metadata_section[..8], b"GUGUMT01");
    }

    #[test]
    fn derive_is_deterministic_across_runs() {
        let source = "struct ResourceCell { id: uint }\nfn main() {\n let value = ResourceCell { id: 1 }\n _ = value\n }";
        let first = derive_source(source);
        let second = derive_source(source);
        assert_eq!(first.type_section, second.type_section);
        assert_eq!(first.metadata_section, second.metadata_section);
        assert_eq!(first.world.fingerprint(), second.world.fingerprint());
    }
}
