//! runtime 源树登记、raw 平面契约常量与 owner-directed return 模型。
//!
//! 本模块固定 runtime raw 平面的内部契约：owner 身份与路由、slab 描述符、dense size
//! class、本地分配与回收路径、remote return message、owner inbox、exactly-once return
//! 与 queue-page grace。契约、verifier 与确定性参照行为由 compiler 持有；Gugu runtime
//! 的源实现随 runtime/调度/GC 阶段落地，本模块不进入镜像执行路径，也不复制正常执行
//! 路径。

use crate::target::{Rt0Kind, TargetName};

pub use gugu_compiler_core::RuntimeSourceRecordV1 as RuntimeSource;
pub use gugu_compiler_core::RuntimeSourceRole;
pub use gugu_compiler_core::{
    RuntimeArtifactError, RuntimeArtifactManifestV1, RuntimeArtifactPayloadV1, RuntimeArtifactV1,
    RuntimeContractRecordV1, RuntimeFunctionArtifactV1, RuntimeGenericTemplateV1,
    RuntimeModuleInterfaceV1, RuntimePrebuiltV1, RuntimeSourceBlobV1, RuntimeSourceRecordV1,
};

/// 目标 → 已验证 runtime artifact 的解析器。
///
/// 前端不嵌入 artifact 字节，也不从目标名推断字节来源：build.rs 产出的静态字节留在 façade，
/// 前端只按目标取用；取不到或校验失败即失败，没有源码回退。
pub trait RuntimeArtifactProvider: std::fmt::Debug + Send + Sync {
    /// 返回目标的已验证 artifact。
    fn artifact(
        &self,
        target: TargetName,
    ) -> Result<&'static RuntimeArtifactV1, RuntimeArtifactError>;
}
pub(crate) mod barrier_layout;
#[allow(dead_code, reason = "屏障契约段由 runtime raw、LIR 与 ImagePlan 消费")]
pub(crate) mod barrier_schema;
pub(crate) mod block_return_schema;
#[cfg(all(test, feature = "slow-tests"))]
mod blocking_io_tests;
#[allow(
    dead_code,
    reason = "cage 预留、checked 解码与 FFI 交接由 world、栈图 walker 与确定性测试消费"
)]
mod cage;
pub(crate) mod cage_control;
#[allow(dead_code, reason = "GC schema 由 metadata 编码与镜像探针契约消费")]
pub(crate) mod candidate_schema;
mod channel_layout;
mod collect_layout;
#[allow(
    dead_code,
    reason = "Map/Set 参照模型由确定性测试消费，尚未降成 Gugu 可调用的集合实现"
)]
pub(crate) mod collections;
#[cfg(all(test, feature = "slow-tests"))]
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
pub(crate) mod contract_view;
#[allow(dead_code, reason = "协程布局与生命周期的确定性参照实现")]
mod coroutine;
mod coroutine_layout;
mod coroutine_schema;
pub(crate) mod demand;
#[allow(
    dead_code,
    reason = "动态根登记契约由镜像 runtime、探针与后续收集器共同消费"
)]
pub(crate) mod dynamic_roots_schema;
#[allow(dead_code, reason = "GC schema 由 metadata 编码与镜像探针契约消费")]
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
pub(crate) mod gc_config;
pub(crate) mod gc_metadata_contract;
pub(crate) mod gc_metadata_schema;
pub(crate) mod gc_metadata_section;
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
/// managed block 身份与 object header 编码；分配和回收算法在镜像 runtime 中。
#[allow(dead_code, reason = "LocalHeap 执行体已下沉，宿主仅保留稳定编码类型")]
pub(crate) mod local_heap;
mod local_heap_layout;
#[allow(
    dead_code,
    reason = "LocalHeap 契约段由 runtime raw、world 与 ImagePlan 消费"
)]
pub(crate) mod local_heap_schema;
pub(crate) mod mark_layout;
#[allow(dead_code, reason = "GC schema 由 metadata 编码与镜像探针契约消费")]
pub(crate) mod mark_schema;
mod model;
#[allow(dead_code, reason = "GC schema 由 metadata 编码与镜像探针契约消费")]
pub(crate) mod pacing_schema;
/// 结局/状态名字目录、credit 诊断访问器与 `PacingPlane::dump` 构成 pacing 诊断面，
/// 唯一消费入口是 runtime dump 与确定性测试套件；真正无人消费的项（分类名目录、
/// 未使用的重导出）已经删除，这里只为仍被 dump 引用的诊断面开例外。
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
#[allow(dead_code, reason = "GC schema 由 metadata 编码与镜像探针契约消费")]
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
#[cfg(all(test, feature = "slow-tests"))]
mod text_tests;

pub(crate) mod cstring;
#[allow(dead_code, reason = "外调交接的确定性参照实现由契约与测试消费")]
mod foreign;
pub(crate) mod foreign_schema;
#[cfg(all(test, feature = "slow-tests"))]
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
pub(crate) use coroutine_layout::{
    COROUTINE_PACK_ALIGN, COROUTINE_PACK_BYTES, COROUTINE_PACK_FLOAT_LANES,
    COROUTINE_PACK_INTEGER_LANES, COROUTINE_PACK_TRACE_NONE, coroutine_pack_float_offset,
    coroutine_pack_integer_offset, coroutine_pack_trace_code, coroutine_pack_trace_shift,
};
pub use edge_schema::{EdgeDemand, EdgeRuntimeContract};
pub use foreign_schema::{ForeignDemand, ForeignRuntimeContract};
pub(crate) use gc_config::GcImageConfig;
pub use gc_metadata_schema::GcMetadataDemand;
pub(crate) use glue::{RuntimeGlueBindings, implied_glue};
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
    CodeLayout, DecodedSection, SafepointLayout, TypedRoot, decode_tables,
    encode as encode_stackmap,
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

#[cfg(all(test, feature = "slow-tests"))]
mod platform_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod stackmap_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod termination_tests;
pub(crate) use harness::DecodeReferenceFixture;

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

#[cfg(test)]
#[allow(dead_code, reason = "测试目标按模块条件读取 runtime source mirror")]
const RUNTIME_CORE_SOURCE: &str = include_str!("../../resources/runtime/core.gg");
#[cfg(test)]
#[expect(
    dead_code,
    reason = "layout mirror tests 读取 build-time runtime source"
)]
const RUNTIME_BARRIER_SOURCE: &str = include_str!("../../resources/runtime/barrier.gg");
#[cfg(test)]
#[expect(
    dead_code,
    reason = "layout mirror tests 读取 build-time runtime source"
)]
const RUNTIME_HEAP_SOURCE: &str = include_str!("../../resources/runtime/heap.gg");
#[cfg(test)]
#[expect(
    dead_code,
    reason = "layout mirror tests 读取 build-time runtime source"
)]
const RUNTIME_ROOTS_SOURCE: &str = include_str!("../../resources/runtime/roots.gg");
#[cfg(test)]
#[expect(dead_code, reason = "slow GC tests 读取 build-time runtime source")]
const RUNTIME_GC_PROBES_SOURCE: &str = include_str!("../../resources/runtime/gc_probes.gg");
#[cfg(test)]
#[expect(
    dead_code,
    reason = "layout mirror tests 读取 build-time runtime source"
)]
const RUNTIME_SCHEDULER_SOURCE: &str = include_str!("../../resources/runtime/scheduler.gg");

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
    /// GC 控制原语：只由 runtime 注入源调用（checker 按注入身份授权）。
    GcConfig,
    /// GC 入口：只由 runtime 注入源调用；保存发起者上下文、切 system stack、
    /// 调用 dispatcher 后恢复，返回值是 dispatcher 的状态字。
    GcEnter,
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
            Self::GcConfig => "gc_config",
            Self::GcEnter => "gc_enter",
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
        glue: "safepoint_slow",
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
    GlueDef {
        glue: "gc_config",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "gc_enter",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "gc_dispatch",
        def: GlueDefKind::Item {
            source: "std/runtime/collect.gg",
            item: "gc_dispatch",
        },
    },
    GlueDef {
        glue: "gc_config_record",
        def: GlueDefKind::Fragment,
    },
    GlueDef {
        glue: "gc_alloc_slow",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "gc_alloc_slow",
        },
    },
    GlueDef {
        glue: "gc_write_barrier",
        def: GlueDefKind::Item {
            source: "std/runtime/barrier.gg",
            item: "gc_write_barrier",
        },
    },
    GlueDef {
        glue: "gc_write_barrier_reserved",
        def: GlueDefKind::Item {
            source: "std/runtime/barrier.gg",
            item: "gc_write_barrier_reserved",
        },
    },
    GlueDef {
        glue: "value_repeat",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "value_repeat",
        },
    },
    GlueDef {
        glue: "gc_region_slow",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "gc_region_slow",
        },
    },
    GlueDef {
        glue: "gc_region_publish",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "gc_region_publish",
        },
    },
    GlueDef {
        glue: "gc_region_reset",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "gc_region_reset",
        },
    },
    GlueDef {
        glue: "gc_region_promote",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "gc_region_promote",
        },
    },
    GlueDef {
        glue: "gc_region_transfer",
        def: GlueDefKind::Item {
            source: "std/runtime/heap.gg",
            item: "gc_region_transfer",
        },
    },
];
/// 判断 runtime glue 是否已经登记定义，或是否被显式列为当前迁移切片的缺口。
///
/// lowering 只在真正发出 runtime 符号调用时调用本查询；由 lowering 内联的 runtime 内建不会
/// 经过这里，因此不会因为名字存在于 `RuntimeCall` 目录而误报。
pub(crate) fn glue_is_declared(name: &str) -> bool {
    GLUE_DEFS.iter().any(|definition| definition.glue == name)
        || glue::RuntimeGlueBindings::pending_glue_names().contains(&name)
}

/// 闭世界 glue 目录里的全部入口名：真实定义与显式缺口记录的并集。
///
/// 诊断按名字反查符号哈希时必须遍历同一份闭世界目录，否则「引用但未实现」的入口会退化成
/// 一堆无法辨认的哈希。
pub(crate) fn declared_glue_names() -> impl Iterator<Item = &'static str> {
    GLUE_DEFS.iter().map(|definition| definition.glue).chain(
        glue::RuntimeGlueBindings::pending_glue_names()
            .iter()
            .copied(),
    )
}

/// compiler 携带的 runtime source manifest、intrinsic 边界与 artifact 身份视图。
///
/// 视图不持有字节：artifact 由建造 session 的一方按目标注入，前端既不嵌入也不回退。
#[derive(Clone, Copy, Debug)]
pub struct RuntimeResources<'a> {
    artifact: &'a RuntimeArtifactV1,
    intrinsics: &'static [IntrinsicBoundary],
}

const INTRINSICS: &[IntrinsicBoundary] = &[
    IntrinsicBoundary::Atomic,
    IntrinsicBoundary::MemoryMapping,
    IntrinsicBoundary::StackSwitch,
    IntrinsicBoundary::SafepointPoll,
    IntrinsicBoundary::GcWriteBarrier,
    IntrinsicBoundary::ForeignBridge,
];

impl<'a> RuntimeResources<'a> {
    /// 构造 artifact 视图；源码 artifact 不得携带未物化的 executable function。
    pub fn new(artifact: &'a RuntimeArtifactV1) -> Result<Self, RuntimeArtifactError> {
        if !artifact.payload.functions.is_empty() {
            return Err(RuntimeArtifactError::Manifest(
                "源码 runtime artifact 不得携带未物化的 executable function".to_owned(),
            ));
        }
        Ok(Self {
            artifact,
            intrinsics: INTRINSICS,
        })
    }

    /// 返回本变更登记的 runtime glue 绑定表。
    pub(crate) fn glue_defs(&self) -> &'static [GlueDef] {
        GLUE_DEFS
    }

    /// runtime 角色源文件的逻辑路径。
    pub(crate) fn runtime_paths(&self) -> impl Iterator<Item = &str> + '_ {
        self.artifact
            .manifest
            .sources
            .iter()
            .filter(|source| source.role == RuntimeSourceRole::Runtime)
            .map(|source| source.logical_path.as_str())
    }

    /// 返回已登记的 source manifest 项。
    pub fn sources(&self) -> &'a [RuntimeSource] {
        &self.artifact.manifest.sources
    }

    /// 返回 compiler/runtime 之间允许的 intrinsic 边界。
    pub fn intrinsic_boundaries(&self) -> &[IntrinsicBoundary] {
        self.intrinsics
    }

    /// 返回 build-time artifact 固化的 source blob；仅供 artifact frontend adapter 使用。
    pub(crate) fn source_blobs(&self) -> &'a [RuntimeSourceBlobV1] {
        &self.artifact.payload.sources
    }

    /// 返回构建期物化的内建块派生表；源码 artifact 没有这一层，加载端必须失败而不是回退。
    pub(crate) fn prebuilt(&self) -> Option<&'a RuntimePrebuiltV1> {
        self.artifact.payload.prebuilt.as_ref()
    }

    /// artifact 的目标名；物化表按 target 派生，加载端必须核对它与本次编译目标一致。
    pub(crate) fn target_name(&self) -> &'a str {
        &self.artifact.manifest.target
    }

    /// 返回 manifest 摘要，用于 action identity。
    pub(crate) fn manifest_digest(&self) -> [u8; 32] {
        let bytes = gugu_compiler_core::canonical_manifest_bytes(&self.artifact.manifest);
        let mut hasher = blake3::Hasher::new_derive_key("gugu-runtime-manifest-v1");
        hasher.update(&bytes);
        *hasher.finalize().as_bytes()
    }

    pub(crate) fn attach(&self, target: TargetName) -> RuntimeAttachment {
        RuntimeAttachment {
            source_count: u32::try_from(self.artifact.manifest.sources.len())
                .expect("内建源数量必须落在 u32"),
            rt0: target.descriptor().rt0.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeAttachment {
    pub(crate) source_count: u32,
    pub(crate) rt0: Rt0Boundary,
}
