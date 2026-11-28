//! 外调交接契约：BlockingBridge 额度、dirty target 与 poller 分流。
//!
//! 数字来自调度器内部规范里的 tuning profile。参照状态机在 `foreign`，不能另写一套上限。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::model::RawModelError;

/// 外调契约段的 schema 版本。
pub const FOREIGN_SCHEMA: u32 = 4;
/// 当前镜像单 processor 的独立 dirty target；不消费 blocking credit。
pub const MAX_DIRTY_WORKERS: u32 = 1;
/// 两池共享映射页但保持各自 FIFO 和请求所有权。
pub const DIRTY_POOL_OFFSET: u32 = 2048;
/// 内存压力期间保留一个执行槽推进队首；在途 worker 只自然排空。
pub const PRESSURE_BLOCKING_WORKERS: u32 = 1;
/// 同时执行普通 blocking native 的 worker 上限。
pub const MAX_BLOCKING_WORKERS: u32 = 8;
/// 每个 admission waiter 的元数据字节。
pub const BLOCKING_WAITER_BYTES: u32 = 64;
/// waiter 队列的字节上限；超出后 admission 以资源耗尽失败。
pub const BLOCKING_QUEUE_CAP_BYTES: u32 = 4096;
/// 一次 service 至多发放的 credit 数。
pub const BLOCKING_SERVICE_BUDGET: u32 = 128;
/// 每个 blocking worker 的 system stack 字节。
pub const BLOCKING_WORKER_STACK_BYTES: u32 = 65_536;
/// runnable 压力打破 attached lease 前的逻辑宽限，单位是调用方给出的逻辑微秒。
pub const RETAKE_GRACE_US: u64 = 20;

/// non-moving native 请求。owner 非零即占用一个 worker credit；只有确认线程退出后归零。
/// 地址字段均为 raw 地址，不允许保存 GC 对象或 coroutine stack 地址。
#[repr(C, align(8))]
pub struct NativeRequest {
    pub thread_id: u64,
    pub operation: u64,
    pub arguments: [u64; 6],
    pub value: u64,
    pub error: u64,
    pub thread_handle: u64,
    pub extra_arguments: [u64; 2],
    pub owner: u64,
    pub cancel: u64,
    pub cancelled: u64,
}

pub const NATIVE_REQUEST_BYTES: usize = size_of::<NativeRequest>();
pub const NATIVE_REQUEST_OPERATION: usize = std::mem::offset_of!(NativeRequest, operation);
pub const NATIVE_REQUEST_ARGUMENTS: usize = std::mem::offset_of!(NativeRequest, arguments);
pub const NATIVE_REQUEST_VALUE: usize = std::mem::offset_of!(NativeRequest, value);
pub const NATIVE_REQUEST_ERROR: usize = std::mem::offset_of!(NativeRequest, error);
pub const NATIVE_REQUEST_THREAD_HANDLE: usize = std::mem::offset_of!(NativeRequest, thread_handle);
pub const NATIVE_REQUEST_EXTRA_ARGUMENTS: usize =
    std::mem::offset_of!(NativeRequest, extra_arguments);
pub const NATIVE_REQUEST_OWNER: usize = std::mem::offset_of!(NativeRequest, owner);
pub const NATIVE_REQUEST_CANCEL: usize = std::mem::offset_of!(NativeRequest, cancel);
pub const NATIVE_REQUEST_CANCELLED: usize = std::mem::offset_of!(NativeRequest, cancelled);

/// 单 managed owner 的原生映射账本，位于 processor 页的保留区。
#[repr(C)]
pub struct NativeMemoryLedger {
    pub reserved: u64,
    pub committed: u64,
    pub mappings: u64,
    pub peak_committed: u64,
}
pub const NATIVE_MEMORY_LEDGER_OFFSET: usize = 3968;

/// 单 managed processor 的平台映射总账本，包含启动限制与 managed heap 提交分区。
#[repr(C)]
pub struct RuntimeMappingLedger {
    pub reserved: u64,
    pub committed: u64,
    pub mappings: u64,
    pub peak_committed: u64,
    pub memory_limit: u64,
    pub heap_committed: u64,
}
pub const RUNTIME_MAPPING_LEDGER_OFFSET: usize = 3920;

/// 用户协程栈分类，系统栈不进入此分区。
#[repr(C)]
pub struct RuntimeStackLedger {
    pub reserved: u64,
    pub committed: u64,
    pub live: u64,
}
pub const RUNTIME_STACK_LEDGER_OFFSET: usize = 3864;

/// 映射压力的延迟消费状态；不在平台提交事务中重入收集器。
#[repr(C)]
pub struct RuntimePressureState {
    pub state: u64,
    pub episodes: u64,
    pub cycles: u64,
    pub ready: u64,
}
pub const RUNTIME_PRESSURE_OFFSET: usize = 3888;

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
    /// 阻塞 I/O 原语站点。每条同时计入 `ordinary`。
    pub blocking_io: u32,
}

/// 已验证的外调 runtime 契约。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ForeignRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// 普通 blocking worker 上限。
    pub max_blocking_workers: u32,
    /// 压力期间的普通 blocking 准入目标；不会撤销在途 credit。
    pub pressure_blocking_workers: u32,
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
    /// 机器请求记录大小与字段偏移；顺序由 native_request_layout 固定。
    pub native_request_layout: [u32; 10],
    /// 单 processor dirty 池的执行上限与同页偏移。
    pub dirty_pool: [u32; 2],
    /// 普通与 dirty C worker 共同消费的 native 包布局。
    pub packet_layout: [u32; 7],
    /// 上游需求。
    pub demand: ForeignDemand,
}

fn native_request_layout() -> [u32; 10] {
    [
        NATIVE_REQUEST_BYTES,
        NATIVE_REQUEST_OPERATION,
        NATIVE_REQUEST_ARGUMENTS,
        NATIVE_REQUEST_VALUE,
        NATIVE_REQUEST_ERROR,
        NATIVE_REQUEST_THREAD_HANDLE,
        NATIVE_REQUEST_EXTRA_ARGUMENTS,
        NATIVE_REQUEST_OWNER,
        NATIVE_REQUEST_CANCEL,
        NATIVE_REQUEST_CANCELLED,
    ]
    .map(|offset| u32::try_from(offset).expect("native 请求布局适配 u32"))
}

impl ForeignRuntimeContract {
    pub fn build(demand: ForeignDemand) -> Result<Self, RawModelError> {
        let contract = Self {
            schema: FOREIGN_SCHEMA,
            max_blocking_workers: MAX_BLOCKING_WORKERS,
            pressure_blocking_workers: PRESSURE_BLOCKING_WORKERS,
            waiter_bytes: BLOCKING_WAITER_BYTES,
            queue_cap_bytes: BLOCKING_QUEUE_CAP_BYTES,
            service_budget: BLOCKING_SERVICE_BUDGET,
            worker_stack_bytes: BLOCKING_WORKER_STACK_BYTES,
            retake_grace_us: RETAKE_GRACE_US,
            native_request_layout: native_request_layout(),
            dirty_pool: [MAX_DIRTY_WORKERS, DIRTY_POOL_OFFSET],
            packet_layout: super::foreign_packet::layout(),
            demand,
        };
        contract.verify()?;
        Ok(contract)
    }

    pub fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != FOREIGN_SCHEMA
            || self.max_blocking_workers != MAX_BLOCKING_WORKERS
            || self.pressure_blocking_workers != PRESSURE_BLOCKING_WORKERS
            || self.waiter_bytes != BLOCKING_WAITER_BYTES
            || self.queue_cap_bytes != BLOCKING_QUEUE_CAP_BYTES
            || self.service_budget != BLOCKING_SERVICE_BUDGET
            || self.worker_stack_bytes != BLOCKING_WORKER_STACK_BYTES
            || self.retake_grace_us != RETAKE_GRACE_US
            || self.native_request_layout != native_request_layout()
            || self.dirty_pool != [MAX_DIRTY_WORKERS, DIRTY_POOL_OFFSET]
            || self.packet_layout != super::foreign_packet::layout()
        {
            return Err(RawModelError::new(
                "外调 worker、waiter 或 retake 宽限与登记 profile 不一致",
            ));
        }
        if !self.queue_cap_bytes.is_multiple_of(self.waiter_bytes) {
            return Err(RawModelError::new(
                "BlockingBridge 队列容量不是 waiter 字节的整数倍",
            ));
        }
        if self.demand.blocking_io > self.demand.ordinary {
            return Err(RawModelError::new("阻塞 I/O 站点超过普通 bridge 站点"));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::canonical::to_vec(self).expect("外调契约可序列化")
    }

    /// 外调契约的域隔离内容身份。
    pub fn fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-foreign-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub fn dump(&self) -> String {
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
            "foreign-demand ordinary={} dirty={} leaf={} asm={} global-asm={} blocking-io={}",
            self.demand.ordinary,
            self.demand.dirty,
            self.demand.leaf,
            self.demand.asm,
            self.demand.global_asm,
            self.demand.blocking_io,
        )
        .expect("String写入");
        output
    }
}

#[cfg(test)]
mod image_profile_tests {
    use super::*;

    #[test]
    fn runtime_stack_ledger_matches_image_storage() {
        let source = include_str!("../../gugu-frontend/resources/runtime/native_memory.gg");
        for (name, offset) in [
            (
                "STACK_RESERVED_OFFSET",
                std::mem::offset_of!(RuntimeStackLedger, reserved),
            ),
            (
                "STACK_COMMITTED_OFFSET",
                std::mem::offset_of!(RuntimeStackLedger, committed),
            ),
            (
                "STACK_LIVE_OFFSET",
                std::mem::offset_of!(RuntimeStackLedger, live),
            ),
        ] {
            assert!(source.lines().any(|line| line
                == format!(
                    "pub const {name}: uint = {}",
                    RUNTIME_STACK_LEDGER_OFFSET + offset
                )));
        }
        use crate::processor::{PROCESSOR_COUNTERS_COUNT, PROCESSOR_COUNTERS_OFFSET};
        assert_eq!(
            PROCESSOR_COUNTERS_OFFSET + PROCESSOR_COUNTERS_COUNT * 8,
            RUNTIME_STACK_LEDGER_OFFSET
        );
        let counters = include_str!("../../gugu-frontend/resources/runtime/counters.gg");
        for (name, value) in [
            ("COUNTERS_OFFSET", PROCESSOR_COUNTERS_OFFSET),
            ("COUNTERS_COUNT", PROCESSOR_COUNTERS_COUNT),
        ] {
            assert!(
                counters
                    .lines()
                    .any(|line| line == format!("pub const {name}: uint = {value}"))
            );
        }
        let scheduler = include_str!("../../gugu-frontend/resources/runtime/scheduler.gg");
        for (name, value) in [
            (
                "PROCESSOR_COROUTINE_REGISTRY_OFFSET",
                crate::processor::PROCESSOR_COROUTINE_REGISTRY_OFFSET,
            ),
            (
                "PROCESSOR_RETIRED_STACKS_OFFSET",
                crate::processor::PROCESSOR_RETIRED_STACKS_OFFSET,
            ),
        ] {
            assert!(
                scheduler
                    .lines()
                    .any(|line| line == format!("pub const {name}: uint = {value}"))
            );
        }
        assert_eq!(
            RUNTIME_STACK_LEDGER_OFFSET + size_of::<RuntimeStackLedger>(),
            RUNTIME_PRESSURE_OFFSET
        );
    }

    #[test]
    fn runtime_pressure_matches_layout_and_pacing_profile() {
        let source = include_str!("../../gugu-frontend/resources/runtime/native_memory.gg");
        for (name, value) in [
            ("PRESSURE_DRAIN_MASK_OFFSET", 3832),
            ("SCHEDULER_COMMITTED_END_OFFSET", 3840),
            (
                "OWNER_DRAIN_BYTES",
                crate::pacing_schema::OWNER_DRAIN_BYTES as usize,
            ),
            (
                "RUNTIME_PRESSURE_STATE_OFFSET",
                RUNTIME_PRESSURE_OFFSET + std::mem::offset_of!(RuntimePressureState, state),
            ),
            (
                "RUNTIME_PRESSURE_EPISODES_OFFSET",
                RUNTIME_PRESSURE_OFFSET + std::mem::offset_of!(RuntimePressureState, episodes),
            ),
            (
                "RUNTIME_PRESSURE_CYCLES_OFFSET",
                RUNTIME_PRESSURE_OFFSET + std::mem::offset_of!(RuntimePressureState, cycles),
            ),
            (
                "RUNTIME_PRESSURE_READY_OFFSET",
                RUNTIME_PRESSURE_OFFSET + std::mem::offset_of!(RuntimePressureState, ready),
            ),
            (
                "RUNTIME_PRESSURE_ENTER_RATIO",
                crate::pacing_schema::PRESSURE_ENTER_RATIO as usize,
            ),
            (
                "RUNTIME_PRESSURE_CLEAR_RATIO",
                crate::pacing_schema::PRESSURE_CLEAR_RATIO as usize,
            ),
        ] {
            let expected = format!("pub const {name}: uint = {value}");
            assert!(source.lines().any(|line| line == expected), "{name}");
        }
        assert_eq!(
            RUNTIME_PRESSURE_OFFSET + size_of::<RuntimePressureState>(),
            RUNTIME_MAPPING_LEDGER_OFFSET
        );
    }

    #[test]
    fn runtime_mapping_ledger_matches_image_storage() {
        let source = include_str!("../../gugu-frontend/resources/runtime/native_memory.gg");
        for (name, offset) in [
            (
                "RUNTIME_RESERVED_OFFSET",
                std::mem::offset_of!(RuntimeMappingLedger, reserved),
            ),
            (
                "RUNTIME_COMMITTED_OFFSET",
                std::mem::offset_of!(RuntimeMappingLedger, committed),
            ),
            (
                "RUNTIME_MAPPINGS_OFFSET",
                std::mem::offset_of!(RuntimeMappingLedger, mappings),
            ),
            (
                "RUNTIME_PEAK_OFFSET",
                std::mem::offset_of!(RuntimeMappingLedger, peak_committed),
            ),
            (
                "HEAP_COMMITTED_OFFSET",
                std::mem::offset_of!(RuntimeMappingLedger, heap_committed),
            ),
            (
                "RUNTIME_LIMIT_OFFSET",
                std::mem::offset_of!(RuntimeMappingLedger, memory_limit),
            ),
        ] {
            let expected = format!(
                "pub const {name}: uint = {}",
                RUNTIME_MAPPING_LEDGER_OFFSET + offset
            );
            assert!(source.lines().any(|line| line == expected), "{name}");
        }
        assert!(
            RUNTIME_MAPPING_LEDGER_OFFSET + size_of::<RuntimeMappingLedger>()
                <= NATIVE_MEMORY_LEDGER_OFFSET
        );
    }

    #[test]
    fn native_memory_ledger_matches_image_storage() {
        let source = include_str!("../../gugu-frontend/resources/runtime/native_memory.gg");
        for (name, offset) in [
            (
                "NATIVE_RESERVED_OFFSET",
                std::mem::offset_of!(NativeMemoryLedger, reserved),
            ),
            (
                "NATIVE_COMMITTED_OFFSET",
                std::mem::offset_of!(NativeMemoryLedger, committed),
            ),
            (
                "NATIVE_MAPPINGS_OFFSET",
                std::mem::offset_of!(NativeMemoryLedger, mappings),
            ),
            (
                "NATIVE_PEAK_OFFSET",
                std::mem::offset_of!(NativeMemoryLedger, peak_committed),
            ),
        ] {
            let expected = format!(
                "pub const {name}: uint = {}",
                NATIVE_MEMORY_LEDGER_OFFSET + offset
            );
            assert!(source.lines().any(|line| line == expected), "{name}");
        }
        assert_eq!(size_of::<NativeMemoryLedger>(), 32);
        assert!(NATIVE_MEMORY_LEDGER_OFFSET + size_of::<NativeMemoryLedger>() <= 4016);
    }

    #[test]
    fn native_request_matches_image_fields_and_rejects_drift() {
        let source = include_str!("../../gugu-frontend/resources/runtime/fs_platform.gg");
        for (name, value) in [
            ("REQUEST_BYTES", NATIVE_REQUEST_BYTES),
            ("REQUEST_OPERATION", NATIVE_REQUEST_OPERATION),
            ("REQUEST_ARGUMENTS", NATIVE_REQUEST_ARGUMENTS),
            ("REQUEST_VALUE", NATIVE_REQUEST_VALUE),
            ("REQUEST_ERROR", NATIVE_REQUEST_ERROR),
            ("REQUEST_THREAD_HANDLE", NATIVE_REQUEST_THREAD_HANDLE),
            ("REQUEST_EXTRA_ARGUMENTS", NATIVE_REQUEST_EXTRA_ARGUMENTS),
            ("REQUEST_OWNER", NATIVE_REQUEST_OWNER),
            ("REQUEST_CANCEL", NATIVE_REQUEST_CANCEL),
            ("REQUEST_CANCELLED", NATIVE_REQUEST_CANCELLED),
        ] {
            assert!(
                source
                    .lines()
                    .any(|line| line == format!("const {name}: uint = {value}")),
                "{name}"
            );
        }
        assert_eq!(NATIVE_REQUEST_BYTES, 128);
        let contract = ForeignRuntimeContract::build(ForeignDemand::default()).unwrap();
        for index in 0..contract.native_request_layout.len() {
            let mut changed = contract.clone();
            changed.native_request_layout[index] += 8;
            assert!(changed.verify().is_err());
            assert_ne!(contract.fingerprint(), changed.fingerprint());
        }
    }

    #[test]
    fn file_bridge_consumes_registered_blocking_profile() {
        let source = include_str!("../../gugu-frontend/resources/runtime/fs_platform.gg");
        for (name, expected) in [
            ("MAX_BLOCKING_WORKERS", MAX_BLOCKING_WORKERS),
            ("MAX_DIRTY_WORKERS", MAX_DIRTY_WORKERS),
            ("DIRTY_POOL_OFFSET", DIRTY_POOL_OFFSET),
            ("PRESSURE_BLOCKING_WORKERS", PRESSURE_BLOCKING_WORKERS),
            ("BLOCKING_WAITER_BYTES", BLOCKING_WAITER_BYTES),
            ("BLOCKING_QUEUE_CAP_BYTES", BLOCKING_QUEUE_CAP_BYTES),
            ("BLOCKING_SERVICE_BUDGET", BLOCKING_SERVICE_BUDGET),
            ("BLOCKING_WORKER_STACK_BYTES", BLOCKING_WORKER_STACK_BYTES),
        ] {
            let prefix = format!("pub const {name}: uint = ");
            let value = source
                .lines()
                .find_map(|line| line.strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("镜像 profile 缺少 {name}"));
            assert_eq!(
                value.parse::<u32>().expect("profile 为固定无符号值"),
                expected,
                "{name}"
            );
        }
        const {
            assert!(
                PRESSURE_BLOCKING_WORKERS > 0 && PRESSURE_BLOCKING_WORKERS <= MAX_BLOCKING_WORKERS
            )
        };
        let contract = ForeignRuntimeContract::build(ForeignDemand::default()).unwrap();
        let mut changed = contract.clone();
        changed.pressure_blocking_workers += 1;
        assert!(changed.verify().is_err());
        assert_ne!(contract.fingerprint(), changed.fingerprint());
        // 每轮扫描两次请求槽，完成和提交合计不能超过 service budget。
        const { assert!(2 * (MAX_BLOCKING_WORKERS + MAX_DIRTY_WORKERS) <= BLOCKING_SERVICE_BUDGET) };
        assert_eq!(
            u64::from(MAX_DIRTY_WORKERS),
            crate::scheduler_schema::dirty_target(1)
        );
        // 两个子池的栈表使用相同偏移，互不重叠且都容纳于控制页。
        const { assert!(64 + MAX_BLOCKING_WORKERS * (128 + 8) <= DIRTY_POOL_OFFSET) };
        const {
            assert!(
                DIRTY_POOL_OFFSET + 64 + MAX_BLOCKING_WORKERS * 128 + MAX_DIRTY_WORKERS * 8 <= 4096
            )
        };
    }
}
