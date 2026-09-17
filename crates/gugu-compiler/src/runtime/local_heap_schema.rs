//! LocalHeap Immix arena、block/line、TLAB、位图、pin side table 与分代触发参数的契约段。
//!
//! 本段把「arena 布局与 side metadata」「generation/representation/object flag 目录」
//! 「minor/major 阶段目录」「object header 与 arena/pin 记录布局」「nursery 触发参数」
//! 固定成带版本的对象，与 `barrier_schema`/`gc_metadata_contract`/`pacing_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! 参数是编译期实现门禁，不是用户可观察的时序或地址：契约只登记容量、位图规模、状态名与
//! 位掩码，不登记宿主地址、线程数或回收时刻。arena/block/line 参数与 GC metadata 契约同源，
//! card 粒度与 barrier 契约同源，host page 粒度与平台 profile 同源。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::barrier_schema::CARD_GRANULARITY_BYTES;
use super::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use super::model::RawModelError;
use super::platform::PlatformProfile;

/// 本地堆契约段 schema。
///
/// 版本 3 相对版本 2 的变化：块记录新增 `candidate_job` 绑定与 `active/candidate/reclaiming/free`
/// 状态语义、lease 计数成为候选 gate 的输入、块世代在释放时推进、`ManagedBlockId` 的 arena 部分
/// 就是 arena descriptor（世代与记录读写必须按 descriptor 定位）。
pub(crate) const LOCAL_HEAP_SCHEMA: u32 = 3;

/// allocation granule：object-start bitmap 每一位对应一个 granule。
pub(crate) const HEAP_GRANULE_BYTES: u32 = 16;
/// 一个 arena 内的 Immix block 数。
pub(crate) const HEAP_BLOCKS_PER_ARENA: u32 = 64;
/// 一个 block 内的 line 数。
pub(crate) const HEAP_LINES_PER_BLOCK: u32 = 256;
/// 一个 processor 本地 TLAB span 覆盖的 block 数。
pub(crate) const HEAP_TLAB_SPAN_BLOCKS: u32 = 8;
/// age 的饱和值与位宽上界。
pub(crate) const HEAP_AGE_MAX: u8 = 15;
/// 超过单个 block 或超过该对齐的请求走 non-moving slow path。
pub(crate) const HEAP_LARGE_ALIGN_LIMIT: u32 = 4096;
/// header 在 payload 之前的固定字节数。
pub(crate) const HEAP_OBJECT_HEADER_BYTES: u32 = 16;

/// arena 状态名；顺序即状态强度。
pub(crate) const HEAP_ARENA_STATE_NAMES: [&str; 7] = [
    "free",
    "nursery",
    "aging",
    "old",
    "resource",
    "pinned",
    "evacuating",
];
/// block 状态名；顺序即状态强度。
pub(crate) const HEAP_BLOCK_STATE_NAMES: [&str; 4] = ["active", "candidate", "reclaiming", "free"];
/// generation 名；顺序即 object header 的编码。
pub(crate) const HEAP_GENERATION_NAMES: [&str; 4] = ["nursery", "aging", "old", "immortal"];
/// managed representation 名；顺序即 object header 的编码。
pub(crate) const HEAP_REPRESENTATION_NAMES: [&str; 4] = [
    "local-direct",
    "turn-region",
    "shared-handle",
    "compressed-ref",
];
/// object header flag 名；顺序即登记顺序。
pub(crate) const HEAP_OBJECT_FLAG_NAMES: [&str; 5] = [
    "forwarded",
    "pinned",
    "release-queued",
    "large-object",
    "has-resource-instance",
];
/// minor cycle 阶段名；顺序即执行顺序。
pub(crate) const HEAP_MINOR_PHASE_NAMES: [&str; 6] = [
    "stop-mutator",
    "flush-remembered-set",
    "copy-nursery",
    "promote-aged",
    "rebuild-summary",
    "resume",
];
/// major cycle 阶段名；顺序即执行顺序。
pub(crate) const HEAP_MAJOR_PHASE_NAMES: [&str; 8] = [
    "snapshot-roots",
    "mark",
    "remark",
    "select-evacuation",
    "evacuate",
    "sweep",
    "rebuild-remembered-set",
    "resume",
];

/// 一条运行时记录的字段布局。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct HeapRecordField {
    pub name: String,
    pub offset: u32,
    pub bytes: u32,
}

/// 与 Gugu side metadata 同源；路由与完整 manager token 仅由世界的 arena 登记表持有。
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HeapBlockRecord {
    pub block_id: u32,
    pub generation: u32,
    pub arena_descriptor: u32,
    pub block_index: u32,
    pub manager_owner: u64,
    pub incoming_leases: u64,
    pub mutation_version: u64,
    pub allocator_leases: u32,
    pub scanner_leases: u32,
    pub evacuation_leases: u32,
    pub candidate_job: u32,
    pub state: u32,
    pub reserved: u32,
}

const _: () = assert!(std::mem::size_of::<HeapBlockRecord>() == 64);
/// 一条运行时记录的完整布局；由 `heap.gg` 逐字段交叉校验。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct HeapRecordLayout {
    pub name: String,
    pub bytes: u32,
    pub alignment: u32,
    pub fields: Vec<HeapRecordField>,
}

/// 编译期登记的 heap 记录布局表。
struct HeapRecordSpec {
    name: &'static str,
    bytes: u32,
    alignment: u32,
    fields: &'static [(&'static str, u32, u32)],
}

const fn field(name: &'static str, offset: u32, bytes: u32) -> (&'static str, u32, u32) {
    (name, offset, bytes)
}

/// heap header、arena、pin 与 block 的字段表。
const HEAP_RECORD_SPECS: [HeapRecordSpec; 4] = [
    HeapRecordSpec {
        name: "ObjectHeader",
        bytes: HEAP_OBJECT_HEADER_BYTES,
        alignment: 8,
        fields: &[
            field("control", 0, 8),
            field("payload_size_or_forward", 8, 8),
        ],
    },
    HeapRecordSpec {
        name: "HeapArenaMetadata",
        bytes: 55_576,
        alignment: 8,
        fields: &[
            field("state", 0, 4),
            field("generation", 4, 4),
            field("mark_epoch", 8, 8),
            field("alloc_cursor", 16, 4),
            field("pin_count", 20, 4),
            field("object_start", 24, 16_384),
            field("mark", 16_408, 16_384),
            field("page_cover", 32_792, 2_048),
            field("cards", 34_840, 4_096),
            field("block_live", 38_936, 256),
            field("line_live", 39_192, 16_384),
        ],
    },
    HeapRecordSpec {
        name: "HeapPinEntry",
        bytes: 16,
        alignment: 8,
        fields: &[
            field("arena", 0, 4),
            field("offset", 4, 4),
            field("generation", 8, 4),
            field("count", 12, 4),
        ],
    },
    HeapRecordSpec {
        name: "HeapBlockRecord",
        bytes: 64,
        alignment: 64,
        fields: &[
            field("block_id", 0, 4),
            field("generation", 4, 4),
            field("arena_descriptor", 8, 4),
            field("block_index", 12, 4),
            field("manager_owner", 16, 8),
            field("incoming_leases", 24, 8),
            field("mutation_version", 32, 8),
            field("allocator_leases", 40, 4),
            field("scanner_leases", 44, 4),
            field("evacuation_leases", 48, 4),
            field("candidate_job", 52, 4),
            field("state", 56, 4),
            field("reserved", 60, 4),
        ],
    },
];

/// nursery 触发与年龄参数；与 GC 增长预算解耦，独立版本化。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct HeapTriggerProfile {
    /// 本 profile 的修订号。
    pub revision: u32,
    /// nursery 自上次 minor 起的分配字节数达到该值时推进 minor。
    pub minor_trigger_bytes: u64,
    /// 对象提升到 old 的年龄。
    pub tenure_age: u8,
    /// 年龄饱和值。
    pub max_age: u8,
}

impl Default for HeapTriggerProfile {
    fn default() -> Self {
        Self {
            revision: 1,
            minor_trigger_bytes: 256 * 1024,
            tenure_age: 2,
            max_age: HEAP_AGE_MAX,
        }
    }
}

impl HeapTriggerProfile {
    /// 校验触发参数：触发量必须落在半个 arena 内，年龄阶梯必须单调且有界。
    pub(crate) fn verify(&self, arena_bytes: u64) -> Result<(), RawModelError> {
        if self.revision == 0 {
            return Err(RawModelError::new("heap trigger revision 必须非零"));
        }
        if self.minor_trigger_bytes == 0 || self.minor_trigger_bytes > arena_bytes / 2 {
            return Err(RawModelError::new(
                "nursery 触发字节数必须落在半个 arena 的 (0, 1/2] 内",
            ));
        }
        if self.tenure_age == 0 || self.tenure_age > self.max_age || self.max_age > HEAP_AGE_MAX {
            return Err(RawModelError::new(
                "age 阶梯必须满足 0 < tenure <= max <= 15",
            ));
        }
        Ok(())
    }
}

/// 从优化后 LIR 与冻结类型表推导的 LocalHeap 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LocalHeapDemand {
    /// LocalHeap placement 的分配站点数。
    pub alloc_sites: u32,
    /// Pinned placement 的分配站点数。
    pub pinned_sites: u32,
    /// Resource placement 的分配站点数。
    pub resource_sites: u32,
    /// SharedHeap placement 的分配站点数。
    pub shared_sites: u32,
    /// TurnRegion 提升站点数。
    pub promote_sites: u32,
    /// `RuntimeCall::Pin` 站点数。
    pub pin_sites: u32,
    /// `RuntimeCall::Unpin` 站点数。
    pub unpin_sites: u32,
    /// managed store 的屏障站点数。
    pub barrier_sites: u32,
    /// 冻结类型表中的 managed 类型数。
    pub managed_types: u32,
    /// footprint 超过单个 Immix block 的类型数。
    pub large_types: u32,
    /// 单个类型 layout 的最大 payload 字节数。
    pub max_object_bytes: u64,
}

impl LocalHeapDemand {
    /// 返回需求视图的稳定指纹。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&self.alloc_sites.to_le_bytes());
        bytes.extend_from_slice(&self.pinned_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_sites.to_le_bytes());
        bytes.extend_from_slice(&self.shared_sites.to_le_bytes());
        bytes.extend_from_slice(&self.promote_sites.to_le_bytes());
        bytes.extend_from_slice(&self.pin_sites.to_le_bytes());
        bytes.extend_from_slice(&self.unpin_sites.to_le_bytes());
        bytes.extend_from_slice(&self.barrier_sites.to_le_bytes());
        bytes.extend_from_slice(&self.managed_types.to_le_bytes());
        bytes.extend_from_slice(&self.large_types.to_le_bytes());
        bytes.extend_from_slice(&self.max_object_bytes.to_le_bytes());
        crate::frontend::mono::keys::hash_domain("gugu-local-heap-demand-v1", &bytes)
    }

    /// 校验需求自身的可证明关系；跨段关系由 `RuntimeRawContractV1::verify` 检查。
    pub(crate) fn verify(&self, block_bytes: u32) -> Result<(), RawModelError> {
        let placements = self
            .pinned_sites
            .checked_add(self.resource_sites)
            .and_then(|total| total.checked_add(self.shared_sites))
            .ok_or_else(|| RawModelError::new("placement 站点数溢出"))?;
        if placements > self.alloc_sites {
            return Err(RawModelError::new(
                "Pinned/Resource/SharedHeap placement 站点数不得超过 LocalHeap placement",
            ));
        }
        if self.large_types > self.managed_types {
            return Err(RawModelError::new("large 类型数不得超过 managed 类型数"));
        }
        let footprint_over_block = self
            .max_object_bytes
            .saturating_add(u64::from(HEAP_OBJECT_HEADER_BYTES))
            > u64::from(block_bytes);
        if (self.large_types != 0) != footprint_over_block {
            return Err(RawModelError::new(
                "large 类型数必须与最大对象 footprint 是否超过 block 一致",
            ));
        }
        Ok(())
    }
}

/// 已验证的 LocalHeap runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LocalHeapRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// arena 字节数。
    pub arena_bytes: u64,
    /// Immix block 字节数。
    pub block_bytes: u32,
    /// Immix line 字节数。
    pub line_bytes: u32,
    /// allocation granule 字节数。
    pub granule_bytes: u32,
    /// 一个 arena 的 block 数。
    pub blocks_per_arena: u32,
    /// 一个 block 的 line 数。
    pub lines_per_block: u32,
    /// 一个 TLAB span 的 block 数。
    pub tlab_span_blocks: u32,
    /// 一个 TLAB span 的字节数。
    pub tlab_span_bytes: u64,
    /// object-start bitmap 的位数。
    pub object_start_bits: u32,
    /// mark bitmap 的位数。
    pub mark_bits: u32,
    /// 两张 bitmap 各自的字节数。
    pub bitmap_bytes: u32,
    /// 宿主页字节数；`page_cover` 的粒度。
    pub page_bytes: u32,
    /// `page_covering_object` 的条目数。
    pub page_cover_entries: u32,
    /// 每个 arena 的 card 表字节数。
    pub card_bytes: u32,
    /// arena 状态目录。
    pub arena_states: Vec<String>,
    /// block 状态目录。
    pub block_states: Vec<String>,
    /// generation 目录。
    pub generations: Vec<String>,
    /// representation 目录。
    pub representations: Vec<String>,
    /// object header flag 目录。
    pub object_flags: Vec<String>,
    /// minor cycle 阶段目录。
    pub minor_phases: Vec<String>,
    /// major cycle 阶段目录。
    pub major_phases: Vec<String>,
    /// 运行时记录布局表。
    pub records: Vec<HeapRecordLayout>,
    /// nursery 触发与年龄参数。
    pub trigger: HeapTriggerProfile,
    /// 上游需求视图。
    pub demand: LocalHeapDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl LocalHeapRuntimeContract {
    /// 由需求视图与平台 profile 构建契约。
    pub(crate) fn build(
        demand: LocalHeapDemand,
        profile: PlatformProfile,
    ) -> Result<Self, RawModelError> {
        let page_bytes = u32::try_from(profile.constants().page_bytes)
            .map_err(|_| RawModelError::new("平台页大小超过 u32"))?;
        let contract = Self {
            schema: LOCAL_HEAP_SCHEMA,
            arena_bytes: GC_ARENA_BYTES,
            block_bytes: GC_BLOCK_BYTES,
            line_bytes: GC_LINE_BYTES,
            granule_bytes: HEAP_GRANULE_BYTES,
            blocks_per_arena: 0,
            lines_per_block: 0,
            tlab_span_blocks: HEAP_TLAB_SPAN_BLOCKS,
            tlab_span_bytes: 0,
            object_start_bits: 0,
            mark_bits: 0,
            bitmap_bytes: 0,
            page_bytes,
            page_cover_entries: 0,
            card_bytes: 0,
            arena_states: names(&HEAP_ARENA_STATE_NAMES),
            block_states: names(&HEAP_BLOCK_STATE_NAMES),
            generations: names(&HEAP_GENERATION_NAMES),
            representations: names(&HEAP_REPRESENTATION_NAMES),
            object_flags: names(&HEAP_OBJECT_FLAG_NAMES),
            minor_phases: names(&HEAP_MINOR_PHASE_NAMES),
            major_phases: names(&HEAP_MAJOR_PHASE_NAMES),
            records: records(),
            trigger: HeapTriggerProfile::default(),
            demand,
            fingerprint: [0; 32],
        };
        let mut contract = contract.derive_sizes()?;
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 由 arena/block/line 参数推导派生规模，保证同一组参数只有一处定义。
    fn derive_sizes(mut self) -> Result<Self, RawModelError> {
        if self.arena_bytes == 0
            || self.block_bytes == 0
            || self.line_bytes == 0
            || self.granule_bytes == 0
            || self.page_bytes == 0
            || !self.block_bytes.is_multiple_of(self.line_bytes)
            || !self.arena_bytes.is_multiple_of(u64::from(self.block_bytes))
            || !self
                .arena_bytes
                .is_multiple_of(u64::from(self.granule_bytes))
            || !self.arena_bytes.is_multiple_of(u64::from(self.page_bytes))
            || !self
                .arena_bytes
                .is_multiple_of(u64::from(CARD_GRANULARITY_BYTES))
        {
            return Err(RawModelError::new(
                "arena/block/line/granule 参数不互相整除",
            ));
        }
        self.blocks_per_arena = u32::try_from(self.arena_bytes / u64::from(self.block_bytes))
            .map_err(|_| RawModelError::new("block 数超过 u32"))?;
        self.lines_per_block = self.block_bytes / self.line_bytes;
        self.tlab_span_bytes = u64::from(self.tlab_span_blocks)
            .checked_mul(u64::from(self.block_bytes))
            .ok_or_else(|| RawModelError::new("TLAB span 字节数溢出"))?;
        self.object_start_bits = u32::try_from(self.arena_bytes / u64::from(self.granule_bytes))
            .map_err(|_| RawModelError::new("object-start 位数超过 u32"))?;
        self.mark_bits = self.object_start_bits;
        self.bitmap_bytes = self.object_start_bits.div_ceil(8);
        self.page_cover_entries = u32::try_from(self.arena_bytes / u64::from(self.page_bytes))
            .map_err(|_| RawModelError::new("page-cover 条目数超过 u32"))?;
        self.card_bytes = u32::try_from(self.arena_bytes / u64::from(CARD_GRANULARITY_BYTES))
            .map_err(|_| RawModelError::new("card 表字节数超过 u32"))?;
        Ok(self)
    }

    /// 返回 schema 版本。
    pub(crate) const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 arena 字节数。
    pub(crate) const fn arena_bytes(&self) -> u64 {
        self.arena_bytes
    }

    /// 返回 block 字节数。
    pub(crate) const fn block_bytes(&self) -> u32 {
        self.block_bytes
    }

    /// 返回 line 字节数。
    pub(crate) const fn line_bytes(&self) -> u32 {
        self.line_bytes
    }

    /// 返回一个 TLAB span 的字节数。
    pub(crate) const fn tlab_span_bytes(&self) -> u64 {
        self.tlab_span_bytes
    }

    /// 返回 nursery 触发与年龄参数。
    pub(crate) const fn trigger(&self) -> HeapTriggerProfile {
        self.trigger
    }

    /// 返回需求视图。
    pub(crate) const fn demand(&self) -> LocalHeapDemand {
        self.demand
    }

    /// 返回每条记录布局的字段数总和。
    pub(crate) fn record_field_count(&self) -> u32 {
        self.records
            .iter()
            .map(|record| record.fields.len() as u32)
            .sum()
    }

    /// 校验常量推导、目录、记录布局与需求关系。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != LOCAL_HEAP_SCHEMA {
            return Err(RawModelError::new("LocalHeap 契约 schema 不匹配"));
        }
        if self.arena_bytes != GC_ARENA_BYTES
            || self.block_bytes != GC_BLOCK_BYTES
            || self.line_bytes != GC_LINE_BYTES
            || self.granule_bytes != HEAP_GRANULE_BYTES
        {
            return Err(RawModelError::new(
                "LocalHeap arena/block/line/granule 与登记常量不一致",
            ));
        }
        let mut expected = self.clone();
        expected.fingerprint = self.fingerprint;
        let expected = expected.derive_sizes()?;
        if self.blocks_per_arena != expected.blocks_per_arena
            || self.lines_per_block != expected.lines_per_block
            || self.tlab_span_bytes != expected.tlab_span_bytes
            || self.object_start_bits != expected.object_start_bits
            || self.mark_bits != expected.mark_bits
            || self.bitmap_bytes != expected.bitmap_bytes
            || self.page_cover_entries != expected.page_cover_entries
            || self.card_bytes != expected.card_bytes
        {
            return Err(RawModelError::new(
                "LocalHeap 派生规模与 arena/block/line 参数不一致",
            ));
        }
        if self.tlab_span_blocks == 0
            || self.tlab_span_blocks > self.blocks_per_arena
            || !self.blocks_per_arena.is_multiple_of(self.tlab_span_blocks)
        {
            return Err(RawModelError::new(
                "TLAB span 必须是 arena block 数的整除因子",
            ));
        }
        if self.object_start_bits % 8 != 0 || self.bitmap_bytes * 8 != self.object_start_bits {
            return Err(RawModelError::new("bitmap 位数必须是整字节"));
        }
        if self.card_bytes % self.blocks_per_arena != 0 {
            return Err(RawModelError::new(
                "arena card 表必须能按 block 均分给全部 Immix block",
            ));
        }
        for (observed, expected) in [
            (&self.arena_states, HEAP_ARENA_STATE_NAMES.as_slice()),
            (&self.block_states, HEAP_BLOCK_STATE_NAMES.as_slice()),
            (&self.generations, HEAP_GENERATION_NAMES.as_slice()),
            (&self.representations, HEAP_REPRESENTATION_NAMES.as_slice()),
            (&self.object_flags, HEAP_OBJECT_FLAG_NAMES.as_slice()),
            (&self.minor_phases, HEAP_MINOR_PHASE_NAMES.as_slice()),
            (&self.major_phases, HEAP_MAJOR_PHASE_NAMES.as_slice()),
        ] {
            if observed.len() != expected.len()
                || observed
                    .iter()
                    .zip(expected.iter())
                    .any(|(name, expected)| name != expected)
            {
                return Err(RawModelError::new("LocalHeap 目录与登记表不一致"));
            }
        }
        verify_records(&self.records)?;
        self.trigger.verify(self.arena_bytes)?;
        self.demand.verify(self.block_bytes)?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("LocalHeap 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.arena_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.block_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.line_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.granule_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.blocks_per_arena.to_le_bytes());
        bytes.extend_from_slice(&self.lines_per_block.to_le_bytes());
        bytes.extend_from_slice(&self.tlab_span_blocks.to_le_bytes());
        bytes.extend_from_slice(&self.tlab_span_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.object_start_bits.to_le_bytes());
        bytes.extend_from_slice(&self.mark_bits.to_le_bytes());
        bytes.extend_from_slice(&self.bitmap_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.page_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.page_cover_entries.to_le_bytes());
        bytes.extend_from_slice(&self.card_bytes.to_le_bytes());
        for directory in [
            &self.arena_states,
            &self.block_states,
            &self.generations,
            &self.representations,
            &self.object_flags,
            &self.minor_phases,
            &self.major_phases,
        ] {
            for name in directory {
                bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
                bytes.extend_from_slice(name.as_bytes());
            }
        }
        for record in &self.records {
            bytes.extend_from_slice(&(record.name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(record.name.as_bytes());
            bytes.extend_from_slice(&record.bytes.to_le_bytes());
            bytes.extend_from_slice(&record.alignment.to_le_bytes());
            bytes.extend_from_slice(&(record.fields.len() as u32).to_le_bytes());
            for field in &record.fields {
                bytes.extend_from_slice(&(field.name.len() as u32).to_le_bytes());
                bytes.extend_from_slice(field.name.as_bytes());
                bytes.extend_from_slice(&field.offset.to_le_bytes());
                bytes.extend_from_slice(&field.bytes.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.trigger.revision.to_le_bytes());
        bytes.extend_from_slice(&self.trigger.minor_trigger_bytes.to_le_bytes());
        bytes.push(self.trigger.tenure_age);
        bytes.push(self.trigger.max_age);
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes
    }

    /// 计算契约指纹。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        crate::frontend::mono::keys::hash_domain(
            "gugu-local-heap-contract-v1",
            &self.canonical_bytes(),
        )
    }

    /// 返回人类可读的契约 dump。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "local-heap schema={} arena={} block={} line={} granule={} tlab-span={} blocks={} lines-per-block={}",
            self.schema,
            self.arena_bytes,
            self.block_bytes,
            self.line_bytes,
            self.granule_bytes,
            self.tlab_span_bytes,
            self.blocks_per_arena,
            self.lines_per_block
        );
        let _ = writeln!(
            out,
            "local-heap-bitmaps object-start-bits={} mark-bits={} bitmap-bytes={} page={} page-cover={} cards={}",
            self.object_start_bits,
            self.mark_bits,
            self.bitmap_bytes,
            self.page_bytes,
            self.page_cover_entries,
            self.card_bytes
        );
        for (label, directory) in [
            ("arena-states", &self.arena_states),
            ("block-states", &self.block_states),
            ("generations", &self.generations),
            ("representations", &self.representations),
            ("object-flags", &self.object_flags),
            ("minor-phases", &self.minor_phases),
            ("major-phases", &self.major_phases),
        ] {
            let _ = writeln!(out, "local-heap-{label} {}", directory.join(","));
        }
        for record in &self.records {
            let _ = writeln!(
                out,
                "local-heap-record {} bytes={} align={} fields={}",
                record.name,
                record.bytes,
                record.alignment,
                record
                    .fields
                    .iter()
                    .map(|field| format!("{}@{}", field.name, field.offset))
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        let _ = writeln!(
            out,
            "local-heap-trigger revision={} minor-trigger-bytes={} tenure-age={} max-age={}",
            self.trigger.revision,
            self.trigger.minor_trigger_bytes,
            self.trigger.tenure_age,
            self.trigger.max_age
        );
        let _ = writeln!(
            out,
            "local-heap-demand alloc={} pinned={} resource={} shared={} promote={} pin={} unpin={} barrier={} managed-types={} large-types={} max-object-bytes={}",
            self.demand.alloc_sites,
            self.demand.pinned_sites,
            self.demand.resource_sites,
            self.demand.shared_sites,
            self.demand.promote_sites,
            self.demand.pin_sites,
            self.demand.unpin_sites,
            self.demand.barrier_sites,
            self.demand.managed_types,
            self.demand.large_types,
            self.demand.max_object_bytes
        );
        let _ = writeln!(
            out,
            "local-heap-fingerprint {}",
            hex_lower(self.fingerprint)
        );
        out
    }
}

fn names(directory: &[&str]) -> Vec<String> {
    directory.iter().map(|name| (*name).to_owned()).collect()
}

fn records() -> Vec<HeapRecordLayout> {
    HEAP_RECORD_SPECS
        .iter()
        .map(|record| HeapRecordLayout {
            name: record.name.to_owned(),
            bytes: record.bytes,
            alignment: record.alignment,
            fields: record
                .fields
                .iter()
                .map(|(name, offset, bytes)| HeapRecordField {
                    name: (*name).to_owned(),
                    offset: *offset,
                    bytes: *bytes,
                })
                .collect(),
        })
        .collect()
}

/// 校验记录布局：字段按偏移递增、不重叠、落在记录内，且记录尺寸与对齐自洽。
fn verify_records(records: &[HeapRecordLayout]) -> Result<(), RawModelError> {
    if records.len() != HEAP_RECORD_SPECS.len() {
        return Err(RawModelError::new("LocalHeap 记录数量与登记表不一致"));
    }
    for record in records {
        let spec = HEAP_RECORD_SPECS
            .iter()
            .find(|spec| spec.name == record.name)
            .ok_or_else(|| RawModelError::new("LocalHeap 记录名未登记"))?;
        if record.bytes != spec.bytes || record.alignment != spec.alignment {
            return Err(RawModelError::new("LocalHeap 记录尺寸或对齐与登记不一致"));
        }
        if record.alignment == 0 || !record.alignment.is_power_of_two() {
            return Err(RawModelError::new("LocalHeap 记录对齐必须是非零二次幂"));
        }
        if !record.bytes.is_multiple_of(record.alignment) {
            return Err(RawModelError::new("LocalHeap 记录尺寸未按对齐取整"));
        }
        if record.fields.len() != spec.fields.len() {
            return Err(RawModelError::new("LocalHeap 记录字段数量与登记不一致"));
        }
        let mut end = 0u32;
        for field in &record.fields {
            let expected = spec
                .fields
                .iter()
                .find(|(name, _, _)| *name == field.name)
                .ok_or_else(|| RawModelError::new("LocalHeap 记录字段未登记"))?;
            if expected.1 != field.offset || expected.2 != field.bytes {
                return Err(RawModelError::new(
                    "LocalHeap 记录字段偏移或尺寸与登记不一致",
                ));
            }
            if field.bytes == 0 || field.offset < end {
                return Err(RawModelError::new("LocalHeap 记录字段重叠或长度为 0"));
            }
            end = field
                .offset
                .checked_add(field.bytes)
                .ok_or_else(|| RawModelError::new("LocalHeap 记录字段范围溢出"))?;
            if end > record.bytes {
                return Err(RawModelError::new("LocalHeap 记录字段越界"));
            }
        }
    }
    Ok(())
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}
