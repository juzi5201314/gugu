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
    GcRootLocationV1, GcRootRangeV1, GcSourceEntryV1, GcTypeEntryV1, GcVtableEntryV1, TraceOp,
    ValueOp, boot_verify, encode_uleb,
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

    for record in &universe.records {
        let trace_offset = u32::try_from(trace_program.len()).expect("trace program 适配 u32");
        let trace = trace_for(record, universe)?;
        let trace_len = u32::try_from(trace.len()).expect("trace program 长度适配 u32");
        trace_program.extend_from_slice(&trace);
        let passing = concrete_passing
            .get(&record.key)
            .copied()
            .unwrap_or(record.passing);
        let value_offset = u32::try_from(value_program.len()).expect("value program 适配 u32");
        let value = value_for(record, passing, universe)?;
        let value_len = u32::try_from(value.len()).expect("value program 长度适配 u32");
        value_program.extend_from_slice(&value);
        let (has_direct, has_interior) = trace_flags(&record.metadata, universe)?;
        let mut flags = 0u8;
        if has_direct {
            flags |= 1;
        }
        if has_interior {
            flags |= 1 << 1;
        }
        if value.len() > 1 {
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
            value_offset: if value.len() > 1 { value_offset } else { 0 },
            trace_len,
            value_len: if value.len() > 1 { value_len } else { 0 },
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

fn trace_for(
    record: &crate::frontend::late::universe::TypeRecord,
    universe: &TypeUniverse,
) -> Result<Vec<u8>, Diagnostic> {
    let mut out = Vec::new();
    emit_trace(&record.metadata, 0, universe, &mut out)?;
    out.push(TraceOp::End as u8);
    Ok(out)
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
            let mut body = Vec::new();
            emit_trace(&child.metadata, 0, universe, &mut body)?;
            body.push(TraceOp::End as u8);
            if body.len() > 1 && *count > 0 {
                out.push(TraceOp::Repeat as u8);
                encode_uleb(out, base / 8);
                encode_uleb(out, *count);
                encode_uleb(out, child.layout.map_or(0, |layout| layout.0 / 8));
                encode_uleb(
                    out,
                    u64::try_from(body.len()).expect("trace body 适配 uleb"),
                );
                out.extend_from_slice(&body);
            }
        }
        MetadataShape::Aggregate { tag, variants } => {
            if let Some((tag_offset, width)) = tag {
                let mut bodies = Vec::with_capacity(variants.len());
                for variant in variants {
                    let mut body = Vec::new();
                    for (child, offset) in variant {
                        let record = universe.record(child)?;
                        emit_trace(&record.metadata, base + *offset, universe, &mut body)?;
                    }
                    body.push(TraceOp::End as u8);
                    bodies.push(body);
                }
                let default = bodies
                    .first()
                    .cloned()
                    .unwrap_or_else(|| vec![TraceOp::End as u8]);
                out.push(TraceOp::Switch as u8);
                // trace program 的地址与偏移一律以 8 字节 word 计。
                encode_uleb(out, (base + *tag_offset) / 8);
                encode_uleb(out, u64::from(*width));
                encode_uleb(
                    out,
                    u64::try_from(default.len()).expect("trace default 适配 uleb"),
                );
                encode_uleb(
                    out,
                    u64::try_from(bodies.len()).expect("trace case 数量适配 uleb"),
                );
                for body in &bodies {
                    encode_uleb(
                        out,
                        u64::try_from(body.len()).expect("trace case 适配 uleb"),
                    );
                }
                out.extend_from_slice(&default);
                for body in bodies {
                    out.extend_from_slice(&body);
                }
            } else {
                for variant in variants {
                    for (child, offset) in variant {
                        let record = universe.record(child)?;
                        emit_trace(&record.metadata, base + *offset, universe, out)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn value_for(
    record: &crate::frontend::late::universe::TypeRecord,
    passing: u8,
    universe: &TypeUniverse,
) -> Result<Vec<u8>, Diagnostic> {
    let mut out = Vec::new();
    emit_value(&record.metadata, passing, 0, universe, &mut out)?;
    out.push(ValueOp::End as u8);
    Ok(out)
}

fn emit_value(
    shape: &MetadataShape,
    passing: u8,
    base: u64,
    universe: &TypeUniverse,
    out: &mut Vec<u8>,
) -> Result<(), Diagnostic> {
    if passing & PassingClass::RESOURCE.bits() != 0 {
        out.push(ValueOp::AcquireResource as u8);
        encode_uleb(out, base);
        out.push(ValueOp::ReleaseResource as u8);
        encode_uleb(out, base);
        return Ok(());
    }
    if passing & PassingClass::COW.bits() != 0 {
        out.push(ValueOp::CowPublish as u8);
        encode_uleb(out, base);
    }
    match shape {
        MetadataShape::Array { element, count } => {
            let child = universe.record(element)?;
            let mut body = Vec::new();
            emit_value(&child.metadata, child.passing, 0, universe, &mut body)?;
            body.push(ValueOp::End as u8);
            if body.len() > 1 && *count > 0 {
                out.push(ValueOp::RepeatValue as u8);
                encode_uleb(out, base);
                encode_uleb(out, *count);
                encode_uleb(out, child.layout.map_or(0, |layout| layout.0));
                encode_uleb(
                    out,
                    u64::try_from(body.len()).expect("value body 适配 uleb"),
                );
                out.extend_from_slice(&body);
            }
        }
        MetadataShape::Aggregate { tag, variants } => {
            if let Some((tag_offset, width)) = tag {
                let mut bodies = Vec::with_capacity(variants.len());
                for variant in variants {
                    let mut body = Vec::new();
                    for (child, offset) in variant {
                        let record = universe.record(child)?;
                        emit_value(
                            &record.metadata,
                            record.passing,
                            base + *offset,
                            universe,
                            &mut body,
                        )?;
                    }
                    body.push(ValueOp::End as u8);
                    bodies.push(body);
                }
                out.push(ValueOp::SwitchValue as u8);
                encode_uleb(out, base + *tag_offset);
                encode_uleb(out, u64::from(*width));
                encode_uleb(
                    out,
                    u64::try_from(bodies.len()).expect("value case 数量适配 uleb"),
                );
                for body in &bodies {
                    encode_uleb(
                        out,
                        u64::try_from(body.len()).expect("value case 适配 uleb"),
                    );
                }
                for body in bodies {
                    out.extend_from_slice(&body);
                }
            } else {
                for variant in variants {
                    for (child, offset) in variant {
                        let record = universe.record(child)?;
                        emit_value(
                            &record.metadata,
                            record.passing,
                            base + *offset,
                            universe,
                            out,
                        )?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
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

    /// 取某类型 trace program 中第一条 SWITCH 的 `(tag_word, tag_width)`。
    fn switch_tag(entry: &GcTypeEntryV1, world: &GcMetadataWorldV1) -> (u64, u64) {
        let program = &world.trace_program
            [entry.trace_offset as usize..(entry.trace_offset + entry.trace_len) as usize];
        let switch = program
            .iter()
            .position(|byte| *byte == TraceOp::Switch as u8)
            .expect("带判别值的聚合必须发出 SWITCH");
        let mut index = switch + 1;
        let tag_word = crate::runtime::gc_metadata_schema::decode_uleb(program, &mut index)
            .expect("tag word 可解码");
        let tag_width = crate::runtime::gc_metadata_schema::decode_uleb(program, &mut index)
            .expect("tag width 可解码");
        (tag_word, tag_width)
    }

    #[test]
    fn trace_program_uses_word_units_for_switch_tag() {
        // `Holder` 的 `Option[int]` 字段位于字节偏移 8。trace program 的 offset
        // 一律以 8 字节 word 计，因此判别值必须编码为 word 1；若误用字节单位就会
        // 得到 8，runtime 将按错误的基准读取判别值。
        let bundle = derive_source(
            "struct Holder { head: uint, body: Option[int] }\nfn main() {\n let value = Holder { head: 1, body: Option.Some(2) }\n _ = value\n }",
        );
        let holder = bundle
            .world
            .types
            .iter()
            .find(|entry| entry.name == "Holder")
            .expect("Holder 必须进入冻结类型表");
        let (tag_word, tag_width) = switch_tag(holder, &bundle.world);
        assert_eq!(
            tag_word, 1,
            "SWITCH tag 必须以 word 计（字节偏移 8 → word 1）"
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
            "顶层 tag 位于 word 0"
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
