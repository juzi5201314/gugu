//! 栈图、统一展开表与源码记录的 runtime 消费。
//!
//! walker 只接受已经编码的节字节：按 PC 定位函数和安全点，展开根位图，并按
//! landing 范围取出 cleanup 链。找不到精确安全点时失败，不猜测相邻记录。

use super::model::RawModelError;
use super::stackmap_codec::{self, HEADER_BYTES};

/// 一次整节走查的计数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Audit {
    /// 函数数。
    pub(crate) functions: u32,
    /// 安全点数。
    pub(crate) safepoints: u32,
    /// 去重 map 数。
    pub(crate) maps: u32,
    /// 根字数。
    pub(crate) roots: u32,
    /// landing 数。
    pub(crate) landings: u32,
    /// 只恢复传播的 landing 数。
    pub(crate) propagate_only: u32,
    /// 源码记录数。
    pub(crate) source_records: u32,
    /// 五种 safepoint kind 的计数。
    pub(crate) kinds: [u32; 5],
    /// 允许栈复制的安全点数。
    pub(crate) copy_allowed: u32,
    /// 栈内指针根数。
    pub(crate) stack_interior_roots: u32,
}

/// 一个已定位的安全点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SafepointHit {
    /// 规范 kind。
    pub(crate) kind: u8,
    /// 记录 flags。
    pub(crate) flags: u8,
    /// 帧字节数。
    pub(crate) frame_size: u32,
    /// 展开表序号。
    pub(crate) unwind_index: u32,
    /// `(种类, 槽下标或寄存器位)`。寄存器位加 0x1_0000 与槽区分。
    pub(crate) roots: Vec<(u8, u32)>,
}

/// 一个已定位的 landing。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LandingHit {
    /// 函数内的 landing 指令偏移。
    pub(crate) landing_pc: u32,
    /// cleanup 链；`u32::MAX` 表示只恢复传播。
    pub(crate) cleanup_chain: u32,
    /// 帧字节数。
    pub(crate) frame_size: u32,
}

/// 走查三节并返回计数。任一不变量失败都拒绝。
pub(crate) fn audit(stackmap: &[u8], unwind: &[u8], source: &[u8]) -> Result<Audit, RawModelError> {
    let (functions, safepoints, maps) = stackmap_codec::decode(stackmap)?;
    let records = map_records(stackmap)?;
    if records.len() != maps as usize {
        return Err(RawModelError::new("map 记录数与 header 不一致"));
    }
    if records.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(RawModelError::new("map 记录没有按字节字典序严格递增"));
    }
    let mut audit = Audit {
        functions,
        safepoints,
        maps,
        roots: 0,
        landings: 0,
        propagate_only: 0,
        source_records: 0,
        kinds: [0; 5],
        copy_allowed: 0,
        stack_interior_roots: 0,
    };
    walk_safepoints(stackmap, &mut audit)?;
    walk_unwind(stackmap, unwind, &mut audit)?;
    walk_sources(stackmap, source, &mut audit)?;
    Ok(audit)
}

/// 按逻辑代码节内的绝对 PC 查找安全点。
pub(crate) fn safepoint_at(stackmap: &[u8], pc: u64) -> Result<SafepointHit, RawModelError> {
    let (functions, _, _) = stackmap_codec::decode(stackmap)?;
    let (index, record) = function_containing(stackmap, functions, pc)?;
    let offset = u32::try_from(pc - record.code_rva).expect("函数内偏移适配 u32");
    let point = safepoint_exact(stackmap, &record, offset)?;
    let roots = roots_of(stackmap, point.map_index)?;
    if record.unwind_index != index {
        return Err(RawModelError::new("unwind 序号与函数序不一致"));
    }
    Ok(SafepointHit {
        kind: point.kind,
        flags: point.flags,
        frame_size: record.frame_size,
        unwind_index: record.unwind_index,
        roots,
    })
}

/// 按绝对 PC 查找覆盖它的 landing。没有 landing 时返回 `Ok(None)`。
pub(crate) fn landing_at(unwind: &[u8], pc: u64) -> Result<Option<LandingHit>, RawModelError> {
    let table = unwind_table(unwind)?;
    for function in &table {
        let end = function
            .code_rva
            .checked_add(u64::from(function.code_size))
            .ok_or_else(|| RawModelError::new("展开函数范围溢出"))?;
        if pc < function.code_rva || pc >= end {
            continue;
        }
        let offset = u32::try_from(pc - function.code_rva).expect("函数内偏移适配 u32");
        for landing in &function.landings {
            if offset >= landing.pc_start && offset < landing.pc_end {
                return Ok(Some(LandingHit {
                    landing_pc: landing.landing_pc,
                    cleanup_chain: landing.cleanup_chain,
                    frame_size: function.frame_size,
                }));
            }
        }
    }
    Ok(None)
}

struct FunctionView {
    code_rva: u64,
    code_size: u32,
    frame_size: u32,
    safepoint_start: u32,
    safepoint_count: u32,
    unwind_index: u32,
}

struct PointView {
    pc_offset: u32,
    map_index: u32,
    kind: u8,
    flags: u8,
}

struct UnwindFunction {
    code_rva: u64,
    code_size: u32,
    frame_size: u32,
    landings: Vec<LandingWord>,
}

struct LandingWord {
    pc_start: u32,
    pc_end: u32,
    landing_pc: u32,
    cleanup_chain: u32,
}

fn walk_safepoints(bytes: &[u8], audit: &mut Audit) -> Result<(), RawModelError> {
    let mut previous_end = 0_u64;
    for index in 0..audit.functions {
        let record = function_at(bytes, index)?;
        if record.code_rva < previous_end {
            return Err(RawModelError::new("函数代码范围重叠"));
        }
        previous_end = record
            .code_rva
            .checked_add(u64::from(record.code_size))
            .ok_or_else(|| RawModelError::new("函数代码范围溢出"))?;
        if record.unwind_index != index {
            return Err(RawModelError::new("unwind 序号与函数序不一致"));
        }
        let mut previous_pc = None;
        for slot in 0..record.safepoint_count {
            let point = point_at(bytes, record.safepoint_start + slot)?;
            if point.pc_offset >= record.code_size {
                return Err(RawModelError::new("安全点越过函数代码"));
            }
            if previous_pc.is_some_and(|seen: u32| seen >= point.pc_offset) {
                return Err(RawModelError::new("安全点偏移没有严格递增"));
            }
            previous_pc = Some(point.pc_offset);
            let roots = roots_of(bytes, point.map_index)?;
            audit.roots += roots.len() as u32;
            audit.stack_interior_roots +=
                roots.iter().filter(|(kind, _)| *kind == 4).count() as u32;
            if point.kind > 4 {
                return Err(RawModelError::new("安全点 kind 未登记"));
            }
            audit.kinds[point.kind as usize] += 1;
            if point.flags & 1 != 0 {
                audit.copy_allowed += 1;
            }
        }
    }
    Ok(())
}

fn walk_unwind(stackmap: &[u8], unwind: &[u8], audit: &mut Audit) -> Result<(), RawModelError> {
    let table = unwind_table(unwind)?;
    if table.len() != audit.functions as usize {
        return Err(RawModelError::new("展开表函数数与栈图不一致"));
    }
    for (index, function) in table.iter().enumerate() {
        let record = function_at(stackmap, index as u32)?;
        if function.code_rva != record.code_rva
            || function.code_size != record.code_size
            || function.frame_size != record.frame_size
        {
            return Err(RawModelError::new("展开表与栈图的帧或代码范围不一致"));
        }
        let mut previous_end = 0_u32;
        for (landing_index, landing) in function.landings.iter().enumerate() {
            if landing.pc_start >= landing.pc_end || landing.pc_end > function.code_size {
                return Err(RawModelError::new("landing 范围越出函数"));
            }
            if landing.landing_pc >= function.code_size {
                return Err(RawModelError::new("landing 目标越出函数"));
            }
            if landing_index > 0 && landing.pc_start < previous_end {
                return Err(RawModelError::new("landing 范围重叠"));
            }
            previous_end = landing.pc_end;
            audit.landings += 1;
            if landing.cleanup_chain == u32::MAX {
                audit.propagate_only += 1;
            }
        }
    }
    Ok(())
}

fn walk_sources(stackmap: &[u8], source: &[u8], audit: &mut Audit) -> Result<(), RawModelError> {
    if source.len() < 32 || &source[..8] != b"GUGUSR01" {
        return Err(RawModelError::new("源码记录表魔数不匹配"));
    }
    if u16::from_le_bytes(source[8..10].try_into().expect("版本")) != 1 {
        return Err(RawModelError::new("源码记录表版本不匹配"));
    }
    let count = u32::from_le_bytes(source[12..16].try_into().expect("计数"));
    let strings_offset = u32::from_le_bytes(source[16..20].try_into().expect("偏移"));
    let strings_len = u32::from_le_bytes(source[20..24].try_into().expect("长度"));
    let section_len = u32::from_le_bytes(source[24..28].try_into().expect("长度"));
    if section_len as usize != source.len()
        || strings_offset as usize + strings_len as usize != source.len()
    {
        return Err(RawModelError::new("源码记录表长度不一致"));
    }
    let pool = &source[strings_offset as usize..];
    for index in 0..count {
        let base = 32 + index as usize * 32;
        let function = u32::from_le_bytes(source[base..base + 4].try_into().expect("字段"));
        let pc_start = u32::from_le_bytes(source[base + 4..base + 8].try_into().expect("字段"));
        let pc_end = u32::from_le_bytes(source[base + 8..base + 12].try_into().expect("字段"));
        let path_offset =
            u32::from_le_bytes(source[base + 12..base + 16].try_into().expect("字段"));
        let path_len = u32::from_le_bytes(source[base + 16..base + 20].try_into().expect("字段"));
        let line = u32::from_le_bytes(source[base + 20..base + 24].try_into().expect("字段"));
        let column = u32::from_le_bytes(source[base + 24..base + 28].try_into().expect("字段"));
        if function >= audit.functions {
            return Err(RawModelError::new("源码记录的函数序号越界"));
        }
        let record = function_at(stackmap, function)?;
        if pc_start >= pc_end || pc_end > record.code_size || line == 0 || column == 0 {
            return Err(RawModelError::new("源码记录范围或行列非法"));
        }
        let end = path_offset as usize + path_len as usize;
        if end > pool.len() || std::str::from_utf8(&pool[path_offset as usize..end]).is_err() {
            return Err(RawModelError::new("源码路径不是合法 UTF-8"));
        }
    }
    audit.source_records = count;
    Ok(())
}

fn function_containing(
    bytes: &[u8],
    functions: u32,
    pc: u64,
) -> Result<(u32, FunctionView), RawModelError> {
    for index in 0..functions {
        let record = function_at(bytes, index)?;
        let end = record.code_rva + u64::from(record.code_size);
        if pc >= record.code_rva && pc < end {
            return Ok((index, record));
        }
    }
    Err(RawModelError::new("PC 不在任何函数范围内"))
}

fn safepoint_exact(
    bytes: &[u8],
    function: &FunctionView,
    offset: u32,
) -> Result<PointView, RawModelError> {
    for slot in 0..function.safepoint_count {
        let point = point_at(bytes, function.safepoint_start + slot)?;
        if point.pc_offset == offset {
            return Ok(point);
        }
    }
    Err(RawModelError::new("PC 不是已登记的安全点"))
}

fn function_at(bytes: &[u8], index: u32) -> Result<FunctionView, RawModelError> {
    let functions_offset = u64::from_le_bytes(bytes[32..40].try_into().expect("偏移"));
    let base = functions_offset as usize + index as usize * 32;
    if bytes.len() < base + 32 {
        return Err(RawModelError::new("函数记录越界"));
    }
    Ok(FunctionView {
        code_rva: u64::from_le_bytes(bytes[base..base + 8].try_into().expect("字段")),
        code_size: u32::from_le_bytes(bytes[base + 8..base + 12].try_into().expect("字段")),
        frame_size: u32::from_le_bytes(bytes[base + 12..base + 16].try_into().expect("字段")),
        safepoint_start: u32::from_le_bytes(bytes[base + 16..base + 20].try_into().expect("字段")),
        safepoint_count: u32::from_le_bytes(bytes[base + 20..base + 24].try_into().expect("字段")),
        unwind_index: u32::from_le_bytes(bytes[base + 24..base + 28].try_into().expect("字段")),
    })
}

fn point_at(bytes: &[u8], index: u32) -> Result<PointView, RawModelError> {
    let offset = u64::from_le_bytes(bytes[40..48].try_into().expect("偏移"));
    let base = offset as usize + index as usize * 12;
    if bytes.len() < base + 12 {
        return Err(RawModelError::new("安全点记录越界"));
    }
    Ok(PointView {
        pc_offset: u32::from_le_bytes(bytes[base..base + 4].try_into().expect("字段")),
        map_index: u32::from_le_bytes(bytes[base + 4..base + 8].try_into().expect("字段")),
        kind: bytes[base + 8],
        flags: bytes[base + 9],
    })
}

fn map_records(bytes: &[u8]) -> Result<Vec<&[u8]>, RawModelError> {
    let map_count = u32::from_le_bytes(bytes[20..24].try_into().expect("计数"));
    let index_offset = u64::from_le_bytes(bytes[48..56].try_into().expect("偏移")) as usize;
    let mut records = Vec::with_capacity(map_count as usize);
    for index in 0..map_count {
        let start = read_u64(bytes, index_offset + index as usize * 8)? as usize;
        let end = read_u64(bytes, index_offset + (index as usize + 1) * 8)? as usize;
        if start > end || end > bytes.len() {
            return Err(RawModelError::new("map 记录范围越界"));
        }
        records.push(&bytes[start..end]);
    }
    let _ = HEADER_BYTES;
    Ok(records)
}

fn roots_of(bytes: &[u8], map_index: u32) -> Result<Vec<(u8, u32)>, RawModelError> {
    let records = map_records(bytes)?;
    let record = records
        .get(map_index as usize)
        .ok_or_else(|| RawModelError::new("map 序号越界"))?;
    if record.len() < 16 {
        return Err(RawModelError::new("map 记录过短"));
    }
    let slot_count = u32::from_le_bytes(record[..4].try_into().expect("槽数"));
    let mut roots = Vec::new();
    for kind in 0..5_u8 {
        let mask = u16::from_le_bytes(
            record[4 + kind as usize * 2..6 + kind as usize * 2]
                .try_into()
                .expect("掩码"),
        );
        for bit in 0..15_u16 {
            if mask & (1 << bit) != 0 {
                roots.push((kind, 0x1_0000 + u32::from(bit)));
            }
        }
    }
    let words = slot_count.div_ceil(8) as usize;
    let mut cursor = 16_usize;
    for kind in 0..5_u8 {
        let bitmap = record
            .get(cursor..cursor + words)
            .ok_or_else(|| RawModelError::new("槽位图越界"))?;
        for (byte_index, byte) in bitmap.iter().copied().enumerate() {
            for bit in 0..8_u32 {
                let slot = byte_index as u32 * 8 + bit;
                if slot < slot_count && byte & (1 << bit) != 0 {
                    roots.push((kind, slot));
                }
            }
        }
        cursor += words;
    }
    Ok(roots)
}

fn unwind_table(bytes: &[u8]) -> Result<Vec<UnwindFunction>, RawModelError> {
    if bytes.len() < 32 || &bytes[..8] != b"GUGUUN01" {
        return Err(RawModelError::new("展开表魔数不匹配"));
    }
    if u16::from_le_bytes(bytes[8..10].try_into().expect("版本")) != 1 {
        return Err(RawModelError::new("展开表版本不匹配"));
    }
    let functions = u32::from_le_bytes(bytes[12..16].try_into().expect("计数"));
    let landing_count = u32::from_le_bytes(bytes[16..20].try_into().expect("计数"));
    let functions_offset = u32::from_le_bytes(bytes[20..24].try_into().expect("偏移"));
    let landings_offset = u32::from_le_bytes(bytes[24..28].try_into().expect("偏移"));
    let section_len = u32::from_le_bytes(bytes[28..32].try_into().expect("长度"));
    if section_len as usize != bytes.len() || functions_offset != 32 {
        return Err(RawModelError::new("展开表长度不一致"));
    }
    let mut table = Vec::with_capacity(functions as usize);
    for index in 0..functions {
        let base = functions_offset as usize + index as usize * 32;
        let code_rva = u64::from_le_bytes(bytes[base..base + 8].try_into().expect("字段"));
        let code_size = u32::from_le_bytes(bytes[base + 8..base + 12].try_into().expect("字段"));
        let frame_size = u32::from_le_bytes(bytes[base + 12..base + 16].try_into().expect("字段"));
        let landing_start =
            u32::from_le_bytes(bytes[base + 20..base + 24].try_into().expect("字段"));
        let landing_len = u16::from_le_bytes(bytes[base + 24..base + 26].try_into().expect("字段"));
        let mut landings = Vec::with_capacity(landing_len as usize);
        for slot in 0..landing_len as u32 {
            let at = landings_offset as usize + (landing_start + slot) as usize * 16;
            if at + 16 > bytes.len() {
                return Err(RawModelError::new("landing 记录越界"));
            }
            landings.push(LandingWord {
                pc_start: u32::from_le_bytes(bytes[at..at + 4].try_into().expect("字段")),
                pc_end: u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("字段")),
                landing_pc: u32::from_le_bytes(bytes[at + 8..at + 12].try_into().expect("字段")),
                cleanup_chain: u32::from_le_bytes(
                    bytes[at + 12..at + 16].try_into().expect("字段"),
                ),
            });
        }
        table.push(UnwindFunction {
            code_rva,
            code_size,
            frame_size,
            landings,
        });
    }
    if table
        .iter()
        .map(|function| function.landings.len())
        .sum::<usize>()
        != landing_count as usize
    {
        return Err(RawModelError::new("landing 总数与 header 不一致"));
    }
    Ok(table)
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, RawModelError> {
    let chunk = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| RawModelError::new("u64 字段越界"))?;
    Ok(u64::from_le_bytes(chunk.try_into().expect("8 字节")))
}
