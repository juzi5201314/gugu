//! M:N 调度基础路径的同源契约；backend 和 CLI 只消费已验证对象。
//!
//! 本段把 [`crate::runtime`] 的 local 容量、remote 分片数、batch 上限、service 节奏固定成
//! 带版本的对象，与 `coroutine_schema` 共用 `build`/`verify`/`canonical_bytes`/
//! `fingerprint`/`dump` 闭环。timer/poller/monitor/GC-stop/foreign-lease/select 的完整
//! 状态机由后续阶段消费同一原语，本阶段契约不为它们预留空字节；`PollControl` 的 epoch 槽
//! 与 `PREEMPT`/`GC_STOP` 位定义只以注释形式落在调度参考模型的 `ProcessorRecord` 中。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::model::RawModelError;
use super::{BATCH_MAX, CACHE_LINE_BYTES, OWNER_INBOX_SHARDS, QUEUE_PAD_BYTES};

/// 本地队列容量；与 `docs/src/internals/scheduler.md` 字面一致。
pub(crate) const SCHED_LOCAL_CAPACITY: u32 = 256;
/// remote batch head 分片数；必须等于 `OWNER_INBOX_SHARDS`。
pub(crate) const SCHED_REMOTE_SHARDS: u32 = 8;
/// 单个 batch 的 item 上限；必须等于 `BATCH_MAX`。
pub(crate) const SCHED_BATCH_MAX: u32 = 128;
/// external service 的 tick 间隔。
pub(crate) const SCHED_SERVICE_INTERVAL: u32 = 61;
/// 一次 external service 至多写入 local 的项数。
pub(crate) const SCHED_SERVICE_BATCH: u32 = 128;
/// 调度契约段的 schema 版本。
pub(crate) const SCHEDULER_SCHEMA: u32 = 1;

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
        {
            return Err(RawModelError::new(
                "调度容量、分片、batch 或 service 节奏与 runtime/调度器契约不一致",
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
            "scheduler-demand spawn={} yield={} suspend={}",
            self.demand.spawn_sites, self.demand.yield_sites, self.demand.suspend_points,
        )
        .expect("String写入");
        output
    }
}
