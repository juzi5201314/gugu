//! runtime 源树登记、raw 平面契约常量与 owner-directed return 模型。
//!
//! 本模块固定 runtime raw 平面的内部契约：owner 身份与路由、slab 描述符、dense size
//! class、本地分配与回收路径、remote return message、owner inbox、exactly-once return
//! 与 queue-page grace。契约、verifier 与确定性参照行为由 compiler 持有；Gugu runtime
//! 的源实现随 runtime/调度/GC 阶段落地，本模块不进入镜像执行路径，也不复制正常执行
//! 路径。

use crate::target::{Rt0Kind, TargetName};

#[allow(
    dead_code,
    reason = "屏障协议与 edge summary 的确定性参照实现，由 harness、契约与测试消费"
)]
mod barrier;
#[allow(dead_code, reason = "屏障布局交叉校验由 RuntimeRawModel 消费")]
mod barrier_layout;
#[allow(dead_code, reason = "屏障契约段由 runtime raw、LIR 与 ImagePlan 消费")]
pub(crate) mod barrier_schema;
pub(crate) mod block_return_schema;
#[cfg(test)]
mod blocking_io_tests;
#[allow(
    dead_code,
    reason = "cage 预留、checked 解码与 FFI 交接由 world、栈图 walker 与确定性测试消费"
)]
mod cage;
pub(crate) mod cage_control;
#[allow(dead_code, reason = "候选平面由周期汇合阶段的世界推进与确定性测试消费")]
pub(crate) mod candidate;
#[allow(dead_code, reason = "候选决议结构由 world 候选推进与确定性测试消费")]
pub(crate) mod candidate_schema;
#[cfg(test)]
mod candidate_tests;
mod channel_layout;
#[allow(
    dead_code,
    reason = "Map/Set 参照模型由确定性测试消费，尚未降成 Gugu 可调用的集合实现"
)]
pub(crate) mod collections;
#[cfg(test)]
mod collections_tests;
#[allow(
    dead_code,
    reason = "typed combining 参照实现由确定性测试与 bench 消费"
)]
mod combining;
#[allow(
    dead_code,
    reason = "typed combining 契约段由 runtime raw、ImagePlan 与 CLI 消费"
)]
pub(crate) mod combining_schema;
#[cfg(test)]
mod combining_tests;
#[allow(
    dead_code,
    reason = "压缩契约段由 runtime raw、world、栈图 walker 与 ImagePlan 消费"
)]
pub(crate) mod compression_schema;
mod context;
#[allow(dead_code, reason = "协程布局与生命周期的确定性参照实现")]
mod coroutine;
mod coroutine_layout;
mod coroutine_schema;
pub(crate) mod edge;
pub(crate) mod edge_schema;

/// rt0 与镜像 runtime 的启动边界：参数块 word 目录与启动载荷布局。
pub(crate) mod entry;
#[allow(
    dead_code,
    reason = "Formatter 写入方法由 comptime 静态格式化与确定性测试消费，尚未降成 Gugu 可调用的 intrinsic"
)]
pub(crate) mod fmt;
#[cfg(test)]
mod fmt_tests;
pub(crate) mod fragments;
#[allow(
    dead_code,
    reason = "GC metadata 契约段由 runtime raw 与 ImagePlan 消费"
)]
pub(crate) mod gc_metadata_contract;
pub(crate) mod gc_metadata_schema;
pub(crate) mod gc_metadata_section;
#[allow(
    dead_code,
    reason = "trace descriptor 解释器由 LocalHeap 与 world 消费"
)]
pub(crate) mod gc_trace;
mod glue;
mod harness;
#[allow(
    dead_code,
    reason = "哈希族与稳定键参照模型由集合模型与确定性测试消费，尚未降成 Gugu 可调用的 intrinsic"
)]
pub(crate) mod hash;
#[cfg(test)]
mod hash_tests;
#[allow(
    dead_code,
    reason = "I/O 组合函数与阻塞等待由确定性测试消费，尚未降成 Gugu 可调用实现"
)]
pub(crate) mod io;
#[cfg(test)]
mod io_tests;
#[allow(
    dead_code,
    reason = "LocalHeap Immix/TLAB/分代参照实现由 world 与确定性测试消费"
)]
pub(crate) mod local_heap;
#[allow(
    dead_code,
    reason = "LocalHeap record 布局交叉校验由 RuntimeRawModel 消费"
)]
mod local_heap_layout;
#[allow(
    dead_code,
    reason = "LocalHeap 契约段由 runtime raw、world 与 ImagePlan 消费"
)]
pub(crate) mod local_heap_schema;
#[allow(
    dead_code,
    reason = "MarkMailbox 与 owner credit 参照实现由 world 与确定性测试消费"
)]
mod mark;
#[allow(dead_code, reason = "mark record 布局交叉校验由 RuntimeRawModel 消费")]
mod mark_layout;
#[allow(dead_code, reason = "mark 契约段由 runtime raw、世界与 ImagePlan 消费")]
pub(crate) mod mark_schema;
mod model;
/// 结局/状态名字目录、credit 诊断访问器与 `PacingPlane::dump` 构成 pacing 诊断面，
/// 唯一消费入口是 runtime dump 与确定性测试套件；真正无人消费的项（分类名目录、
/// 未使用的重导出）已经删除，这里只为仍被 dump 引用的诊断面开例外。
#[allow(dead_code, reason = "pacing 诊断面由 runtime dump 与确定性测试消费")]
mod pacing;
pub(crate) mod pacing_schema;
#[allow(
    dead_code,
    reason = "无损 Path 参照模型由确定性测试消费，尚未降成 Gugu 可调用实现"
)]
pub(crate) mod path;
#[cfg(test)]
mod path_tests;
mod platform_schema;
pub(crate) mod processor;
#[allow(
    dead_code,
    reason = "raw link provenance 与 release 安全 profile 的确定性参照实现由 world 与确定性测试消费"
)]
pub(crate) mod provenance;
pub(crate) mod provenance_schema;
pub(crate) mod region;
pub(crate) mod region_schema;
#[allow(
    dead_code,
    reason = "temporal radix fan-out 的确定性参照实现由 world 与确定性测试消费"
)]
pub(crate) mod routing;
pub(crate) mod routing_schema;
#[allow(
    dead_code,
    reason = "UTF-8/UTF-16 与 COW 参照模型由确定性测试消费，版本常量进入工具链身份"
)]
pub(crate) mod text;
#[cfg(test)]
mod text_tests;

pub(crate) mod cstring;
#[allow(dead_code, reason = "外调交接的确定性参照实现由契约与测试消费")]
mod foreign;
pub(crate) mod foreign_schema;
#[cfg(test)]
mod foreign_tests;
pub(crate) mod scheduler_schema;
#[allow(
    dead_code,
    reason = "stable handle 与 forwarding grace 的确定性参照实现"
)]
mod shared_heap;
mod shared_heap_layout;
#[allow(
    dead_code,
    reason = "shared heap 契约段由 runtime raw、世界与 ImagePlan 消费"
)]
pub(crate) mod shared_heap_schema;
#[allow(dead_code, reason = "栈尺寸与精确复制协议的确定性参照实现")]
mod stack;
#[allow(dead_code, reason = "栈arena与cache的确定性参照实现")]
mod stack_arena;
#[allow(
    dead_code,
    reason = "栈图编解码与 walker 由确定性测试与后端联合验证消费"
)]
mod stackmap;
#[allow(dead_code, reason = "栈图 section 编解码由确定性测试消费")]
mod stackmap_codec;
pub mod stackmap_schema;
mod startup_kinds;
mod startup_schema;
pub(crate) mod sync_layout;
pub mod sync_schema;
mod wait_schema;

pub use barrier_schema::{BarrierDemand, BarrierRuntimeContract};
pub use combining_schema::{
    CombiningDemand, CombiningMode, CombiningPolicyV1, CombiningRuntimeContract,
};
pub use context::ContextSwitchCode;
pub use coroutine::CoroutineContext;
pub use coroutine_schema::{
    CoroutineDemand, CoroutineFieldLayout, CoroutineRecordLayout, CoroutineRuntimeContract,
    StackPolicy,
};
pub use fragments::{FragmentPatch, MainReturnCode, MorestackEntryCode};

pub use block_return_schema::{BlockReturnDemand, BlockReturnRuntimeContract};
pub use compression_schema::{CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract};
pub use edge_schema::{EdgeDemand, EdgeRuntimeContract};
pub use foreign_schema::{ForeignDemand, ForeignRuntimeContract};
pub use gc_metadata_schema::GcMetadataDemand;
pub(crate) use glue::RuntimeGlueBindings;
pub use local_heap_schema::{HeapTriggerProfile, LocalHeapDemand, LocalHeapRuntimeContract};
pub use mark_schema::{MarkDemand, MarkRuntimeContract};
pub use pacing_schema::{GcPacingDemand, GcPacingRuntimeContract};
pub use provenance_schema::{
    ProvenanceDemand, ProvenancePolicyV1, ProvenanceRuntimeContract, SafetyProfile,
};
pub use region_schema::{TurnRegionDemand, TurnRegionRuntimeContract};
pub use routing_schema::{RouteMode, RoutingDemand, RoutingPolicyV1, RoutingRuntimeContract};
pub use scheduler_schema::{SchedulerDemand, SchedulerRuntimeContract};
pub use shared_heap_schema::{SharedHeapDemand, SharedHeapRuntimeContract};
pub(crate) use stackmap::{LandingRecord as StackLanding, consume_metadata};
pub(crate) use stackmap_codec::{
    CodeLayout, DecodedSection, SafepointLayout, decode_tables, encode as encode_stackmap,
};
pub use stackmap_schema::StackMapDemand;
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
mod cage_tests;
#[cfg(test)]
mod compression_tests;
#[cfg(test)]
mod edge_tests;
#[cfg(test)]
mod heap_tests;
#[cfg(test)]
mod platform_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod provenance_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod routing_tests;
#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod shared_heap_tests;
#[cfg(test)]
mod stackmap_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod termination_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
#[path = "trace_tests.rs"]
mod trace_tests;

pub(crate) use harness::DecodeReferenceFixture;

pub use harness::{
    BlockReturnHarness, BlockReturnReport, CardMarkHarness, CardMarkReport, ColdPathHarness,
    ColdPathReport, CompressionHarness, CompressionReport, EdgeCandidateHarness,
    EdgeCandidateReport, HarnessReport, OwnerReturnHarness, RegionTransferHarness,
    RegionTransferReport, ResourceReleaseHarness, ResourceReleaseReport, SharedForwardHarness,
    SharedForwardReport,
};

#[cfg(test)]
pub use extent::EXTENT_CLASS_LADDER;
pub use platform_schema::{PlatformOp, PlatformRangeDemand};

pub(crate) use model::{
    RAW_MODEL_SCHEMA, RawModelInputs, RawPlaneDemand, RawPlanePolicyV1, RawResourceDemand,
    RuntimeRawContractV1, run,
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
const STD_OPTION_SOURCE: &str = include_str!("../../resources/std/option.gg");
const STD_RESULT_SOURCE: &str = include_str!("../../resources/std/result.gg");
const STD_ERROR_SOURCE: &str = include_str!("../../resources/std/error.gg");
const STD_CMP_SOURCE: &str = include_str!("../../resources/std/cmp.gg");
const STD_OPS_SOURCE: &str = include_str!("../../resources/std/ops.gg");
const STD_ITER_SOURCE: &str = include_str!("../../resources/std/iter.gg");
const STD_TEXT_SOURCE: &str = include_str!("../../resources/std/text.gg");
const STD_IO_SOURCE: &str = include_str!("../../resources/std/io.gg");
const STD_PATH_SOURCE: &str = include_str!("../../resources/std/path.gg");
const STD_FMT_SOURCE: &str = include_str!("../../resources/std/fmt.gg");
const STD_HASH_SOURCE: &str = include_str!("../../resources/std/hash.gg");
const STD_COLLECTIONS_SOURCE: &str = include_str!("../../resources/std/collections.gg");
const STD_SYNC_SOURCE: &str = include_str!("../../resources/std/sync.gg");
const RUNTIME_CORE_SOURCE: &str = include_str!("../../resources/runtime/core.gg");
const RUNTIME_PLATFORM_SOURCE: &str = include_str!("../../resources/runtime/platform.gg");
const RUNTIME_COROUTINE_SOURCE: &str = include_str!("../../resources/runtime/coroutine.gg");
const RUNTIME_CHANNEL_SOURCE: &str = include_str!("../../resources/runtime/channel.gg");
const RUNTIME_SCHEDULER_SOURCE: &str = include_str!("../../resources/runtime/scheduler.gg");
const RUNTIME_SYNC_SOURCE: &str = include_str!("../../resources/runtime/sync.gg");
const RUNTIME_BARRIER_SOURCE: &str = include_str!("../../resources/runtime/barrier.gg");
const RUNTIME_HEAP_SOURCE: &str = include_str!("../../resources/runtime/heap.gg");
const RUNTIME_MARK_SOURCE: &str = include_str!("../../resources/runtime/mark.gg");

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

/// runtime 机器 intrinsic 的固定操作集合；glue 名与 `mangle::mangle_runtime` 的输入一一对应，
/// 登记机器片段按同一名字放置符号，`backend` 以 `runtime_named(name)` 生成调用。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RuntimeMachineOp {
    ContextSwitch,
    ContextRestore,
    CurrentSlot,
    ProcessExit,
    ReportWrite,
}

impl RuntimeMachineOp {
    /// glue 名（`mangle::mangle_runtime` 的输入）；诊断与 dump 也使用它。
    pub const fn name(self) -> &'static str {
        match self {
            Self::ContextSwitch => "context_switch",
            Self::ContextRestore => "context_restore",
            Self::CurrentSlot => "current_slot",
            Self::ProcessExit => "process_exit",
            Self::ReportWrite => "report_write",
        }
    }
}

impl std::fmt::Display for RuntimeMachineOp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// 一个 runtime glue 的镜像内定义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GlueDef {
    /// `mangle::mangle_runtime` 的输入名。
    pub(crate) glue: &'static str,
    pub(crate) def: GlueDefKind,
}

/// glue 定义的两种来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlueDefKind {
    /// Gugu 源项定义：绑定该源文件内 `item` 指定的非泛型 `fn`；`item` 与 `glue` 同名时
    /// 也显式写出，绑定关系不依赖重命名推断。
    Item {
        source: &'static str,
        item: &'static str,
    },
    /// 镜像 writer 放置的登记机器片段（`runtime/fragments.rs`）。
    Fragment,
}

/// 本变更登记的 runtime glue 绑定表；后续阶段只增不改语义。
///
/// glue 名与 `RuntimeCall` 目录、`RuntimeMachineOp` 目录及平台原语的 `platform_<name>`
/// 命名一致，绑定表因此是「镜像里哪些 runtime 入口由 Gugu 实现或登记片段接管」的唯一清单。
const GLUE_DEFS: &[GlueDef] = &[
    GlueDef {
        glue: "rt0_start",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "rt0_start",
        },
    },
    GlueDef {
        glue: "panic",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "panic_dispatch",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "panic_dispatch",
        },
    },
    GlueDef {
        glue: "panic_unwind",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "panic_unwind",
        },
    },
    GlueDef {
        glue: "panic_resume",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "defer_push",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "defer_push",
        },
    },
    GlueDef {
        glue: "defer_action",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "defer_action",
        },
    },
    GlueDef {
        glue: "defer_environment",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "defer_environment",
        },
    },
    GlueDef {
        glue: "defer_pop",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "defer_pop",
        },
    },
    GlueDef {
        glue: "morestack_handle",
        def: GlueDefKind::Item {
            source: "std/runtime/core.gg",
            item: "morestack_handle",
        },
    },
    GlueDef {
        glue: "morestack_or_poll",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "context_switch",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "context_restore",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "current_slot",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "process_exit",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "main_return",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "report_write",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "platform_reserve_aligned",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "platform_commit",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "platform_decommit",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "platform_release",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "spawn",
        def: GlueDefKind::Item {
            source: "std/runtime/scheduler.gg",
            item: "spawn",
        },
    },
    GlueDef {
        glue: "yield",
        def: GlueDefKind::Item {
            source: "std/runtime/scheduler.gg",
            item: "yield_now",
        },
    },
    GlueDef {
        glue: "channel_new",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "channel_new",
        },
    },
    GlueDef {
        glue: "channel_close",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "channel_close",
        },
    },
    GlueDef {
        glue: "channel_send",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "channel_send",
        },
    },
    GlueDef {
        glue: "channel_receive",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "channel_receive",
        },
    },
    GlueDef {
        glue: "channel_try_send",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "channel_try_send",
        },
    },
    GlueDef {
        glue: "channel_try_recv",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "channel_try_recv",
        },
    },
    GlueDef {
        glue: "join_wait",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "join_wait",
        },
    },
    GlueDef {
        glue: "select_commit",
        def: GlueDefKind::Item {
            source: "std/runtime/channel.gg",
            item: "select_commit",
        },
    },
];

impl RuntimeResources {
    /// 返回本变更登记的 runtime glue 绑定表。
    pub(crate) fn glue_defs(&self) -> &'static [GlueDef] {
        GLUE_DEFS
    }

    /// runtime 角色源文件的逻辑路径；控制块布局交叉校验按这个集合判定归属。
    pub(crate) fn runtime_paths(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.sources
            .iter()
            .filter(|source| source.role() == RuntimeSourceRole::Runtime)
            .map(RuntimeSource::logical_path)
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
                    logical_path: "std/option.gg",
                    source: STD_OPTION_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/result.gg",
                    source: STD_RESULT_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/error.gg",
                    source: STD_ERROR_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/cmp.gg",
                    source: STD_CMP_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/ops.gg",
                    source: STD_OPS_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/iter.gg",
                    source: STD_ITER_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/text.gg",
                    source: STD_TEXT_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/io.gg",
                    source: STD_IO_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/path.gg",
                    source: STD_PATH_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/fmt.gg",
                    source: STD_FMT_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/hash.gg",
                    source: STD_HASH_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/collections.gg",
                    source: STD_COLLECTIONS_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/sync.gg",
                    source: STD_SYNC_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "std/runtime/core.gg",
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
                    logical_path: "std/runtime/scheduler.gg",
                    source: RUNTIME_SCHEDULER_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/sync.gg",
                    source: RUNTIME_SYNC_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/barrier.gg",
                    source: RUNTIME_BARRIER_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/heap.gg",
                    source: RUNTIME_HEAP_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
                RuntimeSource {
                    logical_path: "std/runtime/mark.gg",
                    source: RUNTIME_MARK_SOURCE,
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
