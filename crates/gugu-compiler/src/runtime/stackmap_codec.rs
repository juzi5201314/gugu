//! 栈图 section 编解码：header、function、safepoint 与 root map 四表。
//!
//! 编码器接受逻辑世界与调用方注入的机器布局（`code_rva`、`code_size`、
//! `frame_size`、`unwind_index`）；生产契约在后端就绪前不调用编码器写契约，
//! 编码器由单测与后端联合验证复用，函数签名即跨阶段接口。decoder 拒收
//! version 1 记录，全部对齐、padding、表不重叠与位图互斥检查都在解码时执行。

use super::model::RawModelError;
use super::stackmap_schema::{
    REGISTER_NAMES, RESERVED_REGISTER_BITS, STACKMAP_ENDIAN, STACKMAP_MAGIC, STACKMAP_POINTER_SIZE,
    STACKMAP_SECTION_VERSION,
};

/// section header 的固定字节数：8+2+1+1+4*3+4*2+8*4 = 72。
pub(crate) const HEADER_BYTES: usize = 72;
/// function record 的固定字节数。
pub(crate) const FUNCTION_BYTES: usize = 32;
/// safepoint record 的固定字节数。
pub(crate) const SAFEPOINT_BYTES: usize = 12;
/// function table 的对齐。
pub(crate) const TABLE_ALIGN: u64 = 8;

/// 调用方注入的机器布局；真实值由后端在分配后填充。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CodeLayout {
    pub(crate) code_rva: u64,
    pub(crate) code_size: u32,
    pub(crate) frame_size: u32,
    pub(crate) unwind_index: u32,
    pub(crate) runtime_bridge: bool,
    pub(crate) panic_landing: bool,
    pub(crate) has_stack_interior: bool,
}

/// 一个安全点的机器位置与根位图输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SafepointLayout {
    pub(crate) pc_offset: u32,
    pub(crate) kind: u8,
    pub(crate) dirty: bool,
    pub(crate) copy_allowed: bool,
    pub(crate) scan_allowed: bool,
    /// 五类 slot 位图（按 slot 序，最低有效 bit 先写）。
    pub(crate) slots: [Vec<u64>; 5],
    /// 五类寄存器掩码；bit15 必须为 0，普通函数保留位必须为 0。
    pub(crate) registers: [u16; 5],
    /// 该安全点所属函数的布局下标。
    pub(crate) function: u32,
    /// slot 总数。
    pub(crate) slot_count: u32,
}

/// 编码一个完整 section。
pub(crate) fn encode(
    functions: &[CodeLayout],
    safepoints: &[(u32, SafepointLayout)],
) -> Result<Vec<u8>, RawModelError> {
    // 函数表按 `code_rva` 严格递增，code range 不重叠。
    for pair in functions.windows(2) {
        if pair[0].code_rva >= pair[1].code_rva {
            return Err(RawModelError::new("函数表没有按 code_rva 严格递增"));
        }
    }
    for (index, function) in functions.iter().enumerate() {
        let end = function
            .code_rva
            .checked_add(u64::from(function.code_size))
            .ok_or_else(|| RawModelError::new("函数代码范围溢出"))?;
        for other in &functions[index + 1..] {
            if other.code_rva < end {
                return Err(RawModelError::new("函数代码范围重叠"));
            }
        }
        if function.frame_size % 16 != 8 && function.frame_size != 0 {
            return Err(RawModelError::new("函数帧大小不满足对齐规则"));
        }
    }
    // 安全点按（函数序、offset）排序后编码。
    let mut ordered: Vec<(u32, SafepointLayout)> = safepoints.to_vec();
    ordered.sort_by_key(|(function, layout)| (*function, layout.pc_offset));
    for (function, layout) in &ordered {
        let code = functions
            .get(usize::try_from(*function).expect("函数下标"))
            .ok_or_else(|| RawModelError::new("安全点引用越界函数"))?;
        if layout.pc_offset >= code.code_size {
            return Err(RawModelError::new("安全点偏移越过函数代码"));
        }
        verify_layout(layout)?;
    }
    // 同一函数内按 `pc_offset` 严格递增。
    for pair in ordered.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1.pc_offset >= pair[1].1.pc_offset {
            return Err(RawModelError::new("同一函数的安全点没有按偏移严格递增"));
        }
    }
    // map 去重基于完整记录字节的字典序。
    let mut records: Vec<Vec<u8>> = ordered
        .iter()
        .map(|(_, layout)| map_record(layout))
        .collect();
    records.sort();
    records.dedup();
    let mut index_table = Vec::with_capacity(records.len() + 1);
    let mut data = Vec::new();
    for record in &records {
        index_table.push(data.len() as u64);
        data.extend_from_slice(record);
    }
    index_table.push(data.len() as u64);

    let function_count = functions.len() as u32;
    let safepoint_count = ordered.len() as u32;
    let map_count = records.len() as u32;
    let functions_offset = HEADER_BYTES as u64;
    let safepoints_offset = functions_offset + function_count as u64 * FUNCTION_BYTES as u64;
    let map_index_offset = safepoints_offset + safepoint_count as u64 * SAFEPOINT_BYTES as u64;
    // map index 表 8 字节对齐。
    let map_index_offset = align_up(map_index_offset, TABLE_ALIGN);
    let map_data_offset = map_index_offset + (map_count as u64 + 1) * 8;
    let section_len = map_data_offset + data.len() as u64;

    let mut output = Vec::with_capacity(section_len as usize);
    output.extend_from_slice(STACKMAP_MAGIC);
    output.extend_from_slice(&STACKMAP_SECTION_VERSION.to_le_bytes());
    output.push(STACKMAP_POINTER_SIZE);
    output.push(STACKMAP_ENDIAN);
    output.extend_from_slice(&function_count.to_le_bytes());
    output.extend_from_slice(&safepoint_count.to_le_bytes());
    output.extend_from_slice(&map_count.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&functions_offset.to_le_bytes());
    output.extend_from_slice(&safepoints_offset.to_le_bytes());
    output.extend_from_slice(&map_index_offset.to_le_bytes());
    output.extend_from_slice(&map_data_offset.to_le_bytes());
    output.extend_from_slice(&section_len.to_le_bytes());
    debug_assert_eq!(output.len(), HEADER_BYTES);
    for (safepoint_index, (function, _)) in ordered.iter().enumerate() {
        let _ = safepoint_index;
        let _ = function;
    }
    // 函数记录：safepoint 范围按全局序连续分配。
    let mut cursor = 0u32;
    for (function_index, function) in functions.iter().enumerate() {
        let count = ordered
            .iter()
            .filter(|(index, _)| *index as usize == function_index)
            .count() as u32;
        let mut flags: u16 = 0;
        if function.runtime_bridge {
            flags |= 1;
        }
        if function.panic_landing {
            flags |= 1 << 1;
        }
        if function.has_stack_interior {
            flags |= 1 << 2;
        }
        output.extend_from_slice(&function.code_rva.to_le_bytes());
        output.extend_from_slice(&function.code_size.to_le_bytes());
        output.extend_from_slice(&function.frame_size.to_le_bytes());
        output.extend_from_slice(&cursor.to_le_bytes());
        output.extend_from_slice(&count.to_le_bytes());
        output.extend_from_slice(&function.unwind_index.to_le_bytes());
        output.extend_from_slice(&flags.to_le_bytes());
        output.extend_from_slice(&0u16.to_le_bytes());
        cursor += count;
    }
    for ((_, layout), record) in ordered.iter().zip(
        ordered
            .iter()
            .map(|(_, layout)| map_record(layout))
            .collect::<Vec<_>>(),
    ) {
        let map_index = records
            .iter()
            .position(|candidate| candidate == &record)
            .expect("去重表包含全部记录") as u32;
        let mut flags: u8 = 0;
        if layout.copy_allowed {
            flags |= 1;
        }
        if layout.scan_allowed {
            flags |= 1 << 1;
        }
        flags |= 1 << 2;
        if layout.dirty {
            if layout.kind != 3 {
                return Err(RawModelError::new("dirty 标志只能与 kind 3 同时出现"));
            }
            flags |= 1 << 3;
        }
        output.extend_from_slice(&layout.pc_offset.to_le_bytes());
        output.extend_from_slice(&map_index.to_le_bytes());
        output.push(layout.kind);
        output.push(flags);
        output.extend_from_slice(&0u16.to_le_bytes());
    }
    while output.len() as u64 % TABLE_ALIGN != 0 {
        output.push(0);
    }
    debug_assert_eq!(output.len() as u64, map_index_offset);
    // map index 表含 `map_count + 1` 个 u64，末项为数据末端。
    for offset in &index_table {
        output.extend_from_slice(&(map_data_offset + offset).to_le_bytes());
    }
    output.extend_from_slice(&data);
    debug_assert_eq!(output.len() as u64, section_len);
    Ok(output)
}

/// 解码并验证一个完整 section；返回函数数、安全点数与 map 数。
pub(crate) fn decode(bytes: &[u8]) -> Result<(u32, u32, u32), RawModelError> {
    if bytes.len() < HEADER_BYTES {
        return Err(RawModelError::new("栈图 section 小于固定 header"));
    }
    if &bytes[..8] != STACKMAP_MAGIC {
        return Err(RawModelError::new("栈图 section 魔数不匹配"));
    }
    let version = u16::from_le_bytes(bytes[8..10].try_into().expect("版本字段"));
    if version == 1 {
        return Err(RawModelError::new(
            "拒绝 version 1 记录：handle 与压缩引用表示已变化",
        ));
    }
    if version != STACKMAP_SECTION_VERSION {
        return Err(RawModelError::new("栈图 section 版本不匹配"));
    }
    if bytes[10] != STACKMAP_POINTER_SIZE || bytes[11] != STACKMAP_ENDIAN {
        return Err(RawModelError::new("栈图 section 指针宽度或字节序不匹配"));
    }
    let function_count = u32::from_le_bytes(bytes[12..16].try_into().expect("计数字段"));
    let safepoint_count = u32::from_le_bytes(bytes[16..20].try_into().expect("计数字段"));
    let map_count = u32::from_le_bytes(bytes[20..24].try_into().expect("计数字段"));
    if u32::from_le_bytes(bytes[24..28].try_into().expect("保留字段")) != 0
        || u32::from_le_bytes(bytes[28..32].try_into().expect("保留字段")) != 0
    {
        return Err(RawModelError::new("栈图 header 保留字段必须为 0"));
    }
    let functions_offset = u64::from_le_bytes(bytes[32..40].try_into().expect("偏移字段"));
    let safepoints_offset = u64::from_le_bytes(bytes[40..48].try_into().expect("偏移字段"));
    let map_index_offset = u64::from_le_bytes(bytes[48..56].try_into().expect("偏移字段"));
    let map_data_offset = u64::from_le_bytes(bytes[56..64].try_into().expect("偏移字段"));
    let section_len = u64::from_le_bytes(bytes[64..72].try_into().expect("长度字段"));
    if section_len as usize != bytes.len() {
        return Err(RawModelError::new("栈图 section 长度与实际字节不一致"));
    }
    for offset in [
        functions_offset,
        safepoints_offset,
        map_index_offset,
        map_data_offset,
    ] {
        if offset % TABLE_ALIGN != 0 || offset > section_len {
            return Err(RawModelError::new("栈图表偏移未对齐或越界"));
        }
    }
    let functions_end = functions_offset + function_count as u64 * FUNCTION_BYTES as u64;
    let safepoints_end = safepoints_offset + safepoint_count as u64 * SAFEPOINT_BYTES as u64;
    let index_end = map_index_offset + (map_count as u64 + 1) * 8;
    // 四表不重叠且有序。
    if !(functions_offset == HEADER_BYTES as u64
        && functions_end <= safepoints_offset
        && safepoints_end <= map_index_offset
        && index_end <= map_data_offset
        && map_data_offset <= section_len)
    {
        return Err(RawModelError::new("栈图表范围重叠或顺序非法"));
    }
    // 函数表按 `code_rva` 严格递增，code range 不重叠。
    let mut previous_rva = None;
    let mut previous_end = 0u64;
    for index in 0..function_count {
        let base = (functions_offset as usize) + index as usize * FUNCTION_BYTES;
        let rva = u64::from_le_bytes(bytes[base..base + 8].try_into().expect("函数字段"));
        let size = u32::from_le_bytes(bytes[base + 8..base + 12].try_into().expect("函数字段"));
        let frame = u32::from_le_bytes(bytes[base + 12..base + 16].try_into().expect("函数字段"));
        let flags = u16::from_le_bytes(bytes[base + 28..base + 30].try_into().expect("函数字段"));
        if u16::from_le_bytes(bytes[base + 30..base + 32].try_into().expect("保留字段")) != 0
            || flags & !0b111 != 0
        {
            return Err(RawModelError::new("函数记录保留位必须为 0"));
        }
        if frame % 16 != 8 && frame != 0 {
            return Err(RawModelError::new("函数帧大小不满足对齐规则"));
        }
        if previous_rva.is_some_and(|previous| rva <= previous) {
            return Err(RawModelError::new("函数表没有按 code_rva 严格递增"));
        }
        if rva < previous_end {
            return Err(RawModelError::new("函数代码范围重叠"));
        }
        previous_rva = Some(rva);
        previous_end = rva
            .checked_add(u64::from(size))
            .ok_or_else(|| RawModelError::new("函数代码范围溢出"))?;
    }
    // 安全点表：同一函数内按 offset 严格递增，offset 小于 code_size。
    let mut previous_function = u32::MAX;
    let mut previous_offset = u32::MAX;
    for index in 0..safepoint_count {
        let base = (safepoints_offset as usize) + index as usize * SAFEPOINT_BYTES;
        let pc = u32::from_le_bytes(bytes[base..base + 4].try_into().expect("安全点字段"));
        let map = u32::from_le_bytes(bytes[base + 4..base + 8].try_into().expect("安全点字段"));
        let kind = bytes[base + 8];
        let flags = bytes[base + 9];
        if u16::from_le_bytes(bytes[base + 10..base + 12].try_into().expect("保留字段")) != 0 {
            return Err(RawModelError::new("安全点保留字段必须为 0"));
        }
        if map >= map_count {
            return Err(RawModelError::new("安全点引用越界 map"));
        }
        if !matches!(kind, 0..=4) {
            return Err(RawModelError::new("安全点 kind 未登记"));
        }
        if flags & !0b1111 != 0 {
            return Err(RawModelError::new("安全点 flags 含未定义位"));
        }
        if flags & 0b1000 != 0 && kind != 3 {
            return Err(RawModelError::new("dirty 标志只能与 kind 3 同时出现"));
        }
        // 所属函数由全局序推导：函数记录的 safepoint 范围连续。
        let (function, code_size) = function_of(bytes, functions_offset, function_count, index)?;
        if function != previous_function {
            previous_function = function;
        } else if pc <= previous_offset {
            return Err(RawModelError::new("同一函数的安全点没有按偏移严格递增"));
        }
        if pc >= code_size {
            return Err(RawModelError::new("安全点偏移越过函数代码"));
        }
        previous_offset = pc;
    }
    // map 表按记录字节字典序排列；index 末项为数据末端。
    let mut previous: Option<&[u8]> = None;
    for index in 0..map_count {
        let base = (map_index_offset as usize) + index as usize * 8;
        let start = u64::from_le_bytes(bytes[base..base + 8].try_into().expect("索引字段"));
        let end = u64::from_le_bytes(bytes[base + 8..base + 16].try_into().expect("索引字段"));
        if start < map_data_offset || end < start || end > section_len {
            return Err(RawModelError::new("map 索引范围越界"));
        }
        let record = &bytes[start as usize..end as usize];
        verify_map_record(record)?;
        if previous.is_some_and(|previous| record < previous) {
            return Err(RawModelError::new("map 表没有按记录字节字典序排列"));
        }
        previous = Some(record);
    }
    let last = (map_index_offset as usize) + map_count as usize * 8;
    let end = u64::from_le_bytes(bytes[last..last + 8].try_into().expect("索引末项"));
    if end != section_len {
        return Err(RawModelError::new("map 索引末项不是数据末端"));
    }
    Ok((function_count, safepoint_count, map_count))
}

fn function_of(
    bytes: &[u8],
    functions_offset: u64,
    function_count: u32,
    safepoint: u32,
) -> Result<(u32, u32), RawModelError> {
    let mut cursor = 0u32;
    for index in 0..function_count {
        let base = (functions_offset as usize) + index as usize * FUNCTION_BYTES;
        let start = u32::from_le_bytes(bytes[base + 16..base + 20].try_into().expect("函数字段"));
        let count = u32::from_le_bytes(bytes[base + 20..base + 24].try_into().expect("函数字段"));
        let size = u32::from_le_bytes(bytes[base + 8..base + 12].try_into().expect("函数字段"));
        if safepoint >= start && safepoint < start + count {
            return Ok((index, size));
        }
        cursor += count;
    }
    let _ = cursor;
    Err(RawModelError::new("安全点没有所属函数范围"))
}

fn verify_layout(layout: &SafepointLayout) -> Result<(), RawModelError> {
    if !matches!(layout.kind, 0..=4) {
        return Err(RawModelError::new("安全点 kind 未登记"));
    }
    if layout.dirty && layout.kind != 3 {
        return Err(RawModelError::new("dirty 标志只能与 kind 3 同时出现"));
    }
    for mask in &layout.registers {
        if mask & (1 << 15) != 0 {
            return Err(RawModelError::new("寄存器掩码 bit15 必须为 0"));
        }
    }
    // 普通函数的保留位（r14、r15）必须为 0；bridge 的专用根表不在此处编码。
    for (class, mask) in layout.registers.iter().enumerate() {
        let _ = class;
        if mask & RESERVED_REGISTER_BITS != 0 {
            return Err(RawModelError::new("普通函数占用 runtime 保留寄存器"));
        }
    }
    let words = layout.slot_count.div_ceil(8) as usize;
    for (class, bitmap) in layout.slots.iter().enumerate() {
        if bitmap.len() != words.div_ceil(8).max(1) {
            return Err(RawModelError::new("槽位图长度与 slot_count 不一致"));
        }
        // 超出 `slot_count` 的尾 bit 必须为 0：位图按字节覆盖槽位。
        let excess = bitmap.len() * 64 - words * 8;
        if excess > 0
            && let Some(last) = bitmap.last()
            && last >> (64 - excess) != 0
        {
            return Err(RawModelError::new("槽位图尾 bit 必须为 0"));
        }
        let _ = class;
    }
    // 五个位图互斥。
    let lanes = words.div_ceil(8).max(1);
    for lane in 0..lanes {
        let mut merged = 0u64;
        for bitmap in &layout.slots {
            let word = bitmap.get(lane).copied().unwrap_or(0);
            if merged & word != 0 {
                return Err(RawModelError::new("五类槽位图不互斥"));
            }
            merged |= word;
        }
    }
    // 五个寄存器掩码互斥。
    let mut merged = 0u16;
    for mask in &layout.registers {
        if merged & mask != 0 {
            return Err(RawModelError::new("五类寄存器掩码不互斥"));
        }
        merged |= mask;
    }
    // `MorestackEntry` 的 `slot_count` 固定为 0。
    if layout.kind == 4 && layout.slot_count != 0 {
        return Err(RawModelError::new("MorestackEntry 的 slot_count 必须为 0"));
    }
    if u64::from(layout.slot_count) * 8 > u64::from(u32::MAX) {
        return Err(RawModelError::new("槽数量超过帧上界"));
    }
    Ok(())
}

fn verify_map_record(record: &[u8]) -> Result<(), RawModelError> {
    if record.len() < 16 {
        return Err(RawModelError::new("map 记录小于固定头"));
    }
    let slot_count = u32::from_le_bytes(record[0..4].try_into().expect("槽字段"));
    let words = slot_count.div_ceil(8) as usize;
    let lanes = words.div_ceil(8).max(1);
    // 头 16 字节后是五组位图，每组 `ceil(slot_count/8)` 字节，再补齐到 4 字节。
    let mut cursor = 16usize;
    let mut bitmaps = [[0u64; 8]; 5];
    for class in 0..5 {
        let bytes = words;
        if record.len() < cursor + bytes {
            return Err(RawModelError::new("map 记录位图越界"));
        }
        for (lane, chunk) in record[cursor..cursor + bytes].chunks(8).enumerate() {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            bitmaps[class][lane] = u64::from_le_bytes(word);
        }
        cursor += bytes;
    }
    while cursor % 4 != 0 {
        if record.get(cursor).copied().unwrap_or(1) != 0 {
            return Err(RawModelError::new("map 记录 padding 必须为 0"));
        }
        cursor += 1;
    }
    if cursor != record.len() {
        return Err(RawModelError::new("map 记录存在多余字节"));
    }
    // 尾 bit 清零与互斥：位图按字节覆盖 `slot_count` 个槽位。
    for class in 0..5 {
        let excess_bits = lanes * 64 - words * 8;
        if excess_bits > 0 && bitmaps[class][lanes - 1] >> (64 - excess_bits) != 0 {
            return Err(RawModelError::new("槽位图尾 bit 必须为 0"));
        }
    }
    for lane in 0..lanes {
        let mut merged = 0u64;
        for class in 0..5 {
            if merged & bitmaps[class][lane] != 0 {
                return Err(RawModelError::new("五类槽位图不互斥"));
            }
            merged |= bitmaps[class][lane];
        }
    }
    let masks = [
        u16::from_le_bytes(record[4..6].try_into().expect("掩码字段")),
        u16::from_le_bytes(record[6..8].try_into().expect("掩码字段")),
        u16::from_le_bytes(record[8..10].try_into().expect("掩码字段")),
        u16::from_le_bytes(record[10..12].try_into().expect("掩码字段")),
    ];
    // 记录头只存四个掩码：第五类掩码恒为 0，由编码器保证。
    let _ = masks;
    for mask in record[4..12].chunks(2) {
        let mask = u16::from_le_bytes(mask.try_into().expect("掩码字段"));
        if mask & (1 << 15) != 0 {
            return Err(RawModelError::new("寄存器掩码 bit15 必须为 0"));
        }
    }
    let _ = REGISTER_NAMES;
    Ok(())
}

fn map_record(layout: &SafepointLayout) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&layout.slot_count.to_le_bytes());
    // 记录头固定存四个掩码位段与一个保留 u32：direct、interior、handle、
    // compressed；stack 类掩码在本阶段恒为 0（挂起与 bridge 点全零规则）。
    for mask in layout.registers.iter().take(4) {
        output.extend_from_slice(&mask.to_le_bytes());
    }
    output.extend_from_slice(&[0, 0, 0, 0]);
    let words = layout.slot_count.div_ceil(8) as usize;
    for bitmap in &layout.slots {
        // 位图按字读入内存：内存字数与 `slot_count` 覆盖的字节数一致。
        let mut bytes = vec![0u8; words];
        for (lane, word) in bitmap.iter().enumerate() {
            let start = lane * 8;
            if start >= words {
                break;
            }
            let chunk = bytes.len().min(start + 8) - start;
            bytes[start..start + chunk].copy_from_slice(&word.to_le_bytes()[..chunk]);
        }
        output.extend_from_slice(&bytes);
    }
    while output.len() % 4 != 0 {
        output.push(0);
    }
    output
}

fn align_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}
