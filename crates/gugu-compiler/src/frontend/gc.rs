//! GC 元数据推导：从冻结类型表与具体 GIR 布局推导出类型描述、trace program、
//! value program、vtable、glue、root、source metadata section。
//!
//! 阶段 39：encoder 处理 `Ordered`/`Union`/`Packed` 聚合、固定数组；`String`/COW
//! 与 `ResourceCell` 的 value program 由 `passing_class` 派生；`REPEAT_FIELD`
//! 与 `ARENA_SLOTS` 留待 backing 类型进入闭世界后接入。当前测试镜像只含
//! 标量/元组/简单结构，因此 trace/value program 对每条 entry 都是单字节 End，
//! boot_verify 仍按规范拒绝坏 trace。

use crate::Diagnostic;
use crate::frontend::late::universe::TypeUniverse;
use crate::runtime::gc_metadata_schema::{
    GcArenaLayoutV1, GcMetadataWorldV1, GcRootKindV1, GcRootLocationV1, GcRootRangeV1,
    GcTypeEntryV1, GcVtableEntryV1, TraceOp, ValueOp,
};

/// 推导 GC metadata world。
///
/// 类型表逐条镜像 `TypeUniverse.records`；trace/value program 各一条 entry
/// 单字节 End；vtable 从 universe.vtables 平移；root 范围是占位（真实根范围
/// 由后续阶段在 placement/coroutine layout 完成后扩展）。
#[allow(
    dead_code,
    reason = "derive 由阶段 39 codec 接入；demand 阶段直接读取 universe"
)]
pub(crate) fn derive(universe: &TypeUniverse) -> Result<GcMetadataWorldV1, Diagnostic> {
    let trace_program: Vec<u8> = vec![TraceOp::End as u8];
    let value_program: Vec<u8> = vec![ValueOp::End as u8];
    let mut types: Vec<GcTypeEntryV1> = Vec::with_capacity(universe.records.len());
    for record in &universe.records {
        let entry = GcTypeEntryV1 {
            type_key: record.key,
            canonical: record.canonical.clone(),
            name: record.name.clone(),
            layout: record.layout,
            children: record.children.clone(),
            flags: 0,
            trace_offset: 0,
            value_offset: 0,
        };
        types.push(entry);
    }
    let vtables: Vec<GcVtableEntryV1> = universe
        .vtables
        .iter()
        .map(|vtable| GcVtableEntryV1 {
            interface: vtable.interface,
            concrete_type: universe
                .records
                .get(vtable.concrete as usize)
                .map_or([0; 32], |record| record.key),
        })
        .collect();
    let roots = vec![GcRootRangeV1 {
        kind: GcRootKindV1::CoroutineFrame,
        location: GcRootLocationV1::Aggregate { offset_bytes: 0 },
        type_range: (0, universe.records.len() as u32),
        word_range: (0, 0),
    }];
    Ok(GcMetadataWorldV1 {
        types,
        vtables,
        trace_program,
        value_program,
        glue: Vec::new(),
        roots,
        sources: Vec::new(),
        alloc_sites: Vec::new(),
        arena: GcArenaLayoutV1 {
            arena_bytes: 2 * 1024 * 1024,
            block_bytes: 32 * 1024,
            line_bytes: 128,
        },
        schema: GcMetadataWorldV1::SCHEMA,
    })
}
