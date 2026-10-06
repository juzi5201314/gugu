//! 镜像内的 GC 配置记录：编译器按已冻结契约生成的只读 `GcImageConfig`。
//!
//! 该记录是 Gugu runtime 读取 GC 参数的唯一来源：字段全部是 64 位机器字，按固定顺序连续
//! 排列，`byte_len` 由 Rust 的布局派生。runtime 侧用同名 `#[repr(C)]` 记录逐字段交叉校验
//! （`record!(GcImageConfig; …)`），镜像里不维护第二组数值常量，也不提供运行时可变的
//! order 或 profile 开关：所有数值都取自已经验证过的 schema 与 profile。

use super::barrier_schema::{
    BARRIER_SCHEMA, CARD_GRANULARITY_BYTES, CARD_MARK_BUFFER_ENTRIES, CARD_MARK_STAMP_ENTRIES,
    CARD_MARKS_PER_WRITE, EDGE_BUFFER_ENTRIES, EDGE_DELTAS_PER_WRITE, SHADE_SLOTS_PER_WRITE,
};
use super::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use super::local_heap_schema::{
    HEAP_GRANULE_BYTES, HEAP_LARGE_ALIGN_LIMIT, HEAP_OBJECT_HEADER_BYTES, HEAP_TLAB_SPAN_BLOCKS,
    HeapTriggerProfile, LOCAL_HEAP_SCHEMA,
};
use super::mark_schema::{MARK_MAILBOX_SHARDS, MARK_PROFILE_REVISION, MARK_SCHEMA};
use super::pacing_schema::{
    ASSIST_QUANTUM, ASSIST_THRESHOLD, EVACUATION_PAUSE_BYTES, EVACUATION_PAUSE_FIELDS,
    EVACUATION_PAUSE_ROOTS, GC_CPU_FRACTION, GC_CPU_WINDOW_COST, MARK_COST_PER_BYTE,
    MIN_GROWTH_BUDGET, OWNER_DRAIN_BYTES, OWNER_DRAIN_INTERVAL_BYTES, OWNER_DRAIN_ITEMS,
    PACING_PROFILE_REVISION, PACING_SCHEMA, PRESSURE_CLEAR_RATIO, PRESSURE_ENTER_RATIO,
    PRESSURE_POLL_BYTES, REMARK_COST_BUDGET,
};

/// 记录自身的修订号；字段顺序或语义变化必须递增并拒绝旧值。
pub(crate) const GC_IMAGE_CONFIG_REVISION: u64 = 1;

/// 记录的字数；`byte_len` 与镜像只读节长度都由它派生。
pub(crate) const GC_IMAGE_CONFIG_WORDS: usize = 43;

/// 编译器生成的只读 GC 配置；字段顺序即 runtime 的读取顺序。
#[repr(C, align(8))]
pub(crate) struct GcImageConfig {
    pub(crate) revision: u64,
    pub(crate) byte_len: u64,
    pub(crate) local_heap_schema: u64,
    pub(crate) barrier_schema: u64,
    pub(crate) mark_schema: u64,
    pub(crate) pacing_schema: u64,
    pub(crate) trigger_revision: u64,
    pub(crate) mark_profile_revision: u64,
    pub(crate) pacing_profile_revision: u64,
    pub(crate) arena_bytes: u64,
    pub(crate) block_bytes: u64,
    pub(crate) line_bytes: u64,
    pub(crate) granule_bytes: u64,
    pub(crate) object_header_bytes: u64,
    pub(crate) tlab_span_blocks: u64,
    pub(crate) minor_trigger_bytes: u64,
    pub(crate) tenure_age: u64,
    pub(crate) max_age: u64,
    pub(crate) large_align_limit: u64,
    pub(crate) card_bytes: u64,
    pub(crate) card_entries: u64,
    pub(crate) stamp_entries: u64,
    pub(crate) edge_entries: u64,
    pub(crate) shade_slots_per_write: u64,
    pub(crate) card_marks_per_write: u64,
    pub(crate) edge_deltas_per_write: u64,
    pub(crate) mark_mailbox_shards: u64,
    pub(crate) min_growth_budget: u64,
    pub(crate) assist_threshold: u64,
    pub(crate) assist_quantum: u64,
    pub(crate) mark_cost_per_byte: u64,
    pub(crate) gc_cpu_fraction: u64,
    pub(crate) gc_cpu_window_cost: u64,
    pub(crate) remark_cost_budget: u64,
    pub(crate) evacuation_pause_bytes: u64,
    pub(crate) evacuation_pause_roots: u64,
    pub(crate) evacuation_pause_fields: u64,
    pub(crate) pressure_poll_bytes: u64,
    pub(crate) owner_drain_items: u64,
    pub(crate) owner_drain_bytes: u64,
    pub(crate) owner_drain_interval_bytes: u64,
    pub(crate) pressure_enter_ratio: u64,
    pub(crate) pressure_clear_ratio: u64,
}

impl GcImageConfig {
    /// 按已冻结契约生成记录；数值取自 schema 常量与已验证 profile，不另设一套默认值。
    pub(crate) fn build() -> Self {
        let trigger = HeapTriggerProfile::default();
        Self {
            revision: GC_IMAGE_CONFIG_REVISION,
            byte_len: u64::try_from(core::mem::size_of::<Self>()).expect("记录字节数适配 u64"),
            local_heap_schema: u64::from(LOCAL_HEAP_SCHEMA),
            barrier_schema: u64::from(BARRIER_SCHEMA),
            mark_schema: u64::from(MARK_SCHEMA),
            pacing_schema: u64::from(PACING_SCHEMA),
            trigger_revision: u64::from(trigger.revision),
            mark_profile_revision: u64::from(MARK_PROFILE_REVISION),
            pacing_profile_revision: u64::from(PACING_PROFILE_REVISION),
            arena_bytes: GC_ARENA_BYTES,
            block_bytes: u64::from(GC_BLOCK_BYTES),
            line_bytes: u64::from(GC_LINE_BYTES),
            granule_bytes: u64::from(HEAP_GRANULE_BYTES),
            object_header_bytes: u64::from(HEAP_OBJECT_HEADER_BYTES),
            tlab_span_blocks: u64::from(HEAP_TLAB_SPAN_BLOCKS),
            minor_trigger_bytes: trigger.minor_trigger_bytes,
            tenure_age: u64::from(trigger.tenure_age),
            max_age: u64::from(trigger.max_age),
            large_align_limit: u64::from(HEAP_LARGE_ALIGN_LIMIT),
            card_bytes: u64::from(CARD_GRANULARITY_BYTES),
            card_entries: u64::from(CARD_MARK_BUFFER_ENTRIES),
            stamp_entries: u64::from(CARD_MARK_STAMP_ENTRIES),
            edge_entries: u64::from(EDGE_BUFFER_ENTRIES),
            shade_slots_per_write: u64::from(SHADE_SLOTS_PER_WRITE),
            card_marks_per_write: u64::from(CARD_MARKS_PER_WRITE),
            edge_deltas_per_write: u64::from(EDGE_DELTAS_PER_WRITE),
            mark_mailbox_shards: u64::from(MARK_MAILBOX_SHARDS),
            min_growth_budget: MIN_GROWTH_BUDGET,
            assist_threshold: ASSIST_THRESHOLD,
            assist_quantum: ASSIST_QUANTUM,
            mark_cost_per_byte: u64::from(MARK_COST_PER_BYTE),
            gc_cpu_fraction: u64::from(GC_CPU_FRACTION),
            gc_cpu_window_cost: GC_CPU_WINDOW_COST,
            remark_cost_budget: REMARK_COST_BUDGET,
            evacuation_pause_bytes: EVACUATION_PAUSE_BYTES,
            evacuation_pause_roots: u64::from(EVACUATION_PAUSE_ROOTS),
            evacuation_pause_fields: u64::from(EVACUATION_PAUSE_FIELDS),
            pressure_poll_bytes: PRESSURE_POLL_BYTES,
            owner_drain_items: u64::from(OWNER_DRAIN_ITEMS),
            owner_drain_bytes: OWNER_DRAIN_BYTES,
            owner_drain_interval_bytes: OWNER_DRAIN_INTERVAL_BYTES,
            pressure_enter_ratio: u64::from(PRESSURE_ENTER_RATIO),
            pressure_clear_ratio: u64::from(PRESSURE_CLEAR_RATIO),
        }
    }

    /// 字段的规范顺序；镜像字节、指纹与 runtime 逐字段校验都按它。
    pub(crate) fn words(&self) -> [u64; GC_IMAGE_CONFIG_WORDS] {
        [
            self.revision,
            self.byte_len,
            self.local_heap_schema,
            self.barrier_schema,
            self.mark_schema,
            self.pacing_schema,
            self.trigger_revision,
            self.mark_profile_revision,
            self.pacing_profile_revision,
            self.arena_bytes,
            self.block_bytes,
            self.line_bytes,
            self.granule_bytes,
            self.object_header_bytes,
            self.tlab_span_blocks,
            self.minor_trigger_bytes,
            self.tenure_age,
            self.max_age,
            self.large_align_limit,
            self.card_bytes,
            self.card_entries,
            self.stamp_entries,
            self.edge_entries,
            self.shade_slots_per_write,
            self.card_marks_per_write,
            self.edge_deltas_per_write,
            self.mark_mailbox_shards,
            self.min_growth_budget,
            self.assist_threshold,
            self.assist_quantum,
            self.mark_cost_per_byte,
            self.gc_cpu_fraction,
            self.gc_cpu_window_cost,
            self.remark_cost_budget,
            self.evacuation_pause_bytes,
            self.evacuation_pause_roots,
            self.evacuation_pause_fields,
            self.pressure_poll_bytes,
            self.owner_drain_items,
            self.owner_drain_bytes,
            self.owner_drain_interval_bytes,
            self.pressure_enter_ratio,
            self.pressure_clear_ratio,
        ]
    }

    /// 规范字节：小端逐字写出，长度等于 `byte_len`。镜像只读节与指纹都消费它。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(core::mem::size_of::<Self>());
        for word in self.words() {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }
}

const _: () = {
    assert!(
        core::mem::size_of::<GcImageConfig>() == GC_IMAGE_CONFIG_WORDS * 8
            && core::mem::align_of::<GcImageConfig>() == 8
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 记录的字节布局必须与字段表一致：`words` 的条数、`byte_len` 与实际布局三者同源，
    /// 任何一侧漂移都会在这里失败而不是让 runtime 读到错位的字段。
    #[test]
    fn image_config_layout_matches_the_word_table() {
        let config = GcImageConfig::build();
        assert_eq!(config.words().len(), GC_IMAGE_CONFIG_WORDS);
        assert_eq!(
            config.byte_len,
            u64::try_from(GC_IMAGE_CONFIG_WORDS * 8).unwrap(),
            "byte_len 必须由字数派生"
        );
        assert_eq!(config.words()[1], config.byte_len);
        assert_eq!(config.canonical_bytes().len(), GC_IMAGE_CONFIG_WORDS * 8);
        assert_eq!(config.revision, GC_IMAGE_CONFIG_REVISION);
        assert!(
            config.card_marks_per_write <= config.shade_slots_per_write
                && config.edge_deltas_per_write <= config.shade_slots_per_write,
            "每写预算必须是配置的上界"
        );
        assert!(
            config.pressure_clear_ratio < config.pressure_enter_ratio,
            "压力进入阈值必须高于解除阈值"
        );
    }

    /// Gugu 侧同名 record 必须与 Rust 记录**逐字段同序**：runtime 按同一份字段表读取镜像里
    /// 的字节，任何一侧增删、重排或长度漂移都在这里失败。
    #[test]
    fn gugu_mirror_lists_the_same_fields_in_the_same_order() {
        let source = include_str!("../../resources/runtime/gc.gg");
        let start = source.find("pub struct GcImageConfig {").expect("镜像记录");
        let body = &source[start..];
        let end = body.find("\n}").expect("记录结尾");
        let fields: Vec<&str> = body[..end]
            .lines()
            .skip(1)
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .filter_map(|line| line.split(':').next())
            .map(str::trim)
            .collect();
        let expected = [
            "revision",
            "byte_len",
            "local_heap_schema",
            "barrier_schema",
            "mark_schema",
            "pacing_schema",
            "trigger_revision",
            "mark_profile_revision",
            "pacing_profile_revision",
            "arena_bytes",
            "block_bytes",
            "line_bytes",
            "granule_bytes",
            "object_header_bytes",
            "tlab_span_blocks",
            "minor_trigger_bytes",
            "tenure_age",
            "max_age",
            "large_align_limit",
            "card_bytes",
            "card_entries",
            "stamp_entries",
            "edge_entries",
            "shade_slots_per_write",
            "card_marks_per_write",
            "edge_deltas_per_write",
            "mark_mailbox_shards",
            "min_growth_budget",
            "assist_threshold",
            "assist_quantum",
            "mark_cost_per_byte",
            "gc_cpu_fraction",
            "gc_cpu_window_cost",
            "remark_cost_budget",
            "evacuation_pause_bytes",
            "evacuation_pause_roots",
            "evacuation_pause_fields",
            "pressure_poll_bytes",
            "owner_drain_items",
            "owner_drain_bytes",
            "owner_drain_interval_bytes",
            "pressure_enter_ratio",
            "pressure_clear_ratio",
        ];
        assert_eq!(fields, expected);
        assert_eq!(
            GcImageConfig::build().byte_len,
            344,
            "记录长度必须是 43 × 8"
        );
    }

    /// 分配器读镜像配置的字段偏移必须等于该字段在记录里的下标 × 8。
    ///
    /// 偏移写错会让分配器把别的字段当几何使用（例如把 granule 当 block），运行期只表现为
    /// 分配越界或永久 refill，不会有任何诊断。
    #[test]
    fn gugu_config_offsets_match_the_mirrored_field_order() {
        let source = include_str!("../../resources/runtime/gc.gg");
        let start = source.find("pub struct GcImageConfig {").expect("镜像记录");
        let body = &source[start..];
        let end = body.find("\n}").expect("记录结尾");
        let fields: Vec<&str> = body[..end]
            .lines()
            .skip(1)
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .filter_map(|line| line.split(':').next())
            .map(str::trim)
            .collect();
        for (constant, field) in [
            ("GC_CONFIG_ARENA_BYTES_OFFSET", "arena_bytes"),
            ("GC_CONFIG_BLOCK_BYTES_OFFSET", "block_bytes"),
            ("GC_CONFIG_LINE_BYTES_OFFSET", "line_bytes"),
            ("GC_CONFIG_GRANULE_BYTES_OFFSET", "granule_bytes"),
            (
                "GC_CONFIG_OBJECT_HEADER_BYTES_OFFSET",
                "object_header_bytes",
            ),
            ("GC_CONFIG_LARGE_ALIGN_LIMIT_OFFSET", "large_align_limit"),
            ("GC_CONFIG_CARD_BYTES_OFFSET", "card_bytes"),
        ] {
            let index = fields
                .iter()
                .position(|name| *name == field)
                .unwrap_or_else(|| panic!("镜像记录缺少字段 {field}"));
            let prefix = format!("pub const {constant}: uint = ");
            let literal = source
                .lines()
                .find_map(|line| line.trim().strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("gc.gg 缺少常量 {constant}"));
            assert_eq!(
                literal.trim().parse::<u64>().expect("偏移字面量"),
                u64::try_from(index).expect("下标适配 u64") * 8,
                "{constant} 与镜像字段序不一致"
            );
        }
    }
}
