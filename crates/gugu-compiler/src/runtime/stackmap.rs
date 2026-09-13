//! 栈图 walker：函数与安全点二分查找、五类根扫描、落地选择与复制输入组装。
//!
//! walker 只消费调用方注入的确定性布局（函数表、安全点表、map 表、handle 表、
//! cage 表），真实机器布局由后端在分配后填充。找不到函数或安全点、帧越界、
//! handle 代际过期、压缩引用越界一律返回 `RuntimeInvariant` 错误，从不猜测
//! 相邻 map 或退回保守扫描。字级复制委托 `stack::StackImage::relocate`，本模块
//! 只产出有序槽表与增量。

use super::model::RawModelError;
use super::stack::StackImage;

/// walker 扫描到的一个根位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScannedRoot {
    /// 直接堆引用：槽内字偏移（相对已用范围起点）。
    Direct(u32),
    /// 内部堆引用：槽内字偏移与对象内字节增量。
    Interior { offset: u32, delta: u32 },
    /// handle 引用：槽内字偏移与解析后的 payload 描述。
    Handle { offset: u32, payload: u64 },
    /// 压缩引用：槽内字偏移与解码后的目标描述。
    Compressed { offset: u32, target: u64 },
    /// 栈内引用：槽内字偏移。
    Stack(u32),
}

/// 调用方注入的确定性函数布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalkFunction {
    pub(crate) code_rva: u64,
    pub(crate) code_size: u32,
    pub(crate) frame_size: u32,
}

/// 调用方注入的确定性安全点布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalkSafepoint {
    pub(crate) function: u32,
    pub(crate) pc_offset: u32,
    pub(crate) kind: u8,
    /// 该点的 map 记录在 map 表中的下标。
    pub(crate) map: u32,
}

/// 调用方注入的确定性 map 记录：五类槽偏移表（相对已用范围起点的字偏移）。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalkMap {
    pub(crate) direct: Vec<u32>,
    pub(crate) interior: Vec<u32>,
    pub(crate) handle: Vec<u32>,
    pub(crate) compressed: Vec<u32>,
    pub(crate) stack: Vec<u32>,
}

/// handle 表项：slot 的当前 payload 与 generation。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleSlot {
    pub(crate) payload: u64,
    pub(crate) generation: u64,
}

/// 压缩笼描述：cage 基址、长度与当前 generation。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CageDescriptor {
    pub(crate) base: u64,
    pub(crate) len: u64,
    pub(crate) generation: u64,
}

/// 落地记录：`[pc_start, pc_end)` 范围内的清理链入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LandingRecord {
    pub(crate) pc_start: u32,
    pub(crate) pc_end: u32,
    pub(crate) landing_pc: u32,
    pub(crate) cleanup: u32,
}

/// walker 的只读视图：全部表按规范排序。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalkWorld<'a> {
    pub(crate) functions: &'a [WalkFunction],
    pub(crate) safepoints: &'a [WalkSafepoint],
    pub(crate) maps: &'a [WalkMap],
    pub(crate) handles: &'a [HandleSlot],
    pub(crate) cages: &'a [CageDescriptor],
    pub(crate) landings: &'a [LandingRecord],
}

/// 在按 `code_rva` 排序的函数表中二分查找包含 `pc` 的函数。
pub(crate) fn find_function(world: &WalkWorld<'_>, pc: u64) -> Result<u32, RawModelError> {
    let mut low = 0usize;
    let mut high = world.functions.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let function = &world.functions[middle];
        let end = function
            .code_rva
            .checked_add(u64::from(function.code_size))
            .ok_or_else(|| RawModelError::new("函数代码范围溢出"))?;
        if pc < function.code_rva {
            high = middle;
        } else if pc >= end {
            low = middle + 1;
        } else {
            return Ok(middle as u32);
        }
    }
    Err(RawModelError::new("程序计数器不在任何函数代码区"))
}

/// 在函数的安全点范围内二分查找精确匹配 `pc_offset` 的记录。
pub(crate) fn find_safepoint(
    world: &WalkWorld<'_>,
    function: u32,
    pc_offset: u32,
) -> Result<u32, RawModelError> {
    let mut candidates: Vec<(u32, &WalkSafepoint)> = world
        .safepoints
        .iter()
        .enumerate()
        .filter(|(_, safepoint)| safepoint.function == function)
        .map(|(index, safepoint)| (index as u32, safepoint))
        .collect();
    candidates.sort_by_key(|(_, safepoint)| safepoint.pc_offset);
    let mut low = 0usize;
    let mut high = candidates.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let offset = candidates[middle].1.pc_offset;
        if pc_offset < offset {
            high = middle;
        } else if pc_offset > offset {
            low = middle + 1;
        } else {
            return Ok(candidates[middle].0);
        }
    }
    Err(RawModelError::new("安全点偏移没有精确匹配的记录"))
}

/// 扫描一个安全点的五类根：按槽偏移排序返回。
///
/// `words` 为已用栈范围的字数组（小端）；handle 与压缩引用的字分别经表做
/// checked 解析，过期代际或越界一律返回错误。
pub(crate) fn scan_roots(
    world: &WalkWorld<'_>,
    safepoint: u32,
    words: &[u64],
) -> Result<Vec<ScannedRoot>, RawModelError> {
    let record = world
        .safepoints
        .get(usize::try_from(safepoint).expect("安全点下标"))
        .ok_or_else(|| RawModelError::new("安全点下标越界"))?;
    let map = world
        .maps
        .get(usize::try_from(record.map).expect("map 下标"))
        .ok_or_else(|| RawModelError::new("安全点引用越界 map"))?;
    // 五类位图互斥由编码器保证；此处按读取顺序复验重叠。
    let mut seen = std::collections::BTreeSet::new();
    for offset in map
        .direct
        .iter()
        .chain(&map.interior)
        .chain(&map.handle)
        .chain(&map.compressed)
        .chain(&map.stack)
    {
        if !seen.insert(offset) {
            return Err(RawModelError::new("同一槽在两类位图中重复出现"));
        }
    }
    let mut roots = Vec::new();
    for offset in &map.direct {
        let word = word_at(words, *offset)?;
        if word == 0 {
            continue;
        }
        roots.push(ScannedRoot::Direct(*offset));
    }
    for offset in &map.interior {
        let word = word_at(words, *offset)?;
        if word == 0 {
            continue;
        }
        // interior 增量由调用方经 span metadata 推导；walker 只透传低 32 位。
        roots.push(ScannedRoot::Interior {
            offset: *offset,
            delta: (word & 0xffff_ffff) as u32,
        });
    }
    for offset in &map.handle {
        let word = word_at(words, *offset)?;
        let slot = usize::try_from(word).map_err(|_| RawModelError::new("handle 槽编号越界"))?;
        let handle = world
            .handles
            .get(slot)
            .ok_or_else(|| RawModelError::new("handle 槽越界"))?;
        if handle.generation == 0 {
            return Err(RawModelError::new("过期 handle 代际不得解析"));
        }
        // 空 handle 字（槽 0 且 payload 为 0）容忍跳过：与 direct 空值规则一致。
        if word == 0 {
            continue;
        }
        roots.push(ScannedRoot::Handle {
            offset: *offset,
            payload: handle.payload,
        });
    }
    for offset in &map.compressed {
        let word = word_at(words, *offset)?;
        if word == 0 {
            continue;
        }
        let cage = (word >> 56) as u8;
        let generation = ((word >> 32) & 0xff_ffff) as u64;
        let offset_value = word & 0xffff_ffff;
        let descriptor = world
            .cages
            .get(usize::from(cage))
            .ok_or_else(|| RawModelError::new("压缩引用 cage 未登记"))?;
        if generation != descriptor.generation {
            return Err(RawModelError::new("压缩引用代际过期"));
        }
        if offset_value >= descriptor.len {
            return Err(RawModelError::new("压缩引用偏移越过 cage 范围"));
        }
        roots.push(ScannedRoot::Compressed {
            offset: *offset,
            target: descriptor.base + offset_value,
        });
    }
    for offset in &map.stack {
        let word = word_at(words, *offset)?;
        if word == 0 {
            continue;
        }
        roots.push(ScannedRoot::Stack(*offset));
    }
    Ok(roots)
}

fn word_at(words: &[u64], offset: u32) -> Result<u64, RawModelError> {
    // `WalkMap` 的偏移以字为单位（与 section 位图的槽序一致）。
    let index = usize::try_from(offset).expect("槽偏移适配宿主");
    words
        .get(index)
        .copied()
        .ok_or_else(|| RawModelError::new("槽偏移越过已用栈范围"))
}

/// 选择覆盖 `pc_offset` 的最内层落地记录。
pub(crate) fn select_landing<'a>(
    world: &WalkWorld<'a>,
    pc_offset: u32,
) -> Result<&'a LandingRecord, RawModelError> {
    let mut best: Option<&LandingRecord> = None;
    for record in world.landings {
        if record.pc_start <= pc_offset && pc_offset < record.pc_end {
            let narrower = best.is_none_or(|best: &LandingRecord| {
                (record.pc_end - record.pc_start) < (best.pc_end - best.pc_start)
            });
            if narrower {
                best = Some(record);
            }
        }
    }
    best.ok_or_else(|| RawModelError::new("程序计数器没有覆盖的落地记录"))
}

/// 由扫描到的栈根组装复制输入：槽偏移按字节换算后必须严格升序且不重叠。
pub(crate) fn copy_input(roots: &[ScannedRoot], used: usize) -> Result<StackImage, RawModelError> {
    let mut offsets: Vec<u32> = roots
        .iter()
        .filter_map(|root| match root {
            ScannedRoot::Stack(offset) => Some(*offset * 8),
            _ => None,
        })
        .collect();
    offsets.sort_unstable();
    offsets.dedup();
    let mut previous_end = 0u32;
    for offset in &offsets {
        if *offset < previous_end
            || !offset.is_multiple_of(8)
            || (*offset as usize)
                .checked_add(8)
                .is_none_or(|end| end > used)
        {
            return Err(RawModelError::new("栈复制槽表越界、重叠或未对齐"));
        }
        previous_end = offset + 8;
    }
    Ok(StackImage {
        bytes: vec![0; used],
        stack_roots: offsets,
        stack_registers: 0,
    })
}

/// 校验 bridge 记录的帧范围：`frame_offset + frame_size` 必须落在已用栈范围内。
pub(crate) fn verify_bridge_frame(
    frame_offset: u64,
    frame_size: u64,
    used: u64,
) -> Result<(), RawModelError> {
    let end = frame_offset
        .checked_add(frame_size)
        .ok_or_else(|| RawModelError::new("bridge 帧范围溢出"))?;
    if end > used {
        return Err(RawModelError::new("bridge 帧越过已用栈范围"));
    }
    Ok(())
}
