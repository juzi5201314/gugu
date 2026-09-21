//! 外调交接契约：BlockingBridge 额度、dirty target 与 poller 分流。
//!
//! 数字来自调度器内部规范里的 tuning profile。参照状态机在 `foreign`，不能另写一套上限。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::model::RawModelError;

/// 外调契约段的 schema 版本。
pub(crate) const FOREIGN_SCHEMA: u32 = 1;
/// 同时执行普通 blocking native 的 worker 上限。
pub(crate) const MAX_BLOCKING_WORKERS: u32 = 8;
/// 每个 admission waiter 的元数据字节。
pub(crate) const BLOCKING_WAITER_BYTES: u32 = 64;
/// waiter 队列的字节上限；超出后 admission 以资源耗尽失败。
pub(crate) const BLOCKING_QUEUE_CAP_BYTES: u32 = 4096;
/// 一次 service 至多发放的 credit 数。
pub(crate) const BLOCKING_SERVICE_BUDGET: u32 = 128;
/// 每个 blocking worker 的 system stack 字节。
pub(crate) const BLOCKING_WORKER_STACK_BYTES: u32 = 65_536;
/// runnable 压力打破 attached lease 前的逻辑宽限，单位是调用方给出的逻辑微秒。
pub(crate) const RETAKE_GRACE_US: u64 = 20;

/// 从优化后 LIR / GIR 数出来的外调与汇编站点。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ForeignDemand {
    /// 普通 `ForeignBridge` 调用点。
    pub ordinary: u32,
    /// `ForeignBridge[DirtyCpu]` 调用点。
    pub dirty: u32,
    /// 直接 `ForeignLeaf` 调用点。
    pub leaf: u32,
    /// 受管 inline `asm` 站点。
    pub asm: u32,
    /// 模块级 `global_asm` 声明。
    pub global_asm: u32,
}

/// 已验证的外调 runtime 契约。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ForeignRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// 普通 blocking worker 上限。
    pub max_blocking_workers: u32,
    /// 每个 waiter 的元数据字节。
    pub waiter_bytes: u32,
    /// waiter 队列字节上限。
    pub queue_cap_bytes: u32,
    /// 一次 service 的 credit 预算。
    pub service_budget: u32,
    /// blocking worker 的 system stack 字节。
    pub worker_stack_bytes: u32,
    /// 压力 retake 的逻辑宽限。
    pub retake_grace_us: u64,
    /// 上游需求。
    pub demand: ForeignDemand,
}

impl ForeignRuntimeContract {
    pub(crate) fn build(demand: ForeignDemand) -> Result<Self, RawModelError> {
        let contract = Self {
            schema: FOREIGN_SCHEMA,
            max_blocking_workers: MAX_BLOCKING_WORKERS,
            waiter_bytes: BLOCKING_WAITER_BYTES,
            queue_cap_bytes: BLOCKING_QUEUE_CAP_BYTES,
            service_budget: BLOCKING_SERVICE_BUDGET,
            worker_stack_bytes: BLOCKING_WORKER_STACK_BYTES,
            retake_grace_us: RETAKE_GRACE_US,
            demand,
        };
        contract.verify()?;
        Ok(contract)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != FOREIGN_SCHEMA
            || self.max_blocking_workers != MAX_BLOCKING_WORKERS
            || self.waiter_bytes != BLOCKING_WAITER_BYTES
            || self.queue_cap_bytes != BLOCKING_QUEUE_CAP_BYTES
            || self.service_budget != BLOCKING_SERVICE_BUDGET
            || self.worker_stack_bytes != BLOCKING_WORKER_STACK_BYTES
            || self.retake_grace_us != RETAKE_GRACE_US
        {
            return Err(RawModelError::new(
                "外调 worker、waiter 或 retake 宽限与登记 profile 不一致",
            ));
        }
        if self.queue_cap_bytes % self.waiter_bytes != 0 {
            return Err(RawModelError::new(
                "BlockingBridge 队列容量不是 waiter 字节的整数倍",
            ));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("外调契约可序列化")
    }

    /// 外调契约的域隔离内容身份。
    pub fn fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-foreign-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "foreign schema={} blocking-workers={} waiter-bytes={} queue-cap={} grace-us={}",
            self.schema,
            self.max_blocking_workers,
            self.waiter_bytes,
            self.queue_cap_bytes,
            self.retake_grace_us,
        )
        .expect("String写入");
        writeln!(
            output,
            "foreign-demand ordinary={} dirty={} leaf={} asm={} global-asm={}",
            self.demand.ordinary,
            self.demand.dirty,
            self.demand.leaf,
            self.demand.asm,
            self.demand.global_asm,
        )
        .expect("String写入");
        output
    }
}
