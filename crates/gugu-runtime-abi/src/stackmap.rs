//! 栈图 walker：函数与安全点二分查找、五类根扫描、落地选择与复制输入组装。
//!
//! walker 只消费调用方注入的确定性布局（函数表、安全点表、map 表、handle 表）与
//! `CompressionPlane`；真实机器布局由后端在分配后填充。找不到函数或安全点、帧越界、
//! handle 代际过期、压缩引用越界一律返回 `RuntimeInvariant` 错误，从不猜测
//! 相邻 map 或退回保守扫描。字级复制委托 `stack::StackImage::relocate`，本模块
//! 只产出有序槽表与增量。

use super::cage::CompressionPlane;
use super::model::RawModelError;
use super::stack::StackImage;

/// walker 扫描到的一个根位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScannedRoot {
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
pub struct WalkFunction {
    pub code_rva: u64,
    pub code_size: u32,
    pub frame_size: u32,
}

/// 调用方注入的确定性安全点布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalkSafepoint {
    pub function: u32,
    pub pc_offset: u32,
    pub kind: u8,
    /// 该点的 map 记录在 map 表中的下标。
    pub map: u32,
}

/// 调用方注入的确定性 map 记录：五类槽偏移表（相对已用范围起点的字偏移）。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WalkMap {
    pub direct: Vec<u32>,
    pub interior: Vec<u32>,
    pub handle: Vec<u32>,
    pub compressed: Vec<u32>,
    pub stack: Vec<u32>,
    /// 由帧内初始化字节门控的根。
    pub typed: Vec<crate::stackmap_codec::TypedRoot>,
}

/// handle 表项：slot 的当前 payload 与 generation。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandleSlot {
    pub payload: u64,
    pub generation: u64,
}

/// 落地记录：`[pc_start, pc_end)` 范围内的清理链入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LandingRecord {
    pub pc_start: u32,
    pub pc_end: u32,
    pub landing_pc: u32,
    pub cleanup: u32,
}

/// walker 的只读视图：全部表按规范排序。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WalkWorld<'a> {
    pub functions: &'a [WalkFunction],
    pub safepoints: &'a [WalkSafepoint],
    pub maps: &'a [WalkMap],
    pub handles: &'a [HandleSlot],
    pub landings: &'a [LandingRecord],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedFrame {
    pub function: u32,
    pub safepoint: u32,
    pub slots: Vec<ScannedRoot>,
    /// Offsets here are architecture register numbers, not frame-word offsets.
    pub registers: Vec<ScannedRoot>,
}

/// Scan an exact return PC and its caller-provided frame/register snapshot.
pub fn scan_frame(
    decoded: &super::stackmap_codec::DecodedSection,
    return_pc: u64,
    words: &[u64],
    registers: &[u64],
    handles: &[HandleSlot],
    compression: &mut CompressionPlane,
) -> Result<ScannedFrame, RawModelError> {
    let functions = walk_functions(decoded);
    let safepoints = walk_safepoints(decoded)?;
    let maps = walk_maps(decoded);
    let world = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles,
        landings: &[],
    };
    let function = find_function(&world, return_pc)?;
    let code = &functions[function as usize];
    let offset = u32::try_from(return_pc - code.code_rva)
        .map_err(|_| RawModelError::new("return PC offset overflow"))?;
    let safepoint = find_safepoint(&world, function, offset)?;
    let point = &safepoints[safepoint as usize];
    let map = decoded
        .maps
        .get(point.map as usize)
        .ok_or_else(|| RawModelError::new("invalid map index"))?;
    if point.kind != 4 && words.len() < code.frame_size as usize / 8 {
        return Err(RawModelError::new(
            "frame snapshot is smaller than the declared frame",
        ));
    }
    if map.registers.iter().any(|mask| {
        mask & (!decoded.architecture.valid_registers() | decoded.architecture.reserved_registers())
            != 0
    }) {
        return Err(RawModelError::new("invalid register snapshot root mask"));
    }
    let slots = scan_roots(&world, safepoint, words, compression)?;
    let register_map = super::stackmap_codec::DecodedMap {
        slots: std::array::from_fn(|class| {
            (0..decoded.architecture.register_count())
                .filter(|reg| map.registers[class] & (1_u32 << reg) != 0)
                .map(u32::from)
                .collect()
        }),
        registers: [0; 5],
        typed: vec![],
    };
    let register_maps = [walk_map(&register_map)];
    let register_points = [WalkSafepoint {
        function: 0,
        pc_offset: 0,
        kind: point.kind,
        map: 0,
    }];
    let register_world = WalkWorld {
        maps: &register_maps,
        safepoints: &register_points,
        handles,
        ..WalkWorld::default()
    };
    let registers = scan_roots(&register_world, 0, registers, compression)?;
    Ok(ScannedFrame {
        function,
        safepoint,
        slots,
        registers,
    })
}

fn walk_functions(decoded: &super::stackmap_codec::DecodedSection) -> Vec<WalkFunction> {
    decoded
        .functions
        .iter()
        .map(|function| WalkFunction {
            code_rva: function.code_rva,
            code_size: function.code_size,
            frame_size: function.frame_size,
        })
        .collect()
}

fn walk_map(map: &super::stackmap_codec::DecodedMap) -> WalkMap {
    WalkMap {
        direct: map.slots[0].clone(),
        interior: map.slots[1].clone(),
        handle: map.slots[2].clone(),
        compressed: map.slots[3].clone(),
        stack: map.slots[4].clone(),
        typed: map.typed.clone(),
    }
}

fn walk_maps(decoded: &super::stackmap_codec::DecodedSection) -> Vec<WalkMap> {
    decoded.maps.iter().map(walk_map).collect()
}

/// 在按 `code_rva` 排序的函数表中二分查找包含 `pc` 的函数。
pub fn find_function(world: &WalkWorld<'_>, pc: u64) -> Result<u32, RawModelError> {
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

/// 在按 `(function, pc_offset)` 规范序排列的安全点表中二分查找精确匹配的记录。
///
/// 全表规范序是 `WalkWorld` 的构造契约：编码路径由 `stackmap_codec::encode`
/// 排序并校验，`walk_safepoints` 构表时有 debug 断言。查找直接对全表做复合键
/// 二分——先定位函数区间，再在区间内精确匹配 `pc_offset`，不做过滤或重排。
pub fn find_safepoint(
    world: &WalkWorld<'_>,
    function: u32,
    pc_offset: u32,
) -> Result<u32, RawModelError> {
    let start = world
        .safepoints
        .partition_point(|safepoint| safepoint.function < function);
    let end = world
        .safepoints
        .partition_point(|safepoint| safepoint.function <= function);
    let range = &world.safepoints[start..end];
    let index = range.partition_point(|safepoint| safepoint.pc_offset < pc_offset);
    match range.get(index) {
        Some(safepoint) if safepoint.pc_offset == pc_offset => Ok((start + index) as u32),
        _ => Err(RawModelError::new("安全点偏移没有精确匹配的记录")),
    }
}

/// 读取帧内逐根初始化字节；偏移必须落在帧范围内。
fn byte_at(words: &[u64], offset: u32) -> Result<u8, RawModelError> {
    let index = usize::try_from(offset).map_err(|_| RawModelError::new("初始化字节偏移越界"))?;
    let word = words
        .get(index / 8)
        .ok_or_else(|| RawModelError::new("初始化字节超出帧范围"))?;
    Ok((word >> (8 * (index % 8))) as u8)
}

/// 扫描一个安全点的五类根：按槽偏移排序返回。
///
/// `words` 为已用栈范围的字数组（小端）；handle 与压缩引用的字分别经表做 checked
/// 解析，过期代际或越界一律返回错误。压缩字经 `CompressionPlane` 的唯一解码路径解析并
/// 计入 decode 统计；空字不解码也不计数。
pub fn scan_roots(
    world: &WalkWorld<'_>,
    safepoint: u32,
    words: &[u64],
    compression: &mut CompressionPlane,
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
        if !seen.insert(*offset) {
            return Err(RawModelError::new("同一槽在两类位图中重复出现"));
        }
    }
    // 逐根初始化条目：只有帧内初始化字节非零时该字才参与扫描——并集条目在别的变体里可能是
    // 标量或另一种根种类，不得当作指针。同一字在编码层允许携带多种 kind（由各自的初始化
    // 字节在运行期择一），因此查重只对实际激活的条目进行。
    let mut gated: [Vec<u32>; 5] = std::array::from_fn(|_| Vec::new());
    for entry in &map.typed {
        if byte_at(words, entry.init_byte)? == 0 {
            continue;
        }
        if !seen.insert(entry.word) {
            return Err(RawModelError::new("同一槽在两类位图中重复出现"));
        }
        let class = usize::try_from(entry.root_kind)
            .ok()
            .filter(|class| *class < gated.len())
            .ok_or_else(|| RawModelError::new("逐根初始化条目根种类越界"))?;
        gated[class].push(entry.word);
    }
    let mut roots = Vec::new();
    for offset in map.direct.iter().chain(&gated[0]) {
        let word = word_at(words, *offset)?;
        if word == 0 {
            continue;
        }
        roots.push(ScannedRoot::Direct(*offset));
    }
    for offset in map.interior.iter().chain(&gated[1]) {
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
    for offset in map.handle.iter().chain(&gated[2]) {
        let word = word_at(words, *offset)?;
        // 全零字是空引用：不查表、不计数。槽 0 不能表示活动 handle。
        if word == 0 {
            continue;
        }
        let slot = usize::try_from(word).map_err(|_| RawModelError::new("handle 槽编号越界"))?;
        let handle = world
            .handles
            .get(slot)
            .ok_or_else(|| RawModelError::new("handle 槽越界"))?;
        if handle.generation == 0 {
            return Err(RawModelError::new("过期 handle 代际不得解析"));
        }
        roots.push(ScannedRoot::Handle {
            offset: *offset,
            payload: handle.payload,
        });
    }
    for offset in map.compressed.iter().chain(&gated[3]) {
        let word = word_at(words, *offset)?;
        if let Some(target) = compression.decode(word)? {
            roots.push(ScannedRoot::Compressed {
                offset: *offset,
                target,
            });
        }
    }
    for offset in map.stack.iter().chain(&gated[4]) {
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
pub fn select_landing<'a>(
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

/// 校验一个函数的落地链：每条记录以自己的 `pc_start` 选中的最内层记录必须就是它自己的落点。
///
/// 落地选择只读落地链本身，不依赖已编码 section；逐函数校验因此不必重解码整张栈图。
pub fn check_landings(landings: &[LandingRecord]) -> Result<u32, RawModelError> {
    let world = WalkWorld {
        landings,
        ..WalkWorld::default()
    };
    for landing in landings {
        let selected = select_landing(&world, landing.pc_start)?;
        if selected.landing_pc != landing.landing_pc || selected.cleanup != landing.cleanup {
            return Err(RawModelError::new("落地链选择与记录不一致"));
        }
    }
    u32::try_from(landings.len()).map_err(|_| RawModelError::new("落地记录数超出 u32"))
}

/// 由扫描到的栈根组装复制输入：槽偏移按字节换算后必须严格升序且不重叠。
pub fn copy_input(roots: &[ScannedRoot], used: usize) -> Result<StackImage, RawModelError> {
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

/// runtime 消费机器栈图与落地链后的计数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsumeReport {
    pub functions: u32,
    pub safepoints: u32,
    pub maps: u32,
    pub landings: u32,
    pub scanned_roots: u32,
    pub copied_slots: u32,
}

/// 用 walker 消费已编码的栈图与落地记录。
///
/// 每个函数与安全点都必须精确命中；落地链按 `pc_start` 选择；根扫描与栈复制
/// 走同一套五类槽表。找不到记录或范围重叠是契约错误，不能猜相邻 map。
pub fn consume_metadata(
    section: &[u8],
    landings: &[LandingRecord],
    compression: &super::compression_schema::CompressionRuntimeContract,
) -> Result<ConsumeReport, RawModelError> {
    let decoded = super::stackmap_codec::decode_tables(section)?;
    let functions = walk_functions(&decoded);
    let safepoints = walk_safepoints(&decoded)?;
    let maps = walk_maps(&decoded);
    let world = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles: &[],
        landings,
    };
    let mut scanned = 0_u32;
    let mut copied = 0_u32;
    let mut plane = super::cage::CompressionPlane::new(compression);
    for (index, function) in functions.iter().enumerate() {
        if function.code_size == 0 {
            continue;
        }
        let found = find_function(&world, function.code_rva)?;
        if found != index as u32 {
            return Err(RawModelError::new("函数表二分查找没有命中自身"));
        }
    }
    for (index, point) in safepoints.iter().enumerate() {
        let function = &functions[point.function as usize];
        let pc = function
            .code_rva
            .checked_add(u64::from(point.pc_offset))
            .ok_or_else(|| RawModelError::new("安全点绝对地址溢出"))?;
        if find_function(&world, pc)? != point.function {
            return Err(RawModelError::new("安全点地址没有落在所属函数"));
        }
        if find_safepoint(&world, point.function, point.pc_offset)? != index as u32 {
            return Err(RawModelError::new("安全点二分查找没有命中自身"));
        }
        // 合成帧必须覆盖整帧：逐根初始化字节位于槽位图范围之外，按 map 的 slot 数开表会
        // 把它们判成越界。
        let words =
            vec![0_u64; usize::try_from(function.frame_size / 8).expect("帧字数适配 usize")];
        let roots = scan_roots(&world, index as u32, &words, &mut plane)?;
        scanned = scanned.saturating_add(roots.len() as u32);
        if decoded.safepoints[index].flags & 1 != 0 {
            let image = copy_input(&roots, words.len() * 8)?;
            copied = copied.saturating_add(image.stack_roots.len() as u32);
        }
        if matches!(point.kind, 0 | 2 | 3)
            && decoded.maps[point.map as usize]
                .registers
                .iter()
                .any(|mask| *mask != 0)
        {
            return Err(RawModelError::new(
                "调用、挂起或 bridge 点不得保留用户寄存器根",
            ));
        }
    }
    check_landings(landings)?;
    Ok(ConsumeReport {
        functions: functions.len() as u32,
        safepoints: safepoints.len() as u32,
        maps: maps.len() as u32,
        landings: landings.len() as u32,
        scanned_roots: scanned,
        copied_slots: copied,
    })
}

fn walk_safepoints(
    decoded: &super::stackmap_codec::DecodedSection,
) -> Result<Vec<WalkSafepoint>, RawModelError> {
    let mut safepoints = Vec::with_capacity(decoded.safepoints.len());
    for (index, function) in decoded.functions.iter().enumerate() {
        let start = function.safepoint_start as usize;
        let end = start + function.safepoint_count as usize;
        if end > decoded.safepoints.len() {
            return Err(RawModelError::new("函数安全点范围越界"));
        }
        for point in &decoded.safepoints[start..end] {
            safepoints.push(WalkSafepoint {
                function: index as u32,
                pc_offset: point.pc_offset,
                kind: point.kind,
                map: point.map_index,
            });
        }
    }
    if safepoints.len() != decoded.safepoints.len() {
        return Err(RawModelError::new("安全点没有全部落入函数范围"));
    }
    // `find_safepoint` 依赖全表按 `(function, pc_offset)` 规范序；编码器已排序并
    // 校验同一函数内严格递增，此处复核整表不变量。
    debug_assert!(
        safepoints.is_sorted_by_key(|safepoint| (safepoint.function, safepoint.pc_offset))
    );
    Ok(safepoints)
}

fn zero_words(map: &WalkMap) -> Vec<u64> {
    let max = map
        .direct
        .iter()
        .chain(&map.interior)
        .chain(&map.handle)
        .chain(&map.compressed)
        .chain(&map.stack)
        .copied()
        .max()
        .map(|slot| slot.saturating_add(1))
        .unwrap_or(0);
    vec![0; max as usize]
}

/// 校验 bridge 记录的帧范围：`frame_offset + frame_size` 必须落在已用栈范围内。
pub fn verify_bridge_frame(
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
