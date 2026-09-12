//! runtime 源树登记、raw 平面契约常量与 owner-directed return 模型。
//!
//! 本模块固定 runtime raw 平面的内部契约：owner 身份与路由、slab 描述符、dense size
//! class、本地分配与回收路径、remote return message、owner inbox、exactly-once return
//! 与 queue-page grace。契约、verifier 与确定性参照行为由 compiler 持有；Gugu runtime
//! 的源实现随 runtime/调度/GC 阶段落地，本模块不进入镜像执行路径，也不复制正常执行
//! 路径。

use crate::target::{Rt0Kind, TargetName};

#[allow(dead_code, reason = "等待协议的确定性参照实现")]
mod channel;
mod channel_layout;
mod context;
#[allow(dead_code, reason = "协程布局与生命周期的确定性参照实现")]
mod coroutine;
mod coroutine_layout;
mod coroutine_schema;
mod harness;
mod model;
mod platform_schema;
#[allow(dead_code, reason = "调度基础路径的确定性参照实现")]
mod scheduler;
mod scheduler_schema;
#[allow(dead_code, reason = "等待协议的确定性参照实现")]
mod select;
#[allow(dead_code, reason = "栈尺寸与精确复制协议的确定性参照实现")]
mod stack;
#[allow(dead_code, reason = "栈arena与cache的确定性参照实现")]
mod stack_arena;
mod startup_kinds;
mod startup_schema;
#[allow(dead_code, reason = "同步协议的确定性参照实现")]
pub mod sync;
pub(crate) mod sync_layout;
pub mod sync_schema;
#[allow(dead_code, reason = "等待协议的确定性参照实现")]
mod wait;
mod wait_schema;

pub use context::ContextSwitchCode;
pub use coroutine::CoroutineContext;
pub use coroutine_schema::{
    CoroutineDemand, CoroutineFieldLayout, CoroutineRecordLayout, CoroutineRuntimeContract,
    StackPolicy,
};
pub use scheduler_schema::{SchedulerDemand, SchedulerRuntimeContract};
pub use sync_schema::{SyncDemand, SyncRuntimeContract};
pub use wait_schema::{WaitDemand, WaitRuntimeContract};

// rt0 启动、生命周期、报告与终止的参照实现：lib 构建只消费契约段，确定性验证
// 套件直接消费这些状态机。
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod lifecycle;
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod report;
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod startup;
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod termination;

// raw plane 的参照实现：lib 构建只调用契约对象与 bench facade，确定性验证套件与 bench
// 直接消费这些状态机与账本。
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod extent;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod inbox;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod ledger;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod message;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod owner;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod platform;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod provider;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod resource;
mod resource_schema;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod size_class;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod slab;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod world;

#[cfg(test)]
mod platform_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod termination_tests;
#[cfg(test)]
mod tests;

pub use harness::{
    ChannelWaitHarness, ChannelWaitReport, HarnessReport, OwnerReturnHarness,
    ResourceReleaseHarness, ResourceReleaseReport, SyncLockHarness, SyncLockReport,
};

#[cfg(test)]
pub use extent::EXTENT_CLASS_LADDER;
pub use platform_schema::{PlatformOp, PlatformRangeDemand};

pub(crate) use model::{
    RawModelInputs, RawPlaneDemand, RawPlanePolicyV1, RawResourceDemand, RuntimeRawContractV1, run,
};
pub use platform::PlatformProfile;
pub(crate) use startup_schema::Rt0Demand;

/// owner inbox 的 shard 数量；与 scheduler 的 remote inbox 保持一致。
pub(crate) const OWNER_INBOX_SHARDS: u32 = 8;
/// 单个 batch 的 item 上限。
pub(crate) const BATCH_MAX: u32 = 128;
/// 高争用元数据的填充粒度。
pub(crate) const QUEUE_PAD_BYTES: u64 = 128;
/// x86_64 两目标的 cache line 字节数。
pub(crate) const CACHE_LINE_BYTES: u64 = 64;
/// scheduler raw 记录使用的分段 slab page 字节数。
pub(crate) const RAW_SLAB_PAGE_BYTES: u64 = 65536;
/// raw slab 的 dense size class 阶梯。
pub(crate) const RAW_CLASS_LADDER: [u32; 7] = [64, 128, 256, 512, 1024, 2048, 4096];
/// ResourceCell slab 的 dense size class 阶梯；class 尺寸包含 64-byte header。
pub(crate) const RESOURCE_CLASS_LADDER: [u32; 7] = [64, 128, 256, 512, 1024, 2048, 4096];
/// 超过该对齐或 class 上界时改用独立 non-moving 整页 mapping。
pub(crate) const RESOURCE_DEDICATED_ALIGN_LIMIT: u32 = 64;
/// consumer-side source slab 聚合 cache 的 set 数量。
pub(crate) const RETURN_SLAB_CACHE_SETS: u32 = 8;
/// 每个 set 的关联 way 数量。
pub(crate) const RETURN_SLAB_CACHE_WAYS: u32 = 2;
/// direct mode 下的 temporal target cache 项数。
pub(crate) const TARGET_CACHE_ENTRIES: u32 = 4;

const STD_PRELUDE_SOURCE: &str = include_str!("../../resources/std/prelude.gg");
const RUNTIME_CORE_SOURCE: &str = include_str!("../../resources/runtime/core.gg");
const RUNTIME_PLATFORM_SOURCE: &str = include_str!("../../resources/runtime/platform.gg");
const RUNTIME_COROUTINE_SOURCE: &str = include_str!("../../resources/runtime/coroutine.gg");
const RUNTIME_CHANNEL_SOURCE: &str = include_str!("../../resources/runtime/channel.gg");
const RUNTIME_SYNC_SOURCE: &str = include_str!("../../resources/runtime/sync.gg");

/// 登记的 runtime 源文件角色。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSourceRole {
    /// Gugu 标准库源文件。
    StandardLibrary,
    /// Gugu runtime 源文件。
    Runtime,
}

/// 一个嵌入 compiler 的 Gugu 源文件单元。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeSource {
    logical_path: &'static str,
    source: &'static str,
    role: RuntimeSourceRole,
}

impl RuntimeSource {
    /// 返回 package 内的规范逻辑路径。
    pub fn logical_path(&self) -> &'static str {
        self.logical_path
    }

    /// 返回源文件内容。
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// 返回源文件角色。
    pub fn role(&self) -> RuntimeSourceRole {
        self.role
    }
}

/// runtime 所需的 compiler-owned intrinsic 边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntrinsicBoundary {
    /// 原子操作。
    Atomic,
    /// 平台内存映射。
    MemoryMapping,
    /// 协程上下文切换。
    StackSwitch,
    /// safepoint 轮询。
    SafepointPoll,
    /// GC 写屏障。
    GcWriteBarrier,
    /// 外部函数交接。
    ForeignBridge,
}

/// 平台入口 rt0 的边界说明。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rt0Boundary {
    /// Linux syscall 入口。
    LinuxSyscall,
    /// Windows 薄 IAT 入口。
    WindowsThinImport,
}
impl std::fmt::Display for Rt0Boundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LinuxSyscall => "linux-syscall",
            Self::WindowsThinImport => "windows-thin-import",
        })
    }
}

impl From<Rt0Kind> for Rt0Boundary {
    fn from(value: Rt0Kind) -> Self {
        match value {
            Rt0Kind::LinuxSyscall => Self::LinuxSyscall,
            Rt0Kind::WindowsThinImport => Self::WindowsThinImport,
        }
    }
}

/// compiler 使用的 Gugu 标准库/runtime 源树登记表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeResources {
    sources: &'static [RuntimeSource],
    intrinsics: &'static [IntrinsicBoundary],
}

impl RuntimeResources {
    /// 返回当前 compiler 构建携带的源树登记。
    pub fn builtin() -> Self {
        Self {
            sources: &[
                RuntimeSource {
                    logical_path: "std/prelude.gg",
                    source: STD_PRELUDE_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "runtime/core.gg",
                    source: RUNTIME_CORE_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/platform.gg",
                    source: RUNTIME_PLATFORM_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/runtime/coroutine.gg",
                    source: RUNTIME_COROUTINE_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/channel.gg",
                    source: RUNTIME_CHANNEL_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/sync.gg",
                    source: RUNTIME_SYNC_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
            ],
            intrinsics: &[
                IntrinsicBoundary::Atomic,
                IntrinsicBoundary::MemoryMapping,
                IntrinsicBoundary::StackSwitch,
                IntrinsicBoundary::SafepointPoll,
                IntrinsicBoundary::GcWriteBarrier,
                IntrinsicBoundary::ForeignBridge,
            ],
        }
    }

    /// 返回已登记的 Gugu 源文件。
    pub fn sources(&self) -> &[RuntimeSource] {
        self.sources
    }

    /// 返回 compiler/runtime 之间允许的 intrinsic 边界。
    pub fn intrinsic_boundaries(&self) -> &[IntrinsicBoundary] {
        self.intrinsics
    }

    pub(crate) fn attach(&self, target: TargetName) -> RuntimeAttachment {
        RuntimeAttachment {
            source_count: self.sources.len() as u32,
            rt0: target.descriptor().rt0.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeAttachment {
    pub(crate) source_count: u32,
    pub(crate) rt0: Rt0Boundary,
}
