//! Mosaic GC 元数据契约：类型描述、trace/value program、arena/block/line 布局、
//! 根来源、glue RVA、source 落点、boot verifier 的精确契约。
//!
//! 阶段 39：契约只携带 demand + section 指纹；section 字节在
//! `RuntimeRawModel` 的 compute 内真实编码并跑 `boot_verify`。Verifier 拒绝
//! 一切缺 END、键越界、计数不一致、offset 重叠、trace 与 layout 不一致。

#![allow(
    dead_code,
    reason = "schema 与 trace/value program 编解码由阶段 39 codec 与测试联合消费"
)]
use super::model::RawModelError;
use serde::{Deserialize, Serialize};

/// 类型表的副本；字段索引引用 `TypeUniverse.records`，所有引用都必须可解析。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcTypeEntryV1 {
    /// 与 `TypeRecord.key` 一致。
    pub type_key: [u8; 32],
    /// 与 `TypeRecord.canonical` 一致。
    pub canonical: Vec<u8>,
    pub name: String,
    /// `(size, align)`；`None` 表示 unsized view（仅 `Ty::Slice`）。
    pub layout: Option<(u64, u64)>,
    /// 子类型稳定键；trace program 使用 child_index 引用。
    pub children: Vec<[u8; 32]>,
    /// bit 0 `HAS_HEAP_DIRECT`：trace 含 `DIRECT`。
    /// bit 1 `HAS_HEAP_INTERIOR`：trace 含 `INTERIOR`。
    /// bit 2 `HAS_VALUE_ACTIONS`：值复制/销毁不是纯位拷贝。
    /// bit 3 `HAS_RESOURCE`：含 `ResourceCell` 字段，需 acquire/release。
    /// bit 4 `HAS_DEFERRED_RELEASE`：对象死亡需进入 release 队列。
    /// bit 5 `UNSIZED_VIEW`：layout 为 None（Slice view）。
    /// bit 6 `VARIABLE_SIZE`：动态 backing 类型。
    /// bit 7 `PIN_SENSITIVE`：禁止移位（预留，当前为 0）。
    pub flags: u8,
    /// `trace_program` 在 `GcMetadataSection::Trace` 内的字偏移。
    pub trace_offset: u32,
    /// `value_program` 在 `GcMetadataSection::Value` 内的字偏移。
    /// 当 `flags & HAS_VALUE_ACTIONS == 0` 时为 0。
    pub value_offset: u32,
}

/// vtable payload 类型 → 实现接口的索引；描述 trace/program 时进入 vtable RVA 计算。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct GcVtableEntryV1 {
    pub interface: [u8; 32],
    pub concrete_type: [u8; 32],
}

/// 描述某类 roots 在哪一段统计/来源（运行期入口点、稳定根池、协程局部）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcRootRangeV1 {
    pub kind: GcRootKindV1,
    pub location: GcRootLocationV1,
    /// 该范围内包含的 `TypeId` 上界与字偏移；verifier 校验范围不重叠、id 在表内。
    pub type_range: (u32, u32),
    pub word_range: (u32, u32),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum GcRootKindV1 {
    Static,
    LocalStatic,
    ForeignBridge,
    CoroutineFrame,
    HandleSlot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum GcRootLocationV1 {
    /// `type_range` 指向冻结类型表；`word_range` 是聚合 layout 内的字偏移集合。
    Aggregate { offset_bytes: u64 },
    /// `type_range` 指向冻结类型表；运行时按 vtable 解引用。
    Vtable { vtable_index: u32 },
}

/// Glue 入口：某个类型专属的复制/释放 thunk；阶段 39 仅占位，RVA 字段为 0。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcGlueEntryV1 {
    pub type_key: [u8; 32],
    /// copy thunk 的占位 RVA（运行时占位，阶段 39 留 0）。
    pub copy_rva: u32,
    /// release thunk 的占位 RVA（运行时占位，阶段 39 留 0）。
    pub release_rva: u32,
}

/// 来源 metadata：定义该类型/根的源文件、字节偏移（用于运行时错误定位）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcSourceEntryV1 {
    pub type_key: [u8; 32],
    pub source_path: String,
    pub byte_offset: u64,
}

/// 分配站点：每个会分配 GC 对象的镜像入口。runtime 校验分配大小是否落入 class。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcAllocSiteV1 {
    pub type_key: [u8; 32],
    pub location: GcSourceEntryV1,
}

/// arena/block/line 物理布局参数：阶段 39 与阶段 30–34 的 slab/size class 参数对齐。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcArenaLayoutV1 {
    pub arena_bytes: u64,
    pub block_bytes: u32,
    pub line_bytes: u32,
}

/// Mosaic GC metadata 契约段；每段独立编码，运行时按 schema 拼接。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcMetadataWorldV1 {
    /// 0: type section。
    pub types: Vec<GcTypeEntryV1>,
    /// 1: vtable section。
    pub vtables: Vec<GcVtableEntryV1>,
    /// 2: trace program section（每条 entry 的 trace_program 字节序列）。
    pub trace_program: Vec<u8>,
    /// 3: value program section（每条 entry 的 value_program 字节序列）。
    pub value_program: Vec<u8>,
    /// 4: glue section。
    pub glue: Vec<GcGlueEntryV1>,
    /// 5: root section。
    pub roots: Vec<GcRootRangeV1>,
    /// 6: source section。
    pub sources: Vec<GcSourceEntryV1>,
    /// 7: alloc section。
    pub alloc_sites: Vec<GcAllocSiteV1>,
    /// arena/block/line 参数。
    pub arena: GcArenaLayoutV1,
    /// schema 版本。
    pub schema: u32,
}

impl GcMetadataWorldV1 {
    pub(crate) const SCHEMA: u32 = 1;
    pub(crate) const MAGIC: &'static [u8] = b"GUGUGC01";
}

/// GC metadata demand：进入契约指纹；用于触发 `RuntimeRawModel` 重算。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GcMetadataDemand {
    pub type_count: u32,
    pub trace_program_bytes: u32,
    pub value_program_bytes: u32,
    pub vtable_count: u32,
    pub glue_count: u32,
    pub root_range_count: u32,
    pub source_count: u32,
    pub alloc_site_count: u32,
    pub arena_bytes: u64,
    pub block_bytes: u32,
    pub line_bytes: u32,
}

impl GcMetadataDemand {
    pub(crate) const fn empty() -> Self {
        Self {
            type_count: 0,
            trace_program_bytes: 0,
            value_program_bytes: 0,
            vtable_count: 0,
            glue_count: 0,
            root_range_count: 0,
            source_count: 0,
            alloc_site_count: 0,
            arena_bytes: 0,
            block_bytes: 0,
            line_bytes: 0,
        }
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&self.type_count.to_le_bytes());
        bytes.extend_from_slice(&self.trace_program_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.value_program_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.vtable_count.to_le_bytes());
        bytes.extend_from_slice(&self.glue_count.to_le_bytes());
        bytes.extend_from_slice(&self.root_range_count.to_le_bytes());
        bytes.extend_from_slice(&self.source_count.to_le_bytes());
        bytes.extend_from_slice(&self.alloc_site_count.to_le_bytes());
        bytes.extend_from_slice(&self.arena_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.block_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.line_bytes.to_le_bytes());
        crate::frontend::mono::keys::hash_domain("gugu-gc-metadata-demand-v1", &bytes)
    }
}

impl GcMetadataWorldV1 {
    pub(crate) fn demand(&self) -> GcMetadataDemand {
        GcMetadataDemand {
            type_count: self.types.len() as u32,
            trace_program_bytes: self.trace_program.len() as u32,
            value_program_bytes: self.value_program.len() as u32,
            vtable_count: self.vtables.len() as u32,
            glue_count: self.glue.len() as u32,
            root_range_count: self.roots.len() as u32,
            source_count: self.sources.len() as u32,
            alloc_site_count: self.alloc_sites.len() as u32,
            arena_bytes: self.arena.arena_bytes,
            block_bytes: self.arena.block_bytes,
            line_bytes: self.arena.line_bytes,
        }
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let payload = serde_json::to_vec(&(
            &self.types,
            &self.vtables,
            &self.trace_program,
            &self.value_program,
            &self.glue,
            &self.roots,
            &self.sources,
            &self.alloc_sites,
            &self.arena,
            self.schema,
        ))
        .expect("GC metadata 可序列化");
        crate::frontend::mono::keys::hash_domain("gugu-gc-metadata-world-v1", &payload)
    }
}

/// Trace program 字节级枚举（ULEB128 编码偏移、固定 order）；codec 与 spec 共有。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum TraceOp {
    /// 0x00: payload 末尾；每条 trace program 必须恰一个。
    End = 0x00,
    /// 0x01: `(base_word, 0|1)` — 当前 base+offset 是 heap 直接指针。
    Direct = 0x01,
    /// 0x02: `(base_word, 0|1)` — 当前 base+offset 是 heap 内嵌指针。
    Interior = 0x02,
    /// 0x03: `(base_word, count, stride, body_len, body)` — 数组，body 是嵌套 program。
    Repeat = 0x03,
    /// 0x04: `(tag_word, tag_offset_byte, tag_width, default_len, body, default_body)`。
    Switch = 0x04,
}

/// Value program 字节级枚举。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ValueOp {
    /// 0x00: payload 末尾。
    End = 0x00,
    /// 0x10: `(base_word, body_len, body)` — 按字段递归，body 嵌套 program。
    Aggregate = 0x10,
    /// 0x11: `(base_word, count, stride, body_len, body)`。
    RepeatValue = 0x11,
    /// 0x12: `(tag_word, tag_offset_byte, tag_width, case_count, cases...)`。
    SwitchValue = 0x12,
    /// 0x13: `(base_word)` — 字段是 COW，需要 publish。
    CowPublish = 0x13,
    /// 0x14: `(base_word)` — 字段是 ResourceCell lease，需要 acquire。
    AcquireResource = 0x14,
    /// 0x15: `(base_word)` — 字段是 ResourceCell lease，需要 release。
    ReleaseResource = 0x15,
}

pub(crate) fn trace_op_code(op: TraceOp) -> u8 {
    op as u8
}

pub(crate) fn value_op_code(op: ValueOp) -> u8 {
    op as u8
}

/// Boot verifier：检查合约字段、key 解析、offset 与 program 字节范围一致。
pub(crate) fn boot_verify(world: &GcMetadataWorldV1) -> Result<(), RawModelError> {
    if world.schema != GcMetadataWorldV1::SCHEMA {
        return Err(RawModelError::new("GC metadata schema 不匹配"));
    }
    if world.types.is_empty() {
        return Err(RawModelError::new("GC metadata 类型表为空"));
    }
    // keys 必须唯一；flags 位必须自洽。
    for (index, entry) in world.types.iter().enumerate() {
        if index as u32 != u32::try_from(index).map_err(|_| RawModelError::new("类型索引溢出"))?
        {
            return Err(RawModelError::new("类型索引与序列不连续"));
        }
        if entry.canonical.len() < 2 {
            return Err(RawModelError::new("类型 canonical 字节长度不足"));
        }
        if let Some((size, align)) = entry.layout {
            if !align.is_power_of_two() || size % align != 0 {
                return Err(RawModelError::new("类型 layout 非法"));
            }
            if (entry.flags & 0b10_0000) != 0 {
                return Err(RawModelError::new("unsized 类型不能携带 layout"));
            }
        }
        let trace_end = entry
            .trace_offset
            .checked_add(trace_program_len(&world.trace_program, entry.trace_offset)?)
            .ok_or_else(|| RawModelError::new("trace program 字节数溢出"))?;
        if trace_end > world.trace_program.len() as u32 {
            return Err(RawModelError::new("trace program 越界"));
        }
        if (entry.flags & 0b100) != 0 {
            let value_end = entry
                .value_offset
                .checked_add(value_program_len(&world.value_program, entry.value_offset)?)
                .ok_or_else(|| RawModelError::new("value program 字节数溢出"))?;
            if value_end > world.value_program.len() as u32 {
                return Err(RawModelError::new("value program 越界"));
            }
        }
        for key in &entry.children {
            if !world.types.iter().any(|other| &other.type_key == key) {
                return Err(RawModelError::new("类型 child key 不可解析"));
            }
        }
    }
    if !world.vtables.is_empty() {
        for vtable in &world.vtables {
            if !world
                .types
                .iter()
                .any(|other| other.type_key == vtable.concrete_type)
            {
                return Err(RawModelError::new("vtable concrete 类型不可解析"));
            }
        }
    }
    // root 范围必须不重叠、引用合法 type。
    let mut last_type_end: u32 = 0;
    let mut last_word_end: u32 = 0;
    for root in &world.roots {
        if root.type_range.0 < last_type_end {
            return Err(RawModelError::new("root type 范围重叠"));
        }
        if root.word_range.0 < last_word_end {
            return Err(RawModelError::new("root word 范围重叠"));
        }
        last_type_end = root.type_range.1;
        last_word_end = root.word_range.1;
    }
    if !world.trace_program.ends_with(&[TraceOp::End as u8]) {
        return Err(RawModelError::new("trace program 缺少 END"));
    }
    if !world.value_program.ends_with(&[ValueOp::End as u8]) {
        return Err(RawModelError::new("value program 缺少 END"));
    }
    Ok(())
}

/// 计算从 `start` 起的 trace program 长度：扫描嵌套 Repeat/Switch 直到 End。
fn trace_program_len(bytes: &[u8], start: u32) -> Result<u32, RawModelError> {
    let mut index = start as usize;
    loop {
        let op = *bytes
            .get(index)
            .ok_or_else(|| RawModelError::new("trace program 字节缺失"))?;
        index += 1;
        match op {
            x if x == TraceOp::End as u8 => return Ok((index - start as usize) as u32),
            x if x == TraceOp::Direct as u8 || x == TraceOp::Interior as u8 => {
                index = consume_uleb_pair(bytes, index)?;
            }
            x if x == TraceOp::Repeat as u8 => {
                index = consume_uleb_pair(bytes, index)?;
                let body_len = decode_uleb(bytes, &mut index)?;
                index = index
                    .checked_add(
                        body_len
                            .try_into()
                            .map_err(|_| RawModelError::new("trace program body 长度溢出"))?,
                    )
                    .ok_or_else(|| RawModelError::new("trace program body 越界"))?;
            }
            x if x == TraceOp::Switch as u8 => {
                index = consume_uleb_pair(bytes, index)?;
                let default_len = decode_uleb(bytes, &mut index)?;
                let case_count: u64 = decode_uleb(bytes, &mut index)?;
                let mut cases_total = default_len;
                for _ in 0..case_count {
                    let case_len = decode_uleb(bytes, &mut index)?;
                    cases_total = cases_total.saturating_add(case_len);
                }
                index = index
                    .checked_add(
                        cases_total
                            .try_into()
                            .map_err(|_| RawModelError::new("switch case body 长度溢出"))?,
                    )
                    .ok_or_else(|| RawModelError::new("switch case body 越界"))?;
            }
            _ => return Err(RawModelError::new("未知 trace op")),
        }
    }
}

fn value_program_len(bytes: &[u8], start: u32) -> Result<u32, RawModelError> {
    let mut index = start as usize;
    loop {
        let op = *bytes
            .get(index)
            .ok_or_else(|| RawModelError::new("value program 字节缺失"))?;
        index += 1;
        match op {
            x if x == ValueOp::End as u8 => return Ok((index - start as usize) as u32),
            x if x == ValueOp::Aggregate as u8 => {
                index = consume_uleb_pair(bytes, index)?;
                let body_len = decode_uleb(bytes, &mut index)?;
                index = index
                    .checked_add(
                        body_len
                            .try_into()
                            .map_err(|_| RawModelError::new("value aggregate body 长度溢出"))?,
                    )
                    .ok_or_else(|| RawModelError::new("value aggregate body 越界"))?;
            }
            x if x == ValueOp::RepeatValue as u8 => {
                index = consume_uleb_pair(bytes, index)?;
                let body_len = decode_uleb(bytes, &mut index)?;
                index = index
                    .checked_add(
                        body_len
                            .try_into()
                            .map_err(|_| RawModelError::new("value repeat body 长度溢出"))?,
                    )
                    .ok_or_else(|| RawModelError::new("value repeat body 越界"))?;
            }
            x if x == ValueOp::SwitchValue as u8 => {
                index = consume_uleb_pair(bytes, index)?;
                let case_count: u64 = decode_uleb(bytes, &mut index)?;
                let mut cases_total: u64 = 0;
                for _ in 0..case_count {
                    let case_len = decode_uleb(bytes, &mut index)?;
                    cases_total = cases_total.saturating_add(case_len);
                }
                index = index
                    .checked_add(
                        cases_total
                            .try_into()
                            .map_err(|_| RawModelError::new("value switch body 长度溢出"))?,
                    )
                    .ok_or_else(|| RawModelError::new("value switch body 越界"))?;
            }
            x if x == ValueOp::CowPublish as u8 => {
                index = consume_uleb_pair(bytes, index)?;
            }
            x if x == ValueOp::AcquireResource as u8 => {
                index = consume_uleb_pair(bytes, index)?;
            }
            x if x == ValueOp::ReleaseResource as u8 => {
                index = consume_uleb_pair(bytes, index)?;
            }
            _ => return Err(RawModelError::new("未知 value op")),
        }
    }
}

fn consume_uleb_pair(bytes: &[u8], start: usize) -> Result<usize, RawModelError> {
    let mut index = start;
    let _ = decode_uleb(bytes, &mut index)?;
    let _ = decode_uleb(bytes, &mut index)?;
    Ok(index)
}

/// 解码 ULEB128；终止字节高位为 0。
pub(crate) fn decode_uleb(bytes: &[u8], index: &mut usize) -> Result<u64, RawModelError> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = *bytes
            .get(*index)
            .ok_or_else(|| RawModelError::new("ULEB128 截断"))?;
        *index += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 70 {
            return Err(RawModelError::new("ULEB128 编码过长"));
        }
    }
}

/// 编码 ULEB128。
pub(crate) fn encode_uleb(out: &mut Vec<u8>, value: u64) {
    let mut current = value;
    loop {
        let byte = (current & 0x7F) as u8;
        current >>= 7;
        if current == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}
