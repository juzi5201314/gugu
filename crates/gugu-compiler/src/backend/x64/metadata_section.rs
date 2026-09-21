//! 把各函数元数据装配成栈图节、统一展开表和源码记录表。
//!
//! 逻辑代码节按 mangled 符号升序、16 字节对齐拼接。`code_rva` 是该节内偏移；
//! 镜像写出只能整体加 load bias，不能改相对顺序。map 去重按完整记录字节的字典序。

use crate::runtime::frame_walk;
use crate::runtime::stackmap_codec::{self, CodeLayout, SafepointLayout};
use crate::target::TargetName;

use super::codegen::FragmentPayload;
use super::metadata::{
    self, FunctionMetadata, METADATA_SCHEMA, SOURCE_MAGIC, SOURCE_SECTION, SOURCE_VERSION,
    UNWIND_MAGIC, UNWIND_SECTION, UNWIND_VERSION,
};

/// 一次编译的机器元数据节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageMetadata {
    /// schema 版本。
    pub(crate) schema: u32,
    /// 目标栈图节名。
    pub(crate) section_name: String,
    /// 栈图节字节。
    pub(crate) stackmap: Vec<u8>,
    /// 统一展开表字节。
    pub(crate) unwind: Vec<u8>,
    /// 源码记录表字节，写入 `.gugu.meta` 时不得被 strip 删除。
    pub(crate) source: Vec<u8>,
    /// 进入栈图表的函数数。
    pub(crate) functions: u32,
    /// 安全点数。
    pub(crate) safepoints: u32,
    /// 去重后的 map 数。
    pub(crate) maps: u32,
    /// landing 数。
    pub(crate) landings: u32,
    /// 源码记录数。
    pub(crate) source_records: u32,
    /// 三节字节的域指纹。
    pub(crate) fingerprint: [u8; 32],
}

/// 装配世界级元数据节，并让 runtime walker 消费一次。
pub(crate) fn assemble(
    fragments: &[FragmentPayload],
    target: TargetName,
) -> Result<ImageMetadata, metadata::MetadataError> {
    for fragment in fragments {
        metadata::verify(&fragment.metadata)?;
        if fragment.metadata.code_size != u32::try_from(fragment.bytes.len()).unwrap_or(u32::MAX) {
            return Err(metadata::MetadataError::new("元数据代码长度与片段不一致"));
        }
    }
    let order = included_order(fragments)?;
    let layouts = assign_rvas(fragments, &order)?;
    let (functions, points) = layouts_of(fragments, &order, &layouts)?;
    let stackmap = stackmap_codec::encode(&functions, &points)
        .map_err(|error| metadata::MetadataError::new(error.message()))?;
    let unwind = encode_unwind(fragments, &order, &layouts);
    let source = encode_sources(fragments, &order, &layouts)?;
    let audit = frame_walk::audit(&stackmap, &unwind, &source)
        .map_err(|error| metadata::MetadataError::new(error.message()))?;
    confirm_hits(&stackmap, &unwind, &functions, &points)?;
    if audit.functions != functions.len() as u32 || audit.safepoints != points.len() as u32 {
        return Err(metadata::MetadataError::new("walker 计数与栈图节不一致"));
    }
    if !metadata::strip_preserves(target, &[".text", ".debug_info", ".comment"]) {
        return Err(metadata::MetadataError::new(
            "strip 保留集漏掉了运行时元数据",
        ));
    }
    if metadata::strip_preserves(target, &[metadata::stackmap_section(target)]) {
        return Err(metadata::MetadataError::new("删除栈图节仍被当成合法 strip"));
    }
    let fingerprint = fingerprint(&stackmap, &unwind, &source);
    Ok(ImageMetadata {
        schema: METADATA_SCHEMA,
        section_name: metadata::stackmap_section(target).to_owned(),
        stackmap,
        unwind,
        source,
        functions: audit.functions,
        safepoints: audit.safepoints,
        maps: audit.maps,
        landings: audit.landings,
        source_records: audit.source_records,
        fingerprint,
    })
}

fn confirm_hits(
    stackmap: &[u8],
    unwind: &[u8],
    functions: &[CodeLayout],
    points: &[(u32, SafepointLayout)],
) -> Result<(), metadata::MetadataError> {
    for (function, layout) in points {
        let code = functions
            .get(*function as usize)
            .ok_or_else(|| metadata::MetadataError::new("安全点函数序号越界"))?;
        let pc = code.code_rva + u64::from(layout.pc_offset);
        let hit = frame_walk::safepoint_at(stackmap, pc)
            .map_err(|error| metadata::MetadataError::new(error.message()))?;
        if hit.kind != layout.kind || hit.frame_size != code.frame_size {
            return Err(metadata::MetadataError::new(
                "walker 读回的安全点与布局不一致",
            ));
        }
    }
    confirm_landings(unwind)
}

fn confirm_landings(unwind: &[u8]) -> Result<(), metadata::MetadataError> {
    if unwind.len() < 32 {
        return Err(metadata::MetadataError::new("展开表过短"));
    }
    let functions = u32::from_le_bytes(unwind[12..16].try_into().expect("计数"));
    for index in 0..functions {
        let base = 32 + index as usize * 32;
        let rva = u64::from_le_bytes(unwind[base..base + 8].try_into().expect("字段"));
        let start = u32::from_le_bytes(unwind[base + 20..base + 24].try_into().expect("字段"));
        let count = u16::from_le_bytes(unwind[base + 24..base + 26].try_into().expect("字段"));
        for slot in 0..count as u32 {
            let at = 32 + functions as usize * 32 + (start + slot) as usize * 16;
            let pc_start = u32::from_le_bytes(unwind[at..at + 4].try_into().expect("字段"));
            let chain = u32::from_le_bytes(unwind[at + 12..at + 16].try_into().expect("字段"));
            let landing_pc = u32::from_le_bytes(unwind[at + 8..at + 12].try_into().expect("字段"));
            let found = frame_walk::landing_at(unwind, rva + u64::from(pc_start))
                .map_err(|error| metadata::MetadataError::new(error.message()))?
                .ok_or_else(|| metadata::MetadataError::new("landing 范围没有被 walker 命中"))?;
            if found.cleanup_chain != chain || found.landing_pc != landing_pc {
                return Err(metadata::MetadataError::new(
                    "walker 读回的 landing 与记录不一致",
                ));
            }
        }
    }
    Ok(())
}

fn included_order(fragments: &[FragmentPayload]) -> Result<Vec<usize>, metadata::MetadataError> {
    let mut order: Vec<usize> = fragments
        .iter()
        .enumerate()
        .filter(|(_, fragment)| fragment.metadata.included)
        .map(|(index, _)| index)
        .collect();
    order.sort_by(|&left, &right| fragments[left].symbol.cmp(&fragments[right].symbol));
    for pair in order.windows(2) {
        if fragments[pair[0]].symbol == fragments[pair[1]].symbol {
            return Err(metadata::MetadataError::new("栈图函数符号重复"));
        }
    }
    Ok(order)
}

fn assign_rvas(
    fragments: &[FragmentPayload],
    order: &[usize],
) -> Result<Vec<u64>, metadata::MetadataError> {
    let mut rvas = vec![0; fragments.len()];
    let mut cursor = 0_u64;
    for index in order {
        rvas[*index] = cursor;
        let size = u64::from(fragments[*index].metadata.code_size);
        let end = cursor
            .checked_add(size)
            .ok_or_else(|| metadata::MetadataError::new("逻辑代码节溢出"))?;
        let aligned = (end + 15) & !15;
        cursor = if aligned <= cursor {
            cursor + 16
        } else {
            aligned
        };
    }
    Ok(rvas)
}

fn layouts_of(
    fragments: &[FragmentPayload],
    order: &[usize],
    rvas: &[u64],
) -> Result<(Vec<CodeLayout>, Vec<(u32, SafepointLayout)>), metadata::MetadataError> {
    let mut functions = Vec::new();
    let mut points = Vec::new();
    for (ordinal, index) in order.iter().copied().enumerate() {
        let metadata = &fragments[index].metadata;
        functions.push(CodeLayout {
            code_rva: rvas[index],
            code_size: metadata.code_size,
            frame_size: metadata.frame_size,
            unwind_index: u32::try_from(ordinal).expect("函数序号适配 u32"),
            runtime_bridge: metadata.runtime_bridge,
            panic_landing: metadata.panic_landing,
            has_stack_interior: metadata.has_stack_interior,
        });
        for point in &metadata.safepoints {
            points.push((
                u32::try_from(ordinal).expect("函数序号适配 u32"),
                safepoint_layout(point, ordinal)?,
            ));
        }
    }
    Ok((functions, points))
}

fn safepoint_layout(
    point: &super::metadata::SafepointPayload,
    function: usize,
) -> Result<SafepointLayout, metadata::MetadataError> {
    if point.slots.len() != 5 {
        return Err(metadata::MetadataError::new("根位图不是五类"));
    }
    Ok(SafepointLayout {
        pc_offset: point.pc_offset,
        kind: point.kind,
        dirty: point.dirty,
        copy_allowed: point.copy_allowed,
        scan_allowed: point.scan_allowed,
        slots: [
            point.slots[0].clone(),
            point.slots[1].clone(),
            point.slots[2].clone(),
            point.slots[3].clone(),
            point.slots[4].clone(),
        ],
        registers: point.registers,
        function: u32::try_from(function).expect("函数序号适配 u32"),
        slot_count: point.slot_count,
    })
}

fn encode_unwind(fragments: &[FragmentPayload], order: &[usize], rvas: &[u64]) -> Vec<u8> {
    let mut landings = Vec::new();
    let mut landing_ranges = Vec::new();
    for index in order {
        let start = landings.len() as u32;
        for landing in &fragments[*index].metadata.landings {
            landings.push((
                landing.pc_start,
                landing.pc_end,
                landing.landing_pc,
                landing.cleanup_chain,
            ));
        }
        landing_ranges.push((start, landings.len() as u32 - start));
    }
    let functions_offset = 32_u32;
    let landings_offset = functions_offset + order.len() as u32 * 32;
    let section_len = landings_offset + landings.len() as u32 * 16;
    let mut output = Vec::with_capacity(section_len as usize);
    output.extend_from_slice(UNWIND_MAGIC);
    output.extend_from_slice(&UNWIND_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&(order.len() as u32).to_le_bytes());
    output.extend_from_slice(&(landings.len() as u32).to_le_bytes());
    output.extend_from_slice(&functions_offset.to_le_bytes());
    output.extend_from_slice(&landings_offset.to_le_bytes());
    output.extend_from_slice(&section_len.to_le_bytes());
    for (ordinal, index) in order.iter().copied().enumerate() {
        write_unwind_function(
            &mut output,
            &fragments[index].metadata,
            rvas[index],
            landing_ranges[ordinal],
        );
    }
    for (start, end, landing, chain) in landings {
        output.extend_from_slice(&start.to_le_bytes());
        output.extend_from_slice(&end.to_le_bytes());
        output.extend_from_slice(&landing.to_le_bytes());
        output.extend_from_slice(&chain.to_le_bytes());
    }
    debug_assert_eq!(output.len() as u32, section_len);
    let _ = UNWIND_SECTION;
    output
}

fn write_unwind_function(
    output: &mut Vec<u8>,
    metadata: &FunctionMetadata,
    rva: u64,
    landings: (u32, u32),
) {
    let mut flags = 0_u16;
    if metadata.panic_landing {
        flags |= 1;
    }
    if metadata.runtime_bridge {
        flags |= 1 << 1;
    }
    output.extend_from_slice(&rva.to_le_bytes());
    output.extend_from_slice(&metadata.code_size.to_le_bytes());
    output.extend_from_slice(&metadata.frame_size.to_le_bytes());
    output.extend_from_slice(&metadata.saved_gpr_mask.to_le_bytes());
    output.extend_from_slice(&flags.to_le_bytes());
    output.extend_from_slice(&landings.0.to_le_bytes());
    output.extend_from_slice(&(landings.1 as u16).to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
}

fn encode_sources(
    fragments: &[FragmentPayload],
    order: &[usize],
    _rvas: &[u64],
) -> Result<Vec<u8>, metadata::MetadataError> {
    let mut paths = Vec::new();
    for index in order {
        for record in &fragments[*index].metadata.sources {
            paths.push(record.path.clone());
        }
    }
    paths.sort();
    paths.dedup();
    let mut pool = Vec::new();
    let mut placed = Vec::new();
    for path in &paths {
        placed.push((pool.len() as u32, path.len() as u32));
        pool.extend_from_slice(path.as_bytes());
    }
    let mut records = Vec::new();
    for (ordinal, index) in order.iter().copied().enumerate() {
        for record in &fragments[index].metadata.sources {
            let (offset, len) = placed
                .iter()
                .zip(&paths)
                .find(|(_, path)| path.as_str() == record.path)
                .map(|(place, _)| *place)
                .ok_or_else(|| metadata::MetadataError::new("源码路径没有进入字符串池"))?;
            records.push(SourceWord {
                function: ordinal as u32,
                pc_start: record.pc_start,
                pc_end: record.pc_end,
                path_offset: offset,
                path_len: len,
                line: record.line,
                column: record.column,
                flags: record.flags,
            });
        }
    }
    records.sort_by_key(|record| {
        (
            record.function,
            record.pc_start,
            record.pc_end,
            record.path_offset,
        )
    });
    let strings_offset = 32 + records.len() as u32 * 32;
    let section_len = strings_offset + pool.len() as u32;
    let mut output = Vec::with_capacity(section_len as usize);
    output.extend_from_slice(SOURCE_MAGIC);
    output.extend_from_slice(&SOURCE_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&(records.len() as u32).to_le_bytes());
    output.extend_from_slice(&strings_offset.to_le_bytes());
    output.extend_from_slice(&(pool.len() as u32).to_le_bytes());
    output.extend_from_slice(&section_len.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    for record in &records {
        write_source(&mut output, record);
    }
    output.extend_from_slice(&pool);
    debug_assert_eq!(output.len() as u32, section_len);
    let _ = SOURCE_SECTION;
    Ok(output)
}

struct SourceWord {
    function: u32,
    pc_start: u32,
    pc_end: u32,
    path_offset: u32,
    path_len: u32,
    line: u32,
    column: u32,
    flags: u32,
}

fn write_source(output: &mut Vec<u8>, record: &SourceWord) {
    output.extend_from_slice(&record.function.to_le_bytes());
    output.extend_from_slice(&record.pc_start.to_le_bytes());
    output.extend_from_slice(&record.pc_end.to_le_bytes());
    output.extend_from_slice(&record.path_offset.to_le_bytes());
    output.extend_from_slice(&record.path_len.to_le_bytes());
    output.extend_from_slice(&record.line.to_le_bytes());
    output.extend_from_slice(&record.column.to_le_bytes());
    output.extend_from_slice(&record.flags.to_le_bytes());
}

fn fingerprint(stackmap: &[u8], unwind: &[u8], source: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("gugu-x64-metadata-v1");
    for bytes in [stackmap, unwind, source] {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    *hasher.finalize().as_bytes()
}
