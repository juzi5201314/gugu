//! typed combining 冷操作的契约段。
//!
//! 本段把「冷操作 tag 目录」「允许与禁止的用途」「operation record 的标量字段集合」
//! 「生命周期状态与迁移」「同步路径」「合并与轮次预算」「由 raw 平面需求推导的记录池
//! 上界」固定成带版本的对象，与 `routing_schema` / `provenance_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! direct 是默认模式：冷操作在请求者上下文逐条执行同一份 handler，不创建任何 record；
//! combined 只在显式 profile 开启后把同一批操作记录进非移动记录池并按轮次合并执行。
//! 模式是 runtime tuning，不改变编译语义；两条路径必须给出逐字节一致的结果。

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::model::RawModelError;
use super::resource_schema::TransitionSchema;

/// combining 契约段 schema。
pub(crate) const COMBINING_SCHEMA: u32 = 1;

/// 内建 combining profile 名。
pub(crate) const COMBINING_PROFILE_NAME: &str = "mosaic-combining";
/// profile revision；tag 目录、record 字段、预算参数或统计口径变化都必须递增。
pub(crate) const COMBINING_PROFILE_REVISION: u32 = 1;

/// 单个 operation record 的规范槽字节。
///
/// 它是轮次字节预算与 `global-range-refill` 记账的计价单位，不是宿主语言结构体大小：
/// 参照实现的 record 额外携带 MCS 链指针与 response 状态，不改变这里的规范取值。
pub(crate) const COMBINING_RECORD_BYTES: u32 = 64;
/// 记录池按 chunk 补充的槽位数；chunk 分配后永不移动。
pub(crate) const COMBINING_RECORD_CHUNK_ITEMS: u32 = 16;
/// 每个 chunk 尾部预留给 `global-range-refill` 的槽位数。
pub(crate) const COMBINING_REFILL_RESERVE_SLOTS: u32 = 1;
/// 非移动 metadata 的 chunk 硬上限（4096 条记录 / 256 KiB）。
pub(crate) const COMBINING_MAX_OPERATION_CHUNKS: u32 = 256;
/// 一次执行最多合并的同类请求数。
pub(crate) const COMBINING_MERGE_LIMIT: u32 = 4;
/// 每轮最多认领的记录数。
pub(crate) const COMBINING_ROUND_ITEM_BUDGET: u32 = 8;
/// 每轮最多消费的请求字节。
pub(crate) const COMBINING_ROUND_BYTE_BUDGET: u64 = 1 << 20;
/// 记录在链上等待到该轮数即超时。
pub(crate) const COMBINING_TIMEOUT_ROUNDS: u32 = 8;

/// 冷操作 tag 目录；顺序即登记顺序，也是 `OperationTag` 的稠密下标。
pub(crate) const OPERATION_TAGS: [&str; 4] = [
    "global-range-refill",
    "extent-coalesce",
    "topology-rebuild",
    "platform-trim",
];

/// 允许进入 combining 的用途；与 tag 目录逐项相同，verifier 强制这一点。
pub(crate) const COMBINING_ALLOWED_USES: [&str; 4] = OPERATION_TAGS;

/// 明确禁止进入 combining 的用途；热路径与任意 closure 都在其中。
pub(crate) const COMBINING_FORBIDDEN_USES: [&str; 10] = [
    "tlab-allocation",
    "raw-local-pop",
    "ordinary-remote-return",
    "channel-linearization",
    "select-linearization",
    "park-wake-linearization",
    "arbitrary-closure",
    "drop-glue",
    "coroutine-stack-call",
    "shared-free-list-guard",
];

/// combining 模式目录；顺序即登记顺序，也是 `CombiningMode` 的稠密下标。
pub(crate) const COMBINING_MODES: [&str; 2] = ["direct", "combined"];

/// operation record 的生命周期状态目录；顺序即登记顺序。
pub(crate) const OPERATION_STATES: [&str; 5] =
    ["free", "published", "claimed", "completed", "cancelled"];

/// operation record 的结局目录；顺序即登记顺序。
pub(crate) const OPERATION_OUTCOMES: [&str; 3] = ["applied", "cancelled", "timed-out"];

/// operation record 的迁移目录；`from`/`to` 必须在状态目录中，`trigger` 必须唯一。
pub(crate) const OPERATION_TRANSITIONS: [OperationTransition; 7] = [
    OperationTransition {
        from: "free",
        to: "published",
        trigger: "publish",
    },
    OperationTransition {
        from: "published",
        to: "claimed",
        trigger: "claim",
    },
    OperationTransition {
        from: "published",
        to: "cancelled",
        trigger: "cancel-before-claim",
    },
    OperationTransition {
        from: "published",
        to: "completed",
        trigger: "timeout",
    },
    OperationTransition {
        from: "claimed",
        to: "completed",
        trigger: "apply",
    },
    OperationTransition {
        from: "completed",
        to: "free",
        trigger: "recycle",
    },
    OperationTransition {
        from: "cancelled",
        to: "free",
        trigger: "recycle",
    },
];

/// 无争用路径的同步原语目录；fast path 只有一条。
pub(crate) const COMBINING_FAST_PATHS: [&str; 1] = ["compare-exchange-claim-word"];
/// 有争用路径的同步原语目录。
pub(crate) const COMBINING_WAIT_PATHS: [&str; 2] = ["mcs-record", "owner-inbox"];
/// wait/wake 的平台接缝目录。
pub(crate) const COMBINING_WAIT_WAKE: [&str; 2] = ["platform-wait", "platform-wake"];

/// combining 统计口径；顺序即登记顺序，与 `CombiningStats` 的字段顺序一致。
pub(crate) const COMBINING_STATISTICS: [&str; 9] = [
    "combining-requests",
    "combining-fast-path-claims",
    "combining-contended-parkings",
    "combining-merged-requests",
    "combining-rounds",
    "combining-executions",
    "combining-cancellations",
    "combining-timeouts",
    "combining-refills",
];

/// 一条 operation record 的固定迁移。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationTransition {
    pub(crate) from: &'static str,
    pub(crate) to: &'static str,
    pub(crate) trigger: &'static str,
}

/// combining 模式；direct 是默认，combined 只在 profile 显式开启后启用。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CombiningMode {
    /// 请求者上下文逐条执行同一份 handler，不创建 record。
    #[default]
    Direct,
    /// 按位记录/flush：争用时挂链，由一轮 combiner 合并执行。
    Combined,
}

impl CombiningMode {
    /// 返回登记目录中的模式名。
    pub const fn name(self) -> &'static str {
        COMBINING_MODES[self as usize]
    }
}

/// combining profile 的显式开关；默认 direct 表示冷操作不进记录池。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CombiningPolicyV1 {
    /// combining 模式。
    pub mode: CombiningMode,
}

impl CombiningPolicyV1 {
    /// direct 模式：请求者上下文直接执行，不创建 operation record。
    pub(crate) const fn direct() -> Self {
        Self {
            mode: CombiningMode::Direct,
        }
    }

    /// combined 模式：冷操作记录进非移动池，按轮次合并执行。
    pub(crate) const fn combined() -> Self {
        Self {
            mode: CombiningMode::Combined,
        }
    }
}

/// 由 raw 平面需求推导的 combining 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CombiningDemand {
    /// owner 数量；每个 owner 一条 combiner 队列。
    pub owners: u32,
    /// 可能发布冷操作的站点上界；raw 与 resource 站点之和。
    pub range_sites: u32,
    /// extent class 阶梯长度；refill 与 coalesce 的计价域。
    pub extent_classes: u32,
    /// 记录条数上界：每个 owner 一个轮次的预算加一份 refill 预留。
    pub records: u32,
    /// 记录池的 chunk 数上界。
    pub chunks: u32,
}

impl CombiningDemand {
    /// 由 raw 平面需求的 owner 数、分配站点数与 extent class 数推导编译期上界。
    pub(crate) fn derive(
        owners: u32,
        runtime_raw_sites: u32,
        resource_sites: u32,
        extent_classes: u32,
    ) -> Self {
        let records = owners.max(1) * COMBINING_ROUND_ITEM_BUDGET + COMBINING_REFILL_RESERVE_SLOTS;
        Self {
            owners,
            range_sites: runtime_raw_sites.saturating_add(resource_sites),
            extent_classes,
            records,
            chunks: records.div_ceil(COMBINING_RECORD_CHUNK_ITEMS),
        }
    }

    /// 校验需求自身的推导关系与登记上界。
    ///
    /// 只为 `owners` 与 `extent_classes` 记需求：`range_sites` 是 raw/resource 站点之和，
    /// 推导它的两个分量属于 plane 需求视图，因此这里只核对记录数与 chunk 数这两个
    /// 由 owner 数直接导出的上界。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        let records =
            self.owners.max(1) * COMBINING_ROUND_ITEM_BUDGET + COMBINING_REFILL_RESERVE_SLOTS;
        if self.records != records {
            return Err(RawModelError::new(
                "combining 需求的记录数与 owner 数推导不一致",
            ));
        }
        if self.chunks != records.div_ceil(COMBINING_RECORD_CHUNK_ITEMS) {
            return Err(RawModelError::new(
                "combining 需求的 chunk 数与记录数推导不一致",
            ));
        }
        if self.extent_classes != super::extent::EXTENT_CLASS_LADDER.len() as u32 {
            return Err(RawModelError::new(
                "combining 需求的 extent class 数与登记阶梯不一致",
            ));
        }
        if self.chunks > COMBINING_MAX_OPERATION_CHUNKS {
            return Err(RawModelError::new(
                "combining 需求的 chunk 数超过非移动 metadata 上限",
            ));
        }
        Ok(())
    }
}

/// 已验证的 typed combining runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CombiningRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// combining 模式。
    pub mode: CombiningMode,
    /// 冷操作 tag 目录。
    pub operation_tags: Vec<String>,
    /// 允许进入 combining 的用途目录。
    pub allowed_uses: Vec<String>,
    /// 禁止进入 combining 的用途目录。
    pub forbidden_uses: Vec<String>,
    /// operation record 的字段集合。
    ///
    /// 字段种类是 crate 内部的 schema 类型，因此这里与 `SharedHeapRuntimeContract` 的
    /// 消息字段集合同样保持 crate 可见：契约对外只经 `dump` 与 JSON 暴露登记结果。
    pub(crate) record_fields: Vec<super::model::MessageFieldSchema>,
    /// 生命周期状态目录。
    pub states: Vec<String>,
    /// 结局目录。
    pub outcomes: Vec<String>,
    /// 生命周期迁移目录。
    pub(crate) transitions: Vec<TransitionSchema>,
    /// 无争用同步路径目录。
    pub fast_paths: Vec<String>,
    /// 有争用同步路径目录。
    pub wait_paths: Vec<String>,
    /// wait/wake 平台接缝目录。
    pub wait_wake: Vec<String>,
    /// 一次执行最多合并的同类请求数。
    pub merge_limit: u32,
    /// 每轮最多认领的记录数。
    pub round_item_budget: u32,
    /// 每轮最多消费的请求字节。
    pub round_byte_budget: u64,
    /// 等待超时的轮数上界。
    pub timeout_rounds: u32,
    /// 单个 record 的规范槽字节。
    pub record_bytes: u32,
    /// 记录池的 chunk 槽位数。
    pub record_chunk_items: u32,
    /// 每个 chunk 预留给 refill 的槽位数。
    pub refill_reserve_slots: u32,
    /// chunk 数硬上限。
    pub max_operation_chunks: u32,
    /// 统计口径目录。
    pub statistics: Vec<String>,
    /// 上游需求视图。
    pub demand: CombiningDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl CombiningRuntimeContract {
    /// 由需求视图与 combining 模式构建契约；direct 模式仍建立完整契约状态。
    pub(crate) fn build(
        demand: CombiningDemand,
        policy: CombiningPolicyV1,
    ) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: COMBINING_SCHEMA,
            profile: COMBINING_PROFILE_NAME.to_owned(),
            profile_revision: COMBINING_PROFILE_REVISION,
            mode: policy.mode,
            operation_tags: owned_names(&OPERATION_TAGS),
            allowed_uses: owned_names(&COMBINING_ALLOWED_USES),
            forbidden_uses: owned_names(&COMBINING_FORBIDDEN_USES),
            record_fields: Self::fixed_record_fields(),
            states: owned_names(&OPERATION_STATES),
            outcomes: owned_names(&OPERATION_OUTCOMES),
            transitions: fixed_transitions(),
            fast_paths: owned_names(&COMBINING_FAST_PATHS),
            wait_paths: owned_names(&COMBINING_WAIT_PATHS),
            wait_wake: owned_names(&COMBINING_WAIT_WAKE),
            merge_limit: COMBINING_MERGE_LIMIT,
            round_item_budget: COMBINING_ROUND_ITEM_BUDGET,
            round_byte_budget: COMBINING_ROUND_BYTE_BUDGET,
            timeout_rounds: COMBINING_TIMEOUT_ROUNDS,
            record_bytes: COMBINING_RECORD_BYTES,
            record_chunk_items: COMBINING_RECORD_CHUNK_ITEMS,
            refill_reserve_slots: COMBINING_REFILL_RESERVE_SLOTS,
            max_operation_chunks: COMBINING_MAX_OPERATION_CHUNKS,
            statistics: owned_names(&COMBINING_STATISTICS),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回 profile 名。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回 combining 模式。
    pub const fn mode(&self) -> CombiningMode {
        self.mode
    }

    /// 返回单次执行的合并上限。
    pub const fn merge_limit(&self) -> u32 {
        self.merge_limit
    }

    /// 返回每轮的记录数预算。
    pub const fn round_item_budget(&self) -> u32 {
        self.round_item_budget
    }

    /// 返回每轮的请求字节预算。
    pub const fn round_byte_budget(&self) -> u64 {
        self.round_byte_budget
    }

    /// 返回等待超时的轮数上界。
    pub const fn timeout_rounds(&self) -> u32 {
        self.timeout_rounds
    }

    /// 返回单个 record 的规范槽字节。
    pub const fn record_bytes(&self) -> u32 {
        self.record_bytes
    }

    /// 返回记录池的 chunk 槽位数。
    pub const fn record_chunk_items(&self) -> u32 {
        self.record_chunk_items
    }

    /// 返回每个 chunk 的 refill 预留槽位数。
    pub const fn refill_reserve_slots(&self) -> u32 {
        self.refill_reserve_slots
    }

    /// 返回 chunk 数硬上限。
    pub const fn max_operation_chunks(&self) -> u32 {
        self.max_operation_chunks
    }

    /// 返回登记的冷操作 tag 数量。
    pub fn operation_tag_count(&self) -> u32 {
        u32::try_from(self.operation_tags.len()).expect("tag 数量适配 u32")
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> CombiningDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 返回 operation record 的固定字段集合。
    ///
    /// 十个字段全部是标量、stable descriptor id、已登记 handle 与 response slot：
    /// 任何一个地址类字段在这里就不可能出现，`verify` 因此能把「record 携带地址」
    /// 变成机器拒绝的非法契约。
    fn fixed_record_fields() -> Vec<super::model::MessageFieldSchema> {
        use super::model::{FieldKind, MessageFieldSchema};
        vec![
            MessageFieldSchema::new("tag", FieldKind::KindTag),
            MessageFieldSchema::new("state", FieldKind::MessageState),
            MessageFieldSchema::new("generation", FieldKind::Generation),
            MessageFieldSchema::new("owner-domain", FieldKind::OwnerDomain),
            MessageFieldSchema::new("owner-id", FieldKind::OwnerId),
            MessageFieldSchema::new("merge-key", FieldKind::MergeKey),
            MessageFieldSchema::new("descriptor", FieldKind::DescriptorIndex),
            MessageFieldSchema::new("scalar", FieldKind::Scalar),
            MessageFieldSchema::new("bytes", FieldKind::Bytes),
            MessageFieldSchema::new("response-slot", FieldKind::ResponseSlot),
        ]
    }

    /// 校验契约：模式、tag/用途目录、record 字段、状态迁移、预算参数与需求闭合。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != COMBINING_SCHEMA
            || self.profile != COMBINING_PROFILE_NAME
            || self.profile_revision != COMBINING_PROFILE_REVISION
        {
            return Err(RawModelError::new("combining profile 参数与登记值不一致"));
        }
        if !COMBINING_MODES.contains(&self.mode.name()) {
            return Err(RawModelError::new("combining 模式不在登记目录中"));
        }
        expect_directory(
            &self.operation_tags,
            &OPERATION_TAGS,
            "combining 冷操作 tag",
        )?;
        expect_directory(
            &self.allowed_uses,
            &COMBINING_ALLOWED_USES,
            "combining 允许用途",
        )?;
        // 允许用途就是 tag 目录：允许项一旦多于已实现 handler，就会出现无人执行的记录。
        if self.allowed_uses != self.operation_tags {
            return Err(RawModelError::new(
                "combining 允许用途与冷操作 tag 目录不一致",
            ));
        }
        expect_directory(
            &self.forbidden_uses,
            &COMBINING_FORBIDDEN_USES,
            "combining 禁止用途",
        )?;
        // 先拒绝把地址放进记录，再比对字段集合：ManagedAddress 之类的漂移必须点名原因，
        // 而不是被一句「字段集合不一致」掩盖。
        for field in &self.record_fields {
            if field.kind.carries_address() {
                return Err(RawModelError::new(format!(
                    "operation record 字段 {} 携带地址，违反标量冷操作契约",
                    field.name
                )));
            }
        }
        if self.record_fields != Self::fixed_record_fields() {
            return Err(RawModelError::new("operation record 字段与登记集合不一致"));
        }
        expect_directory(&self.states, &OPERATION_STATES, "combining 状态")?;
        expect_directory(&self.outcomes, &OPERATION_OUTCOMES, "combining 结局")?;
        // 先逐条点名（未登记状态、空触发名、同源重名），再比对整表：前者给出可诊断的
        // 具体原因，后者负责拒绝「同样是合法状态的另一种排列」这种整段漂移。
        let mut keys: Vec<(&str, &str)> = Vec::with_capacity(self.transitions.len());
        for transition in &self.transitions {
            if !self.states.iter().any(|state| state == &transition.from)
                || !self.states.iter().any(|state| state == &transition.to)
            {
                return Err(RawModelError::new(format!(
                    "combining 迁移引用了未登记状态：{} -> {}",
                    transition.from, transition.to
                )));
            }
            // `recycle` 同时服务 completed 与 cancelled 两个源状态，因此唯一性建立在
            // `(源状态, 触发名)` 上：同一个源状态不允许出现两条同名触发。
            let key = (transition.from.as_str(), transition.trigger.as_str());
            if transition.trigger.is_empty() || keys.contains(&key) {
                return Err(RawModelError::new(format!(
                    "combining 迁移触发名 `{}` 为空或在同一源状态下重复",
                    transition.trigger
                )));
            }
            keys.push(key);
        }
        if self.transitions != fixed_transitions() {
            return Err(RawModelError::new("combining 状态迁移与登记序列不一致"));
        }
        expect_directory(
            &self.fast_paths,
            &COMBINING_FAST_PATHS,
            "combining fast path",
        )?;
        expect_directory(
            &self.wait_paths,
            &COMBINING_WAIT_PATHS,
            "combining wait path",
        )?;
        expect_directory(&self.wait_wake, &COMBINING_WAIT_WAKE, "combining wait/wake")?;
        expect_directory(&self.statistics, &COMBINING_STATISTICS, "combining 统计")?;
        // 预算与记录参数是登记的规范值：任何漂移都必须在这里被拒绝，而不是留给运行时
        // 去发现「预算比契约小」。常量之间的相互约束在文件尾的编译期断言里核对。
        if self.merge_limit != COMBINING_MERGE_LIMIT
            || self.round_item_budget != COMBINING_ROUND_ITEM_BUDGET
            || self.round_byte_budget != COMBINING_ROUND_BYTE_BUDGET
            || self.timeout_rounds != COMBINING_TIMEOUT_ROUNDS
            || self.record_bytes != COMBINING_RECORD_BYTES
            || self.record_chunk_items != COMBINING_RECORD_CHUNK_ITEMS
            || self.refill_reserve_slots != COMBINING_REFILL_RESERVE_SLOTS
            || self.max_operation_chunks != COMBINING_MAX_OPERATION_CHUNKS
        {
            return Err(RawModelError::new(
                "combining 合并上限、轮次预算、超时轮数或记录池参数与登记值不一致",
            ));
        }
        if self.max_operation_chunks < self.demand.chunks {
            return Err(RawModelError::new(
                "combining chunk 上限低于需求推导的 chunk 数",
            ));
        }
        self.demand.verify()?;
        if self.demand.records
            != self.demand.owners.max(1) * self.round_item_budget + self.refill_reserve_slots
        {
            return Err(RawModelError::new("combining 需求记录数与推导公式不一致"));
        }
        if self.demand.chunks != self.demand.records.div_ceil(self.record_chunk_items) {
            return Err(RawModelError::new(
                "combining 需求 chunk 数与推导公式不一致",
            ));
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("combining 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("combining 契约可序列化")
    }

    /// 计算契约指纹。
    ///
    /// `canonical_bytes` 是整段 serde_json（含 `fingerprint` 字段），因此这里对指纹字段
    /// 清零的副本求 hash：否则「先写指纹、再校验相等」永远不可能成立。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.fingerprint = [0; 32];
        *blake3::Hasher::new_derive_key("gugu-combining-runtime-v1")
            .update(&canonical.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回人类可读的契约 dump。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "combining schema={} profile={} revision={} mode={} tags={} merge-limit={} round-items={} round-bytes={} timeout-rounds={}",
            self.schema,
            self.profile,
            self.profile_revision,
            self.mode.name(),
            self.operation_tags.len(),
            self.merge_limit,
            self.round_item_budget,
            self.round_byte_budget,
            self.timeout_rounds
        );
        let _ = writeln!(out, "combining-tags {}", self.operation_tags.join(","));
        let _ = writeln!(out, "combining-allowed {}", self.allowed_uses.join(","));
        let _ = writeln!(out, "combining-forbidden {}", self.forbidden_uses.join(","));
        let record_fields: Vec<String> = self
            .record_fields
            .iter()
            .map(|field| format!("{}:{}", field.name, field.kind.name()))
            .collect();
        let _ = writeln!(out, "combining-record-fields {}", record_fields.join(","));
        let _ = writeln!(out, "combining-states {}", self.states.join(","));
        let _ = writeln!(out, "combining-outcomes {}", self.outcomes.join(","));
        let transitions: Vec<String> = self
            .transitions
            .iter()
            .map(|entry| format!("{}:{}:{}", entry.from, entry.trigger, entry.to))
            .collect();
        let _ = writeln!(out, "combining-transitions {}", transitions.join(","));
        let _ = writeln!(out, "combining-fast-path {}", self.fast_paths.join(","));
        let _ = writeln!(out, "combining-wait-paths {}", self.wait_paths.join(","));
        let _ = writeln!(out, "combining-wait-wake {}", self.wait_wake.join(","));
        let _ = writeln!(out, "combining-statistics {}", self.statistics.join(","));
        let _ = writeln!(
            out,
            "combining-demand owners={} range-sites={} extent-classes={} records={} chunks={}",
            self.demand.owners,
            self.demand.range_sites,
            self.demand.extent_classes,
            self.demand.records,
            self.demand.chunks
        );
        let _ = writeln!(out, "combining-fingerprint {}", hex_lower(self.fingerprint));
        out
    }
}

/// 把静态目录复制成契约里的 owned 名字表。
fn owned_names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// 把固定迁移表复制成契约里的 owned schema。
fn fixed_transitions() -> Vec<TransitionSchema> {
    OPERATION_TRANSITIONS
        .iter()
        .map(|entry| TransitionSchema {
            from: entry.from.to_owned(),
            to: entry.to.to_owned(),
            trigger: entry.trigger.to_owned(),
        })
        .collect()
}

/// 校验目录与登记表逐项一致。
fn expect_directory(
    actual: &[String],
    expected: &[&str],
    label: &str,
) -> Result<(), RawModelError> {
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected.iter())
            .any(|(name, expected)| name != expected)
    {
        return Err(RawModelError::new(format!("{label}目录与登记表不一致")));
    }
    Ok(())
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 登记常量之间的相互约束在编译期核对：预算关系一旦不成立，契约段构造本身就失败。
///
/// verifier 只负责拒绝「与登记值不同」的契约，这里负责保证登记值自身自洽，因此两者
/// 合起来既拒绝漂移，也拒绝把一个自相矛盾的常量集写进 profile。
const _: () = {
    assert!(COMBINING_MERGE_LIMIT >= 2);
    assert!(COMBINING_MERGE_LIMIT <= COMBINING_RECORD_CHUNK_ITEMS);
    assert!(COMBINING_ROUND_ITEM_BUDGET >= 1);
    assert!(COMBINING_TIMEOUT_ROUNDS >= 1);
    assert!(COMBINING_RECORD_BYTES > 0);
    assert!(COMBINING_RECORD_BYTES.is_multiple_of(8));
    assert!(COMBINING_REFILL_RESERVE_SLOTS >= 1);
    assert!(COMBINING_REFILL_RESERVE_SLOTS < COMBINING_RECORD_CHUNK_ITEMS);
    assert!(
        COMBINING_ROUND_BYTE_BUDGET
            >= COMBINING_RECORD_BYTES as u64 * COMBINING_ROUND_ITEM_BUDGET as u64
    );
    assert!(COMBINING_ROUND_BYTE_BUDGET.is_multiple_of(COMBINING_RECORD_BYTES as u64));
    assert!(COMBINING_MAX_OPERATION_CHUNKS >= 1);
};
