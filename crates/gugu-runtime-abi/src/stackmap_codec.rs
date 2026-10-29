//! 栈图 section 编解码：header、function、safepoint 与 root map 四表。
//!
//! 编码器接受调用方注入的机器布局（`code_rva`、`code_size`、`frame_size`、
//! `unwind_index`）。后端在寄存器分配与编码之后调用它写出 section，单测复用同一
//! 函数。decoder 拒收全部历史版本（v1 的 handle 与压缩引用表示、v2 的屏障调用记录
//! 集合都已变化），全部对齐、padding、表不重叠与位图互斥检查都在解码时执行。

use super::model::RawModelError;
use super::stackmap_schema::{
    REGISTER_NAMES, RESERVED_REGISTER_BITS, STACKMAP_ENDIAN, STACKMAP_MAGIC, STACKMAP_POINTER_SIZE,
    STACKMAP_SECTION_VERSION,
};

/// section header 的固定字节数：8+2+1+1+4*3+4*2+8*4 = 72。
pub const HEADER_BYTES: usize = 72;
/// function record 的固定字节数。
pub const FUNCTION_BYTES: usize = 32;
/// safepoint record 的固定字节数。
pub const SAFEPOINT_BYTES: usize = 12;
/// function table 的对齐。
pub const TABLE_ALIGN: u64 = 8;

/// 调用方注入的机器布局；真实值由后端在分配后填充。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodeLayout {
    pub code_rva: u64,
    pub code_size: u32,
    pub frame_size: u32,
    pub unwind_index: u32,
    pub runtime_bridge: bool,
    pub panic_landing: bool,
    pub has_stack_interior: bool,
}

/// 逐根初始化门控的根：该字是否参与扫描由帧内初始化字节决定。
///
/// `slot.roots` 是并集超集，同一字在某些变体里是标量。位图条目不登记这类根，改由本条的
/// `init_byte` 在运行期判定：字节非零才把 `word`（帧内机器字下标，与固定位图同一坐标）
/// 当成 `root_kind` 类根。`word` 不得同时出现在固定位图里，由编码校验保证。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TypedRoot {
    pub word: u32,
    /// 五类根之一：0=Direct、1=Interior、2=Handle、3=Compressed、4=StackInterior。
    pub root_kind: u32,
    /// 帧内逐根初始化字节的字节偏移；非零表示该字已写成指针。
    pub init_byte: u32,
}

/// 逐根初始化条目的固定编码宽度。
pub const TYPED_ROOT_BYTES: usize = 12;

/// 一个安全点的机器位置与根位图输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafepointLayout {
    pub pc_offset: u32,
    pub kind: u8,
    pub dirty: bool,
    pub copy_allowed: bool,
    pub scan_allowed: bool,
    /// 五类 slot 位图（按 slot 序，最低有效 bit 先写）。
    pub slots: [Vec<u64>; 5],
    /// 五类寄存器掩码；bit15 必须为 0，普通函数保留位必须为 0。
    pub registers: [u16; 5],
    /// 由帧内初始化字节门控的根；这些字不得同时出现在 `slots` 位图里。
    pub typed: Vec<TypedRoot>,
    /// 该安全点所属函数的布局下标。
    pub function: u32,
    /// slot 总数。
    pub slot_count: u32,
}

/// 编码一个完整 section。
pub fn encode(
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
    while !(output.len() as u64).is_multiple_of(TABLE_ALIGN) {
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
pub fn decode(bytes: &[u8]) -> Result<(u32, u32, u32), RawModelError> {
    if bytes.len() < HEADER_BYTES {
        return Err(RawModelError::new("栈图 section 小于固定 header"));
    }
    if &bytes[..8] != STACKMAP_MAGIC {
        return Err(RawModelError::new("栈图 section 魔数不匹配"));
    }
    let version = u16::from_le_bytes(bytes[8..10].try_into().expect("版本字段"));
    if version != STACKMAP_SECTION_VERSION {
        // 历史版本一律拒收，不保留双解码路径：v1 的 handle 与压缩引用表示不同，v2 的屏障
        // runtime 调用没有安全点记录。
        return Err(RawModelError::new(match version {
            1 => "拒绝 version 1 记录：handle 与压缩引用表示已变化",
            2 => "拒绝 version 2 记录：屏障 runtime 调用缺少安全点记录",
            _ => "栈图 section 版本不匹配",
        }));
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
    let mut previous_safepoint_end = 0_u32;
    for index in 0..function_count {
        let base = (functions_offset as usize) + index as usize * FUNCTION_BYTES;
        let rva = u64::from_le_bytes(bytes[base..base + 8].try_into().expect("函数字段"));
        let size = u32::from_le_bytes(bytes[base + 8..base + 12].try_into().expect("函数字段"));
        let frame = u32::from_le_bytes(bytes[base + 12..base + 16].try_into().expect("函数字段"));
        let safepoint_start =
            u32::from_le_bytes(bytes[base + 16..base + 20].try_into().expect("函数字段"));
        let safepoint_count_for_function =
            u32::from_le_bytes(bytes[base + 20..base + 24].try_into().expect("函数字段"));
        let safepoint_end = safepoint_start
            .checked_add(safepoint_count_for_function)
            .ok_or_else(|| RawModelError::new("函数安全点范围溢出"))?;
        let flags = u16::from_le_bytes(bytes[base + 28..base + 30].try_into().expect("函数字段"));
        if safepoint_start < previous_safepoint_end || safepoint_end > safepoint_count {
            return Err(RawModelError::new("函数安全点范围没有连续覆盖安全点表"));
        }
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
        previous_safepoint_end = safepoint_end;
    }
    // 安全点表按函数范围连续编码；保持单调游标，避免每个安全点从函数 0 重新扫描。
    let mut previous_function = u32::MAX;
    let mut previous_offset = u32::MAX;
    let mut function_index = 0_u32;
    let mut current_function_start = 0_u32;
    let mut current_function_end = 0_u32;
    let mut current_code_size = 0_u32;
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
        loop {
            if function_index >= function_count {
                return Err(RawModelError::new("安全点没有所属函数范围"));
            }
            if index >= current_function_start && index < current_function_end {
                break;
            }
            if index < current_function_start {
                return Err(RawModelError::new("安全点表函数范围非单调"));
            }
            let function_base = (functions_offset as usize)
                + usize::try_from(function_index).expect("函数下标适配宿主") * FUNCTION_BYTES;
            current_function_start = u32::from_le_bytes(
                bytes[function_base + 16..function_base + 20]
                    .try_into()
                    .expect("函数字段"),
            );
            let count = u32::from_le_bytes(
                bytes[function_base + 20..function_base + 24]
                    .try_into()
                    .expect("函数字段"),
            );
            current_function_end = current_function_start
                .checked_add(count)
                .ok_or_else(|| RawModelError::new("函数安全点范围溢出"))?;
            current_code_size = u32::from_le_bytes(
                bytes[function_base + 8..function_base + 12]
                    .try_into()
                    .expect("函数字段"),
            );
            if index < current_function_start {
                return Err(RawModelError::new("安全点没有所属函数范围"));
            }
            if index >= current_function_end {
                function_index = function_index.saturating_add(1);
            }
        }
        let function = function_index;
        if function != previous_function {
            previous_function = function;
        } else if pc <= previous_offset {
            return Err(RawModelError::new("同一函数的安全点没有按偏移严格递增"));
        }
        if pc >= current_code_size {
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
    // 记录字节的全部结构约束（位图互斥、尾 bit、逐根初始化条目排序与去重）在编码侧
    // 复用同一份校验，避免编码器产出自己解不出的 section。
    verify_map_record(&map_record(layout))?;
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
    // 先检查记录长度，避免损坏的 slot_count 驱动越界读取或不受限分配。
    let bitmap_bytes = words
        .checked_mul(5)
        .and_then(|bytes| bytes.checked_add(16))
        .ok_or_else(|| RawModelError::new("map 记录槽数量溢出"))?;
    if bitmap_bytes > record.len() {
        return Err(RawModelError::new("map 记录位图越界"));
    }
    let mut cursor = 16usize;
    let mut bitmaps: [&[u8]; 5] = [&[]; 5];
    for class in &mut bitmaps {
        *class = &record[cursor..cursor + words];
        cursor += words;
    }
    while !cursor.is_multiple_of(4) {
        if record.get(cursor).copied().unwrap_or(1) != 0 {
            return Err(RawModelError::new("map 记录 padding 必须为 0"));
        }
        cursor += 1;
    }
    // 尾部是逐根初始化条目：计数 + 固定 12 字节条目，按帧内字下标严格递增，且不得与固定
    // 位图重复登记同一个字。
    let header = record
        .get(cursor..cursor + 4)
        .ok_or_else(|| RawModelError::new("map 记录缺少逐根初始化计数"))?;
    let typed = u32::from_le_bytes(header.try_into().expect("计数字段")) as usize;
    let start = cursor + 4;
    let expected = typed
        .checked_mul(TYPED_ROOT_BYTES)
        .and_then(|bytes| start.checked_add(bytes))
        .ok_or_else(|| RawModelError::new("map 记录逐根初始化条目溢出"))?;
    if expected != record.len() {
        return Err(RawModelError::new("map 记录逐根初始化条目长度不符"));
    }
    let mut previous: Option<u32> = None;
    for index in 0..typed {
        let at = start + index * TYPED_ROOT_BYTES;
        let word = u32::from_le_bytes(record[at..at + 4].try_into().expect("条目字段"));
        let kind = u32::from_le_bytes(record[at + 4..at + 8].try_into().expect("条目字段"));
        if kind > 4 {
            return Err(RawModelError::new("逐根初始化条目根种类越界"));
        }
        if previous.is_some_and(|previous| previous >= word) {
            return Err(RawModelError::new("逐根初始化条目必须按字下标严格递增"));
        }
        previous = Some(word);
        let byte = (word / 8) as usize;
        let bit = 1_u8 << (word % 8);
        if byte < words && bitmaps.iter().any(|class| class[byte] & bit != 0) {
            return Err(RawModelError::new("逐根初始化条目与固定位图重复"));
        }
    }
    // 五个位图逐字节检查，帧大小只受函数 frame_size 限制，不存在固定 lane 上界。
    if slot_count % 8 != 0 {
        let mask = u8::MAX << (slot_count % 8);
        if bitmaps
            .iter()
            .any(|class| class.last().is_some_and(|last| last & mask != 0))
        {
            return Err(RawModelError::new("槽位图尾 bit 必须为 0"));
        }
    }
    for offset in 0..words {
        let mut merged = 0u8;
        for class in &bitmaps {
            if merged & class[offset] != 0 {
                return Err(RawModelError::new("五类槽位图不互斥"));
            }
            merged |= class[offset];
        }
    }
    // 记录头包含五类完整掩码；最后两个字节为规范保留字段。
    let mut masks = [0u16; 5];
    for (index, mask) in masks.iter_mut().enumerate() {
        let start = 4 + index * 2;
        *mask = u16::from_le_bytes(record[start..start + 2].try_into().expect("掩码字段"));
        if *mask & (1 << 15) != 0 {
            return Err(RawModelError::new("寄存器掩码 bit15 必须为 0"));
        }
    }
    if u16::from_le_bytes(record[14..16].try_into().expect("保留字段")) != 0 {
        return Err(RawModelError::new("map 记录保留字段必须为 0"));
    }
    let mut merged = 0u16;
    for mask in masks {
        if merged & mask != 0 {
            return Err(RawModelError::new("五类寄存器掩码不互斥"));
        }
        merged |= mask;
    }
    let _ = REGISTER_NAMES;
    Ok(())
}

fn map_record(layout: &SafepointLayout) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&layout.slot_count.to_le_bytes());
    // 记录头固定存五个 u16 掩码：direct、interior、handle、compressed、stack；
    // 最后两个字节为保留字段，确保第五类寄存器根不被截断。
    for mask in &layout.registers {
        output.extend_from_slice(&mask.to_le_bytes());
    }
    output.extend_from_slice(&[0, 0]);
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
    // 尾部追加逐根初始化条目：先计数，再固定 12 字节条目。条目按帧内字下标严格递增，
    // 与固定位图互斥（校验在 `verify_map_record` 执行）。
    output.extend_from_slice(
        &u32::try_from(layout.typed.len())
            .expect("逐根初始化条目数适配 u32")
            .to_le_bytes(),
    );
    for root in &layout.typed {
        output.extend_from_slice(&root.word.to_le_bytes());
        output.extend_from_slice(&root.root_kind.to_le_bytes());
        output.extend_from_slice(&root.init_byte.to_le_bytes());
    }
    output
}

fn align_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

/// 解码后的函数记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedFunction {
    pub code_rva: u64,
    pub code_size: u32,
    pub frame_size: u32,
    pub safepoint_start: u32,
    pub safepoint_count: u32,
    pub unwind_index: u32,
    pub flags: u16,
}

/// 解码后的安全点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedSafepoint {
    pub pc_offset: u32,
    pub map_index: u32,
    pub kind: u8,
    pub flags: u8,
}

/// 解码后的根图：五类槽下标与寄存器掩码。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedMap {
    pub slots: [Vec<u32>; 5],
    pub registers: [u16; 5],
    /// 由帧内初始化字节门控的根，按字下标递增。
    pub typed: Vec<TypedRoot>,
}

/// 解码后的栈图 section。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedSection {
    pub functions: Vec<DecodedFunction>,
    pub safepoints: Vec<DecodedSafepoint>,
    pub maps: Vec<DecodedMap>,
}

/// 解码完整 section，供 walker 消费最终机器布局。
pub fn decode_tables(bytes: &[u8]) -> Result<DecodedSection, RawModelError> {
    decode(bytes)?;
    let function_count = u32::from_le_bytes(bytes[12..16].try_into().expect("计数字段"));
    let safepoint_count = u32::from_le_bytes(bytes[16..20].try_into().expect("计数字段"));
    let map_count = u32::from_le_bytes(bytes[20..24].try_into().expect("计数字段"));
    let functions_offset = u64::from_le_bytes(bytes[32..40].try_into().expect("偏移字段")) as usize;
    let safepoints_offset =
        u64::from_le_bytes(bytes[40..48].try_into().expect("偏移字段")) as usize;
    let map_index_offset = u64::from_le_bytes(bytes[48..56].try_into().expect("偏移字段")) as usize;
    let mut functions = Vec::with_capacity(function_count as usize);
    for index in 0..function_count as usize {
        let base = functions_offset + index * FUNCTION_BYTES;
        functions.push(DecodedFunction {
            code_rva: u64::from_le_bytes(bytes[base..base + 8].try_into().expect("函数字段")),
            code_size: u32::from_le_bytes(bytes[base + 8..base + 12].try_into().expect("函数字段")),
            frame_size: u32::from_le_bytes(
                bytes[base + 12..base + 16].try_into().expect("函数字段"),
            ),
            safepoint_start: u32::from_le_bytes(
                bytes[base + 16..base + 20].try_into().expect("函数字段"),
            ),
            safepoint_count: u32::from_le_bytes(
                bytes[base + 20..base + 24].try_into().expect("函数字段"),
            ),
            unwind_index: u32::from_le_bytes(
                bytes[base + 24..base + 28].try_into().expect("函数字段"),
            ),
            flags: u16::from_le_bytes(bytes[base + 28..base + 30].try_into().expect("函数字段")),
        });
    }
    let mut safepoints = Vec::with_capacity(safepoint_count as usize);
    for index in 0..safepoint_count as usize {
        let base = safepoints_offset + index * SAFEPOINT_BYTES;
        safepoints.push(DecodedSafepoint {
            pc_offset: u32::from_le_bytes(bytes[base..base + 4].try_into().expect("安全点字段")),
            map_index: u32::from_le_bytes(
                bytes[base + 4..base + 8].try_into().expect("安全点字段"),
            ),
            kind: bytes[base + 8],
            flags: bytes[base + 9],
        });
    }
    let mut maps = Vec::with_capacity(map_count as usize);
    for index in 0..map_count as usize {
        let base = map_index_offset + index * 8;
        let start =
            u64::from_le_bytes(bytes[base..base + 8].try_into().expect("索引字段")) as usize;
        let end =
            u64::from_le_bytes(bytes[base + 8..base + 16].try_into().expect("索引字段")) as usize;
        maps.push(decode_map(&bytes[start..end])?);
    }
    Ok(DecodedSection {
        functions,
        safepoints,
        maps,
    })
}

fn decode_map(record: &[u8]) -> Result<DecodedMap, RawModelError> {
    let slot_count = u32::from_le_bytes(record[0..4].try_into().expect("槽字段"));
    let mut registers = [0_u16; 5];
    for (index, mask) in registers.iter_mut().enumerate() {
        let start = 4 + index * 2;
        *mask = u16::from_le_bytes(record[start..start + 2].try_into().expect("掩码字段"));
    }
    let bytes = slot_count.div_ceil(8) as usize;
    let mut slots: [Vec<u32>; 5] = std::array::from_fn(|_| Vec::new());
    let mut cursor = 16_usize;
    for class in &mut slots {
        for byte_index in 0..bytes {
            let byte = record[cursor + byte_index];
            for bit in 0..8 {
                let slot = byte_index * 8 + bit;
                if slot < slot_count as usize && byte & (1 << bit) != 0 {
                    class.push(slot as u32);
                }
            }
        }
        cursor += bytes;
    }
    while !cursor.is_multiple_of(4) {
        cursor += 1;
    }
    let count = u32::from_le_bytes(record[cursor..cursor + 4].try_into().expect("计数字段"));
    let mut typed = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let at = cursor + 4 + index * TYPED_ROOT_BYTES;
        typed.push(TypedRoot {
            word: u32::from_le_bytes(record[at..at + 4].try_into().expect("条目字段")),
            root_kind: u32::from_le_bytes(record[at + 4..at + 8].try_into().expect("条目字段")),
            init_byte: u32::from_le_bytes(record[at + 8..at + 12].try_into().expect("条目字段")),
        });
    }
    Ok(DecodedMap {
        slots,
        registers,
        typed,
    })
}
