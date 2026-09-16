//! Mosaic GC 元数据 schema：类型描述、trace 描述符、value program、arena/block/line 布局、
//! 根来源、glue RVA、source 落点与 boot verifier 的精确契约。
//!
//! 本模块只定义逻辑世界、descriptor 与 program 的长度解析与校验规则；镜像 section 的字节布局
//! 与编解码在 `gc_metadata_section`。boot verifier 必须拒绝未知 kind/opcode、非 canonical
//! ULEB128、缺 END、键越界、计数不一致、pool 未被 descriptor 紧密覆盖、bitmap 保留位或
//! 重叠位、trace/value 长度漂移与操作数越界。

use std::collections::BTreeSet;

use super::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
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
    /// trace descriptor 在 `GcMetadataSection::Trace` 内的字节偏移。
    pub trace_offset: u32,
    /// value program 在 `GcMetadataSection::Value` 内的字节偏移；无动作时为 0。
    pub value_offset: u32,
    /// 当前类型 trace descriptor 的字节长度。
    pub trace_len: u32,
    /// 当前类型 value program 的字节长度；无动作时为 0。
    pub value_len: u32,
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

/// Glue 入口：某个类型专属的复制/释放 thunk；链接阶段尚未分配 RVA 时为 0。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GcGlueEntryV1 {
    pub type_key: [u8; 32],
    /// copy thunk 的链接时 RVA；未生成独立 thunk 时为 0。
    pub copy_rva: u32,
    /// release thunk 的链接时 RVA；未生成独立 thunk 时为 0。
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

/// arena/block/line 物理布局参数：与 raw slab/size class 的参数同源。
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
    /// 2: trace descriptor pool（每条 entry 的 descriptor 字节序列）。
    pub trace_program: Vec<u8>,
    /// 3: value program pool（每条 entry 的 value_program 字节序列）。
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
    /// schema 2：trace descriptor 增加 kind 字节与 Bitmap/Program 双表示，value program 改为
    /// 正向/逆向两阶段动作指令。
    pub(crate) const SCHEMA: u32 = 2;
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
    /// 编码后的 type/meta section 总字节数。
    pub type_section_bytes: u32,
    pub metadata_section_bytes: u32,
    /// 真实 world 内容指纹；计数相同但布局/program 变化时仍失效缓存。
    pub world_fingerprint: [u8; 32],
}

impl GcMetadataDemand {
    /// 返回空需求。
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
            type_section_bytes: 0,
            metadata_section_bytes: 0,
            world_fingerprint: [0; 32],
        }
    }

    /// 返回需求视图的稳定指纹。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(128);
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
        bytes.extend_from_slice(&self.type_section_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.metadata_section_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.world_fingerprint);
        crate::frontend::mono::keys::hash_domain("gugu-gc-metadata-demand-v2", &bytes)
    }
}

impl GcMetadataWorldV1 {
    /// 从 world 内容推导需求视图；section 长度由编码阶段回填。
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
            type_section_bytes: 0,
            metadata_section_bytes: 0,
            world_fingerprint: self.fingerprint(),
        }
    }

    /// 返回 world 内容指纹；编码变化必须使旧缓存失效。
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
        crate::frontend::mono::keys::hash_domain("gugu-gc-metadata-world-v2", &payload)
    }
}

/// trace descriptor 的表示判别值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum TraceKind {
    /// 0：类型没有 tracked managed pointer，descriptor 只占一个字节。
    None = 0,
    /// 1：定长、pointer word 数不超过 256 的扁平 word 位图。
    Bitmap = 1,
    /// 2：通用 opcode program。
    Program = 2,
}

/// 单个 trace bitmap 允许的最大 pointer word 数。
pub(crate) const TRACE_BITMAP_MAX_WORDS: u32 = 256;

/// trace program 字节级枚举（ULEB128 编码偏移、固定 order）；codec 与 spec 共有。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum TraceOp {
    /// 0x00: payload 末尾；每条 program 必须恰一个。
    End = 0x00,
    /// 0x01: `(base_word, count)` — 连续 `count` 个 heap 直接指针 word。
    Direct = 0x01,
    /// 0x02: `(base_word, count)` — 连续 `count` 个 heap 内嵌指针 word。
    Interior = 0x02,
    /// 0x03: `(base_word, count, stride_word, body_len u32, body)` — 定长数组重复 nested program。
    Repeat = 0x03,
    /// 0x04: `(base_word, count_byte_offset, count_width, stride_word, body_len u32, body)` —
    /// 从 payload 字段读取运行时元素数后重复 body。
    RepeatField = 0x04,
    /// 0x05: `(tag_byte_offset, tag_width, case_count, cases, default_len u32, default)`，
    /// 每个 case 为 `tag_value u64`、`body_len u32`、body，按无符号 tag 严格递增。
    Switch = 0x05,
    /// 0x06: 无操作数；按固定 arena backing 记录逐槽应用运行时 `TypeId` descriptor。
    ArenaSlots = 0x06,
}

/// value program 字节级枚举。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ValueOp {
    /// 0x00: payload 末尾。
    End = 0x00,
    /// 0x10: `(byte_offset, type_index)` — 调用字段的 copy 语义。
    CopyField = 0x10,
    /// 0x11: `(byte_offset, type_index)` — 按逆序调用字段的 drop 语义。
    DropField = 0x11,
    /// 0x12: `(byte_offset, type_index)` — 调用字段的 publish 语义。
    PublishField = 0x12,
    /// 0x13: `(byte_offset, type_index)` — 获得该字段类型的 resource 租约。
    AcquireResource = 0x13,
    /// 0x14: `(byte_offset, type_index)` — 释放该字段类型的 resource 租约。
    ReleaseResource = 0x14,
    /// 0x15: `(base_word, count, stride_word, body_len u32, body)` — 定长数组重复字段动作。
    RepeatValue = 0x15,
    /// 0x16: `(tag_byte_offset, tag_width, case_count, cases, default_len u32, default)`。
    SwitchValue = 0x16,
}

/// value 动作的类别位：正向类与逆向类不能在同一个 wrapper body 内混用。
pub(crate) const VALUE_CLASS_FORWARD: u8 = 1;
/// 逆向类动作位。
pub(crate) const VALUE_CLASS_BACKWARD: u8 = 2;
/// 两类动作的并集。
pub(crate) const VALUE_CLASS_ALL: u8 = VALUE_CLASS_FORWARD | VALUE_CLASS_BACKWARD;

/// Boot verifier：检查合约字段、key 解析、descriptor/program 字节范围与 pool 覆盖一致。
pub(crate) fn boot_verify(world: &GcMetadataWorldV1) -> Result<(), RawModelError> {
    if world.schema != GcMetadataWorldV1::SCHEMA {
        return Err(RawModelError::new("GC metadata schema 不匹配"));
    }
    // 空闭世界（例如空包）合法：类型表与两个 program pool 都必须为空，不能只缺其中一项。
    if world.types.is_empty() {
        if !world.trace_program.is_empty()
            || !world.value_program.is_empty()
            || !world.glue.is_empty()
            || !world.vtables.is_empty()
            || !world.roots.is_empty()
            || !world.sources.is_empty()
            || !world.alloc_sites.is_empty()
        {
            return Err(RawModelError::new(
                "空类型表不得携带 program、root 或 vtable",
            ));
        }
        return verify_arena(world);
    }
    if world.types.len() > u32::MAX as usize
        || world.vtables.len() > u32::MAX as usize
        || world.trace_program.len() > u32::MAX as usize
        || world.value_program.len() > u32::MAX as usize
        || world.glue.len() > u32::MAX as usize
        || world.roots.len() > u32::MAX as usize
        || world.sources.len() > u32::MAX as usize
        || world.alloc_sites.len() > u32::MAX as usize
    {
        return Err(RawModelError::new(
            "GC metadata 数量或 program 长度超过 u32",
        ));
    }
    // keys 必须唯一且按 TypeId 的稳定键顺序排列；flags 位必须自洽。
    let mut keys = BTreeSet::new();
    let mut previous_key = None;
    for entry in &world.types {
        if previous_key.is_some_and(|key| key >= entry.type_key) {
            return Err(RawModelError::new("类型 key 未按稳定顺序排列"));
        }
        previous_key = Some(entry.type_key);
        if !keys.insert(entry.type_key) {
            return Err(RawModelError::new("类型 key 重复"));
        }
        if entry.canonical.len() < 2 {
            return Err(RawModelError::new("类型 canonical 字节长度不足"));
        }
        if entry.layout.is_none() != (entry.flags & 0b10_0000 != 0) {
            return Err(RawModelError::new("unsized flag 与 layout 不一致"));
        }
        if (entry.flags & 0b1_0000 != 0) && (entry.flags & 0b1000 == 0) {
            return Err(RawModelError::new(
                "deferred release flag 缺少 resource flag",
            ));
        }
        if let Some((size, align)) = entry.layout {
            if !align.is_power_of_two() || size % align != 0 {
                return Err(RawModelError::new("类型 layout 非法"));
            }
            if (entry.flags & 0b10_0000) != 0 {
                return Err(RawModelError::new("unsized 类型不能携带 layout"));
            }
        }
        for key in &entry.children {
            if !world.types.iter().any(|other| &other.type_key == key) {
                return Err(RawModelError::new("类型 child key 不可解析"));
            }
        }
    }
    if !world.vtables.is_empty() {
        let mut vtable_keys = BTreeSet::new();
        for vtable in &world.vtables {
            if !vtable_keys.insert((vtable.concrete_type, vtable.interface)) {
                return Err(RawModelError::new("vtable key 重复"));
            }
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
    let mut last_type_start = 0u32;
    let mut last_type_end = 0u32;
    let mut last_word_end: u32 = 0;
    for root in &world.roots {
        if root.type_range.0 >= root.type_range.1
            || root.type_range.1 > world.types.len() as u32
            || root.word_range.0 >= root.word_range.1
        {
            return Err(RawModelError::new("root range 为空或越界"));
        }
        if root.type_range.0 < last_type_start
            || (root.type_range.0 != last_type_start && root.type_range.0 < last_type_end)
        {
            return Err(RawModelError::new("root type 范围未排序或重叠"));
        }
        if root.word_range.0 < last_word_end {
            return Err(RawModelError::new("root word 范围重叠"));
        }
        last_type_start = root.type_range.0;
        last_type_end = last_type_end.max(root.type_range.1);
        last_word_end = root.word_range.1;
    }
    verify_pools(world)?;
    verify_arena(world)
}

/// 校验 trace/value pool 被各 entry 的 descriptor/program 从 0 起紧密覆盖，并执行跨字段检查。
fn verify_pools(world: &GcMetadataWorldV1) -> Result<(), RawModelError> {
    let type_count =
        u32::try_from(world.types.len()).map_err(|_| RawModelError::new("类型数量超过 u32"))?;
    let mut trace_cursor = 0usize;
    let mut value_cursor = 0usize;
    for entry in &world.types {
        let offset = usize::try_from(entry.trace_offset)
            .map_err(|_| RawModelError::new("trace 起点溢出"))?;
        if offset != trace_cursor {
            return Err(RawModelError::new("trace pool 未被 descriptor 紧密覆盖"));
        }
        let length = usize::try_from(trace_descriptor_len(
            &world.trace_program,
            entry.trace_offset,
        )?)
        .map_err(|_| RawModelError::new("trace descriptor 长度溢出"))?;
        if entry.trace_len as usize != length {
            return Err(RawModelError::new("trace descriptor 长度字段不一致"));
        }
        // Bitmap 表示只描述定长对象的 payload word 数。
        if world.trace_program.get(offset) == Some(&(TraceKind::Bitmap as u8)) {
            let words = read_u32_le(&world.trace_program, offset + 4)?;
            if let Some((size, _)) = entry.layout {
                if u64::from(words) != size.div_ceil(8) {
                    return Err(RawModelError::new("trace bitmap word 数与类型 size 不一致"));
                }
            } else {
                return Err(RawModelError::new("unsized 类型不得使用 trace bitmap"));
            }
        }
        let (has_direct, has_interior) =
            trace_descriptor_presence(&world.trace_program, entry.trace_offset)?;
        if (entry.flags & 1 != 0) != has_direct || (entry.flags & 0b10 != 0) != has_interior {
            return Err(RawModelError::new(
                "类型 pointer flags 与 trace descriptor 不一致",
            ));
        }
        trace_cursor = offset
            .checked_add(length)
            .ok_or_else(|| RawModelError::new("trace pool 范围溢出"))?;
        if entry.flags & 0b100 != 0 {
            if entry.value_len == 0 {
                return Err(RawModelError::new(
                    "HAS_VALUE_ACTIONS entry 缺少 value program",
                ));
            }
            let value_offset = usize::try_from(entry.value_offset)
                .map_err(|_| RawModelError::new("value 起点溢出"))?;
            if value_offset != value_cursor {
                return Err(RawModelError::new("value pool 未被 program 紧密覆盖"));
            }
            let value_length = usize::try_from(value_program_len(
                &world.value_program,
                entry.value_offset,
                type_count,
            )?)
            .map_err(|_| RawModelError::new("value program 长度溢出"))?;
            if entry.value_len as usize != value_length {
                return Err(RawModelError::new("value program 长度字段不一致"));
            }
            value_cursor = value_offset
                .checked_add(value_length)
                .ok_or_else(|| RawModelError::new("value pool 范围溢出"))?;
        } else if entry.value_offset != 0 || entry.value_len != 0 {
            return Err(RawModelError::new(
                "无 value action 的 entry 不得携带 program",
            ));
        }
    }
    if trace_cursor != world.trace_program.len() {
        return Err(RawModelError::new("trace pool 存在未覆盖字节"));
    }
    if value_cursor != world.value_program.len() {
        return Err(RawModelError::new("value pool 存在未覆盖字节"));
    }
    Ok(())
}

/// 校验 arena/block/line 与契约常量一致；空 world 与真实 world 共用同一条路径。
fn verify_arena(world: &GcMetadataWorldV1) -> Result<(), RawModelError> {
    if world.arena.arena_bytes != GC_ARENA_BYTES
        || world.arena.block_bytes != GC_BLOCK_BYTES
        || world.arena.line_bytes != GC_LINE_BYTES
    {
        return Err(RawModelError::new("GC arena/block/line 与契约常量不一致"));
    }
    Ok(())
}

/// 计算从 `start` 起的 trace descriptor 长度：kind 字节加表示相关负载。
pub(crate) fn trace_descriptor_len(bytes: &[u8], start: u32) -> Result<u32, RawModelError> {
    let start = usize::try_from(start).map_err(|_| RawModelError::new("trace 起点溢出"))?;
    let kind = *bytes
        .get(start)
        .ok_or_else(|| RawModelError::new("trace descriptor 字节缺失"))?;
    match kind {
        x if x == TraceKind::None as u8 => Ok(1),
        x if x == TraceKind::Bitmap as u8 => {
            let reserved = bytes
                .get(start + 1..start + 4)
                .ok_or_else(|| RawModelError::new("trace bitmap 头截断"))?;
            if reserved.iter().any(|byte| *byte != 0) {
                return Err(RawModelError::new("trace bitmap reserved 字段非零"));
            }
            let word_count = read_u32_le(bytes, start + 4)?;
            if word_count == 0 || word_count > TRACE_BITMAP_MAX_WORDS {
                return Err(RawModelError::new("trace bitmap word 数越界"));
            }
            let bitmap_bytes =
                usize::try_from(word_count.div_ceil(8)).expect("bitmap 字节数适配宿主");
            let body = 8usize
                .checked_add(bitmap_bytes * 2)
                .ok_or_else(|| RawModelError::new("trace bitmap 长度溢出"))?;
            let length = body
                .checked_next_multiple_of(4)
                .ok_or_else(|| RawModelError::new("trace bitmap 长度溢出"))?;
            let end = start
                .checked_add(length)
                .ok_or_else(|| RawModelError::new("trace bitmap 范围溢出"))?;
            if end > bytes.len() {
                return Err(RawModelError::new("trace bitmap 越界"));
            }
            let direct = &bytes[start + 8..start + 8 + bitmap_bytes];
            let interior = &bytes[start + 8 + bitmap_bytes..start + 8 + bitmap_bytes * 2];
            for index in 0..bitmap_bytes {
                if direct[index] & interior[index] != 0 {
                    return Err(RawModelError::new("trace bitmap 直接与 interior 位重叠"));
                }
            }
            if bytes[start + body..end].iter().any(|byte| *byte != 0) {
                return Err(RawModelError::new("trace bitmap padding 非零"));
            }
            let tail = bitmap_bytes * 8 - word_count as usize;
            if tail > 0 {
                let mask = !((1u8 << (8 - tail)) - 1);
                if direct[bitmap_bytes - 1] & mask != 0 || interior[bitmap_bytes - 1] & mask != 0 {
                    return Err(RawModelError::new("trace bitmap 尾部无效位非零"));
                }
            }
            u32::try_from(length).map_err(|_| RawModelError::new("trace bitmap 长度溢出"))
        }
        x if x == TraceKind::Program as u8 => {
            let program_len = read_u32_le(bytes, start + 1)?;
            let program_start = start + 5;
            let program_end = program_start
                .checked_add(usize::try_from(program_len).expect("program 长度适配宿主"))
                .ok_or_else(|| RawModelError::new("trace program 范围溢出"))?;
            if program_end > bytes.len() {
                return Err(RawModelError::new("trace program 越界"));
            }
            let start_word = u32::try_from(program_start)
                .map_err(|_| RawModelError::new("trace program 起点溢出"))?;
            if trace_program_len(bytes, start_word)? != program_len {
                return Err(RawModelError::new("trace program 长度字段不一致"));
            }
            5u32.checked_add(program_len)
                .ok_or_else(|| RawModelError::new("trace descriptor 长度溢出"))
        }
        _ => Err(RawModelError::new("未知 trace descriptor kind")),
    }
}

/// trace program 一次扫描的结果：字节长度与是否含直接/interior 指针。
struct TraceOutcome {
    len: u32,
    has_direct: bool,
    has_interior: bool,
}

/// 计算从 `start` 起的 trace program 长度：扫描嵌套 Repeat/RepeatField/Switch 直到 End。
pub(crate) fn trace_program_len(bytes: &[u8], start: u32) -> Result<u32, RawModelError> {
    Ok(trace_program_at(bytes, start, 0)?.len)
}

/// 返回 trace descriptor 是否携带直接/interior 指针；用于与类型 flags 交叉校验。
pub(crate) fn trace_descriptor_presence(
    bytes: &[u8],
    start: u32,
) -> Result<(bool, bool), RawModelError> {
    let start_usize = usize::try_from(start).map_err(|_| RawModelError::new("trace 起点溢出"))?;
    match bytes.get(start_usize) {
        Some(&kind) if kind == TraceKind::None as u8 => Ok((false, false)),
        Some(&kind) if kind == TraceKind::Bitmap as u8 => {
            let word_count = read_u32_le(bytes, start_usize + 4)?;
            let bitmap_bytes =
                usize::try_from(word_count.div_ceil(8)).expect("bitmap 字节数适配宿主");
            let direct = bytes
                .get(start_usize + 8..start_usize + 8 + bitmap_bytes)
                .ok_or_else(|| RawModelError::new("trace bitmap 越界"))?;
            let interior = bytes
                .get(start_usize + 8 + bitmap_bytes..start_usize + 8 + bitmap_bytes * 2)
                .ok_or_else(|| RawModelError::new("trace bitmap 越界"))?;
            Ok((
                direct.iter().any(|byte| *byte != 0),
                interior.iter().any(|byte| *byte != 0),
            ))
        }
        Some(&kind) if kind == TraceKind::Program as u8 => {
            let program_len = read_u32_le(bytes, start_usize + 1)?;
            let start_word = u32::try_from(start_usize + 5)
                .map_err(|_| RawModelError::new("trace program 起点溢出"))?;
            let outcome = trace_program_at(bytes, start_word, 0)?;
            if outcome.len != program_len {
                return Err(RawModelError::new("trace program 长度字段不一致"));
            }
            Ok((outcome.has_direct, outcome.has_interior))
        }
        _ => Err(RawModelError::new("未知 trace descriptor kind")),
    }
}

fn trace_program_at(bytes: &[u8], start: u32, depth: u8) -> Result<TraceOutcome, RawModelError> {
    if depth > 32 {
        return Err(RawModelError::new("trace program 嵌套过深"));
    }
    let start = usize::try_from(start).map_err(|_| RawModelError::new("trace 起点溢出"))?;
    let mut index = start;
    let mut outcome = TraceOutcome {
        len: 0,
        has_direct: false,
        has_interior: false,
    };
    loop {
        let op = *bytes
            .get(index)
            .ok_or_else(|| RawModelError::new("trace program 字节缺失"))?;
        index += 1;
        match op {
            x if x == TraceOp::End as u8 => {
                outcome.len = u32::try_from(index - start)
                    .map_err(|_| RawModelError::new("trace program 长度溢出"))?;
                return Ok(outcome);
            }
            x if x == TraceOp::Direct as u8 || x == TraceOp::Interior as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let _ = decode_uleb(bytes, &mut index)?;
                if x == TraceOp::Direct as u8 {
                    outcome.has_direct = true;
                } else {
                    outcome.has_interior = true;
                }
            }
            x if x == TraceOp::Repeat as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let _ = decode_uleb(bytes, &mut index)?;
                let _ = decode_uleb(bytes, &mut index)?;
                index = consume_body(bytes, index, depth, &mut outcome, "trace repeat")?;
            }
            x if x == TraceOp::RepeatField as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let _ = decode_uleb(bytes, &mut index)?;
                let width = decode_uleb(bytes, &mut index)?;
                require_field_width(width, "REPEAT_FIELD count_width")?;
                let _ = decode_uleb(bytes, &mut index)?;
                index = consume_body(bytes, index, depth, &mut outcome, "trace repeat-field")?;
            }
            x if x == TraceOp::Switch as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let width = decode_uleb(bytes, &mut index)?;
                require_field_width(width, "SWITCH tag_width")?;
                let case_count = usize::try_from(decode_uleb(bytes, &mut index)?)
                    .map_err(|_| RawModelError::new("trace case 数量溢出"))?;
                let mut previous_tag: Option<u64> = None;
                for _ in 0..case_count {
                    let tag = read_u64_le(bytes, index)?;
                    if previous_tag.is_some_and(|previous| tag <= previous) {
                        return Err(RawModelError::new("trace case tag 未严格递增"));
                    }
                    previous_tag = Some(tag);
                    index += 8;
                    index = consume_body(bytes, index, depth, &mut outcome, "trace case")?;
                }
                index = consume_body(bytes, index, depth, &mut outcome, "trace default")?;
            }
            x if x == TraceOp::ArenaSlots as u8 => {}
            _ => return Err(RawModelError::new("未知 trace op")),
        }
    }
}

/// 读取一个 u32 LE body 长度并校验 body 恰好是一个合法 nested program，并合并其指针存在位。
fn consume_body(
    bytes: &[u8],
    start: usize,
    depth: u8,
    outcome: &mut TraceOutcome,
    what: &str,
) -> Result<usize, RawModelError> {
    let length = read_u32_le(bytes, start)?;
    let body_start = start
        .checked_add(4)
        .ok_or_else(|| RawModelError::new(format!("{what} body 起点溢出")))?;
    let body_len = usize::try_from(length).map_err(|_| RawModelError::new("body 长度溢出"))?;
    let body_end = body_start
        .checked_add(body_len)
        .ok_or_else(|| RawModelError::new("body 范围溢出"))?;
    if body_end > bytes.len() {
        return Err(RawModelError::new("body 越界"));
    }
    let start_word =
        u32::try_from(body_start).map_err(|_| RawModelError::new("body 起点适配 u32 失败"))?;
    let nested = trace_program_at(bytes, start_word, depth + 1)?;
    if nested.len as usize != body_len {
        return Err(RawModelError::new("body 长度与 program 不一致"));
    }
    outcome.has_direct |= nested.has_direct;
    outcome.has_interior |= nested.has_interior;
    Ok(body_end)
}

/// 计算从 `start` 起的 value program 长度；`type_count` 用于校验操作数范围。
pub(crate) fn value_program_len(
    bytes: &[u8],
    start: u32,
    type_count: u32,
) -> Result<u32, RawModelError> {
    Ok(value_program_len_at(bytes, start, type_count, 0)?.0)
}

fn value_program_len_at(
    bytes: &[u8],
    start: u32,
    type_count: u32,
    depth: u8,
) -> Result<(u32, u8), RawModelError> {
    if depth > 32 {
        return Err(RawModelError::new("value program 嵌套过深"));
    }
    let start = usize::try_from(start).map_err(|_| RawModelError::new("value 起点溢出"))?;
    let mut index = start;
    let mut classes = 0u8;
    loop {
        let op = *bytes
            .get(index)
            .ok_or_else(|| RawModelError::new("value program 字节缺失"))?;
        index += 1;
        match op {
            x if x == ValueOp::End as u8 => {
                let length = u32::try_from(index - start)
                    .map_err(|_| RawModelError::new("value program 长度溢出"))?;
                return Ok((length, classes));
            }
            x if x == ValueOp::CopyField as u8
                || x == ValueOp::DropField as u8
                || x == ValueOp::PublishField as u8 =>
            {
                let _ = decode_uleb(bytes, &mut index)?;
                let type_index = decode_uleb(bytes, &mut index)?;
                if type_index >= u64::from(type_count) {
                    return Err(RawModelError::new("value 指令的类型索引越界"));
                }
                classes |= if x == ValueOp::DropField as u8 {
                    VALUE_CLASS_BACKWARD
                } else {
                    VALUE_CLASS_FORWARD
                };
            }
            x if x == ValueOp::AcquireResource as u8 || x == ValueOp::ReleaseResource as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let type_index = decode_uleb(bytes, &mut index)?;
                if type_index >= u64::from(type_count) {
                    return Err(RawModelError::new("resource 指令的类型索引越界"));
                }
                classes |= if x == ValueOp::ReleaseResource as u8 {
                    VALUE_CLASS_BACKWARD
                } else {
                    VALUE_CLASS_FORWARD
                };
            }
            x if x == ValueOp::RepeatValue as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let _ = decode_uleb(bytes, &mut index)?;
                let _ = decode_uleb(bytes, &mut index)?;
                index = consume_value_body(bytes, index, type_count, depth, "REPEAT_VALUE")?;
            }
            x if x == ValueOp::SwitchValue as u8 => {
                let _ = decode_uleb(bytes, &mut index)?;
                let width = decode_uleb(bytes, &mut index)?;
                require_field_width(width, "SWITCH_VALUE tag_width")?;
                let case_count = usize::try_from(decode_uleb(bytes, &mut index)?)
                    .map_err(|_| RawModelError::new("value case 数量溢出"))?;
                let mut previous_tag: Option<u64> = None;
                for _ in 0..case_count {
                    let tag = read_u64_le(bytes, index)?;
                    if previous_tag.is_some_and(|previous| tag <= previous) {
                        return Err(RawModelError::new("value case tag 未严格递增"));
                    }
                    previous_tag = Some(tag);
                    index += 8;
                    index =
                        consume_value_body(bytes, index, type_count, depth, "SWITCH_VALUE case")?;
                }
                index =
                    consume_value_body(bytes, index, type_count, depth, "SWITCH_VALUE default")?;
            }
            _ => return Err(RawModelError::new("未知 value op")),
        }
    }
}

/// 读取一个 u32 LE body 长度并校验 body 是类别同质的 nested value program。
fn consume_value_body(
    bytes: &[u8],
    start: usize,
    type_count: u32,
    depth: u8,
    what: &str,
) -> Result<usize, RawModelError> {
    let length = read_u32_le(bytes, start)?;
    let body_start = start
        .checked_add(4)
        .ok_or_else(|| RawModelError::new(format!("{what} body 起点溢出")))?;
    let body_len = usize::try_from(length).map_err(|_| RawModelError::new("body 长度溢出"))?;
    let body_end = body_start
        .checked_add(body_len)
        .ok_or_else(|| RawModelError::new("body 范围溢出"))?;
    if body_end > bytes.len() {
        return Err(RawModelError::new("body 越界"));
    }
    let start_word =
        u32::try_from(body_start).map_err(|_| RawModelError::new("body 起点适配 u32 失败"))?;
    let (length, classes) = value_program_len_at(bytes, start_word, type_count, depth + 1)?;
    if classes == VALUE_CLASS_ALL {
        return Err(RawModelError::new("value wrapper body 混用正向与逆向动作"));
    }
    if usize::try_from(length).expect("body 长度适配宿主") != body_len {
        return Err(RawModelError::new("body 长度与 program 不一致"));
    }
    Ok(body_end)
}

/// 字段偏移的操作数宽度只允许 1、2、4、8 字节。
fn require_field_width(width: u64, what: &str) -> Result<(), RawModelError> {
    if matches!(width, 1 | 2 | 4 | 8) {
        Ok(())
    } else {
        Err(RawModelError::new(format!("{what} 只允许 1、2、4 或 8")))
    }
}

/// 读取一个小端 u32。
fn read_u32_le(bytes: &[u8], start: usize) -> Result<u32, RawModelError> {
    let end = start
        .checked_add(4)
        .ok_or_else(|| RawModelError::new("u32 字段范围溢出"))?;
    bytes
        .get(start..end)
        .ok_or_else(|| RawModelError::new("u32 字段越界"))
        .map(|slice| u32::from_le_bytes(slice.try_into().expect("u32 字段宽度")))
}

/// 读取一个小端 u64。
fn read_u64_le(bytes: &[u8], start: usize) -> Result<u64, RawModelError> {
    let end = start
        .checked_add(8)
        .ok_or_else(|| RawModelError::new("u64 字段范围溢出"))?;
    bytes
        .get(start..end)
        .ok_or_else(|| RawModelError::new("u64 字段越界"))
        .map(|slice| u64::from_le_bytes(slice.try_into().expect("u64 字段宽度")))
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
        let payload = u64::from(byte & 0x7F);
        if shift == 63 && payload > 1 {
            return Err(RawModelError::new("ULEB128 数值溢出"));
        }
        result |= payload << shift;
        if byte & 0x80 == 0 {
            if shift != 0 && payload == 0 {
                return Err(RawModelError::new("ULEB128 非 canonical 编码"));
            }
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
