//! hybrid write barrier、remembered set 与 card-mark 传输契约；backend 和 CLI 只消费已验证对象。
//!
//! 本段把 Yuasa deletion + Dijkstra insertion 的六步序列、processor-local `CardMarkBuffer`
//! 与 dedup stamp 表的布局、六个 flush 原因、`CardMarkBatch` 的消息字段、arena card table
//! 粒度与 minor stop 的 drain 门禁固定成带版本的对象，与 `gc_metadata_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。真实 field 地址、managed
//! pointer 与 arena 基址都不进入本段：barrier 账本只保存稳定 descriptor、generation、
//! card 序号与 cycle epoch。

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::mem::{align_of, offset_of, size_of};

use super::gc_metadata_contract::GC_ARENA_BYTES;
use super::model::{FieldKind, MessageFieldSchema, MessageSchemaV1, RawModelError};
/// barrier 契约段的 schema 版本。
pub(crate) const BARRIER_SCHEMA: u32 = 1;

/// card table 的粒度：每 512 heap 字节一个 dirty byte。
pub(crate) const CARD_GRANULARITY_BYTES: u32 = 512;
/// card 粒度的以 2 为底指数；地址到 card 序号只做一次右移。
pub(crate) const CARD_GRANULARITY_SHIFT: u32 = 9;
/// 每个 `LogicalProcessor` 的 remembered-set buffer 项数。
pub(crate) const CARD_MARK_BUFFER_ENTRIES: u32 = 256;
/// dedup 直接映射 stamp 表的项数；与 buffer 项数同值。
pub(crate) const CARD_MARK_STAMP_ENTRIES: u32 = 256;
/// 一项 `CardMarkEntry` 的规范字节数。
pub(crate) const CARD_MARK_ENTRY_BYTES: u32 = 32;
/// 一项 `CardMarkStamp` 的规范字节数。
pub(crate) const CARD_MARK_STAMP_BYTES: u32 = 32;
/// 一条 hybrid barrier 写入消费的 shade slot 上界：deletion + insertion。
pub(crate) const SHADE_SLOTS_PER_WRITE: u32 = 2;
/// 一条 hybrid barrier 写入消费的 card-mark slot 上界。
pub(crate) const CARD_MARKS_PER_WRITE: u32 = 1;
/// stamp 直接映射的混合函数 revision；参与契约指纹。
pub(crate) const CARD_MARK_STAMP_MIX_REVISION: u32 = 1;
/// `CardMarkBatch` 的规范字段数。
pub(crate) const CARD_MARK_BATCH_FIELDS: u32 = 13;

/// hybrid barrier 的规范步骤名（顺序即执行顺序）。
///
/// 第 4 步是实际 field store，第 5 步才把 card 键写进 barrier 账本，因此
/// 「实际 field store 先于 barrier 账本发布」是契约里的位置不变量。
pub(crate) const HYBRID_BARRIER_STEPS: [&str; 6] = [
    "read-old",
    "shade-old-deleted",
    "shade-new-inserted",
    "store",
    "card-mark",
    "edge-summary",
];

/// barrier 账本 flush 的六个原因名。
pub(crate) const CARD_MARK_FLUSH_REASONS: [&str; 6] = [
    "buffer-full",
    "processor-handoff",
    "foreign-bridge",
    "memory-pressure",
    "minor-stop",
    "producer-stop-gate",
];

/// barrier 种类名：两种 shade 语义、direct field barrier 与两种 edge summary。
pub(crate) const BARRIER_KIND_NAMES: [&str; 5] = [
    "yuasa-deletion",
    "dijkstra-insertion",
    "direct-field",
    "edge-add",
    "edge-drop",
];

/// 一个 processor-local `CardMarkEntry`：只保存稳定键，不保存 field 地址。
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CardMarkEntry {
    /// arena descriptor 的稠密编号。
    pub arena_descriptor: u64,
    /// arena 的 generation；回收后旧键必须被拒绝。
    pub arena_generation: u32,
    /// arena 内的 card 序号。
    pub card_index: u32,
    /// 产生该键的 GC cycle epoch。
    pub cycle_epoch: u64,
    /// 该键在 flush 前的累计置位次数；只用于诊断与幂等性核对。
    pub marks: u64,
}

/// 一个 dedup stamp 项：直接映射到某个 entry 槽。
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CardMarkStamp {
    /// stamp 键的低 32 位；0 表示空槽。
    pub key_low: u64,
    /// stamp 指向的 entry 槽编号。
    pub entry: u32,
    /// 该 stamp 当时的 arena generation。
    pub arena_generation: u32,
    /// 该 stamp 当时的 cycle epoch。
    pub cycle_epoch: u64,
    /// 保留位，必须为 0。
    pub reserved: u64,
}

/// processor-local remembered-set buffer 的固定上界。
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CardMarkBufferHead {
    /// 当前有效项数；恒不超过 `CARD_MARK_BUFFER_ENTRIES`。
    pub len: u32,
    /// 当前 cycle epoch。
    pub cycle_epoch: u32,
    /// 未发布的 pending bytes。
    pub pending_bytes: u64,
    /// 该 buffer 的 arena allocation owner 槽位；非 owner 时为其 stably 编号。
    pub owner_slot: u64,
    /// 保留位，必须为 0。
    pub reserved: u64,
    /// 对齐填充，使头部独占一条 cache line。
    pub padding: [u8; 32],
}

/// arena 的 card table 描述符；每 `CARD_GRANULARITY_BYTES` 一个 dirty byte。
#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CardTableDescriptor {
    /// arena descriptor 的稠密编号。
    pub arena_descriptor: u64,
    /// arena 的 generation。
    pub arena_generation: u32,
    /// arena 内的 card 总数。
    pub card_count: u32,
    /// 当前被置位的 card 数。
    pub dirty: u32,
    /// 是否已有 minor stop 请求等待 drain。
    pub minor_pending: u32,
}

/// 从优化后 LIR 推导的 barrier 需求，不是运行时站点数量。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BarrierDemand {
    /// region 内已预留的 hybrid barrier 数。
    pub reserved_barriers: u32,
    /// 仍以裸 `GcWriteBarrier` 存在的 barrier 数；结构 verifier 模式下才可能非零。
    pub bare_barriers: u32,
    /// `NoSafepointRegion` 数量。
    pub regions: u32,
    /// barrier permit 数量。
    pub permits: u32,
    /// `max_shades` 的最大值。
    pub max_shades_permit: u32,
    /// `max_card_marks` 的最大值。
    pub max_card_marks_permit: u32,
    /// permit 覆盖的 shade 额度总和。
    pub shade_slots: u32,
    /// permit 覆盖的 card-mark 额度总和。
    pub card_mark_slots: u32,
    /// 可能触发 edge summary 的 managed store 站点数。
    pub edge_summary_sites: u32,
    /// 可能触发 barrier 账本记账的写入站点总数。
    pub card_mark_sites: u32,
}

impl BarrierDemand {
    /// 全部 barrier 站点数。
    pub const fn barrier_sites(self) -> u32 {
        self.reserved_barriers + self.bare_barriers
    }

    /// 需求视图的规范指纹；字段顺序即序列化顺序，避免两处各自序列化而产生漂移。
    pub(crate) fn fingerprint(self) -> [u8; 32] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.reserved_barriers.to_le_bytes());
        bytes.extend_from_slice(&self.bare_barriers.to_le_bytes());
        bytes.extend_from_slice(&self.regions.to_le_bytes());
        bytes.extend_from_slice(&self.permits.to_le_bytes());
        bytes.extend_from_slice(&self.max_shades_permit.to_le_bytes());
        bytes.extend_from_slice(&self.max_card_marks_permit.to_le_bytes());
        bytes.extend_from_slice(&self.shade_slots.to_le_bytes());
        bytes.extend_from_slice(&self.card_mark_slots.to_le_bytes());
        bytes.extend_from_slice(&self.edge_summary_sites.to_le_bytes());
        bytes.extend_from_slice(&self.card_mark_sites.to_le_bytes());
        *blake3::Hasher::new_derive_key("gugu-barrier-demand-v1")
            .update(&bytes)
            .finalize()
            .as_bytes()
    }
}

/// barrier 账本的分类占用；与阶段 32 的 owner 互斥账本分开报告。
///
/// `pending_batch_bytes` 是已经进入 staging 但尚未被 arena owner 消费的 card batch 字节；
/// pressure episode 的消费与 forced full cycle 由后续阶段接入同一分类。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BarrierPressureV1 {
    /// 每个 active processor 的 buffer 常驻字节（含头部）。
    pub buffer_bytes: u64,
    /// 每个 active processor 的 dedup stamp 常驻字节。
    pub stamp_bytes: u64,
    /// 每个 arena 的 card table 常驻字节。
    pub per_arena_table_bytes: u64,
    /// 每一个 processor 的 barrier 元数据总量。
    pub per_processor_bytes: u64,
}

impl BarrierPressureV1 {
    /// active processor 的 barrier 元数据总量。
    pub const fn processor_total(self, processors: u64) -> u64 {
        self.per_processor_bytes * processors
    }

    /// active arena 的 remembered-set 元数据总量。
    pub const fn arena_total(self, arenas: u64) -> u64 {
        self.per_arena_table_bytes * arenas
    }
}

/// 一个 barrier record 字段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BarrierFieldLayout {
    /// 字段名。
    pub name: String,
    /// 字段字节偏移。
    pub offset: u32,
}

/// 一个 barrier record 的固定布局。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BarrierRecordLayout {
    /// record 名。
    pub name: String,
    /// record 字节数。
    pub bytes: u32,
    /// record 对齐。
    pub alignment: u32,
    /// 声明顺序的字段偏移。
    pub fields: Vec<BarrierFieldLayout>,
}

/// 已验证的 barrier runtime 契约；版本变化使 RuntimeRawModel 和 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BarrierRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// card table 粒度（字节）。
    pub card_granularity_bytes: u32,
    /// 地址到 card 序号的右移量。
    pub card_granularity_shift: u32,
    /// buffer 项数。
    pub card_mark_buffer_entries: u32,
    /// dedup stamp 项数。
    pub card_mark_stamp_entries: u32,
    /// 单项 card entry 字节数。
    pub card_mark_entry_bytes: u32,
    /// 单项 stamp 字节数。
    pub card_mark_stamp_bytes: u32,
    /// 每条写入的 shade slot 上界。
    pub shade_slots_per_write: u32,
    /// 每条写入的 card-mark slot 上界。
    pub card_marks_per_write: u32,
    /// stamp 混合函数 revision。
    pub stamp_mix_revision: u32,
    /// 六步 hybrid barrier 的规范顺序。
    pub hybrid_steps: Vec<String>,
    /// 六个 flush 原因。
    pub flush_reasons: Vec<String>,
    /// barrier 与 edge summary 种类。
    pub barrier_kinds: Vec<String>,
    /// `CardMarkBatch` 的字段集合；禁止携带地址。
    pub(crate) message: MessageSchemaV1,
    /// 固定 record 目录。
    pub records: Vec<BarrierRecordLayout>,
    /// 分类占用。
    pub pressure: BarrierPressureV1,
    /// 上游 LIR 需求。
    pub demand: BarrierDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl BarrierRuntimeContract {
    /// 返回内部 schema 版本。
    pub fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 card 粒度。
    pub fn card_granularity_bytes(&self) -> u32 {
        self.card_granularity_bytes
    }

    /// 返回 buffer 项数。
    pub fn card_mark_buffer_entries(&self) -> u32 {
        self.card_mark_buffer_entries
    }

    /// 返回 dedup stamp 项数。
    pub fn card_mark_stamp_entries(&self) -> u32 {
        self.card_mark_stamp_entries
    }

    /// 返回 flush 原因数量。
    pub fn flush_reason_count(&self) -> u32 {
        u32::try_from(self.flush_reasons.len()).expect("flush 原因数量适配 u32")
    }

    /// 返回 `CardMarkBatch` 字段数。
    pub fn card_mark_batch_field_count(&self) -> u32 {
        u32::try_from(self.message.fields.len()).expect("字段数量适配 u32")
    }

    /// 返回 record 数量。
    pub fn record_count(&self) -> u32 {
        u32::try_from(self.records.len()).expect("record 数量适配 u32")
    }

    /// 返回上游 LIR 需求。
    pub fn demand(&self) -> BarrierDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 由上游 LIR 需求构建契约；常驻占用按单位尺寸登记，不固定 processor/arena 数量。
    pub(crate) fn build(demand: BarrierDemand) -> Result<Self, RawModelError> {
        let buffer_bytes = u64::from(CARD_MARK_ENTRY_BYTES) * u64::from(CARD_MARK_BUFFER_ENTRIES)
            + u64::try_from(size_of::<CardMarkBufferHead>()).expect("head 适配 u64");
        let stamp_bytes = u64::from(CARD_MARK_STAMP_BYTES) * u64::from(CARD_MARK_STAMP_ENTRIES);
        let mut contract = Self {
            schema: BARRIER_SCHEMA,
            card_granularity_bytes: CARD_GRANULARITY_BYTES,
            card_granularity_shift: CARD_GRANULARITY_SHIFT,
            card_mark_buffer_entries: CARD_MARK_BUFFER_ENTRIES,
            card_mark_stamp_entries: CARD_MARK_STAMP_ENTRIES,
            card_mark_entry_bytes: CARD_MARK_ENTRY_BYTES,
            card_mark_stamp_bytes: CARD_MARK_STAMP_BYTES,
            shade_slots_per_write: SHADE_SLOTS_PER_WRITE,
            card_marks_per_write: CARD_MARKS_PER_WRITE,
            stamp_mix_revision: CARD_MARK_STAMP_MIX_REVISION,
            hybrid_steps: HYBRID_BARRIER_STEPS
                .iter()
                .map(|step| (*step).to_owned())
                .collect(),
            flush_reasons: CARD_MARK_FLUSH_REASONS
                .iter()
                .map(|reason| (*reason).to_owned())
                .collect(),
            barrier_kinds: BARRIER_KIND_NAMES
                .iter()
                .map(|kind| (*kind).to_owned())
                .collect(),
            message: MessageSchemaV1::card_mark(),
            records: fixed_layouts(),
            pressure: BarrierPressureV1 {
                buffer_bytes,
                stamp_bytes,
                per_processor_bytes: buffer_bytes + stamp_bytes,
                per_arena_table_bytes: GC_ARENA_CARDS,
            },
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 校验契约：常量、目录、消息字段、record 布局、占用与指纹。
    ///
    /// 检查被拆成"协议常量"、"步骤与目录"、"record 与消息字段"和"分类占用"四组，任何
    /// 一组失败都指向一个独立的漂移来源，便于按 `RawModelError` 文本定位。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        self.verify_constants()?;
        self.verify_catalogs()?;
        self.verify_layouts()?;
        self.verify_pressure()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("barrier 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 校验粒度、容量、slot 上界与 stamp mix revision。
    fn verify_constants(&self) -> Result<(), RawModelError> {
        if self.schema != BARRIER_SCHEMA
            || self.card_granularity_bytes != CARD_GRANULARITY_BYTES
            || self.card_granularity_shift != CARD_GRANULARITY_SHIFT
            || self.card_mark_buffer_entries != CARD_MARK_BUFFER_ENTRIES
            || self.card_mark_stamp_entries != CARD_MARK_STAMP_ENTRIES
            || self.card_mark_entry_bytes != CARD_MARK_ENTRY_BYTES
            || self.card_mark_stamp_bytes != CARD_MARK_STAMP_BYTES
            || self.shade_slots_per_write != SHADE_SLOTS_PER_WRITE
            || self.card_marks_per_write != CARD_MARKS_PER_WRITE
            || self.stamp_mix_revision != CARD_MARK_STAMP_MIX_REVISION
        {
            return Err(RawModelError::new(
                "barrier 粒度、buffer/stamp 容量或 slot 上界与登记值不一致",
            ));
        }
        if self.card_granularity_bytes != 1 << self.card_granularity_shift {
            return Err(RawModelError::new("card 粒度与右移量不一致"));
        }
        if self.card_mark_buffer_entries != self.card_mark_stamp_entries {
            return Err(RawModelError::new(
                "dedup stamp 表必须与 buffer 同容量以保持直接映射不变量",
            ));
        }
        Ok(())
    }

    /// 校验六步序列、flush 原因与 barrier 种类目录。
    fn verify_catalogs(&self) -> Result<(), RawModelError> {
        if self.hybrid_steps != HYBRID_BARRIER_STEPS {
            return Err(RawModelError::new("hybrid barrier 六步序列与规范不一致"));
        }
        let index_of = |name: &str| {
            self.hybrid_steps
                .iter()
                .position(|step| step == name)
                .ok_or_else(|| RawModelError::new("hybrid barrier 缺少登记步骤"))
        };
        if index_of("store")? >= index_of("card-mark")? {
            return Err(RawModelError::new(
                "实际 field store 必须先于 barrier 账本发布",
            ));
        }
        if self.flush_reasons != CARD_MARK_FLUSH_REASONS {
            return Err(RawModelError::new("barrier flush 原因目录与规范不一致"));
        }
        if self.barrier_kinds != BARRIER_KIND_NAMES {
            return Err(RawModelError::new("barrier 种类目录与规范不一致"));
        }
        Ok(())
    }

    /// 校验 record 布局、`CardMarkBatch` 字段集合与尺寸常量。
    fn verify_layouts(&self) -> Result<(), RawModelError> {
        if u32::try_from(self.message.fields.len()).expect("字段数量适配 u32")
            != CARD_MARK_BATCH_FIELDS
        {
            return Err(RawModelError::new("CardMarkBatch 字段数与登记值不一致"));
        }
        self.message
            .verify_family(MessageFamilyTag::CardMark)
            .map_err(|error| RawModelError::new(error.message().to_owned()))?;
        if self.records != fixed_layouts() {
            return Err(RawModelError::new(
                "barrier record 布局与 machine 布局不一致",
            ));
        }
        if u32::try_from(size_of::<CardMarkEntry>()).expect("entry 适配 u32")
            != CARD_MARK_ENTRY_BYTES
            || u32::try_from(size_of::<CardMarkStamp>()).expect("stamp 适配 u32")
                != CARD_MARK_STAMP_BYTES
        {
            return Err(RawModelError::new("card entry/stamp 字节数与契约不一致"));
        }
        Ok(())
    }

    /// 校验 processor 与 arena 的分类占用口径。
    fn verify_pressure(&self) -> Result<(), RawModelError> {
        let expected_buffer = u64::from(self.card_mark_entry_bytes)
            * u64::from(self.card_mark_buffer_entries)
            + u64::try_from(size_of::<CardMarkBufferHead>()).expect("head 适配 u64");
        let expected_stamp =
            u64::from(self.card_mark_stamp_bytes) * u64::from(self.card_mark_stamp_entries);
        if self.pressure.buffer_bytes != expected_buffer
            || self.pressure.stamp_bytes != expected_stamp
            || self.pressure.per_processor_bytes != expected_buffer + expected_stamp
        {
            return Err(RawModelError::new("barrier 分类占用与布局不一致"));
        }
        if self.pressure.per_arena_table_bytes != GC_ARENA_CARDS {
            return Err(RawModelError::new("card table 单位占用与 arena 布局不一致"));
        }
        if self.demand.card_mark_sites != 0 && self.pressure.per_processor_bytes == 0 {
            return Err(RawModelError::new(
                "存在 card-mark 站点但 barrier 单位占用为零",
            ));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.card_granularity_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.card_granularity_shift.to_le_bytes());
        bytes.extend_from_slice(&self.card_mark_buffer_entries.to_le_bytes());
        bytes.extend_from_slice(&self.card_mark_stamp_entries.to_le_bytes());
        bytes.extend_from_slice(&self.card_mark_entry_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.card_mark_stamp_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.shade_slots_per_write.to_le_bytes());
        bytes.extend_from_slice(&self.card_marks_per_write.to_le_bytes());
        bytes.extend_from_slice(&self.stamp_mix_revision.to_le_bytes());
        push_names(&mut bytes, &self.hybrid_steps);
        push_names(&mut bytes, &self.flush_reasons);
        push_names(&mut bytes, &self.barrier_kinds);
        // 消息身份只有一处定义：族判别值与字段集合都由 `MessageSchemaV1` 负责编码，
        // 避免同一份字段目录在契约与消息侧各自序列化而产生漂移。
        bytes.extend_from_slice(&self.message.canonical_bytes());
        for record in &self.records {
            push_names(&mut bytes, std::slice::from_ref(&record.name));
            bytes.extend_from_slice(&record.bytes.to_le_bytes());
            bytes.extend_from_slice(&record.alignment.to_le_bytes());
            for field in &record.fields {
                push_names(&mut bytes, std::slice::from_ref(&field.name));
                bytes.extend_from_slice(&field.offset.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.pressure.buffer_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.pressure.stamp_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.pressure.per_arena_table_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.pressure.per_processor_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes
    }

    /// barrier 契约的域隔离内容身份。
    pub fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-barrier-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "barrier schema={} card={} buffer={} stamps={} shades-per-write={} cards-per-write={} mix={}",
            self.schema,
            self.card_granularity_bytes,
            self.card_mark_buffer_entries,
            self.card_mark_stamp_entries,
            self.shade_slots_per_write,
            self.card_marks_per_write,
            self.stamp_mix_revision,
        )
        .expect("String写入");
        writeln!(output, "barrier-steps {}", self.hybrid_steps.join(" -> ")).expect("String写入");
        writeln!(
            output,
            "barrier-flush-reasons {}",
            self.flush_reasons.join(",")
        )
        .expect("String写入");
        writeln!(output, "barrier-kinds {}", self.barrier_kinds.join(",")).expect("String写入");
        for record in &self.records {
            writeln!(
                output,
                "barrier-record {} bytes={} align={}",
                record.name, record.bytes, record.alignment
            )
            .expect("String写入");
            for field in &record.fields {
                writeln!(
                    output,
                    "barrier-field {}.{} offset={}",
                    record.name, field.name, field.offset
                )
                .expect("String写入");
            }
        }
        writeln!(
            output,
            "barrier-demand reserved={} bare={} regions={} permits={} shades={} cards={} edge-sites={} sites={}",
            self.demand.reserved_barriers,
            self.demand.bare_barriers,
            self.demand.regions,
            self.demand.permits,
            self.demand.shade_slots,
            self.demand.card_mark_slots,
            self.demand.edge_summary_sites,
            self.demand.card_mark_sites,
        )
        .expect("String写入");
        writeln!(
            output,
            "barrier-pressure buffer={} stamp={} per-processor={} per-arena-table={}",
            self.pressure.buffer_bytes,
            self.pressure.stamp_bytes,
            self.pressure.per_processor_bytes,
            self.pressure.per_arena_table_bytes,
        )
        .expect("String写入");
        writeln!(
            output,
            "barrier-fingerprint {}",
            hex_lower(self.fingerprint)
        )
        .expect("String写入");
        output
    }
}

/// 把名称序列追加进规范字节：长度前缀加内容，避免分隔符歧义。
fn push_names(bytes: &mut Vec<u8>, names: &[String]) {
    for name in names {
        bytes.extend_from_slice(
            &u32::try_from(name.len())
                .expect("名称长度适配 u32")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(name.as_bytes());
    }
}

/// 一个 GC arena 的 card 数量；与 GC metadata 的 arena 布局同源。
pub(crate) const GC_ARENA_CARDS: u64 = GC_ARENA_BYTES / CARD_GRANULARITY_BYTES as u64;

/// 从一个地址取 arena 内 card 序号。
pub(crate) const fn card_index(address: u64) -> u64 {
    address >> CARD_GRANULARITY_SHIFT
}

/// 从一个 arena 基址与地址计算 arena 内 card 序号。
pub(crate) const fn arena_card(base: u64, address: u64) -> Option<u64> {
    if address < base || address - base >= GC_ARENA_BYTES {
        return None;
    }
    Some((address - base) >> CARD_GRANULARITY_SHIFT)
}

/// dedup stamp 的直接映射槽；混合函数与 revision 一起进入契约指纹。
pub(crate) fn stamp_slot(arena_descriptor: u64, arena_generation: u32, card: u32) -> u32 {
    let mut value = arena_descriptor
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(arena_generation).wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add(u64::from(card).wrapping_mul(0x94D0_49BB_1331_11EB));
    value ^= value >> 29;
    value = value.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    value ^= value >> 32;
    (value % u64::from(CARD_MARK_STAMP_ENTRIES)) as u32
}

/// 消息族的判别值；与 `message.rs` 的车道编码一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MessageFamilyTag {
    /// raw return 消息。
    Return,
    /// GC 工作消息：remembered-set card batch。
    CardMark,
}

impl MessageFamilyTag {
    /// 返回族判别值。
    pub(crate) const fn raw(self) -> u8 {
        match self {
            Self::Return => 0,
            Self::CardMark => 1,
        }
    }
}

/// 登记 `CardMarkBatch` 的字段集合：只允许稳定 descriptor、generation、card 区间、
/// cycle epoch 与 bytes，任何地址字段都在 verifier 中被拒绝。
pub(crate) fn card_mark_fields() -> Vec<MessageFieldSchema> {
    let mut fields = vec![
        MessageFieldSchema::new("arena", FieldKind::DescriptorIndex),
        MessageFieldSchema::new("arena_generation", FieldKind::Generation),
        MessageFieldSchema::new("bytes", FieldKind::Bytes),
        MessageFieldSchema::new("card_count", FieldKind::CardCount),
        MessageFieldSchema::new("card_start", FieldKind::CardIndex),
        MessageFieldSchema::new("cycle_epoch", FieldKind::Epoch),
        MessageFieldSchema::new("family", FieldKind::KindTag),
        MessageFieldSchema::new("integrity", FieldKind::Integrity),
        MessageFieldSchema::new("state", FieldKind::MessageState),
        MessageFieldSchema::new("target.owner_id", FieldKind::OwnerId),
        MessageFieldSchema::new("target.route_key", FieldKind::RouteKey),
        MessageFieldSchema::new("target.generation", FieldKind::Generation),
        MessageFieldSchema::new("target.domain", FieldKind::OwnerDomain),
    ];
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    fields
}

/// 返回 barrier 固定 record 目录。
fn fixed_layouts() -> Vec<BarrierRecordLayout> {
    macro_rules! record {
        ($ty:ty; $($field:ident),+ $(,)?) => {
            BarrierRecordLayout {
                name: stringify!($ty).to_owned(),
                bytes: u32::try_from(size_of::<$ty>()).expect("record适配u32"),
                alignment: u32::try_from(align_of::<$ty>()).expect("alignment适配u32"),
                fields: vec![$(BarrierFieldLayout {
                    name: stringify!($field).to_owned(),
                    offset: u32::try_from(offset_of!($ty, $field)).expect("offset适配u32"),
                }),+],
            }
        };
    }
    vec![
        record!(CardMarkEntry; arena_descriptor, arena_generation, card_index, cycle_epoch, marks),
        record!(CardMarkStamp; key_low, entry, arena_generation, cycle_epoch, reserved),
        record!(CardMarkBufferHead; len, cycle_epoch, pending_bytes, owner_slot, reserved),
        record!(CardTableDescriptor; arena_descriptor, arena_generation, card_count, dirty, minor_pending),
    ]
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
