//! GC debt、credit、pacing 与 pressure hysteresis 的契约段；backend 与 CLI 只消费已验证对象。
//!
//! 本段把 `GcPacingProfile` 的固定参数、cost unit 口径、pressure 状态机、forced full cycle
//! 的 exactly-once 计数、remark continuation、evacuation pause budget 与 owner credit 的来源
//! 目录固定成带版本的对象，与 `barrier_schema`/`gc_metadata_contract`/`ledger` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! 参数是编译期实现门禁，不是用户可观察的时间单位：契约只登记整数 cost unit、比例与上界，
//! 不登记宿主时钟、线程数或调度时刻。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::gc_metadata_contract::GC_BLOCK_BYTES;
use super::ledger::{LEDGER_PARTITION_COMMITTED, LedgerSchemaV1};
use super::message::RETURN_NODE_BYTES;
use super::model::RawModelError;

/// pacing 契约段的 schema 版本。
pub(crate) const PACING_SCHEMA: u32 = 2;

/// 内建 pacing profile 名；与 runtime tuning profile 一起版本化。
pub(crate) const PACING_PROFILE_NAME: &str = "mosaic-default";
/// pacing profile 的 revision；任何参数变化都必须递增。
pub(crate) const PACING_PROFILE_REVISION: u32 = 2;

/// cost unit 的名字；debt、assist 与 GC CPU 窗口都用它计量。
pub(crate) const PACING_COST_UNIT: &str = "mark-cost-unit";

/// cycle 增长预算的下限；存活记录很小时避免每分配几次就进入 GC。
pub(crate) const MIN_GROWTH_BUDGET: u64 = 1 << 23;
/// 触发一次 assist 的 mark debt 阈值。
pub(crate) const ASSIST_THRESHOLD: u64 = 1 << 20;
/// 一次 assist 最多偿还的 cost unit 数。
pub(crate) const ASSIST_QUANTUM: u64 = 1 << 16;
/// 每 byte allocation debt 折算的 mark cost unit。
pub(crate) const MARK_COST_PER_BYTE: u32 = 1;
/// GC worker 在滑动 cost window 中可使用的 CPU 比例（百分数）。
pub(crate) const GC_CPU_FRACTION: u32 = 25;
/// 滑动 cost window 的容量；窗口在 cycle 边界与显式 recycle 时前进。
pub(crate) const GC_CPU_WINDOW_COST: u64 = 1 << 24;
/// 一次 remark 允许消耗的 cost unit 上界。
pub(crate) const REMARK_COST_BUDGET: u64 = 1 << 22;
/// 一次 relocation 允许复制的 payload 字节上界。
pub(crate) const EVACUATION_PAUSE_BYTES: u64 = 2 * 1024 * 1024;
/// 一次 relocation 允许更新的 exact root 数上界。
pub(crate) const EVACUATION_PAUSE_ROOTS: u32 = 4096;
/// 一次 relocation 允许更新的字段数上界。
pub(crate) const EVACUATION_PAUSE_FIELDS: u32 = 65536;
/// 缓存 committed 快照的刷新间隔：每累计分配这么多字节强制轮询一次真实快照。
pub(crate) const PRESSURE_POLL_BYTES: u64 = 1 << 20;
/// episode 内一次有界 owner drain 对单个 shard 的 item 预算。
pub(crate) const OWNER_DRAIN_ITEMS: u32 = 64;
/// episode 内一次有界 owner drain 对单个 shard 的 byte 预算。
pub(crate) const OWNER_DRAIN_BYTES: u64 = 1 << 16;
/// episode 内两次有界 owner drain 之间必须新增的分配字节。
pub(crate) const OWNER_DRAIN_INTERVAL_BYTES: u64 = 1 << 20;

/// 开启 pressure episode 的占用比例（百分数）。
pub(crate) const PRESSURE_ENTER_RATIO: u32 = 85;
/// 结束 pressure episode 的占用比例（百分数）。
pub(crate) const PRESSURE_CLEAR_RATIO: u32 = 70;
/// pressure 状态名；顺序即状态强度。
pub(crate) const PRESSURE_STATE_NAMES: [&str; 3] = ["steady", "drain", "emergency"];
/// 结束 episode 前必须各自完成一次 owner drain 的字节分类；与账本分类同源。
///
/// 顺序即内存账本 `committed` 分区的稳定名字序（独立计数器在前、兜底成员在末位），
/// 使「episode 结束条件」与「账本成员表」逐项对齐，而不是两处各自排序。
pub(crate) const DRAIN_CLASS_NAMES: [&str; 3] = [
    "owner-cache-bytes",
    "pending-return-bytes",
    "reclaimable-bytes",
];
/// assist 的四种结局名。
pub(crate) const ASSIST_OUTCOME_NAMES: [&str; 4] =
    ["none", "within-quantum", "quantum-truncated", "no-work"];
/// remark 的两种结局名。
pub(crate) const REMARK_OUTCOME_NAMES: [&str; 2] = ["complete", "continuation"];
/// evacuation 决策的两种结局名。
pub(crate) const EVACUATION_OUTCOME_NAMES: [&str; 2] = ["admit", "defer"];
/// 占用 owner credit 的在飞来源目录。
pub(crate) const CREDIT_SOURCE_NAMES: [&str; 5] = [
    "barrier-buffer",
    "card-mark-batch",
    "edge-delta",
    "pending-return",
    "producer-staging",
];

/// 从优化后 LIR 与冻结世界推导的 pacing 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GcPacingDemand {
    /// 产生 allocation debt 的分配站点数：`GcAlloc`、`RegionAlloc` 与 `PromoteManaged`。
    pub alloc_sites: u32,
    /// 可能产生 remembered-set 与 edge summary 工作的屏障站点数。
    pub barrier_sites: u32,
    /// 允许执行一次 assist 的 slow edge 数：分配站点加显式 safepoint。
    pub slow_edges: u32,
    /// 冻结类型表的类型数；决定 trace 工作量的上界口径。
    pub managed_types: u32,
}

impl GcPacingDemand {
    /// 返回需求视图的稳定指纹；字段顺序即编码顺序。
    pub(crate) fn fingerprint(self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&self.alloc_sites.to_le_bytes());
        bytes.extend_from_slice(&self.barrier_sites.to_le_bytes());
        bytes.extend_from_slice(&self.slow_edges.to_le_bytes());
        bytes.extend_from_slice(&self.managed_types.to_le_bytes());
        *blake3::Hasher::new_derive_key("gugu-gc-pacing-demand-v1")
            .update(&bytes)
            .finalize()
            .as_bytes()
    }
}

/// 已验证的 pacing runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GcPacingRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// cost unit 名。
    pub cost_unit: String,
    /// cycle 增长预算下限。
    pub min_growth_budget: u64,
    /// assist 触发阈值。
    pub assist_threshold: u64,
    /// 单次 assist 的偿还上界。
    pub assist_quantum: u64,
    /// 每 byte 折算的 mark cost unit。
    pub mark_cost_per_byte: u32,
    /// GC worker 可用的 CPU 比例。
    pub gc_cpu_fraction: u32,
    /// 滑动 cost window 容量。
    pub gc_cpu_window_cost: u64,
    /// remark cost 上界。
    pub remark_cost_budget: u64,
    /// evacuation payload 上界。
    pub evacuation_pause_bytes: u64,
    /// evacuation root 上界。
    pub evacuation_pause_roots: u32,
    /// evacuation 字段上界。
    pub evacuation_pause_fields: u32,
    /// 缓存 committed 快照的刷新间隔。
    pub pressure_poll_bytes: u64,
    /// 有界 owner drain 的单 shard item 预算。
    pub owner_drain_items: u32,
    /// 有界 owner drain 的单 shard byte 预算。
    pub owner_drain_bytes: u64,
    /// 两次有界 owner drain 之间必须新增的分配字节。
    pub owner_drain_interval_bytes: u64,
    /// episode 开启比例。
    pub pressure_enter_ratio: u32,
    /// episode 结束比例。
    pub pressure_clear_ratio: u32,
    /// pressure 状态目录。
    pub pressure_states: Vec<String>,
    /// 必须各自完成一次 drain 的字节分类目录。
    pub drain_classes: Vec<String>,
    /// 这三个分类所属的账本分区。
    pub drain_partition: String,
    /// assist 结局目录。
    pub assist_outcomes: Vec<String>,
    /// remark 结局目录。
    pub remark_outcomes: Vec<String>,
    /// evacuation 结局目录。
    pub evacuation_outcomes: Vec<String>,
    /// credit 来源目录。
    pub credit_sources: Vec<String>,
    /// 上游需求视图。
    pub demand: GcPacingDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl GcPacingRuntimeContract {
    /// 返回内部 schema 版本。
    pub fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 profile 名。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回 cost unit 名。
    pub fn cost_unit(&self) -> &str {
        &self.cost_unit
    }

    /// 返回增长预算下限。
    pub const fn min_growth_budget(&self) -> u64 {
        self.min_growth_budget
    }

    /// 返回 assist 触发阈值。
    pub const fn assist_threshold(&self) -> u64 {
        self.assist_threshold
    }

    /// 返回单次 assist 的偿还上界。
    pub const fn assist_quantum(&self) -> u64 {
        self.assist_quantum
    }

    /// 返回每 byte 的 mark cost。
    pub const fn mark_cost_per_byte(&self) -> u32 {
        self.mark_cost_per_byte
    }

    /// 返回 GC CPU 比例。
    pub const fn gc_cpu_fraction(&self) -> u32 {
        self.gc_cpu_fraction
    }

    /// 返回滑动窗口容量。
    pub const fn gc_cpu_window_cost(&self) -> u64 {
        self.gc_cpu_window_cost
    }

    /// 返回窗口内允许 GC 消耗的 cost unit 数。
    pub const fn gc_cpu_window_budget(&self) -> u64 {
        self.gc_cpu_window_cost * self.gc_cpu_fraction as u64 / 100
    }

    /// 返回 remark cost 上界。
    pub const fn remark_cost_budget(&self) -> u64 {
        self.remark_cost_budget
    }

    /// 返回 evacuation payload 上界。
    pub const fn evacuation_pause_bytes(&self) -> u64 {
        self.evacuation_pause_bytes
    }

    /// 返回 evacuation root 上界。
    pub const fn evacuation_pause_roots(&self) -> u32 {
        self.evacuation_pause_roots
    }

    /// 返回 evacuation 字段上界。
    pub const fn evacuation_pause_fields(&self) -> u32 {
        self.evacuation_pause_fields
    }

    /// 返回缓存 committed 快照的刷新间隔。
    pub const fn pressure_poll_bytes(&self) -> u64 {
        self.pressure_poll_bytes
    }

    /// 返回有界 owner drain 的单 shard item 预算。
    pub const fn owner_drain_items(&self) -> u32 {
        self.owner_drain_items
    }

    /// 返回有界 owner drain 的单 shard byte 预算。
    pub const fn owner_drain_bytes(&self) -> u64 {
        self.owner_drain_bytes
    }

    /// 返回两次有界 owner drain 之间的分配字节间隔。
    pub const fn owner_drain_interval_bytes(&self) -> u64 {
        self.owner_drain_interval_bytes
    }

    /// 返回 episode 开启比例。
    pub const fn pressure_enter_ratio(&self) -> u32 {
        self.pressure_enter_ratio
    }

    /// 返回 episode 结束比例。
    pub const fn pressure_clear_ratio(&self) -> u32 {
        self.pressure_clear_ratio
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> GcPacingDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 由上游需求构建契约；参数是固定 profile 的登记值。
    pub(crate) fn build(demand: GcPacingDemand) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: PACING_SCHEMA,
            profile: PACING_PROFILE_NAME.to_owned(),
            profile_revision: PACING_PROFILE_REVISION,
            cost_unit: PACING_COST_UNIT.to_owned(),
            min_growth_budget: MIN_GROWTH_BUDGET,
            assist_threshold: ASSIST_THRESHOLD,
            assist_quantum: ASSIST_QUANTUM,
            mark_cost_per_byte: MARK_COST_PER_BYTE,
            gc_cpu_fraction: GC_CPU_FRACTION,
            gc_cpu_window_cost: GC_CPU_WINDOW_COST,
            remark_cost_budget: REMARK_COST_BUDGET,
            evacuation_pause_bytes: EVACUATION_PAUSE_BYTES,
            evacuation_pause_roots: EVACUATION_PAUSE_ROOTS,
            evacuation_pause_fields: EVACUATION_PAUSE_FIELDS,
            pressure_poll_bytes: PRESSURE_POLL_BYTES,
            owner_drain_items: OWNER_DRAIN_ITEMS,
            owner_drain_bytes: OWNER_DRAIN_BYTES,
            owner_drain_interval_bytes: OWNER_DRAIN_INTERVAL_BYTES,
            pressure_enter_ratio: PRESSURE_ENTER_RATIO,
            pressure_clear_ratio: PRESSURE_CLEAR_RATIO,
            pressure_states: PRESSURE_STATE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            drain_classes: DRAIN_CLASS_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            drain_partition: LEDGER_PARTITION_COMMITTED.to_owned(),
            assist_outcomes: ASSIST_OUTCOME_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            remark_outcomes: REMARK_OUTCOME_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            evacuation_outcomes: EVACUATION_OUTCOME_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            credit_sources: CREDIT_SOURCE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 校验契约：常量、目录、hysteresis 关系、跨契约同源与指纹。
    ///
    /// 检查按「参数自洽」「hysteresis」「目录」「跨契约同源」「指纹」分组，任一组失败都指向
    /// 一个独立的漂移来源。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        self.verify_constants()?;
        self.verify_hysteresis()?;
        self.verify_catalogs()?;
        self.verify_cross_contract()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("pacing 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 校验固定参数与 profile 身份。
    fn verify_constants(&self) -> Result<(), RawModelError> {
        if self.schema != PACING_SCHEMA
            || self.profile != PACING_PROFILE_NAME
            || self.profile_revision != PACING_PROFILE_REVISION
            || self.cost_unit != PACING_COST_UNIT
            || self.min_growth_budget != MIN_GROWTH_BUDGET
            || self.assist_threshold != ASSIST_THRESHOLD
            || self.assist_quantum != ASSIST_QUANTUM
            || self.mark_cost_per_byte != MARK_COST_PER_BYTE
            || self.gc_cpu_fraction != GC_CPU_FRACTION
            || self.gc_cpu_window_cost != GC_CPU_WINDOW_COST
            || self.remark_cost_budget != REMARK_COST_BUDGET
            || self.evacuation_pause_bytes != EVACUATION_PAUSE_BYTES
            || self.evacuation_pause_roots != EVACUATION_PAUSE_ROOTS
            || self.evacuation_pause_fields != EVACUATION_PAUSE_FIELDS
            || self.pressure_poll_bytes != PRESSURE_POLL_BYTES
            || self.owner_drain_items != OWNER_DRAIN_ITEMS
            || self.owner_drain_bytes != OWNER_DRAIN_BYTES
            || self.owner_drain_interval_bytes != OWNER_DRAIN_INTERVAL_BYTES
            || self.pressure_enter_ratio != PRESSURE_ENTER_RATIO
            || self.pressure_clear_ratio != PRESSURE_CLEAR_RATIO
        {
            return Err(RawModelError::new(
                "pacing profile 参数与登记值不一致，或参数未随 revision 变化",
            ));
        }
        if self.mark_cost_per_byte == 0 {
            return Err(RawModelError::new("mark cost per byte 不能为零"));
        }
        if self.gc_cpu_fraction == 0 || self.gc_cpu_fraction > 100 {
            return Err(RawModelError::new("GC CPU 比例必须在 1..=100"));
        }
        if !self.min_growth_budget.is_power_of_two()
            || self.min_growth_budget < GC_BLOCK_BYTES as u64
        {
            return Err(RawModelError::new(
                "增长预算下限必须是不少于一个 block 的二次幂",
            ));
        }
        // assist 必须能装进窗口预算，否则每次 assist 都会立即越界，profile 自相矛盾。
        if self.gc_cpu_window_budget() < self.assist_quantum {
            return Err(RawModelError::new(
                "GC CPU 窗口预算容不下一次 assist quantum",
            ));
        }
        if self.assist_threshold < self.assist_quantum
            || self.remark_cost_budget < self.assist_quantum
        {
            return Err(RawModelError::new(
                "assist 阈值与 remark 预算都必须容纳至少一次 assist quantum",
            ));
        }
        if self.pressure_poll_bytes == 0 || self.owner_drain_interval_bytes == 0 {
            return Err(RawModelError::new(
                "pressure 轮询与有界 drain 间隔都必须为正",
            ));
        }
        // 有界 drain 的 byte 预算必须容下至少一个 return node，否则每次 drain 都无法推进。
        if self.owner_drain_items == 0 || self.owner_drain_bytes < u64::from(RETURN_NODE_BYTES) {
            return Err(RawModelError::new(
                "有界 owner drain 预算必须至少容纳一个 return node",
            ));
        }
        Ok(())
    }

    /// 校验 pressure hysteresis 与 relocation 预算的整除关系。
    fn verify_hysteresis(&self) -> Result<(), RawModelError> {
        if !(self.pressure_clear_ratio > 0
            && self.pressure_clear_ratio < self.pressure_enter_ratio
            && self.pressure_enter_ratio < 100)
        {
            return Err(RawModelError::new(
                "pressure 比例必须满足 0 < clear < enter < 100",
            ));
        }
        // 候选 block 要么整体发布，要么整体延后：payload 上界必须覆盖整数个 block。
        if self.evacuation_pause_bytes < u64::from(GC_BLOCK_BYTES)
            || !self
                .evacuation_pause_bytes
                .is_multiple_of(u64::from(GC_BLOCK_BYTES))
        {
            return Err(RawModelError::new(
                "evacuation payload 上界必须是 block 的整数倍",
            ));
        }
        if self.evacuation_pause_roots == 0 || self.evacuation_pause_fields == 0 {
            return Err(RawModelError::new("evacuation root/field 上界不能为零"));
        }
        Ok(())
    }

    /// 校验状态、分类与结局目录。
    fn verify_catalogs(&self) -> Result<(), RawModelError> {
        if self.pressure_states != PRESSURE_STATE_NAMES {
            return Err(RawModelError::new("pressure 状态目录与规范不一致"));
        }
        if self.drain_classes != DRAIN_CLASS_NAMES {
            return Err(RawModelError::new("drain 字节分类目录与规范不一致"));
        }
        if self.assist_outcomes != ASSIST_OUTCOME_NAMES {
            return Err(RawModelError::new("assist 结局目录与规范不一致"));
        }
        if self.remark_outcomes != REMARK_OUTCOME_NAMES {
            return Err(RawModelError::new("remark 结局目录与规范不一致"));
        }
        if self.evacuation_outcomes != EVACUATION_OUTCOME_NAMES {
            return Err(RawModelError::new("evacuation 结局目录与规范不一致"));
        }
        if self.credit_sources != CREDIT_SOURCE_NAMES {
            return Err(RawModelError::new("owner credit 来源目录与规范不一致"));
        }
        Ok(())
    }

    /// 校验 drain 分类与内存账本同源：episode 的结束条件是账本自己的互斥分类。
    fn verify_cross_contract(&self) -> Result<(), RawModelError> {
        let ledger = LedgerSchemaV1::fixed();
        if self.drain_partition != LEDGER_PARTITION_COMMITTED {
            return Err(RawModelError::new(
                "drain 分类必须属于内存账本的物理 committed 分区",
            ));
        }
        // 账本成员表按「独立计数器在前」排列，契约按稳定名字序登记：这里先滤除兜底成员
        // 再排序，两边比较的始终是同一组分类，而不是两处各自定义的顺序。
        let mut members: Vec<String> = ledger
            .members(LEDGER_PARTITION_COMMITTED)
            .iter()
            .filter(|name| {
                ledger
                    .categories
                    .iter()
                    .find(|category| &&category.name == name)
                    .is_some_and(|category| !category.residual)
            })
            .cloned()
            .collect();
        members.sort_unstable();
        if members != self.drain_classes {
            return Err(RawModelError::new(
                "drain 分类必须与内存账本 committed 分区的独立计数器一致",
            ));
        }
        // trim 批按 extent 整块判定；pause 上界必须覆盖最大 extent class，否则
        // relocation_batch 会永久延后全部候选，trim 不再进展而没有任何报错。
        let largest_extent = super::extent::EXTENT_CLASS_LADDER
            .iter()
            .copied()
            .max()
            .expect("extent class 阶梯非空");
        if self.evacuation_pause_bytes < largest_extent {
            return Err(RawModelError::new(
                "evacuation payload 上界必须覆盖最大 extent class",
            ));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        push_text(&mut bytes, &self.profile);
        bytes.extend_from_slice(&self.profile_revision.to_le_bytes());
        push_text(&mut bytes, &self.cost_unit);
        bytes.extend_from_slice(&self.min_growth_budget.to_le_bytes());
        bytes.extend_from_slice(&self.assist_threshold.to_le_bytes());
        bytes.extend_from_slice(&self.assist_quantum.to_le_bytes());
        bytes.extend_from_slice(&self.mark_cost_per_byte.to_le_bytes());
        bytes.extend_from_slice(&self.gc_cpu_fraction.to_le_bytes());
        bytes.extend_from_slice(&self.gc_cpu_window_cost.to_le_bytes());
        bytes.extend_from_slice(&self.remark_cost_budget.to_le_bytes());
        bytes.extend_from_slice(&self.evacuation_pause_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.evacuation_pause_roots.to_le_bytes());
        bytes.extend_from_slice(&self.evacuation_pause_fields.to_le_bytes());
        bytes.extend_from_slice(&self.pressure_poll_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.owner_drain_items.to_le_bytes());
        bytes.extend_from_slice(&self.owner_drain_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.owner_drain_interval_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.pressure_enter_ratio.to_le_bytes());
        bytes.extend_from_slice(&self.pressure_clear_ratio.to_le_bytes());
        push_names(&mut bytes, &self.pressure_states);
        push_names(&mut bytes, &self.drain_classes);
        push_text(&mut bytes, &self.drain_partition);
        push_names(&mut bytes, &self.assist_outcomes);
        push_names(&mut bytes, &self.remark_outcomes);
        push_names(&mut bytes, &self.evacuation_outcomes);
        push_names(&mut bytes, &self.credit_sources);
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes
    }

    /// pacing 契约的域隔离内容身份。
    pub fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-gc-pacing-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "pacing schema={} profile={} revision={} cost-unit={}",
            self.schema, self.profile, self.profile_revision, self.cost_unit,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-budget min-growth={} assist-threshold={} assist-quantum={} mark-cost-per-byte={}",
            self.min_growth_budget,
            self.assist_threshold,
            self.assist_quantum,
            self.mark_cost_per_byte,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-cpu fraction={} window={} budget={}",
            self.gc_cpu_fraction,
            self.gc_cpu_window_cost,
            self.gc_cpu_window_budget(),
        )
        .expect("String写入");
        writeln!(output, "pacing-remark budget={}", self.remark_cost_budget).expect("String写入");
        writeln!(
            output,
            "pacing-evacuation bytes={} roots={} fields={}",
            self.evacuation_pause_bytes, self.evacuation_pause_roots, self.evacuation_pause_fields,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-drain poll={} items={} bytes={} interval={}",
            self.pressure_poll_bytes,
            self.owner_drain_items,
            self.owner_drain_bytes,
            self.owner_drain_interval_bytes,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-pressure enter={} clear={} states={}",
            self.pressure_enter_ratio,
            self.pressure_clear_ratio,
            self.pressure_states.join(","),
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-drain-classes {} partition={}",
            self.drain_classes.join(","),
            self.drain_partition,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-assist-outcomes {}",
            self.assist_outcomes.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-remark-outcomes {}",
            self.remark_outcomes.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-evacuation-outcomes {}",
            self.evacuation_outcomes.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-credit-sources {}",
            self.credit_sources.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-demand alloc-sites={} barrier-sites={} slow-edges={} managed-types={}",
            self.demand.alloc_sites,
            self.demand.barrier_sites,
            self.demand.slow_edges,
            self.demand.managed_types,
        )
        .expect("String写入");
        writeln!(output, "pacing-fingerprint {}", hex_lower(self.fingerprint)).expect("String写入");
        output
    }
}

fn push_text(bytes: &mut Vec<u8>, text: &str) {
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(0);
}

fn push_names(bytes: &mut Vec<u8>, names: &[String]) {
    bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for name in names {
        push_text(bytes, name);
    }
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for byte in bytes {
        text.push(char::from(TABLE[usize::from(byte >> 4)]));
        text.push(char::from(TABLE[usize::from(byte & 0x0f)]));
    }
    text
}
