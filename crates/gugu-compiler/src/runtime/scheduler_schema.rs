//! M:N 调度基础路径的同源契约；backend 和 CLI 只消费已验证对象。
//!
//! 本段把 [`crate::runtime`] 的 local 容量、remote 分片数、batch 上限、service 节奏以及
//! `LogicalProcessorPrefix` 的 poll/ownership/TLAB/TurnRegion 偏移固定成带版本的对象，
//! 与 `coroutine_schema` 共用 `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//! timer/poller/monitor/GC-stop/foreign-lease 的完整状态机由后续模块消费同一原语；select
//! 等待协议由 `WaitRuntimeContract` 固定。偏移只来自 `processor` 布局神谕的 `offset_of!`。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::model::RawModelError;
use super::processor;
use super::{BATCH_MAX, CACHE_LINE_BYTES, OWNER_INBOX_SHARDS, QUEUE_PAD_BYTES};

/// local deque 的编码变体；profile 选定一种，release 镜像不生成运行时 mode 分支。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalDequeMode {
    /// 经典 64-bit ticket 变体 `Classic64Deque`。
    Classic64,
    /// 单 `u64` 打包 55-bit steal ticket 与 9-bit 距离的 `Packed55Deque`。
    #[expect(
        dead_code,
        reason = "当前 profile 选择 Classic64；Packed55 由调度参照实现登记，供后续 profile 校准选择"
    )]
    Packed55,
}

/// 调优 profile 的 revision；任何字段变更都属于 backend schema 变更。
pub(crate) const TUNING_PROFILE_REVISION: u32 = 1;

/// 调度调优参数；[`RUNTIME_TUNING_PROFILE`] 是唯一来源，其余常量从这里派生。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeTuningProfile {
    /// local deque 的编码变体。
    pub(crate) deque_mode: LocalDequeMode,
    /// 本地队列容量。
    pub(crate) local_capacity: u32,
    /// remote batch head 分片数。
    pub(crate) remote_shards: u32,
    /// 单个 batch 的 item 上限。
    pub(crate) batch_max: u32,
    /// external service 的 tick 间隔。
    pub(crate) service_interval: u32,
    /// 一次 external service 至多写入 local 的项数。
    pub(crate) service_batch: u32,
    /// 高争用元数据的填充粒度。
    pub(crate) queue_pad_bytes: u64,
    /// 缓存行字节数。
    pub(crate) cache_line_bytes: u64,
    /// profile revision。
    pub(crate) revision: u32,
}

/// 当前登记的调度调优 profile。
pub(crate) const RUNTIME_TUNING_PROFILE: RuntimeTuningProfile = RuntimeTuningProfile {
    deque_mode: LocalDequeMode::Classic64,
    local_capacity: 256,
    remote_shards: 8,
    batch_max: 128,
    service_interval: 61,
    service_batch: 128,
    queue_pad_bytes: QUEUE_PAD_BYTES,
    cache_line_bytes: CACHE_LINE_BYTES,
    revision: TUNING_PROFILE_REVISION,
};

impl RuntimeTuningProfile {
    /// 域隔离的内容身份。
    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-runtime-tuning-profile-v1");
        hasher.update(&[match self.deque_mode {
            LocalDequeMode::Classic64 => 0,
            LocalDequeMode::Packed55 => 1,
        }]);
        hasher.update(&self.local_capacity.to_le_bytes());
        hasher.update(&self.remote_shards.to_le_bytes());
        hasher.update(&self.batch_max.to_le_bytes());
        hasher.update(&self.service_interval.to_le_bytes());
        hasher.update(&self.service_batch.to_le_bytes());
        hasher.update(&self.queue_pad_bytes.to_le_bytes());
        hasher.update(&self.cache_line_bytes.to_le_bytes());
        hasher.update(&self.revision.to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// 本地队列容量；与 `docs/src/internals/scheduler.md` 字面一致。
pub(crate) const SCHED_LOCAL_CAPACITY: u32 = RUNTIME_TUNING_PROFILE.local_capacity;
/// remote batch head 分片数；必须等于 `OWNER_INBOX_SHARDS`。
pub(crate) const SCHED_REMOTE_SHARDS: u32 = RUNTIME_TUNING_PROFILE.remote_shards;
/// 单个 batch 的 item 上限；必须等于 `BATCH_MAX`。
pub(crate) const SCHED_BATCH_MAX: u32 = RUNTIME_TUNING_PROFILE.batch_max;
/// external service 的 tick 间隔。
pub(crate) const SCHED_SERVICE_INTERVAL: u32 = RUNTIME_TUNING_PROFILE.service_interval;
/// 一次 external service 至多写入 local 的项数。
pub(crate) const SCHED_SERVICE_BATCH: u32 = RUNTIME_TUNING_PROFILE.service_batch;
/// 调度契约段的 schema 版本。
pub(crate) const SCHEDULER_SCHEMA: u32 = 2;

/// 从真实 LIR 推导的调度需求，不是运行时协程数量。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SchedulerDemand {
    /// 协程创建点数量；复用 `coroutine_demand().creation_sites`。
    pub spawn_sites: u32,
    /// LIR 全 body 中 `RuntimeCall::Yield` 出现次数。
    pub yield_sites: u32,
    /// 显式挂起点数量；复用 `coroutine_demand().suspend_points`。
    pub suspend_points: u32,
}

/// 已验证的调度 runtime 契约；版本变化使 RuntimeRawModel 和 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SchedulerRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// 本地队列容量。
    pub local_capacity: u32,
    /// remote batch head 分片数。
    pub remote_shards: u32,
    /// 单个 batch 的 item 上限。
    pub batch_max: u32,
    /// external service 的 tick 间隔。
    pub service_interval: u32,
    /// 一次 external service 的写入上限。
    pub service_batch: u32,
    /// 高争用元数据的填充粒度。
    pub queue_pad_bytes: u64,
    /// 缓存行字节数。
    pub cache_line_bytes: u64,
    /// `[r15 + poll_flags]` 的字节偏移。
    pub poll_flags_offset: u32,
    /// `[r15 + ownership]` 的字节偏移。
    pub ownership_offset: u32,
    /// `[r15 + tlab.cursor]` 的字节偏移。
    pub tlab_cursor_offset: u32,
    /// `[r15 + tlab.limit]` 的字节偏移。
    pub tlab_limit_offset: u32,
    /// `[r15 + turn_region.cursor]` 的字节偏移。
    pub turn_region_cursor_offset: u32,
    /// `[r15 + turn_region.limit]` 的字节偏移。
    pub turn_region_limit_offset: u32,
    /// 上游 LIR 需求。
    pub demand: SchedulerDemand,
}

impl SchedulerRuntimeContract {
    /// 返回内部 schema 版本。
    pub fn schema(&self) -> u32 {
        self.schema
    }
    /// 返回本地队列容量。
    pub fn local_capacity(&self) -> u32 {
        self.local_capacity
    }
    /// 返回 remote 分片数。
    pub fn remote_shards(&self) -> u32 {
        self.remote_shards
    }
    /// 返回 batch 上限。
    pub fn batch_max(&self) -> u32 {
        self.batch_max
    }
    /// 返回 service 间隔。
    pub fn service_interval(&self) -> u32 {
        self.service_interval
    }
    /// 返回 service 批量。
    pub fn service_batch(&self) -> u32 {
        self.service_batch
    }
    /// 返回 poll_flags 偏移。
    pub fn poll_flags_offset(&self) -> u32 {
        self.poll_flags_offset
    }
    /// 返回 ownership 偏移。
    pub fn ownership_offset(&self) -> u32 {
        self.ownership_offset
    }
    /// 返回 TLAB cursor 偏移。
    pub fn tlab_cursor_offset(&self) -> u32 {
        self.tlab_cursor_offset
    }
    /// 返回 TLAB limit 偏移。
    pub fn tlab_limit_offset(&self) -> u32 {
        self.tlab_limit_offset
    }
    /// 返回 TurnRegion cursor 偏移。
    pub fn turn_region_cursor_offset(&self) -> u32 {
        self.turn_region_cursor_offset
    }
    /// 返回 TurnRegion limit 偏移。
    pub fn turn_region_limit_offset(&self) -> u32 {
        self.turn_region_limit_offset
    }
    pub(crate) fn build(demand: SchedulerDemand) -> Result<Self, RawModelError> {
        let contract = Self {
            schema: SCHEDULER_SCHEMA,
            local_capacity: SCHED_LOCAL_CAPACITY,
            remote_shards: SCHED_REMOTE_SHARDS,
            batch_max: SCHED_BATCH_MAX,
            service_interval: SCHED_SERVICE_INTERVAL,
            service_batch: SCHED_SERVICE_BATCH,
            queue_pad_bytes: QUEUE_PAD_BYTES,
            cache_line_bytes: CACHE_LINE_BYTES,
            poll_flags_offset: processor::poll_flags_offset(),
            ownership_offset: processor::ownership_offset(),
            tlab_cursor_offset: processor::tlab_cursor_offset(),
            tlab_limit_offset: processor::tlab_limit_offset(),
            turn_region_cursor_offset: processor::turn_region_cursor_offset(),
            turn_region_limit_offset: processor::turn_region_limit_offset(),
            demand,
        };
        contract.verify()?;
        Ok(contract)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != SCHEDULER_SCHEMA
            || self.local_capacity != SCHED_LOCAL_CAPACITY
            || self.remote_shards != SCHED_REMOTE_SHARDS
            || self.batch_max != SCHED_BATCH_MAX
            || self.service_interval != SCHED_SERVICE_INTERVAL
            || self.service_batch != SCHED_SERVICE_BATCH
            || self.queue_pad_bytes != QUEUE_PAD_BYTES
            || self.cache_line_bytes != CACHE_LINE_BYTES
            || self.poll_flags_offset != processor::poll_flags_offset()
            || self.ownership_offset != processor::ownership_offset()
            || self.tlab_cursor_offset != processor::tlab_cursor_offset()
            || self.tlab_limit_offset != processor::tlab_limit_offset()
            || self.turn_region_cursor_offset != processor::turn_region_cursor_offset()
            || self.turn_region_limit_offset != processor::turn_region_limit_offset()
        {
            return Err(RawModelError::new(
                "调度容量、分片、batch、service 节奏或 processor 偏移与 runtime/调度器契约不一致",
            ));
        }
        if self.remote_shards != OWNER_INBOX_SHARDS {
            return Err(RawModelError::new(
                "调度 remote 分片数与 owner inbox 分片数不一致",
            ));
        }
        if self.batch_max != BATCH_MAX {
            return Err(RawModelError::new("调度 batch 上限与登记值不一致"));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("调度契约可序列化")
    }

    /// 调度契约的域隔离内容身份。
    pub fn fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-scheduler-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "scheduler schema={} local={} shards={} batch={} service-interval={} service-batch={}",
            self.schema,
            self.local_capacity,
            self.remote_shards,
            self.batch_max,
            self.service_interval,
            self.service_batch,
        )
        .expect("String写入");
        writeln!(
            output,
            "scheduler-layout poll-flags={} tlab-cursor={} turn-region-cursor={}",
            self.poll_flags_offset, self.tlab_cursor_offset, self.turn_region_cursor_offset,
        )
        .expect("String写入");
        writeln!(
            output,
            "scheduler-demand spawn={} yield={} suspend={}",
            self.demand.spawn_sites, self.demand.yield_sites, self.demand.suspend_points,
        )
        .expect("String写入");
        output
    }
}
