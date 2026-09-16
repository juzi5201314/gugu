//! trace descriptor 的运行时解释器：按 Bitmap 或 Program 表示扫描对象的 managed pointer word。
//!
//! 解释器只消费已验证的 descriptor 字节（`gc_metadata_schema::boot_verify` 的同一套 tiling
//! 与位图不变量），不依赖调用方重复校验。递归深度受 `TRACE_MAX_DEPTH` 约束，所有取字节都走
//! 边界检查，因此损坏的 program 只产生 `RawInvariant` 而不是 panic。
//!
//! REPEAT/REPEAT_FIELD 的 base 与 stride 以 8 字节 word 计；SWITCH 的 tag 以 payload 字节计；
//! `ARENA_SLOTS` 不携带自己的扫描位，由调用方按 backing 的 initialized 位图与元素 descriptor
//! 展开，因此这里只把它标记为「含 arena backing 语义」。

use super::gc_metadata_schema::{TraceKind, TraceOp, decode_uleb};
use super::slab::RawInvariant;

/// trace program 允许的最大嵌套深度；与验证器共用同一上界。
pub(crate) const TRACE_MAX_DEPTH: u8 = 32;

/// 一次 descriptor 扫描的结果。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TraceScan {
    /// 扫描到的 managed pointer word 数。
    pub pointers: u32,
    /// descriptor 是否含 `ARENA_SLOTS`，需要按 backing initialized 位图展开。
    pub arena_slots: bool,
}

/// 访问一个 managed pointer word 的字节视图；`address` 是该 word 的 payload 地址。
pub(crate) type TraceVisitor<'a> = dyn FnMut(&mut [u8; 8], u64) -> Result<(), RawInvariant> + 'a;

/// 按 descriptor 扫描一个对象的 payload。
///
/// `payload_base` 是 `payload[0]` 的地址，因此 `address = payload_base + word * 8`。
/// 对 Bitmap 表示，direct 位所在 word 与 interior 位所在 word 都按 managed pointer 处理；
/// 调用方按 `address` 决定是直接解析还是用 `page_covering_object` 回表。
pub(crate) fn walk_descriptor(
    descriptor: &[u8],
    payload: &mut [u8],
    payload_base: u64,
    visit: &mut TraceVisitor<'_>,
) -> Result<TraceScan, RawInvariant> {
    match descriptor.first().copied() {
        Some(kind) if kind == TraceKind::None as u8 => Ok(TraceScan::default()),
        Some(kind) if kind == TraceKind::Bitmap as u8 => {
            walk_bitmap(descriptor, payload, payload_base, visit)
        }
        Some(kind) if kind == TraceKind::Program as u8 => {
            let length = read_u32(descriptor, 1)?;
            let program = descriptor
                .get(5..5 + length as usize)
                .ok_or_else(|| RawInvariant::new("trace program 越界"))?;
            let mut index = 0usize;
            let mut scan = TraceScan::default();
            let end = walk_program(
                program,
                &mut index,
                payload,
                payload_base,
                0,
                0,
                visit,
                &mut scan,
            )?;
            if end != program.len() {
                return Err(RawInvariant::new("trace program 未恰好在 END 处结束"));
            }
            Ok(scan)
        }
        _ => Err(RawInvariant::new("未知 trace descriptor kind")),
    }
}

/// Bitmap 表示：`kind, reserved[3], word_count u32, direct, interior, padding`。
fn walk_bitmap(
    descriptor: &[u8],
    payload: &mut [u8],
    payload_base: u64,
    visit: &mut TraceVisitor<'_>,
) -> Result<TraceScan, RawInvariant> {
    let word_count = read_u32(descriptor, 4)?;
    let bitmap_bytes = usize::try_from(word_count.div_ceil(8)).expect("bitmap 字节数适配宿主");
    let direct = descriptor
        .get(8..8 + bitmap_bytes)
        .ok_or_else(|| RawInvariant::new("trace bitmap 越界"))?;
    let interior = descriptor
        .get(8 + bitmap_bytes..8 + bitmap_bytes * 2)
        .ok_or_else(|| RawInvariant::new("trace bitmap 越界"))?;
    let mut scan = TraceScan::default();
    for byte_index in 0..bitmap_bytes {
        let mut bits =
            (u16::from(direct[byte_index]) | (u16::from(interior[byte_index]) << 8)) as u32;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            bits &= bits - 1;
            let word = u64::from(byte_index as u32) * 8 + u64::from(bit);
            visit_word(payload, payload_base, word, 0, visit, &mut scan)?;
        }
    }
    if u64::from(scan.pointers) > u64::from(word_count) {
        return Err(RawInvariant::new("trace bitmap 扫描位数超过 word 数"));
    }
    Ok(scan)
}

/// 扫描一个 program body；`shift_words` 是外层 REPEAT 的累加位移。
#[expect(
    clippy::too_many_arguments,
    reason = "解释器把 program、payload 与扫描统计逐层传递，避免为每层分配帧结构"
)]
fn walk_program(
    program: &[u8],
    index: &mut usize,
    payload: &mut [u8],
    payload_base: u64,
    shift_words: u64,
    depth: u8,
    visit: &mut TraceVisitor<'_>,
    scan: &mut TraceScan,
) -> Result<usize, RawInvariant> {
    if depth > TRACE_MAX_DEPTH {
        return Err(RawInvariant::new("trace program 解释嵌套过深"));
    }
    loop {
        let op = *program
            .get(*index)
            .ok_or_else(|| RawInvariant::new("trace program 缺少 END"))?;
        *index += 1;
        match op {
            x if x == TraceOp::End as u8 => return Ok(*index),
            x if x == TraceOp::Direct as u8 || x == TraceOp::Interior as u8 => {
                let base = uleb(program, index)?;
                let count = uleb(program, index)?;
                for item in 0..count {
                    let word = base
                        .checked_add(item)
                        .and_then(|word| word.checked_add(shift_words))
                        .ok_or_else(|| RawInvariant::new("trace word 下标溢出"))?;
                    visit_word(payload, payload_base, word, 0, visit, scan)?;
                }
            }
            x if x == TraceOp::Repeat as u8 => {
                let base = uleb(program, index)?;
                let count = uleb(program, index)?;
                let stride = uleb(program, index)?;
                let body_len = read_u32(program, *index)?;
                *index += 4;
                let body_end = *index + body_len as usize;
                if body_end > program.len() {
                    return Err(RawInvariant::new("trace repeat body 越界"));
                }
                for item in 0..count {
                    let shift = base
                        .checked_add(item.saturating_mul(stride))
                        .and_then(|shift| shift.checked_add(shift_words))
                        .ok_or_else(|| RawInvariant::new("trace repeat 位移溢出"))?;
                    let mut cursor = *index;
                    walk_program(
                        program,
                        &mut cursor,
                        payload,
                        payload_base,
                        shift,
                        depth + 1,
                        visit,
                        scan,
                    )?;
                }
                *index = body_end;
            }
            x if x == TraceOp::RepeatField as u8 => {
                let base = uleb(program, index)?;
                let count_offset = uleb(program, index)?;
                let width = uleb(program, index)?;
                let stride = uleb(program, index)?;
                let body_len = read_u32(program, *index)?;
                *index += 4;
                let body_end = *index + body_len as usize;
                if body_end > program.len() {
                    return Err(RawInvariant::new("trace repeat-field body 越界"));
                }
                let count = read_field(payload, count_offset, width)?;
                for item in 0..count {
                    let shift = base
                        .checked_add(item.saturating_mul(stride))
                        .and_then(|shift| shift.checked_add(shift_words))
                        .ok_or_else(|| RawInvariant::new("trace repeat-field 位移溢出"))?;
                    let mut cursor = *index;
                    walk_program(
                        program,
                        &mut cursor,
                        payload,
                        payload_base,
                        shift,
                        depth + 1,
                        visit,
                        scan,
                    )?;
                }
                *index = body_end;
            }
            x if x == TraceOp::Switch as u8 => {
                let tag_offset = uleb(program, index)?;
                let width = uleb(program, index)?;
                let case_count = uleb(program, index)?;
                let tag = read_field(payload, tag_offset, width)?;
                let mut chosen: Option<(usize, usize)> = None;
                for _ in 0..case_count {
                    let case_tag = read_u64(program, *index)?;
                    *index += 8;
                    let body_len = read_u32(program, *index)?;
                    *index += 4;
                    let body_end = *index + body_len as usize;
                    if body_end > program.len() {
                        return Err(RawInvariant::new("trace switch case body 越界"));
                    }
                    if case_tag == tag && chosen.is_none() {
                        chosen = Some((*index, body_end));
                    }
                    *index = body_end;
                }
                let default_len = read_u32(program, *index)?;
                *index += 4;
                let default_end = *index + default_len as usize;
                if default_end > program.len() {
                    return Err(RawInvariant::new("trace switch default body 越界"));
                }
                let default = (*index, default_end);
                let (body_start, body_end) = chosen.unwrap_or(default);
                let mut cursor = body_start;
                walk_program(
                    program,
                    &mut cursor,
                    payload,
                    payload_base,
                    shift_words,
                    depth + 1,
                    visit,
                    scan,
                )?;
                *index = body_end;
            }
            x if x == TraceOp::ArenaSlots as u8 => scan.arena_slots = true,
            _ => return Err(RawInvariant::new("未知 trace op")),
        }
    }
}

/// 访问一个 payload word；`extra` 保留给调用方需要的附加位移。
fn visit_word(
    payload: &mut [u8],
    payload_base: u64,
    word: u64,
    extra: u64,
    visit: &mut TraceVisitor<'_>,
    scan: &mut TraceScan,
) -> Result<(), RawInvariant> {
    let offset = word
        .checked_mul(8)
        .and_then(|offset| offset.checked_add(extra))
        .ok_or_else(|| RawInvariant::new("trace word 字节偏移溢出"))?;
    let offset =
        usize::try_from(offset).map_err(|_| RawInvariant::new("trace word 偏移超出宿主"))?;
    let end = offset
        .checked_add(8)
        .ok_or_else(|| RawInvariant::new("trace word 范围溢出"))?;
    let address = payload_base
        .checked_add(offset as u64)
        .ok_or_else(|| RawInvariant::new("trace word 地址溢出"))?;
    let word_bytes: &mut [u8; 8] = payload
        .get_mut(offset..end)
        .ok_or_else(|| RawInvariant::new("trace word 越过 payload"))?
        .try_into()
        .expect("word 视图宽度固定");
    visit(word_bytes, address)?;
    scan.pointers = scan
        .pointers
        .checked_add(1)
        .ok_or_else(|| RawInvariant::new("trace 指针计数溢出"))?;
    Ok(())
}

/// 按 `width` 从 payload 的小端字段读取计数或判别值。
fn read_field(payload: &[u8], offset: u64, width: u64) -> Result<u64, RawInvariant> {
    if !matches!(width, 1 | 2 | 4 | 8) {
        return Err(RawInvariant::new("trace 字段宽度必须是 1、2、4 或 8"));
    }
    let offset =
        usize::try_from(offset).map_err(|_| RawInvariant::new("trace 字段偏移超出宿主"))?;
    let end = offset
        .checked_add(width as usize)
        .ok_or_else(|| RawInvariant::new("trace 字段范围溢出"))?;
    let bytes = payload
        .get(offset..end)
        .ok_or_else(|| RawInvariant::new("trace 字段越过 payload"))?;
    let mut value = 0u64;
    for (shift, byte) in bytes.iter().enumerate() {
        value |= u64::from(*byte) << (shift * 8);
    }
    Ok(value)
}

/// 解码 ULEB 操作数；验证器错误转换成平面不变量。
fn uleb(program: &[u8], index: &mut usize) -> Result<u64, RawInvariant> {
    decode_uleb(program, index).map_err(|error| RawInvariant::new(error.message().to_owned()))
}

fn read_u32(bytes: &[u8], start: usize) -> Result<u32, RawInvariant> {
    let end = start
        .checked_add(4)
        .ok_or_else(|| RawInvariant::new("u32 范围溢出"))?;
    let slice: [u8; 4] = bytes
        .get(start..end)
        .ok_or_else(|| RawInvariant::new("u32 越界"))?
        .try_into()
        .expect("u32 宽度固定");
    Ok(u32::from_le_bytes(slice))
}

fn read_u64(bytes: &[u8], start: usize) -> Result<u64, RawInvariant> {
    let end = start
        .checked_add(8)
        .ok_or_else(|| RawInvariant::new("u64 范围溢出"))?;
    let slice: [u8; 8] = bytes
        .get(start..end)
        .ok_or_else(|| RawInvariant::new("u64 越界"))?
        .try_into()
        .expect("u64 宽度固定");
    Ok(u64::from_le_bytes(slice))
}
