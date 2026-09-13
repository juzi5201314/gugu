//! GC metadata 镜像 section：`.gugu.types`/`.gugutyp` 与 `.gugu.meta`/`.ggmeta`
//! 的字节布局、编码器与结构校验器。
//!
//! 所有 offset 都是 section 内的绝对字节位置；pool 一律 8 字节对齐，并按
//! records → trace → value → name（metadata 侧再加 root → vtable index →
//! vtable data → source/alloc → string）顺序紧密排布。编码结果必须能通过
//! `verify_sections`；编码器与校验器共享同一组记录尺寸常量，任何一侧漂移都会
//! 在写出镜像前失败。

use super::gc_metadata_schema::{
    GcMetadataWorldV1, GcRootKindV1, GcRootLocationV1, GcSourceEntryV1, boot_verify,
    trace_program_len, value_program_len,
};
use super::model::RawModelError;

/// 编码真实 type/meta section；所有 offset 按 section 内绝对字节位置记录。
pub(crate) fn encode_sections(
    world: &GcMetadataWorldV1,
) -> Result<(Vec<u8>, Vec<u8>), RawModelError> {
    boot_verify(world)?;
    let type_section = encode_type_section(world)?;
    let metadata_section = encode_metadata_section(world)?;
    verify_sections(&type_section, &metadata_section)?;
    Ok((type_section, metadata_section))
}

const TYPE_HEADER_BYTES: usize = 88;
const TYPE_RECORD_BYTES: usize = 80;
const META_HEADER_BYTES: usize = 88;
const ROOT_RECORD_BYTES: usize = 32;
const SOURCE_RECORD_BYTES: usize = 48;
const VTABLE_BYTES: usize = 64;

fn encode_type_section(world: &GcMetadataWorldV1) -> Result<Vec<u8>, RawModelError> {
    // name pool 按字节序去重；`BTreeSet` 同时给出确定性顺序与唯一性。
    let mut names = std::collections::BTreeSet::<Vec<u8>>::new();
    for entry in &world.types {
        names.insert(entry.name.as_bytes().to_vec());
    }
    let mut name_pool = Vec::new();
    let mut name_offsets = std::collections::BTreeMap::new();
    for bytes in &names {
        let offset =
            u32::try_from(name_pool.len()).map_err(|_| RawModelError::new("name pool 溢出"))?;
        name_offsets.insert(bytes.clone(), offset);
        name_pool.extend_from_slice(bytes);
    }
    let records_offset = TYPE_HEADER_BYTES as u64;
    let records_bytes = u64::try_from(world.types.len())
        .map_err(|_| RawModelError::new("type record 数量溢出"))?
        .checked_mul(TYPE_RECORD_BYTES as u64)
        .ok_or_else(|| RawModelError::new("type record 长度溢出"))?;
    let trace_offset = align_up_u64(
        records_offset
            .checked_add(records_bytes)
            .ok_or_else(|| RawModelError::new("type trace offset 溢出"))?,
        8,
    )?;
    let trace_len = u64::try_from(world.trace_program.len())
        .map_err(|_| RawModelError::new("type trace 长度溢出"))?;
    let value_offset = align_up_u64(
        trace_offset
            .checked_add(trace_len)
            .ok_or_else(|| RawModelError::new("type value offset 溢出"))?,
        8,
    )?;
    let value_len = u64::try_from(world.value_program.len())
        .map_err(|_| RawModelError::new("type value 长度溢出"))?;
    let name_offset = align_up_u64(
        value_offset
            .checked_add(value_len)
            .ok_or_else(|| RawModelError::new("type name offset 溢出"))?,
        8,
    )?;
    let section_len = name_offset
        .checked_add(
            u64::try_from(name_pool.len()).map_err(|_| RawModelError::new("name pool 长度溢出"))?,
        )
        .ok_or_else(|| RawModelError::new("type section 长度溢出"))?;
    let section_len_usize = usize::try_from(section_len)
        .map_err(|_| RawModelError::new("type section 超过宿主地址空间"))?;
    let mut out = vec![0u8; TYPE_HEADER_BYTES];
    out[0..8].copy_from_slice(b"GUGUTY01");
    out[8..10].copy_from_slice(&1u16.to_le_bytes());
    out[10] = 8;
    out[11] = 1;
    put_u32(&mut out, 12, world.types.len() as u32)?;
    put_u32(&mut out, 16, TYPE_RECORD_BYTES as u32)?;
    put_u32(&mut out, 20, 0)?;
    put_u64(&mut out, 24, records_offset);
    put_u64(&mut out, 32, trace_offset);
    put_u64(&mut out, 40, world.trace_program.len() as u64);
    put_u64(&mut out, 48, value_offset);
    put_u64(&mut out, 56, world.value_program.len() as u64);
    put_u64(&mut out, 64, name_offset);
    put_u64(&mut out, 72, name_pool.len() as u64);
    put_u64(&mut out, 80, section_len);
    pad_to(&mut out, records_offset as usize);
    for entry in &world.types {
        let mut record = vec![0u8; TYPE_RECORD_BYTES];
        let (size, align) = entry.layout.unwrap_or((0, 1));
        let align = u32::try_from(align).map_err(|_| RawModelError::new("type align 超过 u32"))?;
        record[0..8].copy_from_slice(&size.to_le_bytes());
        record[8..12].copy_from_slice(&align.to_le_bytes());
        record[12..16].copy_from_slice(&u32::from(entry.flags).to_le_bytes());
        let name = entry.name.as_bytes();
        let offset = *name_offsets.get(name).expect("name 已登记");
        let name_len =
            u32::try_from(name.len()).map_err(|_| RawModelError::new("type name 长度超过 u32"))?;
        record[16..20].copy_from_slice(&offset.to_le_bytes());
        record[20..24].copy_from_slice(&name_len.to_le_bytes());
        record[24..28].copy_from_slice(&entry.trace_offset.to_le_bytes());
        record[28..32].copy_from_slice(&entry.trace_len.to_le_bytes());
        record[32..36].copy_from_slice(&entry.value_offset.to_le_bytes());
        record[36..40].copy_from_slice(&entry.value_len.to_le_bytes());
        out.extend_from_slice(&record);
    }
    pad_to(&mut out, trace_offset as usize);
    out.extend_from_slice(&world.trace_program);
    pad_to(&mut out, value_offset as usize);
    out.extend_from_slice(&world.value_program);
    pad_to(&mut out, name_offset as usize);
    out.extend_from_slice(&name_pool);
    if out.len() != section_len_usize {
        return Err(RawModelError::new("type section 长度计算不一致"));
    }
    Ok(out)
}

fn encode_metadata_section(world: &GcMetadataWorldV1) -> Result<Vec<u8>, RawModelError> {
    let mut strings = std::collections::BTreeMap::<String, u32>::new();
    for source in &world.sources {
        strings.entry(source.source_path.clone()).or_insert(0);
    }
    for site in &world.alloc_sites {
        strings
            .entry(site.location.source_path.clone())
            .or_insert(0);
    }
    let mut string_pool = Vec::new();
    for (path, offset) in &mut strings {
        if path.len() > u32::MAX as usize {
            return Err(RawModelError::new("source path 长度超过 u32"));
        }
        *offset =
            u32::try_from(string_pool.len()).map_err(|_| RawModelError::new("source pool 溢出"))?;
        string_pool.extend_from_slice(path.as_bytes());
    }
    let root_offset = align_up_u64(META_HEADER_BYTES as u64, 8)?;
    let root_bytes = u64::try_from(world.roots.len())
        .map_err(|_| RawModelError::new("root 数量溢出"))?
        .checked_mul(ROOT_RECORD_BYTES as u64)
        .ok_or_else(|| RawModelError::new("root records 长度溢出"))?;
    let vtable_index_count = u64::try_from(world.vtables.len())
        .map_err(|_| RawModelError::new("vtable 数量溢出"))?
        .checked_add(1)
        .ok_or_else(|| RawModelError::new("vtable index 数量溢出"))?;
    let vtable_index_bytes = vtable_index_count
        .checked_mul(8)
        .ok_or_else(|| RawModelError::new("vtable index 长度溢出"))?;
    let vtable_index_offset = align_up_u64(
        root_offset
            .checked_add(root_bytes)
            .ok_or_else(|| RawModelError::new("vtable index offset 溢出"))?,
        8,
    )?;
    let vtable_bytes = u64::try_from(world.vtables.len())
        .map_err(|_| RawModelError::new("vtable 数量溢出"))?
        .checked_mul(VTABLE_BYTES as u64)
        .ok_or_else(|| RawModelError::new("vtable data 长度溢出"))?;
    let vtable_data_offset = align_up_u64(
        vtable_index_offset
            .checked_add(vtable_index_bytes)
            .ok_or_else(|| RawModelError::new("vtable data offset 溢出"))?,
        8,
    )?;
    let source_bytes = u64::try_from(world.sources.len())
        .map_err(|_| RawModelError::new("source 数量溢出"))?
        .checked_mul(SOURCE_RECORD_BYTES as u64)
        .ok_or_else(|| RawModelError::new("source records 长度溢出"))?;
    let source_offset = align_up_u64(
        vtable_data_offset
            .checked_add(vtable_bytes)
            .ok_or_else(|| RawModelError::new("source offset 溢出"))?,
        8,
    )?;
    let alloc_bytes = u64::try_from(world.alloc_sites.len())
        .map_err(|_| RawModelError::new("alloc site 数量溢出"))?
        .checked_mul(SOURCE_RECORD_BYTES as u64)
        .ok_or_else(|| RawModelError::new("alloc records 长度溢出"))?;
    let alloc_offset = align_up_u64(
        source_offset
            .checked_add(source_bytes)
            .ok_or_else(|| RawModelError::new("alloc offset 溢出"))?,
        8,
    )?;
    let string_offset = align_up_u64(
        alloc_offset
            .checked_add(alloc_bytes)
            .ok_or_else(|| RawModelError::new("string offset 溢出"))?,
        8,
    )?;
    let section_len = string_offset
        .checked_add(
            u64::try_from(string_pool.len())
                .map_err(|_| RawModelError::new("source pool 长度溢出"))?,
        )
        .ok_or_else(|| RawModelError::new("metadata section 长度溢出"))?;
    let section_len_usize = usize::try_from(section_len)
        .map_err(|_| RawModelError::new("metadata section 超过宿主地址空间"))?;
    let mut out = vec![0u8; META_HEADER_BYTES];
    out[0..8].copy_from_slice(b"GUGUMT01");
    out[8..10].copy_from_slice(&1u16.to_le_bytes());
    out[10] = 8;
    out[11] = 1;
    put_u32(&mut out, 12, world.roots.len() as u32)?;
    put_u32(&mut out, 16, world.vtables.len() as u32)?;
    put_u32(&mut out, 20, world.sources.len() as u32)?;
    put_u32(&mut out, 24, 0)?;
    put_u32(&mut out, 28, world.alloc_sites.len() as u32)?;
    put_u64(&mut out, 32, root_offset);
    put_u64(&mut out, 40, vtable_index_offset);
    put_u64(&mut out, 48, vtable_data_offset);
    put_u64(&mut out, 56, source_offset);
    put_u64(&mut out, 64, string_offset);
    put_u64(&mut out, 72, string_pool.len() as u64);
    put_u64(&mut out, 80, section_len);
    pad_to(&mut out, root_offset as usize);
    for root in &world.roots {
        let mut record = vec![0u8; ROOT_RECORD_BYTES];
        let location = match &root.location {
            GcRootLocationV1::Aggregate { offset_bytes } => *offset_bytes,
            GcRootLocationV1::Vtable { vtable_index } => u64::from(*vtable_index),
        };
        record[0..8].copy_from_slice(&location.to_le_bytes());
        record[8..12].copy_from_slice(&root.type_range.0.to_le_bytes());
        record[12..14].copy_from_slice(&root_kind(root.kind.clone()).to_le_bytes());
        record[16..24]
            .copy_from_slice(&u64::from(root.word_range.1 - root.word_range.0).to_le_bytes());
        record[24..32].copy_from_slice(&8u64.to_le_bytes());
        out.extend_from_slice(&record);
    }
    pad_to(&mut out, vtable_index_offset as usize);
    for index in 0..=world.vtables.len() {
        let value = if index == world.vtables.len() {
            world.vtables.len() as u64 * VTABLE_BYTES as u64
        } else {
            index as u64 * VTABLE_BYTES as u64
        };
        out.extend_from_slice(&value.to_le_bytes());
    }
    pad_to(&mut out, vtable_data_offset as usize);
    for vtable in &world.vtables {
        out.extend_from_slice(&vtable.interface);
        out.extend_from_slice(&vtable.concrete_type);
    }
    pad_to(&mut out, source_offset as usize);
    for source in &world.sources {
        encode_source_record(&mut out, source, &strings)?;
    }
    pad_to(&mut out, alloc_offset as usize);
    for site in &world.alloc_sites {
        encode_source_record(&mut out, &site.location, &strings)?;
    }
    pad_to(&mut out, string_offset as usize);
    out.extend_from_slice(&string_pool);
    if out.len() != section_len_usize {
        return Err(RawModelError::new("metadata section 长度计算不一致"));
    }
    Ok(out)
}

fn encode_source_record(
    out: &mut Vec<u8>,
    source: &GcSourceEntryV1,
    strings: &std::collections::BTreeMap<String, u32>,
) -> Result<(), RawModelError> {
    let mut record = vec![0u8; SOURCE_RECORD_BYTES];
    record[0..32].copy_from_slice(&source.type_key);
    let offset = *strings
        .get(&source.source_path)
        .ok_or_else(|| RawModelError::new("source path 未登记"))?;
    let path_len = u32::try_from(source.source_path.len())
        .map_err(|_| RawModelError::new("source path 长度超过 u32"))?;
    record[32..36].copy_from_slice(&offset.to_le_bytes());
    record[36..40].copy_from_slice(&path_len.to_le_bytes());
    record[40..48].copy_from_slice(&source.byte_offset.to_le_bytes());
    out.extend_from_slice(&record);
    Ok(())
}

fn root_kind(kind: GcRootKindV1) -> u16 {
    match kind {
        GcRootKindV1::Static => 0,
        GcRootKindV1::LocalStatic => 1,
        GcRootKindV1::ForeignBridge => 2,
        GcRootKindV1::CoroutineFrame => 3,
        GcRootKindV1::HandleSlot => 4,
    }
}

pub(crate) fn verify_sections(
    type_section: &[u8],
    metadata_section: &[u8],
) -> Result<(), RawModelError> {
    if type_section.len() < TYPE_HEADER_BYTES || &type_section[0..8] != b"GUGUTY01" {
        return Err(RawModelError::new("type section header 非法"));
    }
    if u16::from_le_bytes(type_section[8..10].try_into().expect("type version")) != 1
        || type_section[10] != 8
        || type_section[11] != 1
    {
        return Err(RawModelError::new("type section 版本或 ABI 非法"));
    }
    let type_count = read_u32(type_section, 12)? as usize;
    if read_u32(type_section, 16)? as usize != TYPE_RECORD_BYTES || read_u32(type_section, 20)? != 0
    {
        return Err(RawModelError::new("type section record header 非法"));
    }
    let records_offset = usize::try_from(read_u64(type_section, 24)?)
        .map_err(|_| RawModelError::new("type records offset 溢出"))?;
    let trace_offset = usize::try_from(read_u64(type_section, 32)?)
        .map_err(|_| RawModelError::new("type trace offset 溢出"))?;
    let trace_len = usize::try_from(read_u64(type_section, 40)?)
        .map_err(|_| RawModelError::new("type trace 长度溢出"))?;
    let value_offset = usize::try_from(read_u64(type_section, 48)?)
        .map_err(|_| RawModelError::new("type value offset 溢出"))?;
    let value_len = usize::try_from(read_u64(type_section, 56)?)
        .map_err(|_| RawModelError::new("type value 长度溢出"))?;
    let name_offset = usize::try_from(read_u64(type_section, 64)?)
        .map_err(|_| RawModelError::new("type name offset 溢出"))?;
    let name_len = usize::try_from(read_u64(type_section, 72)?)
        .map_err(|_| RawModelError::new("type name 长度溢出"))?;
    let section_len = usize::try_from(read_u64(type_section, 80)?)
        .map_err(|_| RawModelError::new("type section 长度溢出"))?;
    let records_len = type_count
        .checked_mul(TYPE_RECORD_BYTES)
        .ok_or_else(|| RawModelError::new("type records 长度溢出"))?;
    let records_end = records_offset
        .checked_add(records_len)
        .ok_or_else(|| RawModelError::new("type records 范围溢出"))?;
    let trace_end = trace_offset
        .checked_add(trace_len)
        .ok_or_else(|| RawModelError::new("type trace 范围溢出"))?;
    let value_end = value_offset
        .checked_add(value_len)
        .ok_or_else(|| RawModelError::new("type value 范围溢出"))?;
    let name_end = name_offset
        .checked_add(name_len)
        .ok_or_else(|| RawModelError::new("type name 范围溢出"))?;
    if section_len != type_section.len()
        || records_offset < TYPE_HEADER_BYTES
        || records_end > type_section.len()
        || trace_end > type_section.len()
        || value_end > type_section.len()
        || name_end > type_section.len()
        || [records_offset, trace_offset, value_offset, name_offset]
            .iter()
            .any(|offset| offset % 8 != 0)
    {
        return Err(RawModelError::new("type section offset 越界"));
    }
    if !(records_offset <= records_end
        && records_end <= trace_offset
        && trace_offset <= trace_end
        && trace_end <= value_offset
        && value_offset <= value_end
        && value_end <= name_offset
        && name_offset <= name_end)
    {
        return Err(RawModelError::new("type section table 顺序非法"));
    }
    let trace_pool = &type_section[trace_offset..trace_end];
    let value_pool = &type_section[value_offset..value_end];
    let name_pool = &type_section[name_offset..name_end];
    let records = &type_section[records_offset..records_end];
    for entry in records.as_chunks::<TYPE_RECORD_BYTES>().0 {
        let flags = u32::from_le_bytes(entry[12..16].try_into().expect("type flags"));
        let record_trace_offset =
            u32::from_le_bytes(entry[24..28].try_into().expect("trace offset")) as usize;
        let record_trace_len =
            u32::from_le_bytes(entry[28..32].try_into().expect("trace len")) as usize;
        let record_value_offset =
            u32::from_le_bytes(entry[32..36].try_into().expect("value offset")) as usize;
        let record_value_len =
            u32::from_le_bytes(entry[36..40].try_into().expect("value len")) as usize;
        let name_offset_in_pool =
            u32::from_le_bytes(entry[16..20].try_into().expect("name offset")) as usize;
        let name_len_in_pool =
            u32::from_le_bytes(entry[20..24].try_into().expect("name len")) as usize;
        let name_end_in_pool = name_offset_in_pool
            .checked_add(name_len_in_pool)
            .ok_or_else(|| RawModelError::new("type name record 范围溢出"))?;
        if name_end_in_pool > name_pool.len()
            || std::str::from_utf8(&name_pool[name_offset_in_pool..name_end_in_pool]).is_err()
        {
            return Err(RawModelError::new("type name record 范围非法"));
        }
        if flags & !0xff != 0
            || ((flags & (1 << 2)) != 0) != (record_value_len != 0)
            || (record_value_len == 0 && record_value_offset != 0)
        {
            return Err(RawModelError::new(
                "type record flags 与 value program 不一致",
            ));
        }
        if record_trace_offset
            .checked_add(record_trace_len)
            .is_none_or(|end| end > trace_pool.len())
            || record_value_offset
                .checked_add(record_value_len)
                .is_none_or(|end| end > value_pool.len())
        {
            return Err(RawModelError::new("type record program 范围越界"));
        }
        let trace_start = u32::try_from(record_trace_offset)
            .map_err(|_| RawModelError::new("trace record offset 溢出"))?;
        if trace_program_len(trace_pool, trace_start)? as usize != record_trace_len {
            return Err(RawModelError::new("type record trace END 不一致"));
        }
        if record_value_len != 0 {
            let value_start = u32::try_from(record_value_offset)
                .map_err(|_| RawModelError::new("value record offset 溢出"))?;
            if value_program_len(value_pool, value_start)? as usize != record_value_len {
                return Err(RawModelError::new("type record value END 不一致"));
            }
        }
    }
    verify_metadata_section(metadata_section)
}

fn verify_metadata_section(section: &[u8]) -> Result<(), RawModelError> {
    if section.len() < META_HEADER_BYTES || &section[0..8] != b"GUGUMT01" {
        return Err(RawModelError::new("metadata section header 非法"));
    }
    if u16::from_le_bytes(section[8..10].try_into().expect("metadata version")) != 1
        || section[10] != 8
        || section[11] != 1
    {
        return Err(RawModelError::new("metadata section 版本或 ABI 非法"));
    }
    let root_count = read_u32(section, 12)? as usize;
    let vtable_count = read_u32(section, 16)? as usize;
    let source_count = read_u32(section, 20)? as usize;
    if read_u32(section, 24)? != 0 {
        return Err(RawModelError::new("metadata section reserved 字段非法"));
    }
    let alloc_count = read_u32(section, 28)? as usize;
    let root_offset = usize::try_from(read_u64(section, 32)?)
        .map_err(|_| RawModelError::new("root offset 溢出"))?;
    let vtable_index = usize::try_from(read_u64(section, 40)?)
        .map_err(|_| RawModelError::new("vtable index offset 溢出"))?;
    let vtable_data = usize::try_from(read_u64(section, 48)?)
        .map_err(|_| RawModelError::new("vtable data offset 溢出"))?;
    let source_offset = usize::try_from(read_u64(section, 56)?)
        .map_err(|_| RawModelError::new("source offset 溢出"))?;
    let string_offset = usize::try_from(read_u64(section, 64)?)
        .map_err(|_| RawModelError::new("string offset 溢出"))?;
    let string_len = usize::try_from(read_u64(section, 72)?)
        .map_err(|_| RawModelError::new("string 长度溢出"))?;
    let section_len = usize::try_from(read_u64(section, 80)?)
        .map_err(|_| RawModelError::new("metadata section 长度溢出"))?;
    let roots_len = root_count
        .checked_mul(ROOT_RECORD_BYTES)
        .ok_or_else(|| RawModelError::new("root records 长度溢出"))?;
    let vtable_index_len = vtable_count
        .checked_add(1)
        .and_then(|count| count.checked_mul(8))
        .ok_or_else(|| RawModelError::new("vtable index 长度溢出"))?;
    let vtable_data_len = vtable_count
        .checked_mul(VTABLE_BYTES)
        .ok_or_else(|| RawModelError::new("vtable data 长度溢出"))?;
    let source_len = source_count
        .checked_add(alloc_count)
        .and_then(|count| count.checked_mul(SOURCE_RECORD_BYTES))
        .ok_or_else(|| RawModelError::new("source records 长度溢出"))?;
    let root_end = root_offset
        .checked_add(roots_len)
        .ok_or_else(|| RawModelError::new("root records 范围溢出"))?;
    let vtable_index_end = vtable_index
        .checked_add(vtable_index_len)
        .ok_or_else(|| RawModelError::new("vtable index 范围溢出"))?;
    let vtable_data_end = vtable_data
        .checked_add(vtable_data_len)
        .ok_or_else(|| RawModelError::new("vtable data 范围溢出"))?;
    let source_end = source_offset
        .checked_add(source_len)
        .ok_or_else(|| RawModelError::new("source records 范围溢出"))?;
    let string_end = string_offset
        .checked_add(string_len)
        .ok_or_else(|| RawModelError::new("string 范围溢出"))?;
    if section_len != section.len()
        || root_offset < META_HEADER_BYTES
        || root_end > section.len()
        || vtable_index_end > section.len()
        || vtable_data_end > section.len()
        || source_end > section.len()
        || string_end > section.len()
        || [
            root_offset,
            vtable_index,
            vtable_data,
            source_offset,
            string_offset,
        ]
        .iter()
        .any(|offset| offset % 8 != 0)
    {
        return Err(RawModelError::new("metadata section offset 越界"));
    }
    if !(root_offset <= root_end
        && root_end <= vtable_index
        && vtable_index <= vtable_index_end
        && vtable_index_end <= vtable_data
        && vtable_data <= vtable_data_end
        && vtable_data_end <= source_offset
        && source_offset <= source_end
        && source_end <= string_offset
        && string_offset <= string_end)
    {
        return Err(RawModelError::new("metadata section table 顺序非法"));
    }
    let index_bytes = &section[vtable_index..vtable_index_end];
    let mut previous_index = 0u64;
    for index in 0..=vtable_count {
        let offset = index.checked_mul(8).expect("vtable index 范围已校验");
        let value = read_u64(index_bytes, offset)?;
        if (index == 0 && value != 0) || value < previous_index || value > vtable_data_len as u64 {
            return Err(RawModelError::new("vtable index 范围非法"));
        }
        previous_index = value;
    }
    if previous_index != vtable_data_len as u64 {
        return Err(RawModelError::new("vtable index 末端不匹配"));
    }
    let source_pool = &section[string_offset..string_end];
    for record in section[source_offset..source_end]
        .as_chunks::<SOURCE_RECORD_BYTES>()
        .0
    {
        let path_offset =
            u32::from_le_bytes(record[32..36].try_into().expect("source path offset")) as usize;
        let path_len =
            u32::from_le_bytes(record[36..40].try_into().expect("source path len")) as usize;
        let path_end = path_offset
            .checked_add(path_len)
            .ok_or_else(|| RawModelError::new("source path 范围溢出"))?;
        if path_end > source_pool.len()
            || std::str::from_utf8(&source_pool[path_offset..path_end]).is_err()
        {
            return Err(RawModelError::new("source path 范围非法"));
        }
    }
    Ok(())
}

fn put_u32(buffer: &mut [u8], offset: usize, value: u32) -> Result<(), RawModelError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| RawModelError::new("u32 offset 溢出"))?;
    buffer
        .get_mut(offset..end)
        .ok_or_else(|| RawModelError::new("u32 offset 越界"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(buffer: &mut [u8], offset: usize, value: u64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u32(buffer: &[u8], offset: usize) -> Result<u32, RawModelError> {
    buffer
        .get(offset..offset + 4)
        .ok_or_else(|| RawModelError::new("u32 字段越界"))
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("u32")))
}

fn read_u64(buffer: &[u8], offset: usize) -> Result<u64, RawModelError> {
    buffer
        .get(offset..offset + 8)
        .ok_or_else(|| RawModelError::new("u64 字段越界"))
        .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("u64")))
}

fn align_up_u64(value: u64, align: u64) -> Result<u64, RawModelError> {
    value
        .checked_add(align - 1)
        .and_then(|value| (value / align).checked_mul(align))
        .ok_or_else(|| RawModelError::new("section offset 溢出"))
}

fn pad_to(buffer: &mut Vec<u8>, offset: usize) {
    if buffer.len() < offset {
        buffer.resize(offset, 0);
    }
}
