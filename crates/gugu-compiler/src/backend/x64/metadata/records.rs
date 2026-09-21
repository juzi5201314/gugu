//! landing、源码记录与函数内元数据校验。

use crate::frontend::hir;
use crate::lir::body::{Body, Terminator};
use crate::source::SourceMap;

use super::super::alloc::Allocated;
use super::super::inst::Assembled;
use super::{
    CALLEE_SAVED_ROOTS, FunctionMetadata, LandingPayload, MetadataError, PROPAGATE_ONLY, SiteSpan,
    SourcePayload,
};

pub(super) fn landing_of(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    block_starts: &[Option<u32>],
) -> Result<Option<LandingPayload>, MetadataError> {
    if !span.terminator {
        return Ok(None);
    }
    let Terminator::Invoke { unwind, .. } = &body.blocks[span.block as usize].terminator else {
        return Ok(None);
    };
    let edge = body
        .edges
        .get(unwind.index())
        .ok_or_else(|| MetadataError::new("展开边越界"))?;
    let call = super::last_call(&allocated.sequence, span.insts.0, span.insts.1)
        .ok_or_else(|| MetadataError::new("展开调用没有 call 指令"))?;
    let pc_start = super::offset_at(assembled, call)?;
    let pc_end = super::offset_at(assembled, call + 1)?;
    if pc_start >= pc_end || pc_end > super::code_len(assembled) {
        return Err(MetadataError::new("展开调用范围非法"));
    }
    let landing_pc = block_starts
        .get(edge.to.index())
        .copied()
        .flatten()
        .ok_or_else(|| MetadataError::new("landing 块没有机器码"))?;
    Ok(Some(LandingPayload {
        pc_start,
        pc_end,
        landing_pc,
        cleanup_chain: cleanup_chain(body, edge.to.0),
    }))
}

pub(super) fn cleanup_chain(body: &Body, target: u32) -> u32 {
    let Some(block) = body.blocks.get(target as usize) else {
        return PROPAGATE_ONLY;
    };
    let empty = block.instructions.start == block.instructions.end;
    if empty && matches!(block.terminator, Terminator::ResumePanic { .. }) {
        PROPAGATE_ONLY
    } else {
        target
    }
}

pub(super) fn source_of(
    body: &Body,
    span: &SiteSpan,
    sources: &SourceMap,
    hir_sources: &[hir::Source],
) -> SourcePayload {
    let info = if span.terminator {
        body.blocks[span.block as usize].source.clone()
    } else {
        body.instructions
            .get(span.instruction as usize)
            .map(|instruction| instruction.source.clone())
            .unwrap_or_else(|| body.blocks[span.block as usize].source.clone())
    };
    let cleanup = body
        .blocks
        .get(span.block as usize)
        .is_some_and(|block| block.cleanup);
    let mut flags = u32::from(cleanup);
    let (path, line, column, synthetic) = locate(
        sources,
        hir_sources,
        info.location.source,
        info.location.start,
    );
    if synthetic {
        flags |= 2;
    }
    SourcePayload {
        pc_start: span.bytes.0,
        pc_end: span.bytes.1,
        path,
        line,
        column,
        flags,
    }
}

pub(super) fn locate(
    sources: &SourceMap,
    hir_sources: &[hir::Source],
    index: u32,
    offset: u32,
) -> (String, u32, u32, bool) {
    let Some(file) = hir_sources.get(index as usize) else {
        return ("<synthetic>".to_owned(), 1, 1, true);
    };
    for snapshot in sources.snapshots() {
        if snapshot.logical_path() != file.path {
            continue;
        }
        if let Ok(position) = snapshot.line_column(offset as usize) {
            return (file.path.clone(), position.line, position.column, false);
        }
    }
    (file.path.clone(), 1, 1, true)
}

pub(super) fn check_safepoints(metadata: &FunctionMetadata) -> Result<(), MetadataError> {
    let mut previous = None;
    for point in &metadata.safepoints {
        if point.pc_offset >= metadata.code_size || !metadata.boundaries.contains(&point.pc_offset)
        {
            return Err(MetadataError::new("安全点不在指令边界上"));
        }
        if previous.is_some_and(|seen: u32| seen >= point.pc_offset) {
            return Err(MetadataError::new("安全点偏移没有严格递增"));
        }
        previous = Some(point.pc_offset);
        if point.kind == 4 && point.slot_count != 0 {
            return Err(MetadataError::new("MorestackEntry 的槽数必须为 0"));
        }
        if matches!(point.kind, 2 | 3) && point.registers.iter().any(|mask| *mask != 0) {
            return Err(MetadataError::new("挂起或 bridge 点含有寄存器根"));
        }
        if point.kind == 0
            && point
                .registers
                .iter()
                .any(|mask| mask & !CALLEE_SAVED_ROOTS != 0)
        {
            return Err(MetadataError::new("调用返回点含有调用者保存寄存器根"));
        }
        if point.dirty && point.kind != 3 {
            return Err(MetadataError::new("dirty 标志只能与 bridge 同时出现"));
        }
    }
    Ok(())
}

pub(super) fn check_landings(metadata: &FunctionMetadata) -> Result<(), MetadataError> {
    let mut previous_end = 0_u32;
    for (index, landing) in metadata.landings.iter().enumerate() {
        if landing.pc_start >= landing.pc_end || landing.pc_end > metadata.code_size {
            return Err(MetadataError::new("landing 范围越出函数"));
        }
        if landing.landing_pc >= metadata.code_size {
            return Err(MetadataError::new("landing 目标越出函数"));
        }
        if index > 0 && landing.pc_start < previous_end {
            return Err(MetadataError::new("landing 范围重叠"));
        }
        previous_end = landing.pc_end;
    }
    Ok(())
}

pub(super) fn check_sources(metadata: &FunctionMetadata) -> Result<(), MetadataError> {
    for record in &metadata.sources {
        if record.pc_start >= record.pc_end || record.pc_end > metadata.code_size {
            return Err(MetadataError::new("源码记录范围越出函数"));
        }
        if record.line == 0 || record.column == 0 {
            return Err(MetadataError::new("源码行列必须从 1 开始"));
        }
        if record.flags & !0b11 != 0 {
            return Err(MetadataError::new("源码记录含未定义标志"));
        }
    }
    Ok(())
}
