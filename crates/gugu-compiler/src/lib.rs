#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Gugu compiler 的阶段化 bootstrap 接口。
//!
//! compiler 前端：源码快照、词法及 AST、名称/类型/控制流检查、版本化查询与冻结 HIR。
//! 后端只消费已验证 LIR 形成内存 image plan；机器代码生成由后续后端阶段实现。

mod action;
mod backend;
mod diagnostics;
#[cfg(test)]
mod diagnostics_tests;
mod frontend;
mod lir;
mod project;
mod query;
mod runtime;
mod source;
mod target;
#[cfg(test)]
mod target_tests;
pub use query::{
    DependencyFingerprint, ObjectCache, ObjectError, ObjectKey, QueryContext, QueryEngine,
    QueryError, QueryKey, QueryKind, QueryResult, QueryState,
};

pub use action::{ActionGraph, ActionKind, ActionNode, ActionStatus};
pub use backend::x64::codegen::{
    X64Fragment, X64FragmentFrame, X64FragmentRelocation, X64Fragments,
};
pub use backend::x64::harness::{
    X64Case, X64Code, X64DecodeFixture, X64Harness, X64HarnessError, X64HarnessReport,
    X64MemorySlot, X64Register, X64RelocationView,
};
pub use diagnostics::{Diagnostic, DiagnosticCode, Diagnostics, Severity};
pub use frontend::format::{FormatError, format_source};
pub use project::{
    ActionInputs, ActionKey, CacheError, CachePolicy, DependencyCache, DependencyDomain,
    DependencyInput, DependencySource, DependencySpec, LockGraph, LockedDependency, LockedPackage,
    Package, PackageFiles, PackageId, PackageMetadata, PackageSource, Project, ProjectError,
    ResolveOptions, Target, TargetArtifact, TargetCondition, TargetKind, TargetSelection,
    TargetView, Version, VersionReq, Workspace, candidates_from_lock, default_cache_root,
    materialize_vendor, prepare_dependency_inputs,
};
pub use runtime::{
    BarrierDemand, BarrierRuntimeContract, BlockReturnDemand, BlockReturnHarness,
    BlockReturnReport, BlockReturnRuntimeContract, CardMarkHarness, CardMarkReport,
    ChannelWaitHarness, ChannelWaitReport, ColdPathHarness, ColdPathReport, CompressionDemand,
    CompressionHarness, CompressionPolicyV1, CompressionReport, CompressionRuntimeContract,
    ContextSwitchCode, CoroutineContext, CoroutineDemand, CoroutineFieldLayout,
    CoroutineRecordLayout, CoroutineRuntimeContract, EdgeCandidateHarness, EdgeCandidateReport,
    EdgeDemand, EdgeRuntimeContract, GcMetadataDemand, GcPacingDemand, GcPacingRuntimeContract,
    HarnessReport, HeapTriggerProfile, IntrinsicBoundary, LocalHeapDemand,
    LocalHeapRuntimeContract, MarkDemand, MarkRuntimeContract, OwnerReturnHarness,
    PlatformRangeDemand, RegionTransferHarness, RegionTransferReport, ResourceReleaseHarness,
    ResourceReleaseReport, Rt0Boundary, RuntimeResources, RuntimeSource, RuntimeSourceRole,
    SchedulerDemand, SchedulerRuntimeContract, SharedForwardHarness, SharedForwardReport,
    SharedHeapDemand, SharedHeapRuntimeContract, StackMapDemand, StackPolicy, SyncDemand,
    SyncLockHarness, SyncLockReport, SyncRuntimeContract, TurnRegionDemand,
    TurnRegionRuntimeContract, WaitDemand, WaitRuntimeContract,
};
pub use runtime::{CombiningDemand, CombiningMode, CombiningPolicyV1, CombiningRuntimeContract};
pub use runtime::{ProvenanceDemand, ProvenancePolicyV1, ProvenanceRuntimeContract, SafetyProfile};
pub use runtime::{RouteMode, RoutingDemand, RoutingPolicyV1, RoutingRuntimeContract};

use frontend::mono::roots::RootCategoryV1;
pub use source::{
    ExpansionId, ExpansionInput, ExpansionRecord, LineColumn, SourceError, SourceFileId, SourceMap,
    SourceMapError, SourceSlot, SourceSnapshot, SourceTableId, Span, SpanError,
    normalize_logical_path,
};
pub use target::{
    Architecture, BackendCostProfile, CpuBaseline, CpuFeature, ObjectFormat, OperatingSystem,
    PointerCompression, Rt0Kind, TargetDescriptor, TargetName, TargetParseError,
    baseline_cost_profile,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use backend::BackendPlan;
use frontend::{SourceInput, cfg::CfgContext};
use runtime::{
    RawModelInputs, RawPlaneDemand, RawPlanePolicyV1, RawResourceDemand, Rt0Demand,
    RuntimeRawContractV1,
};

/// 一次 bootstrap 编译请求。
#[derive(Clone, Debug)]
pub struct CompileRequest {
    target: TargetName,
    input: CompileInput,
    compression: CompressionPolicyV1,
    routing: RoutingPolicyV1,
    provenance: ProvenancePolicyV1,
    combining: CombiningPolicyV1,
}

impl CompileRequest {
    /// 创建空 package 请求。
    pub fn empty_package(target: TargetName) -> Self {
        Self {
            target,
            input: CompileInput::EmptyPackage,
            compression: CompressionPolicyV1::disabled(),
            routing: RoutingPolicyV1::default(),
            provenance: ProvenancePolicyV1::release(),
            combining: CombiningPolicyV1::direct(),
        }
    }

    /// 创建内存中的单文件请求，适合确定性测试和编辑器集成。
    pub fn single_file(
        path: impl Into<PathBuf>,
        source: impl Into<String>,
        target: TargetName,
    ) -> Self {
        Self {
            target,
            input: CompileInput::SingleFileSource {
                path: path.into(),
                source: source.into(),
            },
            compression: CompressionPolicyV1::disabled(),
            routing: RoutingPolicyV1::default(),
            provenance: ProvenancePolicyV1::release(),
            combining: CombiningPolicyV1::direct(),
        }
    }

    /// 创建从文件系统读取的单文件请求；逻辑路径按输入路径推导。
    pub fn single_file_path(path: impl Into<PathBuf>, target: TargetName) -> Self {
        Self {
            target,
            input: CompileInput::File {
                path: path.into(),
                logical_path: None,
                require_main: true,
            },
            compression: CompressionPolicyV1::disabled(),
            routing: RoutingPolicyV1::default(),
            provenance: ProvenancePolicyV1::release(),
            combining: CombiningPolicyV1::direct(),
        }
    }

    /// 创建库 target 入口请求：读取源码快照但不要求可执行 `main`。
    pub fn library_file(path: impl Into<PathBuf>, target: TargetName) -> Self {
        Self {
            target,
            input: CompileInput::File {
                path: path.into(),
                logical_path: None,
                require_main: false,
            },
            compression: CompressionPolicyV1::disabled(),
            routing: RoutingPolicyV1::default(),
            provenance: ProvenancePolicyV1::release(),
            combining: CombiningPolicyV1::direct(),
        }
    }

    /// 创建项目 target 请求并携带该解析域的 feature 与直接依赖别名。
    pub fn project_target(
        package: &Package,
        package_target: &Target,
        package_identity: impl Into<String>,
        output_target: TargetName,
        enabled_features: Vec<String>,
        external_packages: BTreeSet<String>,
    ) -> Self {
        let target = if package_target.is_host_target() {
            TargetName::host().unwrap_or(output_target)
        } else {
            output_target
        };
        let require_main = matches!(package_target.kind(), TargetKind::Bin | TargetKind::Example)
            || (package_target.kind() == TargetKind::Bench && !package_target.harness());
        Self {
            target,
            input: CompileInput::Project {
                package_root: package.root().to_path_buf(),
                source_root: package_target.source_root().to_path_buf(),
                entry: package_target.entry().to_path_buf(),
                package_identity: package_identity.into(),
                require_main,
                declared_features: package.declared_features().to_vec(),
                enabled_features,
                external_packages,
                test: package_target.kind() == TargetKind::Test,
                bench: package_target.kind() == TargetKind::Bench,
                custom_cfg: BTreeMap::new(),
            },
            compression: CompressionPolicyV1::disabled(),
            routing: RoutingPolicyV1::default(),
            provenance: ProvenancePolicyV1::release(),
            combining: CombiningPolicyV1::direct(),
        }
    }

    /// 显式设置 cage profile 开关；默认关闭，即 full-pointer 语义。
    ///
    /// 策略必须在 lowering 前已知：压缩需求由优化后 LIR 推导，不允许事后改契约。
    pub fn with_compression_policy(mut self, policy: CompressionPolicyV1) -> Self {
        self.compression = policy;
        self
    }

    /// 显式设置路由 profile；默认 direct，即 owner inbox 直达语义。
    ///
    /// mode 是 runtime tuning profile：它不改变编译语义，只随 raw policy 进入契约与
    /// action key，供世界与确定性测试按契约配置路由平面。
    pub fn with_routing_policy(mut self, policy: RoutingPolicyV1) -> Self {
        self.routing = policy;
        self
    }

    /// 显式设置 release 安全 profile；默认 release，即基线 provenance 检查语义。
    ///
    /// profile 是 runtime tuning：debug/security 只追加显式登记的额外检查，不改变编译
    /// 语义，只随 raw policy 进入契约与 action key，供世界与确定性测试按契约配置
    /// provenance 平面。
    pub fn with_provenance_policy(mut self, policy: ProvenancePolicyV1) -> Self {
        self.provenance = policy;
        self
    }

    /// 显式设置 combining profile；默认 direct，即冷操作在请求者上下文直接执行。
    ///
    /// mode 是 runtime tuning profile：combined 只在显式开启后把同一批冷操作记录进
    /// 非移动 operation record 池并按轮次合并执行，不改变编译语义，只随 raw policy
    /// 进入契约与 action key，供世界、bench 与确定性测试按契约配置 combining 平面。
    pub fn with_combining_policy(mut self, policy: CombiningPolicyV1) -> Self {
        self.combining = policy;
        self
    }
}

#[derive(Clone, Debug)]
enum CompileInput {
    EmptyPackage,
    SingleFileSource {
        path: PathBuf,
        source: String,
    },
    File {
        path: PathBuf,
        logical_path: Option<PathBuf>,
        require_main: bool,
    },
    Project {
        package_root: PathBuf,
        source_root: PathBuf,
        entry: PathBuf,
        package_identity: String,
        require_main: bool,
        declared_features: Vec<String>,
        enabled_features: Vec<String>,
        external_packages: BTreeSet<String>,
        test: bool,
        bench: bool,
        custom_cfg: BTreeMap<String, Option<String>>,
    },
}

/// compiler 返回的内存结果。
#[derive(Clone, Debug)]
pub struct Compilation {
    graph: ActionGraph,
    diagnostics: Diagnostics,
    source_map: SourceMap,
    image_plan: Option<ImagePlan>,
    hir: Option<frontend::hir::Validated>,
    gir: Option<frontend::gir::GirWorldV1>,
    gir_stats: frontend::gir::pass::GirPassStats,
    lir: Option<lir::Validated>,
    raw_contract: Option<RuntimeRawContractV1>,
    x64: Option<backend::x64::codegen::X64World>,
    action_key: Option<project::ActionKey>,
}

impl Compilation {
    /// 返回 action graph。
    pub fn action_graph(&self) -> &ActionGraph {
        &self.graph
    }

    /// 返回按规范排序的诊断。
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// 返回本次 action 的源码与展开表。
    pub fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    /// 返回成功时的内存镜像计划。
    pub fn image_plan(&self) -> Option<&ImagePlan> {
        self.image_plan.as_ref()
    }

    /// 判断本次 action 是否成功且没有错误诊断。
    pub fn is_success(&self) -> bool {
        self.lir.is_some() && !self.diagnostics.has_errors()
    }

    /// 返回完整编译 action 的内容寻址 key；前端或 LIR 失败时为 `None`。
    pub fn action_key(&self) -> Option<project::ActionKey> {
        self.action_key
    }

    /// 返回 generic GIR 的稳定 dump；前端失败时为 `None`。
    pub fn dump_gir(&self) -> Option<String> {
        let hir = self.hir.as_ref()?;
        let gir = self.gir.as_ref()?;
        Some(frontend::gir::dump_world(hir.module(), gir, self.gir_stats))
    }

    /// 返回 generic GIR 世界指纹。
    pub fn gir_fingerprint(&self) -> Option<[u8; 32]> {
        self.gir.as_ref().map(|world| world.fingerprint)
    }

    /// 返回已通过 verifier 的 LIR dump。
    pub fn dump_lir(&self) -> Option<String> {
        self.lir.as_ref().map(lir::Validated::dump)
    }

    /// 返回 LIR 世界指纹。
    pub fn lir_fingerprint(&self) -> Option<[u8; 32]> {
        self.lir.as_ref().map(lir::Validated::fingerprint)
    }

    /// 返回 runtime raw 平面契约的稳定 dump；契约失败时为 `None`。
    pub fn dump_runtime(&self) -> Option<String> {
        self.raw_contract.as_ref().map(RuntimeRawContractV1::dump)
    }

    /// 返回 runtime raw 平面契约指纹。
    pub fn runtime_raw_fingerprint(&self) -> Option<[u8; 32]> {
        self.raw_contract
            .as_ref()
            .map(RuntimeRawContractV1::fingerprint)
    }

    /// 返回机器码片段的稳定 dump；没有可执行入口或片段生成失败时为 `None`。
    pub fn dump_x64(&self) -> Option<String> {
        self.x64.as_ref().map(backend::x64::codegen::X64World::dump)
    }

    /// 返回机器码片段世界指纹。
    pub fn x64_fingerprint(&self) -> Option<[u8; 32]> {
        self.x64
            .as_ref()
            .map(backend::x64::codegen::X64World::fingerprint)
    }

    /// 返回全部机器码片段的公开视图；没有可执行入口或片段生成失败时为 `None`。
    pub fn x64_fragments(&self) -> Option<X64Fragments> {
        let stack_check_offset = self
            .raw_contract
            .as_ref()
            .map(|contract| contract.coroutine().stack_check_offset)?;
        self.x64
            .as_ref()
            .map(|world| world.view(stack_check_offset))
    }

    /// 返回已验证的契约对象本身；供确定性测试与 `EdgeCandidateHarness` 用真实契约配置 world。
    pub(crate) fn raw_contract(&self) -> Option<&RuntimeRawContractV1> {
        self.raw_contract.as_ref()
    }

    /// 返回规范退出码；内部 IR 不变量失败与用户源码错误分开报告。
    pub fn exit_code(&self) -> i32 {
        if self.diagnostics.items().iter().any(|diagnostic| {
            matches!(
                diagnostic.code(),
                DiagnosticCode::LirInvariant
                    | DiagnosticCode::RuntimeRawInvariant
                    | DiagnosticCode::ResourceInvariant
                    | DiagnosticCode::BackendInvariant
            )
        }) {
            101
        } else {
            i32::from(!self.is_success())
        }
    }
}

/// 编译器入口。
#[derive(Clone, Debug, Default)]
pub struct Compiler {
    queries: std::sync::Arc<query::QueryEngine>,
}

impl Compiler {
    /// 创建 bootstrap compiler。
    pub fn new() -> Self {
        Self::default()
    }

    /// 执行一次确定性的 bootstrap action graph。
    pub fn compile(&self, request: CompileRequest) -> Compilation {
        let CompileRequest {
            target,
            input,
            compression,
            routing,
            provenance,
            combining,
        } = request;
        let mut graph = ActionGraph::new();
        let mut diagnostics = Diagnostics::default();
        let descriptor = target.descriptor();
        graph.complete(ActionKind::ResolveTarget, target.to_string());

        let mut loaded = match load_input(input, target) {
            Ok(loaded) => loaded,
            Err(error) => {
                diagnostics.push(error.diagnostic());
                graph.fail(ActionKind::LoadSources, "源文件读取或快照校验失败");
                graph.skip_after(ActionKind::LoadSources, "前置 action 失败");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    source_map: SourceMap::empty(),
                    image_plan: None,
                    hir: None,
                    gir: None,
                    gir_stats: Default::default(),
                    lir: None,
                    raw_contract: None,
                    x64: None,
                    action_key: None,
                };
            }
        };
        graph.complete(ActionKind::LoadSources, loaded.detail());

        let frontend_result = frontend::bootstrap(loaded.as_source_input(), &self.queries);
        let source_map = loaded.source_map;
        let frontend = match frontend_result {
            Ok(output) => output,
            Err(errors) => {
                for diagnostic in errors {
                    diagnostics.push(diagnostic);
                }
                graph.fail(ActionKind::Frontend, "配置、名称或语义检查失败");
                graph.skip_after(ActionKind::Frontend, "前置 action 失败");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    source_map,
                    image_plan: None,
                    hir: None,
                    gir: None,
                    gir_stats: Default::default(),
                    lir: None,
                    raw_contract: None,
                    x64: None,
                    action_key: None,
                };
            }
        };
        graph.complete(ActionKind::Frontend, frontend.detail());
        for diagnostic in frontend.lints.iter().cloned() {
            diagnostics.push(diagnostic);
        }
        let lir = match lir::build(
            &frontend.hir,
            &frontend.gir,
            &frontend.mono,
            target,
            compression,
            &self.queries,
            &source_map,
        ) {
            Ok(lir) => lir,
            Err(errors) => {
                for error in errors {
                    diagnostics.push(error);
                }
                graph.fail(ActionKind::BuildIr, "LIR 构造或结构校验失败");
                graph.skip_after(ActionKind::BuildIr, "内部表示无效");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    source_map,
                    image_plan: None,
                    hir: Some(frontend.hir),
                    gir: Some(frontend.gir),
                    gir_stats: frontend.gir_stats,
                    lir: None,
                    raw_contract: None,
                    x64: None,
                    action_key: None,
                };
            }
        };
        let gc_metadata = match frontend::gc::derive(
            &frontend.mono.universe,
            &frontend.gir,
            frontend.hir.module(),
        ) {
            Ok(bundle) => bundle,
            Err(error) => {
                diagnostics.push(error);
                graph.fail(ActionKind::BuildIr, "GC metadata 构造失败");
                graph.skip_after(ActionKind::BuildIr, "GC metadata 无效");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    source_map,
                    image_plan: None,
                    hir: Some(frontend.hir),
                    gir: Some(frontend.gir),
                    gir_stats: frontend.gir_stats,
                    lir: Some(lir),
                    raw_contract: None,
                    x64: None,
                    action_key: None,
                };
            }
        };
        // LocalHeap 类型侧口径与 GC metadata 共用同一份冻结类型表：managed 类型数、超过单个
        // Immix block 的类型数与最大 payload 都由这些记录推导，避免两处口径漂移。
        let managed_type_count = frontend.mono.universe.records.len() as u32;
        let mut large_type_count = 0_u32;
        let mut max_object_bytes = 0_u64;
        for record in &frontend.mono.universe.records {
            let (size, _) = record.layout.unwrap_or((0, 1));
            max_object_bytes = max_object_bytes.max(size);
            if size + u64::from(runtime::local_heap_schema::HEAP_OBJECT_HEADER_BYTES)
                > u64::from(runtime::gc_metadata_contract::GC_BLOCK_BYTES)
            {
                large_type_count += 1;
            }
        }
        // runtime raw 平面契约：输入来自冻结前端产物与目标描述，与 LIR 一起构成内部表示。
        let coroutine_demand = lir.coroutine_demand();
        let shared_heap_demand = lir.shared_heap_demand(max_object_bytes);
        let demand = RawPlaneDemand {
            coroutine_sites: coroutine_demand.creation_sites,
            checked_entries: coroutine_demand.checked_entries,
            suspend_points: coroutine_demand.suspend_points,
            resource_sites: frontend.gir.placement.counts().resource,
            runtime_raw_sites: frontend.gir.placement.counts().runtime_raw,
            owners: 0,
            message_nodes: 0,
            turn_region: lir.turn_region_demand(),
            shared_heap: shared_heap_demand,
        };
        let rt0_demand = {
            let module = frontend.hir.module();
            let main_returns_result = module
                .entry
                .and_then(|entry| {
                    frontend
                        .gir
                        .bodies
                        .iter()
                        .find(|body| body.owner == entry)
                        .map(|body| body.signature.result)
                })
                .and_then(|result| module.types.get(result.index()))
                .is_some_and(|result| matches!(result, frontend::hir::Type::Result(..)));
            Rt0Demand {
                entry_present: module.entry.is_some(),
                main_returns_result,
            }
        };
        let action_counts = frontend.gir.resource_action_counts();
        let resource_demand = RawResourceDemand {
            resource_sites: frontend.gir.placement.counts().resource,
            acquire_sites: action_counts.0,
            release_sites: action_counts.1,
            transfer_sites: action_counts.2,
            finalize_sites: action_counts.3,
            owners: 0,
            kinds: 0,
        };
        let barrier_demand = lir.barrier_demand();
        let gc_demand = gc_metadata_demand(&gc_metadata);
        let local_heap_demand =
            lir.local_heap_demand(managed_type_count, large_type_count, max_object_bytes);
        // mark 需求完全由已有三份 demand 推导，不新增 LIR 遍历，避免出现第二份站点口径；
        // 跨 owner ticket 的来源就是 SharedHeap 的共享字段屏障站点。
        let mark_demand = runtime::MarkDemand {
            root_sites: gc_demand.root_range_count,
            barrier_sites: barrier_demand.card_mark_sites,
            ticket_sites: shared_heap_demand.mark_sites,
            edge_delta_sites: barrier_demand.edge_summary_sites,
        };
        // 栈图与压缩引用来自同一次栈图世界推导：压缩根槽与解码点必须与同一份 LIR 对齐。
        let (stackmap_demand, compression_demand) = lir.stackmap_demands(frontend.hir.module());
        let raw_contract = match runtime::run(
            RawModelInputs {
                target,
                // cage profile 是编译期策略：demand 由优化后 LIR 推导，policy 必须在
                // lowering 前已知，这里只把它带进 raw 平面契约，不做事后修正。
                policy: RawPlanePolicyV1 {
                    compression,
                    routing,
                    provenance,
                    combining,
                    ..RawPlanePolicyV1::default()
                },
                demand,
                resource_demand,
                rt0_demand,
                scheduler_demand: lir.scheduler_demand(),
                wait_demand: lir.wait_demand(),
                sync_demand: lir.sync_demand(),
                stackmap_demand,
                compression_demand,
                gc_metadata_demand: gc_demand,
                barrier_demand,
                pacing_demand: lir.pacing_demand(frontend.mono.universe.records.len() as u32),
                mark_demand,
                local_heap_demand,
                gc_type_section: &gc_metadata.type_section,
                gc_metadata_section: &gc_metadata.metadata_section,
                profile: runtime::PlatformProfile::from(target),
                lir_fingerprint: lir.fingerprint(),
                placement_fingerprint: frontend.gir.placement.fingerprint,
                sources: &source_map,
                hir: frontend.hir.module(),
                gir: &frontend.gir,
            },
            &self.queries,
        ) {
            Ok(contract) => contract,
            Err(errors) => {
                for error in errors {
                    diagnostics.push(error);
                }
                graph.fail(ActionKind::BuildIr, "runtime raw 契约校验失败");
                graph.skip_after(ActionKind::BuildIr, "runtime 契约无效");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    source_map,
                    image_plan: None,
                    hir: Some(frontend.hir),
                    gir: Some(frontend.gir),
                    gir_stats: frontend.gir_stats,
                    lir: Some(lir),
                    raw_contract: None,
                    x64: None,
                    action_key: None,
                };
            }
        };
        let gir_blocks = frontend
            .gir
            .bodies
            .iter()
            .map(|body| body.blocks.len())
            .sum::<usize>();
        let gir_stmts = frontend
            .gir
            .bodies
            .iter()
            .map(|body| body.statements.len())
            .sum::<usize>();
        graph.complete(
            ActionKind::BuildIr,
            format!(
                "{} 个定义，{} 个已冻结 HIR owner，{} 个 GIR body / {} 个 block / {} 条语句，{} 个 LIR body / {} 条指令 / {} 个 Mem effect",
                frontend.hir.module().definitions.len(),
                frontend.hir.module().owners.len(),
                frontend.gir.bodies.len(),
                gir_blocks,
                gir_stmts,
                lir.body_count(), lir.instructions(), lir.memory_operations()
            ),
        );

        // 机器码片段：只有可执行入口才产出；统计供给 PlanBackend 的镜像计划。
        let entry_instance = frontend
            .mono
            .root_categories
            .iter()
            .position(|category| *category == RootCategoryV1::Entry)
            .and_then(|index| frontend.mono.roots.get(index).copied());
        let x64 = if frontend.hir.module().entry.is_some() {
            let outcome = entry_instance
                .ok_or_else(|| {
                    vec![Diagnostic::error(
                        DiagnosticCode::BackendInvariant,
                        "可执行入口没有对应的 mono 根",
                        None,
                    )]
                })
                .and_then(|entry| {
                    backend::x64::codegen::build(
                        &lir,
                        &frontend.mono.universe,
                        &raw_contract,
                        target,
                        &self.queries,
                        &source_map,
                        &entry,
                    )
                });
            match outcome {
                Ok(world) => {
                    graph.complete(
                        ActionKind::Codegen,
                        format!(
                            "{} 个实例，{} 个机器站点，{} 字节，{} 个重定位",
                            world.fragment_count(),
                            world.site_count(),
                            world.encoded_bytes(),
                            world.relocation_count()
                        ),
                    );
                    Some(world)
                }
                Err(errors) => {
                    for error in errors {
                        diagnostics.push(error);
                    }
                    graph.fail(ActionKind::Codegen, "机器片段生成失败");
                    graph.skip_after(ActionKind::Codegen, "机器片段无效");
                    diagnostics.sort();
                    return Compilation {
                        graph,
                        diagnostics,
                        source_map,
                        image_plan: None,
                        hir: Some(frontend.hir),
                        gir: Some(frontend.gir),
                        gir_stats: frontend.gir_stats,
                        lir: Some(lir),
                        raw_contract: Some(raw_contract),
                        x64: None,
                        action_key: None,
                    };
                }
            }
        } else {
            graph.complete(ActionKind::Codegen, "没有可执行入口");
            None
        };

        let action_key = Some(compilation_action_key(
            target,
            &source_map,
            &frontend,
            &lir,
            &raw_contract,
            x64.as_ref(),
            loaded
                .plan
                .as_ref()
                .map(|plan| (plan.require_main, &plan.cfg)),
        ));

        let hir = frontend.hir;
        let gir = frontend.gir;

        let Some(backend_plan) = x64.as_ref().and_then(|x64| {
            backend::plan(
                target,
                &hir,
                &frontend.mono,
                &gir,
                &lir,
                &raw_contract,
                x64,
                frontend.analysis.runtime_checks_elided_count,
            )
        }) else {
            graph.complete(ActionKind::PlanBackend, "没有可执行入口");
            graph.skip_after(ActionKind::PlanBackend, "没有可执行入口");
            diagnostics.sort();
            return Compilation {
                graph,
                diagnostics,
                source_map,
                image_plan: None,
                hir: Some(hir),
                gir: Some(gir),
                gir_stats: frontend.gir_stats,
                lir: Some(lir),
                raw_contract: Some(raw_contract),
                x64,
                action_key,
            };
        };
        graph.complete(ActionKind::PlanBackend, "内存 image plan");

        let runtime = RuntimeResources::builtin();
        let attachment = runtime.attach(descriptor.name);
        graph.complete(
            ActionKind::AttachRuntime,
            format!(
                "{} 个 Gugu 源单元，rt0={}，{} 个 raw size class / {} 个 shard / {} 个常驻 node",
                attachment.source_count,
                attachment.rt0,
                raw_contract.class_count(),
                raw_contract.shard_count(),
                raw_contract.message_node_capacity()
            ),
        );
        graph.complete(ActionKind::ValidateImage, "image plan 校验通过");
        graph.skip_after(ActionKind::ValidateImage, "仅保留内存计划，未写出镜像");

        let image_plan = Some(ImagePlan::new(backend_plan, attachment, &raw_contract));
        diagnostics.sort();
        Compilation {
            graph,
            diagnostics,
            source_map,
            image_plan,
            hir: Some(hir),
            gir: Some(gir),
            gir_stats: frontend.gir_stats,
            lir: Some(lir),
            raw_contract: Some(raw_contract),
            x64,
            action_key,
        }
    }
}

/// 将真实 GC metadata bundle 的 section 大小写入 runtime demand。
fn gc_metadata_demand(bundle: &frontend::gc::GcMetadataBundle) -> runtime::GcMetadataDemand {
    let mut demand = bundle.world.demand();
    demand.type_section_bytes = bundle.type_section.len() as u32;
    demand.metadata_section_bytes = bundle.metadata_section.len() as u32;
    demand
}

/// 前端 action 的完整输入集合：identity、host/target、源码摘要、cfg 与 registry 摘要。
fn compilation_action_key(
    target: TargetName,
    source_map: &SourceMap,
    frontend: &frontend::FrontendOutput,
    lir: &lir::Validated,
    raw_contract: &RuntimeRawContractV1,
    x64: Option<&backend::x64::codegen::X64World>,
    plan: Option<(bool, &frontend::cfg::CfgContext)>,
) -> project::ActionKey {
    const EMPTY_PLAN: (bool, Option<&frontend::cfg::CfgContext>) = (true, None);
    let (require_main, cfg) = plan
        .map(|(require_main, cfg)| (require_main, Some(cfg)))
        .unwrap_or(EMPTY_PLAN);
    let fallback_cfg;
    let cfg = match cfg {
        Some(cfg) => cfg,
        None => {
            fallback_cfg = frontend::cfg::CfgContext::target_only(target);
            &fallback_cfg
        }
    };
    let mut inputs = ActionInputs::new(
        format!("gugu-compiler-{}", env!("CARGO_PKG_VERSION")),
        target.to_string(),
        target.to_string(),
        if require_main { "bin" } else { "lib" },
    );
    for snapshot in source_map.snapshots() {
        inputs.add_source(snapshot.logical_path(), snapshot.content());
    }
    for (key, value) in cfg.action_inputs() {
        inputs.set_cfg(key, value);
    }
    inputs.set_comptime_registry(frontend.comptime_registry.1);
    inputs.set_type_universe(frontend.mono.universe.fingerprint);
    inputs.set_late_constants(frontend.mono.late.fingerprint);
    for (key, hash) in &frontend.expansion_inputs.macros {
        inputs.add_macro_input(key.clone(), *hash);
    }
    if !frontend.expansion_inputs.budget.is_empty() {
        inputs.set_macro_budget(&frontend.expansion_inputs.budget);
    }
    inputs.set_analysis_policy(frontend::analysis::AnalysisPolicyV1::default().canonical_bytes());
    inputs.set_analysis_world(frontend.analysis.input_fingerprint);
    inputs.set_generic_gir(frontend.gir.fingerprint);
    inputs.set_lir(lir.fingerprint());
    inputs.set_runtime_raw(raw_contract.fingerprint());
    inputs.set_target_descriptor(target.descriptor().digest());
    if let Some(x64) = x64 {
        inputs.set_backend_encoder(x64.encoder_fingerprint());
        inputs.set_backend_fragments(x64.fingerprint());
    }
    inputs.set_query_registry(crate::query::registry_fingerprint());
    let mut policy = frontend::gir::pass::policy_bytes();
    policy.extend_from_slice(&lir::optimization_policy_bytes());
    inputs.set_optimization_policy(policy);
    for (key, digest) in &frontend.mono.public_summaries {
        inputs.add_public_summary(key.clone(), digest);
    }
    inputs.key()
}

#[derive(Clone, Debug)]
struct FrontendPlan {
    entry: String,
    source_root: String,
    package_identity: String,
    require_main: bool,
    cfg: CfgContext,
    external_packages: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct LoadedInput {
    source_map: SourceMap,
    plan: Option<FrontendPlan>,
}

impl LoadedInput {
    fn as_source_input(&mut self) -> SourceInput<'_> {
        match &mut self.plan {
            None => SourceInput::EmptyPackage,
            Some(plan) => SourceInput::Sources {
                source_map: &mut self.source_map,
                entry: &plan.entry,
                source_root: &plan.source_root,
                package_identity: &plan.package_identity,
                require_main: plan.require_main,
                cfg: &plan.cfg,
                external_packages: &plan.external_packages,
            },
        }
    }

    fn detail(&self) -> String {
        match &self.plan {
            None => "空 package".to_owned(),
            Some(plan) => format!(
                "{} 个源码模块，入口 {}",
                self.source_map.snapshots().len(),
                plan.entry
            ),
        }
    }
}

#[derive(Debug)]
enum LoadInputError {
    Read {
        path: PathBuf,
        error: std::io::Error,
    },
    Snapshot(SourceError),
    /// target 源码树的结构违规：bootstrap 无法识别的源码树形态（例如符号链接）。
    SourceStructure {
        path: PathBuf,
        message: String,
    },
    ModuleTree {
        path: PathBuf,
        message: String,
    },
}

impl LoadInputError {
    fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::Read { path, error } => Diagnostic::source_read(path, error),
            Self::Snapshot(error) => Diagnostic::source_error(error),
            Self::SourceStructure { path, message } => Diagnostic::error(
                DiagnosticCode::MalformedSource,
                message,
                Some(Span::detached(path, 0, 0)),
            ),
            Self::ModuleTree { path, message } => Diagnostic::error(
                DiagnosticCode::ModuleInvalidPath,
                message,
                Some(Span::detached(path, 0, 0)),
            ),
        }
    }
}

fn load_input(input: CompileInput, target: TargetName) -> Result<LoadedInput, LoadInputError> {
    match input {
        CompileInput::EmptyPackage => Ok(LoadedInput {
            source_map: SourceMap::empty(),
            plan: None,
        }),
        CompileInput::SingleFileSource { path, source } => {
            let snapshot =
                SourceSnapshot::from_bytes(logical_input_path(&path), source.into_bytes())
                    .map_err(LoadInputError::Snapshot)?;
            loaded_single(snapshot, target, true)
        }
        CompileInput::File {
            path,
            logical_path,
            require_main,
        } => {
            let bytes = fs::read(&path).map_err(|error| LoadInputError::Read {
                path: path.clone(),
                error,
            })?;
            let logical = logical_path.unwrap_or_else(|| logical_input_path(&path));
            let snapshot =
                SourceSnapshot::from_bytes(logical, bytes).map_err(LoadInputError::Snapshot)?;
            loaded_single(snapshot, target, require_main)
        }
        CompileInput::Project {
            package_root,
            source_root,
            entry,
            package_identity,
            require_main,
            declared_features,
            enabled_features,
            external_packages,
            test,
            bench,
            custom_cfg,
        } => load_project_sources(
            &package_root,
            &source_root,
            &entry,
            package_identity,
            require_main,
            CfgContext::new(
                target,
                declared_features,
                enabled_features,
                test,
                bench,
                custom_cfg,
            ),
            external_packages,
        ),
    }
}

fn loaded_single(
    snapshot: SourceSnapshot,
    target: TargetName,
    require_main: bool,
) -> Result<LoadedInput, LoadInputError> {
    let entry = snapshot.logical_path().to_owned();
    let source_map =
        SourceMap::new(vec![snapshot]).map_err(|error| LoadInputError::ModuleTree {
            path: PathBuf::from(&entry),
            message: error.to_string(),
        })?;
    Ok(LoadedInput {
        source_map,
        plan: Some(FrontendPlan {
            entry,
            source_root: String::new(),
            package_identity: "single-file".to_owned(),
            require_main,
            cfg: CfgContext::target_only(target),
            external_packages: BTreeSet::new(),
        }),
    })
}

fn load_project_sources(
    package_root: &Path,
    source_root: &Path,
    entry: &Path,
    package_identity: String,
    require_main: bool,
    cfg: CfgContext,
    external_packages: BTreeSet<String>,
) -> Result<LoadedInput, LoadInputError> {
    let mut paths = Vec::new();
    collect_source_paths(source_root, &mut paths)?;
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        snapshots.push(read_project_snapshot(package_root, &path)?);
    }
    let entry = project_logical_path(package_root, entry)?;
    let source_root = if package_root == source_root {
        String::new()
    } else {
        project_logical_path(package_root, source_root)?
    };
    let source_map = SourceMap::new(snapshots).map_err(|error| LoadInputError::ModuleTree {
        path: package_root.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(LoadedInput {
        source_map,
        plan: Some(FrontendPlan {
            entry,
            source_root,
            package_identity,
            require_main,
            cfg,
            external_packages,
        }),
    })
}

fn collect_source_paths(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), LoadInputError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| LoadInputError::Read {
            path: directory.to_path_buf(),
            error,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| LoadInputError::Read {
            path: directory.to_path_buf(),
            error,
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let kind = entry.file_type().map_err(|error| LoadInputError::Read {
            path: entry.path(),
            error,
        })?;
        if kind.is_symlink() {
            return Err(LoadInputError::SourceStructure {
                path: entry.path(),
                message: "target 源码树不允许符号链接".to_owned(),
            });
        }
        if kind.is_dir() {
            collect_source_paths(&entry.path(), paths)?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "gg") {
            paths.push(entry.path());
        }
    }
    Ok(())
}

fn read_project_snapshot(
    package_root: &Path,
    path: &Path,
) -> Result<SourceSnapshot, LoadInputError> {
    let bytes = fs::read(path).map_err(|error| LoadInputError::Read {
        path: path.to_path_buf(),
        error,
    })?;
    let logical = path
        .strip_prefix(package_root)
        .map_err(|_| LoadInputError::ModuleTree {
            path: path.to_path_buf(),
            message: "模块源码越过 package 根".to_owned(),
        })?;
    SourceSnapshot::from_bytes(logical, bytes).map_err(LoadInputError::Snapshot)
}

fn project_logical_path(package_root: &Path, path: &Path) -> Result<String, LoadInputError> {
    let relative = path
        .strip_prefix(package_root)
        .map_err(|_| LoadInputError::ModuleTree {
            path: path.to_path_buf(),
            message: "target 路径越过 package 根".to_owned(),
        })?;
    crate::source::normalize_logical_path(relative).map_err(LoadInputError::Snapshot)
}

fn logical_input_path(path: &std::path::Path) -> PathBuf {
    let relative = if !path.is_absolute() {
        path.to_path_buf()
    } else {
        // 绝对输入优先按工作目录相对名推导；无法剥离时退回末尾两个普通分量，
        // 保留目录信息、避免同名不同目录被合并，也不依赖宿主硬编码占位名。
        std::env::current_dir()
            .ok()
            .and_then(|current| path.strip_prefix(current).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(PathBuf::from)
            .or_else(|| trailing_components(path))
            .unwrap_or_default()
    };
    // 无论宿主平台路径分隔符为何（如 Windows 下为反斜杠），
    // 逻辑输入路径均统一归一为以 `/` 分隔的规范形态。
    PathBuf::from(path_to_logical_string(&relative))
}

fn path_to_logical_string(path: &std::path::Path) -> String {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => {
                if let Some(s) = value.to_str() {
                    components.push(s);
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.push("..");
            }
            _ => {}
        }
    }
    components.join("/")
}

/// 取出绝对路径末尾两个普通分量作为逻辑名，方便诊断定位且与工作目录无关。
fn trailing_components(path: &std::path::Path) -> Option<PathBuf> {
    let names = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str().map(str::to_owned),
            _ => None,
        })
        .rev()
        .take(2)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return None;
    }
    let logical = names.into_iter().rev().collect::<Vec<_>>().join("/");
    Some(PathBuf::from(logical))
}

/// 内存镜像计划，不是可执行文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImagePlan {
    target: TargetName,
    entry: String,
    function_count: u32,
    runtime_source_count: u32,
    runtime_checks_elided_count: u32,
    mono_instance_count: u32,
    mono_root_count: u32,
    mono_graph_fingerprint: [u8; 32],
    type_id_count: u32,
    type_universe_fingerprint: [u8; 32],
    late_constant_count: u32,
    late_constants_fingerprint: [u8; 32],
    gir_body_count: u32,
    gir_block_count: u32,
    gir_statement_count: u32,
    gir_fingerprint: [u8; 32],
    lir_body_count: u32,
    lir_block_count: u32,
    lir_instruction_count: u32,
    lir_memory_operation_count: u32,
    lir_safepoint_count: u32,
    lir_fingerprint: [u8; 32],
    optimization_revision: u32,
    poll_budget: u32,
    poll_count: u32,
    poll_free_leaf_count: u32,
    poll_summary_fingerprint: [u8; 32],
    raw_size_class_count: u32,
    raw_shard_count: u32,
    raw_batch_max_items: u32,
    raw_batch_soft_bytes: u64,
    raw_message_node_capacity: u32,
    raw_model_fingerprint: [u8; 32],
    coroutine_runtime: CoroutineRuntimeContract,
    scheduler_local_capacity: u32,
    scheduler_remote_shard_count: u32,
    scheduler_batch_max_items: u32,
    scheduler_service_interval: u32,
    scheduler_service_batch: u32,
    scheduler_contract_fingerprint: [u8; 32],
    scheduler_runtime: SchedulerRuntimeContract,
    wait_inline_select_cases: u32,
    wait_scratch_class_count: u32,
    wait_node_class_count: u32,
    wait_contract_fingerprint: [u8; 32],
    wait_demand: WaitDemand,
    wait_runtime: WaitRuntimeContract,
    sync_contract_fingerprint: [u8; 32],
    sync_demand: crate::runtime::SyncDemand,
    sync_primitive_count: u32,
    sync_runtime: crate::runtime::SyncRuntimeContract,
    stackmap_function_count: u32,
    stackmap_safepoint_count: u32,
    stackmap_map_count: u32,
    stackmap_root_words: u32,
    stackmap_contract_fingerprint: [u8; 32],
    stackmap_demand: crate::runtime::StackMapDemand,
    gc_metadata_type_count: u32,
    gc_metadata_trace_bytes: u32,
    gc_metadata_value_bytes: u32,
    gc_metadata_vtable_count: u32,
    gc_metadata_root_count: u32,
    gc_metadata_arena_bytes: u64,
    gc_metadata_block_bytes: u32,
    gc_metadata_line_bytes: u32,
    gc_metadata_contract_fingerprint: [u8; 32],
    gc_metadata_demand: crate::runtime::GcMetadataDemand,
    gc_type_section: Vec<u8>,
    gc_metadata_section: Vec<u8>,
    gc_type_section_fingerprint: [u8; 32],
    gc_metadata_section_fingerprint: [u8; 32],
    barrier_contract_fingerprint: [u8; 32],
    barrier_demand: crate::runtime::BarrierDemand,
    barrier_card_granularity_bytes: u32,
    barrier_card_mark_buffer_entries: u32,
    barrier_card_mark_stamp_entries: u32,
    barrier_flush_reason_count: u32,
    barrier_card_mark_batch_fields: u32,
    barrier_record_count: u32,
    barrier_runtime: crate::runtime::BarrierRuntimeContract,
    edge_contract_fingerprint: [u8; 32],
    edge_demand: crate::runtime::EdgeDemand,
    edge_runtime: crate::runtime::EdgeRuntimeContract,
    local_heap_contract_fingerprint: [u8; 32],
    local_heap_demand: crate::runtime::LocalHeapDemand,
    local_heap_runtime: crate::runtime::LocalHeapRuntimeContract,
    shared_heap_contract_fingerprint: [u8; 32],
    shared_heap_demand: crate::runtime::SharedHeapDemand,
    shared_heap_runtime: crate::runtime::SharedHeapRuntimeContract,
    block_return_contract_fingerprint: [u8; 32],
    block_return_demand: crate::runtime::BlockReturnDemand,
    block_return_runtime: crate::runtime::BlockReturnRuntimeContract,
    compression_contract_fingerprint: [u8; 32],
    compression_demand: crate::runtime::CompressionDemand,
    compression_runtime: crate::runtime::CompressionRuntimeContract,
    compression_capability: PointerCompression,
    routing_contract_fingerprint: [u8; 32],
    routing_demand: crate::runtime::RoutingDemand,
    routing_runtime: crate::runtime::RoutingRuntimeContract,
    provenance_contract_fingerprint: [u8; 32],
    provenance_demand: crate::runtime::ProvenanceDemand,
    provenance_runtime: crate::runtime::ProvenanceRuntimeContract,
    combining_contract_fingerprint: [u8; 32],
    combining_demand: crate::runtime::CombiningDemand,
    combining_runtime: crate::runtime::CombiningRuntimeContract,
    mark_contract_fingerprint: [u8; 32],
    mark_demand: crate::runtime::MarkDemand,
    mark_runtime: crate::runtime::MarkRuntimeContract,
    mark_cycle_state_count: u32,
    mark_condition_count: u32,
    mark_snapshot_participant_count: u32,
    mark_credit_pool: u64,
    mark_mailbox_consumer_count: u32,
    mark_ticket_field_count: u32,
    mark_record_count: u32,
    pacing_contract_fingerprint: [u8; 32],
    pacing_profile: String,
    pacing_profile_revision: u32,
    pacing_min_growth_budget: u64,
    pacing_assist_threshold: u64,
    pacing_assist_quantum: u64,
    pacing_mark_cost_per_byte: u32,
    pacing_gc_cpu_fraction: u32,
    pacing_gc_cpu_window_cost: u64,
    pacing_remark_cost_budget: u64,
    pacing_evacuation_pause_bytes: u64,
    pacing_evacuation_pause_roots: u32,
    pacing_evacuation_pause_fields: u32,
    pacing_pressure_enter_ratio: u32,
    pacing_pressure_clear_ratio: u32,
    pacing_credit_source_count: u32,
    pacing_pressure_poll_bytes: u64,
    pacing_owner_drain_items: u32,
    pacing_owner_drain_bytes: u64,
    pacing_owner_drain_interval_bytes: u64,
    turn_region_sites: u32,
    turn_region_publish_sites: u32,
    turn_region_reset_sites: u32,
    turn_region_promote_sites: u32,
    turn_region_transfer_sites: u32,
    turn_region_capacity_class_count: u32,
    turn_region_object_limit: u32,
    turn_region_max_bytes: u64,
    turn_region_total_bytes: u64,
    turn_region_contract_fingerprint: [u8; 32],
    turn_region_demand: crate::runtime::TurnRegionDemand,
    turn_region_runtime: crate::runtime::TurnRegionRuntimeContract,
    pacing_demand: crate::runtime::GcPacingDemand,
    pacing_runtime: crate::runtime::GcPacingRuntimeContract,
    resource_cell_header_bytes: u32,
    resource_class_count: u32,
    resource_kind_count: u32,
    release_descriptor_count: u32,
    resource_sites: u32,
    release_sites: u32,
    platform_profile: String,
    platform_op_count: u32,
    platform_range_class_count: u32,
    platform_contract_fingerprint: [u8; 32],
    platform_range_demand: PlatformRangeDemand,
    ledger_category_count: u32,
    rt0_step_count: u32,
    rt0_lifecycle_count: u32,
    startup_config_var_count: u32,
    startup_fatal_count: u32,
    report_reason_count: u32,
    rt0_emergency_buffer_bytes: u32,
    rt0_contract_fingerprint: [u8; 32],
    rt0_entry_present: bool,
    rt0_main_returns_result: bool,
    placement_count: u32,
    turn_region_count: u32,
    local_heap_count: u32,
    shared_heap_count: u32,
    placement_fingerprint: [u8; 32],
    rt0: Rt0Boundary,
    semantic_fingerprint: [u8; 32],
    target_descriptor_digest: [u8; 32],
    target_page_size: u32,
    target_cpu_baseline: CpuBaseline,
    import_policy_revision: u32,
    x64_encoder_fingerprint: [u8; 32],
    x64_form_count: u32,
    x64_lowering_revision: u32,
    x64_site_count: u32,
    x64_instruction_count: u32,
    x64_encoded_bytes: u32,
    x64_relocation_count: u32,
    x64_cold_edge_count: u32,
    x64_decode_sequence_bytes: u32,
    x64_fragment_fingerprint: [u8; 32],
    x64_rel8_count: u32,
    x64_hot_block_count: u32,
    x64_cold_block_count: u32,
    x64_entry_symbol: String,
    x64_frame_size_max: u32,
    x64_spill_slot_count: u32,
    x64_spill_bytes: u32,
    x64_saved_gpr_count: u32,
    x64_reload_count: u32,
    x64_spill_store_count: u32,
    x64_copy_move_count: u32,
    x64_copy_cycle_count: u32,
    x64_peak_live_gpr: u32,
    x64_peak_live_xmm: u32,
    x64_allocated_values: u32,
    coroutine_stack_check_offset: u32,
    scheduler_poll_flags_offset: u32,
}

impl ImagePlan {
    fn new(
        plan: BackendPlan,
        attachment: runtime::RuntimeAttachment,
        raw: &RuntimeRawContractV1,
    ) -> Self {
        let gc_type_section_fingerprint =
            frontend::mono::keys::hash_domain("gugu-gc-type-section-v1", &plan.gc_type_section);
        let gc_metadata_section_fingerprint = frontend::mono::keys::hash_domain(
            "gugu-gc-metadata-section-v1",
            &plan.gc_metadata_section,
        );
        Self {
            target: plan.target,
            entry: plan.entry,
            function_count: plan.function_count,
            runtime_source_count: attachment.source_count,
            runtime_checks_elided_count: plan.runtime_checks_elided_count,
            mono_instance_count: plan.mono_instance_count,
            mono_root_count: plan.mono_root_count,
            mono_graph_fingerprint: plan.mono_graph_fingerprint,
            type_id_count: plan.type_id_count,
            type_universe_fingerprint: plan.type_universe_fingerprint,
            late_constant_count: plan.late_constant_count,
            late_constants_fingerprint: plan.late_constants_fingerprint,
            gir_body_count: plan.gir_body_count,
            gir_block_count: plan.gir_block_count,
            gir_statement_count: plan.gir_statement_count,
            gir_fingerprint: plan.gir_fingerprint,
            lir_body_count: plan.lir_body_count,
            lir_block_count: plan.lir_block_count,
            lir_instruction_count: plan.lir_instruction_count,
            lir_memory_operation_count: plan.lir_memory_operation_count,
            lir_safepoint_count: plan.lir_safepoint_count,
            lir_fingerprint: plan.lir_fingerprint,
            optimization_revision: plan.optimization_revision,
            poll_budget: plan.poll_budget,
            poll_count: plan.poll_count,
            poll_free_leaf_count: plan.poll_free_leaf_count,
            poll_summary_fingerprint: plan.poll_summary_fingerprint,
            raw_size_class_count: raw.class_count(),
            raw_shard_count: raw.shard_count(),
            raw_batch_max_items: raw.batch_limits().items,
            raw_batch_soft_bytes: raw.batch_limits().batch_soft_bytes,
            raw_message_node_capacity: raw.message_node_capacity(),
            raw_model_fingerprint: raw.fingerprint(),
            coroutine_runtime: plan.coroutine_runtime,
            scheduler_local_capacity: plan.scheduler_local_capacity,
            scheduler_remote_shard_count: plan.scheduler_remote_shard_count,
            scheduler_batch_max_items: plan.scheduler_batch_max_items,
            scheduler_service_interval: plan.scheduler_service_interval,
            scheduler_service_batch: plan.scheduler_service_batch,
            scheduler_contract_fingerprint: plan.scheduler_contract_fingerprint,
            scheduler_runtime: plan.scheduler_runtime,
            wait_inline_select_cases: plan.wait_inline_select_cases,
            wait_scratch_class_count: plan.wait_scratch_class_count,
            wait_node_class_count: plan.wait_node_class_count,
            wait_contract_fingerprint: plan.wait_contract_fingerprint,
            wait_demand: plan.wait_demand,
            wait_runtime: plan.wait_runtime,
            sync_contract_fingerprint: plan.sync_contract_fingerprint,
            sync_demand: plan.sync_demand,
            sync_primitive_count: plan.sync_primitive_count,
            sync_runtime: plan.sync_runtime,
            stackmap_function_count: plan.stackmap_function_count,
            stackmap_safepoint_count: plan.stackmap_safepoint_count,
            stackmap_map_count: plan.stackmap_map_count,
            stackmap_root_words: plan.stackmap_root_words,
            stackmap_contract_fingerprint: plan.stackmap_contract_fingerprint,
            stackmap_demand: plan.stackmap_demand,
            gc_metadata_type_count: plan.gc_metadata_type_count,
            gc_metadata_trace_bytes: plan.gc_metadata_trace_bytes,
            gc_metadata_value_bytes: plan.gc_metadata_value_bytes,
            gc_metadata_vtable_count: plan.gc_metadata_vtable_count,
            gc_metadata_root_count: plan.gc_metadata_root_count,
            gc_metadata_arena_bytes: plan.gc_metadata_arena_bytes,
            gc_metadata_block_bytes: plan.gc_metadata_block_bytes,
            gc_metadata_line_bytes: plan.gc_metadata_line_bytes,
            gc_metadata_contract_fingerprint: plan.gc_metadata_contract_fingerprint,
            gc_metadata_demand: plan.gc_metadata_demand,
            gc_type_section: plan.gc_type_section,
            gc_metadata_section: plan.gc_metadata_section,
            gc_type_section_fingerprint,
            gc_metadata_section_fingerprint,
            barrier_contract_fingerprint: plan.barrier_contract_fingerprint,
            barrier_demand: plan.barrier_demand,
            barrier_card_granularity_bytes: plan.barrier_card_granularity_bytes,
            barrier_card_mark_buffer_entries: plan.barrier_card_mark_buffer_entries,
            barrier_card_mark_stamp_entries: plan.barrier_card_mark_stamp_entries,
            barrier_flush_reason_count: plan.barrier_flush_reason_count,
            barrier_card_mark_batch_fields: plan.barrier_card_mark_batch_fields,
            barrier_record_count: plan.barrier_record_count,
            barrier_runtime: plan.barrier_runtime,
            edge_contract_fingerprint: plan.edge_contract_fingerprint,
            edge_demand: plan.edge_demand,
            edge_runtime: plan.edge_runtime,
            local_heap_contract_fingerprint: plan.local_heap_contract_fingerprint,
            local_heap_demand: plan.local_heap_demand,
            local_heap_runtime: plan.local_heap_runtime,
            shared_heap_contract_fingerprint: plan.shared_heap_contract_fingerprint,
            shared_heap_demand: plan.shared_heap_demand,
            shared_heap_runtime: plan.shared_heap_runtime,
            block_return_contract_fingerprint: plan.block_return_contract_fingerprint,
            block_return_demand: plan.block_return_demand,
            block_return_runtime: plan.block_return_runtime,
            compression_contract_fingerprint: plan.compression_contract_fingerprint,
            compression_demand: plan.compression_demand,
            compression_runtime: plan.compression_runtime,
            compression_capability: plan.compression_capability,
            routing_contract_fingerprint: plan.routing_contract_fingerprint,
            routing_demand: plan.routing_demand,
            routing_runtime: plan.routing_runtime,
            provenance_contract_fingerprint: plan.provenance_contract_fingerprint,
            provenance_demand: plan.provenance_demand,
            provenance_runtime: plan.provenance_runtime,
            combining_contract_fingerprint: plan.combining_contract_fingerprint,
            combining_demand: plan.combining_demand,
            combining_runtime: plan.combining_runtime,
            mark_contract_fingerprint: plan.mark_contract_fingerprint,
            mark_demand: plan.mark_demand,
            mark_runtime: plan.mark_runtime,
            mark_cycle_state_count: plan.mark_cycle_state_count,
            mark_condition_count: plan.mark_condition_count,
            mark_snapshot_participant_count: plan.mark_snapshot_participant_count,
            mark_credit_pool: plan.mark_credit_pool,
            mark_mailbox_consumer_count: plan.mark_mailbox_consumer_count,
            mark_ticket_field_count: plan.mark_ticket_field_count,
            mark_record_count: plan.mark_record_count,
            pacing_contract_fingerprint: plan.pacing_contract_fingerprint,
            pacing_profile: plan.pacing_profile,
            pacing_profile_revision: plan.pacing_profile_revision,
            pacing_min_growth_budget: plan.pacing_min_growth_budget,
            pacing_assist_threshold: plan.pacing_assist_threshold,
            pacing_assist_quantum: plan.pacing_assist_quantum,
            pacing_mark_cost_per_byte: plan.pacing_mark_cost_per_byte,
            pacing_gc_cpu_fraction: plan.pacing_gc_cpu_fraction,
            pacing_gc_cpu_window_cost: plan.pacing_gc_cpu_window_cost,
            pacing_remark_cost_budget: plan.pacing_remark_cost_budget,
            pacing_evacuation_pause_bytes: plan.pacing_evacuation_pause_bytes,
            pacing_evacuation_pause_roots: plan.pacing_evacuation_pause_roots,
            pacing_evacuation_pause_fields: plan.pacing_evacuation_pause_fields,
            pacing_pressure_enter_ratio: plan.pacing_pressure_enter_ratio,
            pacing_pressure_clear_ratio: plan.pacing_pressure_clear_ratio,
            pacing_credit_source_count: plan.pacing_credit_source_count,
            pacing_pressure_poll_bytes: plan.pacing_pressure_poll_bytes,
            pacing_owner_drain_items: plan.pacing_owner_drain_items,
            pacing_owner_drain_bytes: plan.pacing_owner_drain_bytes,
            pacing_owner_drain_interval_bytes: plan.pacing_owner_drain_interval_bytes,
            turn_region_sites: plan.turn_region_sites,
            turn_region_publish_sites: plan.turn_region_publish_sites,
            turn_region_reset_sites: plan.turn_region_reset_sites,
            turn_region_promote_sites: plan.turn_region_promote_sites,
            turn_region_transfer_sites: plan.turn_region_transfer_sites,
            turn_region_capacity_class_count: plan.turn_region_capacity_class_count,
            turn_region_object_limit: plan.turn_region_object_limit,
            turn_region_max_bytes: plan.turn_region_max_bytes,
            turn_region_total_bytes: plan.turn_region_total_bytes,
            turn_region_contract_fingerprint: plan.turn_region_contract_fingerprint,
            turn_region_demand: plan.turn_region_demand,
            turn_region_runtime: plan.turn_region_runtime,
            pacing_demand: plan.pacing_demand,
            pacing_runtime: plan.pacing_runtime,
            resource_cell_header_bytes: plan.resource_cell_header_bytes,
            resource_class_count: plan.resource_class_count,
            resource_kind_count: plan.resource_kind_count,
            release_descriptor_count: plan.release_descriptor_count,
            resource_sites: plan.resource_sites,
            release_sites: plan.release_sites,
            platform_profile: plan.platform_profile,
            platform_op_count: plan.platform_op_count,
            platform_range_class_count: plan.platform_range_class_count,
            platform_contract_fingerprint: plan.platform_contract_fingerprint,
            platform_range_demand: plan.platform_demand,
            ledger_category_count: plan.ledger_category_count,
            rt0_step_count: raw.rt0().steps().len() as u32,
            rt0_lifecycle_count: raw.rt0().states().len() as u32,
            startup_config_var_count: raw.rt0().startup_vars().len() as u32,
            startup_fatal_count: raw.rt0().fatal_kinds().len() as u32,
            report_reason_count: raw.rt0().report_reasons().len() as u32,
            rt0_emergency_buffer_bytes: raw.rt0().emergency().capacity_bytes,
            rt0_contract_fingerprint: raw.rt0().fingerprint(),
            rt0_entry_present: raw.rt0().demand().entry_present,
            rt0_main_returns_result: raw.rt0().demand().main_returns_result,
            placement_count: plan.placement_count,
            turn_region_count: plan.turn_region_count,
            local_heap_count: plan.local_heap_count,
            shared_heap_count: plan.shared_heap_count,
            placement_fingerprint: plan.placement_fingerprint,
            rt0: attachment.rt0,
            semantic_fingerprint: plan.semantic_fingerprint,
            target_descriptor_digest: plan.target_descriptor_digest,
            target_page_size: plan.target_page_size,
            target_cpu_baseline: plan.target_cpu_baseline,
            import_policy_revision: plan.import_policy_revision,
            x64_encoder_fingerprint: plan.x64_encoder_fingerprint,
            x64_form_count: plan.x64_form_count,
            x64_lowering_revision: plan.x64_lowering_revision,
            x64_site_count: plan.x64_site_count,
            x64_instruction_count: plan.x64_instruction_count,
            x64_encoded_bytes: plan.x64_encoded_bytes,
            x64_relocation_count: plan.x64_relocation_count,
            x64_cold_edge_count: plan.x64_cold_edge_count,
            x64_decode_sequence_bytes: plan.x64_decode_sequence_bytes,
            x64_fragment_fingerprint: plan.x64_fragment_fingerprint,
            x64_rel8_count: plan.x64_rel8_count,
            x64_hot_block_count: plan.x64_hot_block_count,
            x64_cold_block_count: plan.x64_cold_block_count,
            x64_entry_symbol: plan.x64_entry_symbol,
            x64_frame_size_max: plan.x64_frame_size_max,
            x64_spill_slot_count: plan.x64_spill_slot_count,
            x64_spill_bytes: plan.x64_spill_bytes,
            x64_saved_gpr_count: plan.x64_saved_gpr_count,
            x64_reload_count: plan.x64_reload_count,
            x64_spill_store_count: plan.x64_spill_store_count,
            x64_copy_move_count: plan.x64_copy_move_count,
            x64_copy_cycle_count: plan.x64_copy_cycle_count,
            x64_peak_live_gpr: plan.x64_peak_live_gpr,
            x64_peak_live_xmm: plan.x64_peak_live_xmm,
            x64_allocated_values: plan.x64_allocated_values,
            coroutine_stack_check_offset: plan.coroutine_stack_check_offset,
            scheduler_poll_flags_offset: plan.scheduler_poll_flags_offset,
        }
    }

    /// 返回已验证的协程布局、栈策略与实际context切换片段。
    pub fn coroutine_runtime(&self) -> &CoroutineRuntimeContract {
        &self.coroutine_runtime
    }

    /// 返回目标名称。
    pub fn target(&self) -> TargetName {
        self.target
    }

    /// 返回入口符号。
    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// 返回已冻结的函数、闭包及 async body 数量。
    pub fn function_count(&self) -> u32 {
        self.function_count
    }

    /// 返回计划附加的 Gugu 源单元数量。
    pub fn runtime_source_count(&self) -> u32 {
        self.runtime_source_count
    }

    /// 返回计划使用的 rt0 边界。
    pub fn rt0(&self) -> Rt0Boundary {
        self.rt0
    }

    /// 返回平台范围适配的 profile 名。
    pub fn platform_profile(&self) -> &str {
        &self.platform_profile
    }

    /// 返回平台范围操作目录数量。
    pub fn platform_op_count(&self) -> u32 {
        self.platform_op_count
    }

    /// 返回二次幂 extent 阶梯的 class 数量。
    pub fn platform_range_class_count(&self) -> u32 {
        self.platform_range_class_count
    }

    /// 返回平台范围契约段的稳定指纹。
    pub fn platform_contract_fingerprint(&self) -> [u8; 32] {
        self.platform_contract_fingerprint
    }

    /// 返回平台范围需求下界。
    pub fn platform_range_demand(&self) -> PlatformRangeDemand {
        self.platform_range_demand
    }

    /// 返回内存账本的分类数量。
    pub fn ledger_category_count(&self) -> u32 {
        self.ledger_category_count
    }

    /// 返回目标描述符指纹。
    pub fn target_descriptor_digest(&self) -> [u8; 32] {
        self.target_descriptor_digest
    }

    /// 返回目标页大小。
    pub fn target_page_size(&self) -> u32 {
        self.target_page_size
    }

    /// 返回目标 CPU 基线。
    pub fn target_cpu_baseline(&self) -> CpuBaseline {
        self.target_cpu_baseline
    }

    /// 返回导入策略 revision。
    pub fn import_policy_revision(&self) -> u32 {
        self.import_policy_revision
    }

    /// 返回 x64 encoder 契约指纹。
    pub fn x64_encoder_fingerprint(&self) -> [u8; 32] {
        self.x64_encoder_fingerprint
    }

    /// 返回 form 目录长度。
    pub fn x64_form_count(&self) -> u32 {
        self.x64_form_count
    }

    /// 返回 lowering 规则 revision。
    pub fn x64_lowering_revision(&self) -> u32 {
        self.x64_lowering_revision
    }

    /// 返回 lowered 站点总数。
    pub fn x64_site_count(&self) -> u32 {
        self.x64_site_count
    }

    /// 返回站点机器指令总数。
    pub fn x64_instruction_count(&self) -> u32 {
        self.x64_instruction_count
    }

    /// 返回片段字节总数。
    pub fn x64_encoded_bytes(&self) -> u32 {
        self.x64_encoded_bytes
    }

    /// 返回重定位总数。
    pub fn x64_relocation_count(&self) -> u32 {
        self.x64_relocation_count
    }

    /// 返回冷边站点数。
    pub fn x64_cold_edge_count(&self) -> u32 {
        self.x64_cold_edge_count
    }

    /// 返回压缩引用解码序列字节数。
    pub fn x64_decode_sequence_bytes(&self) -> u32 {
        self.x64_decode_sequence_bytes
    }

    /// 返回机器码片段世界指纹。
    pub fn x64_fragment_fingerprint(&self) -> [u8; 32] {
        self.x64_fragment_fingerprint
    }

    /// 返回收缩后的 rel8 条数。
    pub fn x64_rel8_count(&self) -> u32 {
        self.x64_rel8_count
    }

    /// 返回热块数。
    pub fn x64_hot_block_count(&self) -> u32 {
        self.x64_hot_block_count
    }

    /// 返回冷块数。
    pub fn x64_cold_block_count(&self) -> u32 {
        self.x64_cold_block_count
    }

    /// 返回入口 mangled 符号。
    pub fn x64_entry_symbol(&self) -> &str {
        &self.x64_entry_symbol
    }

    /// 返回分配后的最大栈帧字节数。
    pub fn x64_frame_size_max(&self) -> u32 {
        self.x64_frame_size_max
    }

    /// 返回溢出槽总数。
    pub fn x64_spill_slot_count(&self) -> u32 {
        self.x64_spill_slot_count
    }

    /// 返回溢出区字节数。
    pub fn x64_spill_bytes(&self) -> u32 {
        self.x64_spill_bytes
    }

    /// 返回保存的 callee-saved GPR 数。
    pub fn x64_saved_gpr_count(&self) -> u32 {
        self.x64_saved_gpr_count
    }

    /// 返回溢出重载次数。
    pub fn x64_reload_count(&self) -> u32 {
        self.x64_reload_count
    }

    /// 返回溢出写回次数。
    pub fn x64_spill_store_count(&self) -> u32 {
        self.x64_spill_store_count
    }

    /// 返回并行拷贝移动总数。
    pub fn x64_copy_move_count(&self) -> u32 {
        self.x64_copy_move_count
    }

    /// 返回并行拷贝破环次数。
    pub fn x64_copy_cycle_count(&self) -> u32 {
        self.x64_copy_cycle_count
    }

    /// 返回峰值活跃 GPR 数。
    pub fn x64_peak_live_gpr(&self) -> u32 {
        self.x64_peak_live_gpr
    }

    /// 返回峰值活跃 XMM 数。
    pub fn x64_peak_live_xmm(&self) -> u32 {
        self.x64_peak_live_xmm
    }

    /// 返回分配器处理的值总数。
    pub fn x64_allocated_values(&self) -> u32 {
        self.x64_allocated_values
    }

    /// 返回协程栈检查偏移。
    pub fn coroutine_stack_check_offset(&self) -> u32 {
        self.coroutine_stack_check_offset
    }

    /// 返回 `[r15 + poll_flags]` 偏移。
    pub fn scheduler_poll_flags_offset(&self) -> u32 {
        self.scheduler_poll_flags_offset
    }

    /// 返回 rt0 启动序列的步骤数量。
    pub fn rt0_step_count(&self) -> u32 {
        self.rt0_step_count
    }

    /// 返回生命周期状态数量。
    pub fn rt0_lifecycle_count(&self) -> u32 {
        self.rt0_lifecycle_count
    }

    /// 返回启动变量的数量。
    pub fn startup_config_var_count(&self) -> u32 {
        self.startup_config_var_count
    }

    /// 返回 fatal 分类的数量。
    pub fn startup_fatal_count(&self) -> u32 {
        self.startup_fatal_count
    }

    /// 返回报告 reason 目录的数量。
    pub fn report_reason_count(&self) -> u32 {
        self.report_reason_count
    }

    /// 返回 emergency buffer 容量。
    pub fn rt0_emergency_buffer_bytes(&self) -> u32 {
        self.rt0_emergency_buffer_bytes
    }

    /// 返回 rt0 启动契约段的稳定指纹。
    pub fn rt0_contract_fingerprint(&self) -> [u8; 32] {
        self.rt0_contract_fingerprint
    }

    /// 返回编译产物是否含 `main` 入口。
    pub fn rt0_entry_present(&self) -> bool {
        self.rt0_entry_present
    }

    /// 返回 `main` 是否返回 `Result[(), E]`。
    pub fn rt0_main_returns_result(&self) -> bool {
        self.rt0_main_returns_result
    }

    /// 返回已验证前端语义的稳定指纹，用于区分相同入口的不同程序。
    pub fn semantic_fingerprint(&self) -> [u8; 32] {
        self.semantic_fingerprint
    }

    /// 返回 AbstractAnalysis 判定为 `Proved` 的运行时检查数量。
    pub fn runtime_checks_elided_count(&self) -> u32 {
        self.runtime_checks_elided_count
    }

    /// 返回闭世界可达的单态化实例数量。
    pub fn mono_instance_count(&self) -> u32 {
        self.mono_instance_count
    }

    /// 返回闭世界根数量（入口/导出/used/static/asm/harness）。
    pub fn mono_root_count(&self) -> u32 {
        self.mono_root_count
    }

    /// 返回闭世界实例图指纹；实例集合或边变化必然改变该值。
    pub fn mono_graph_fingerprint(&self) -> [u8; 32] {
        self.mono_graph_fingerprint
    }
    /// 返回当前镜像的稠密类型编号数量。
    pub fn type_id_count(&self) -> u32 {
        self.type_id_count
    }
    /// 返回冻结类型集合的内容身份。
    pub fn type_universe_fingerprint(&self) -> [u8; 32] {
        self.type_universe_fingerprint
    }
    /// 返回已物化的后期常量与类型重定位数量。
    pub fn late_constant_count(&self) -> u32 {
        self.late_constant_count
    }
    /// 返回本镜像消费的后期结果指纹。
    pub fn late_constants_fingerprint(&self) -> [u8; 32] {
        self.late_constants_fingerprint
    }
    /// 返回 generic GIR body 数量。
    pub fn gir_body_count(&self) -> u32 {
        self.gir_body_count
    }
    /// 返回 generic GIR block 数量。
    pub fn gir_block_count(&self) -> u32 {
        self.gir_block_count
    }
    /// 返回 generic GIR 语句数量。
    pub fn gir_statement_count(&self) -> u32 {
        self.gir_statement_count
    }
    /// 返回 generic GIR 世界指纹。
    pub fn gir_fingerprint(&self) -> [u8; 32] {
        self.gir_fingerprint
    }
    /// 返回 placement 记录与分配点总数。
    pub fn placement_count(&self) -> u32 {
        self.placement_count
    }
    /// 返回已证明的 TurnRegion 选择数量。
    pub fn turn_region_count(&self) -> u32 {
        self.turn_region_count
    }
    /// 返回 LocalHeap 选择数量。
    pub fn local_heap_count(&self) -> u32 {
        self.local_heap_count
    }
    /// 返回 SharedHeap 选择数量。
    pub fn shared_heap_count(&self) -> u32 {
        self.shared_heap_count
    }
    /// 返回 placement 世界指纹。
    pub fn placement_fingerprint(&self) -> [u8; 32] {
        self.placement_fingerprint
    }

    /// 返回具体 LIR body 数量。
    pub fn lir_body_count(&self) -> u32 {
        self.lir_body_count
    }
    /// 返回 LIR block 数量。
    pub fn lir_block_count(&self) -> u32 {
        self.lir_block_count
    }
    /// 返回 LIR 指令数量。
    pub fn lir_instruction_count(&self) -> u32 {
        self.lir_instruction_count
    }
    /// 返回消费并产生 Mem 的操作数量。
    pub fn lir_memory_operation_count(&self) -> u32 {
        self.lir_memory_operation_count
    }
    /// 返回已登记 safepoint 数量。
    pub fn lir_safepoint_count(&self) -> u32 {
        self.lir_safepoint_count
    }
    /// 返回已验证 LIR 的确定性指纹。
    pub fn lir_fingerprint(&self) -> [u8; 32] {
        self.lir_fingerprint
    }
    /// 返回固定优化管线的 revision。
    pub fn optimization_revision(&self) -> u32 {
        self.optimization_revision
    }
    /// 返回 poll 预算。
    pub fn poll_budget(&self) -> u32 {
        self.poll_budget
    }
    /// 返回预算化 poll 数量。
    pub fn poll_count(&self) -> u32 {
        self.poll_count
    }
    /// 返回 poll-free 叶调用目标数量。
    pub fn poll_free_leaf_count(&self) -> u32 {
        self.poll_free_leaf_count
    }
    /// 返回 poll 摘要指纹。
    pub fn poll_summary_fingerprint(&self) -> [u8; 32] {
        self.poll_summary_fingerprint
    }
    /// 返回 runtime raw 平面的 dense size class 数量。
    pub fn raw_size_class_count(&self) -> u32 {
        self.raw_size_class_count
    }
    /// 返回 owner inbox 的 shard 数量。
    pub fn raw_shard_count(&self) -> u32 {
        self.raw_shard_count
    }
    /// 返回 batch 的 item 上限。
    pub fn raw_batch_max_items(&self) -> u32 {
        self.raw_batch_max_items
    }
    /// 返回 batch 的 byte 上限。
    pub fn raw_batch_soft_bytes(&self) -> u64 {
        self.raw_batch_soft_bytes
    }
    /// 返回常驻 message node 容量下限。
    pub fn raw_message_node_capacity(&self) -> u32 {
        self.raw_message_node_capacity
    }
    /// 返回 runtime raw 平面契约指纹。
    pub fn raw_model_fingerprint(&self) -> [u8; 32] {
        self.raw_model_fingerprint
    }
    /// 返回调度本地队列容量。
    pub fn scheduler_local_capacity(&self) -> u32 {
        self.scheduler_local_capacity
    }
    /// 返回调度 remote 分片数。
    pub fn scheduler_remote_shard_count(&self) -> u32 {
        self.scheduler_remote_shard_count
    }
    /// 返回调度 batch 上限。
    pub fn scheduler_batch_max_items(&self) -> u32 {
        self.scheduler_batch_max_items
    }
    /// 返回调度 service 间隔。
    pub fn scheduler_service_interval(&self) -> u32 {
        self.scheduler_service_interval
    }
    /// 返回调度 service 批量。
    pub fn scheduler_service_batch(&self) -> u32 {
        self.scheduler_service_batch
    }
    /// 返回调度契约指纹。
    pub fn scheduler_contract_fingerprint(&self) -> [u8; 32] {
        self.scheduler_contract_fingerprint
    }
    /// 返回调度契约段。
    pub fn scheduler_runtime(&self) -> &SchedulerRuntimeContract {
        &self.scheduler_runtime
    }
    /// 返回内联 select case 上限。
    pub fn wait_inline_select_cases(&self) -> u32 {
        self.wait_inline_select_cases
    }
    /// 返回 scratch class 数量（不含 0 号内联 class）。
    pub fn wait_scratch_class_count(&self) -> u32 {
        self.wait_scratch_class_count
    }
    /// 返回 wait-node class 数量。
    pub fn wait_node_class_count(&self) -> u32 {
        self.wait_node_class_count
    }
    /// 返回等待契约指纹。
    pub fn wait_contract_fingerprint(&self) -> [u8; 32] {
        self.wait_contract_fingerprint
    }
    /// 返回等待需求视图。
    pub fn wait_demand(&self) -> WaitDemand {
        self.wait_demand
    }
    /// 返回等待契约段。
    pub fn wait_runtime(&self) -> &WaitRuntimeContract {
        &self.wait_runtime
    }
    /// 返回同步契约指纹。
    pub fn sync_contract_fingerprint(&self) -> [u8; 32] {
        self.sync_contract_fingerprint
    }
    /// 返回同步需求视图。
    pub fn sync_demand(&self) -> crate::runtime::SyncDemand {
        self.sync_demand
    }
    /// 返回同步控制块数量。
    pub fn sync_primitive_count(&self) -> u32 {
        self.sync_primitive_count
    }
    /// 返回同步契约段。
    pub fn sync_runtime(&self) -> &crate::runtime::SyncRuntimeContract {
        &self.sync_runtime
    }
    /// 返回栈图逻辑函数记录数。
    pub fn stackmap_function_count(&self) -> u32 {
        self.stackmap_function_count
    }
    /// 返回栈图逻辑安全点记录数。
    pub fn stackmap_safepoint_count(&self) -> u32 {
        self.stackmap_safepoint_count
    }
    /// 返回栈图去重 map 记录数。
    pub fn stackmap_map_count(&self) -> u32 {
        self.stackmap_map_count
    }
    /// 返回栈图五类根字数合计。
    pub fn stackmap_root_words(&self) -> u32 {
        self.stackmap_root_words
    }
    /// 返回栈图契约指纹。
    pub fn stackmap_contract_fingerprint(&self) -> [u8; 32] {
        self.stackmap_contract_fingerprint
    }
    /// 返回栈图需求视图。
    pub fn stackmap_demand(&self) -> crate::runtime::StackMapDemand {
        self.stackmap_demand
    }
    /// 返回 GC metadata 类型表条目数。
    pub fn gc_metadata_type_count(&self) -> u32 {
        self.gc_metadata_type_count
    }
    /// 返回 GC metadata trace program 字节数。
    pub fn gc_metadata_trace_bytes(&self) -> u32 {
        self.gc_metadata_trace_bytes
    }
    /// 返回 GC metadata value program 字节数。
    pub fn gc_metadata_value_bytes(&self) -> u32 {
        self.gc_metadata_value_bytes
    }
    /// 返回 GC metadata vtable 条目数。
    pub fn gc_metadata_vtable_count(&self) -> u32 {
        self.gc_metadata_vtable_count
    }
    /// 返回 GC metadata root 范围条目数。
    pub fn gc_metadata_root_count(&self) -> u32 {
        self.gc_metadata_root_count
    }
    /// 返回 GC arena 字节数。
    pub fn gc_metadata_arena_bytes(&self) -> u64 {
        self.gc_metadata_arena_bytes
    }
    /// 返回 GC block 字节数。
    pub fn gc_metadata_block_bytes(&self) -> u32 {
        self.gc_metadata_block_bytes
    }
    /// 返回 GC line 字节数。
    pub fn gc_metadata_line_bytes(&self) -> u32 {
        self.gc_metadata_line_bytes
    }
    /// 返回 GC metadata 契约指纹。
    pub fn gc_metadata_contract_fingerprint(&self) -> [u8; 32] {
        self.gc_metadata_contract_fingerprint
    }
    /// 返回 GC metadata 需求视图。
    pub fn gc_metadata_demand(&self) -> crate::runtime::GcMetadataDemand {
        self.gc_metadata_demand
    }
    /// 返回 hybrid write barrier 契约指纹。
    pub fn barrier_contract_fingerprint(&self) -> [u8; 32] {
        self.barrier_contract_fingerprint
    }
    /// 返回 barrier 需求视图。
    pub fn barrier_demand(&self) -> crate::runtime::BarrierDemand {
        self.barrier_demand
    }
    /// 返回 card table 粒度（字节）。
    pub fn barrier_card_granularity_bytes(&self) -> u32 {
        self.barrier_card_granularity_bytes
    }
    /// 返回 processor-local buffer 项数。
    pub fn barrier_card_mark_buffer_entries(&self) -> u32 {
        self.barrier_card_mark_buffer_entries
    }
    /// 返回 dedup stamp 表项数。
    pub fn barrier_card_mark_stamp_entries(&self) -> u32 {
        self.barrier_card_mark_stamp_entries
    }
    /// 返回 barrier flush 原因数量。
    pub fn barrier_flush_reason_count(&self) -> u32 {
        self.barrier_flush_reason_count
    }
    /// 返回 `CardMarkBatch` 字段数。
    pub fn barrier_card_mark_batch_fields(&self) -> u32 {
        self.barrier_card_mark_batch_fields
    }
    /// 返回 barrier record 数量。
    pub fn barrier_record_count(&self) -> u32 {
        self.barrier_record_count
    }
    /// 返回已验证的 barrier 契约段。
    pub fn barrier_runtime(&self) -> &crate::runtime::BarrierRuntimeContract {
        &self.barrier_runtime
    }
    /// 返回 LocalHeap 契约指纹。
    pub fn local_heap_contract_fingerprint(&self) -> [u8; 32] {
        self.local_heap_contract_fingerprint
    }

    /// 返回 `EdgeDelta` 消息与候选回收契约指纹。
    pub fn edge_contract_fingerprint(&self) -> [u8; 32] {
        self.edge_contract_fingerprint
    }

    /// 返回候选回收需求视图：edge 站点数与 scratch 预留。
    pub fn edge_demand(&self) -> crate::runtime::EdgeDemand {
        self.edge_demand
    }

    /// 返回已验证的 `EdgeDelta` 消息与候选回收契约段。
    pub fn edge_runtime(&self) -> &crate::runtime::EdgeRuntimeContract {
        &self.edge_runtime
    }

    /// 返回 nursery 触发与年龄参数。
    pub fn local_heap_trigger(&self) -> crate::runtime::HeapTriggerProfile {
        self.local_heap_runtime.trigger()
    }
    /// 返回 LocalHeap 需求视图。
    pub fn local_heap_demand(&self) -> crate::runtime::LocalHeapDemand {
        self.local_heap_demand
    }
    /// 返回已验证的 LocalHeap Immix/TLAB/分代契约段。
    pub fn local_heap_runtime(&self) -> &crate::runtime::LocalHeapRuntimeContract {
        &self.local_heap_runtime
    }

    /// 返回 SharedHeap 契约指纹。
    pub fn shared_heap_contract_fingerprint(&self) -> [u8; 32] {
        self.shared_heap_contract_fingerprint
    }

    /// 返回 SharedHeap 需求视图。
    pub fn shared_heap_demand(&self) -> crate::runtime::SharedHeapDemand {
        self.shared_heap_demand
    }

    /// 返回已验证的 SharedHeap stable handle 与 forwarding grace 契约段。
    pub fn shared_heap_runtime(&self) -> &crate::runtime::SharedHeapRuntimeContract {
        &self.shared_heap_runtime
    }

    /// 返回 block return 契约指纹。
    pub fn block_return_contract_fingerprint(&self) -> [u8; 32] {
        self.block_return_contract_fingerprint
    }

    /// 返回 block return 需求视图。
    pub fn block_return_demand(&self) -> crate::runtime::BlockReturnDemand {
        self.block_return_demand
    }

    /// 返回已验证的 owner-directed managed block return 契约段。
    pub fn block_return_runtime(&self) -> &crate::runtime::BlockReturnRuntimeContract {
        &self.block_return_runtime
    }

    /// 返回压缩契约指纹。
    pub fn compression_contract_fingerprint(&self) -> [u8; 32] {
        self.compression_contract_fingerprint
    }

    /// 返回压缩需求视图。
    pub fn compression_demand(&self) -> crate::runtime::CompressionDemand {
        self.compression_demand
    }

    /// 返回已验证的 checked pointer compression 契约段。
    pub fn compression_runtime(&self) -> &crate::runtime::CompressionRuntimeContract {
        &self.compression_runtime
    }

    /// 返回目标对 checked pointer compression 的能力声明。
    pub fn compression_capability(&self) -> PointerCompression {
        self.compression_capability
    }

    /// 返回 routing 契约指纹。
    pub fn routing_contract_fingerprint(&self) -> [u8; 32] {
        self.routing_contract_fingerprint
    }

    /// 返回 routing 需求视图。
    pub fn routing_demand(&self) -> crate::runtime::RoutingDemand {
        self.routing_demand
    }

    /// 返回已验证的 temporal radix fan-out 契约段。
    pub fn routing_runtime(&self) -> &crate::runtime::RoutingRuntimeContract {
        &self.routing_runtime
    }

    /// 返回 raw link provenance 契约指纹。
    pub fn provenance_contract_fingerprint(&self) -> [u8; 32] {
        self.provenance_contract_fingerprint
    }

    /// 返回 raw link provenance 需求视图。
    pub fn provenance_demand(&self) -> crate::runtime::ProvenanceDemand {
        self.provenance_demand
    }

    /// 返回已验证的 raw link provenance 与 release 安全 profile 契约段。
    pub fn provenance_runtime(&self) -> &crate::runtime::ProvenanceRuntimeContract {
        &self.provenance_runtime
    }

    /// 返回 typed combining 契约指纹。
    pub fn combining_contract_fingerprint(&self) -> [u8; 32] {
        self.combining_contract_fingerprint
    }

    /// 返回 typed combining 需求视图。
    pub fn combining_demand(&self) -> crate::runtime::CombiningDemand {
        self.combining_demand
    }

    /// 返回已验证的 typed combining 冷操作契约段。
    pub fn combining_runtime(&self) -> &crate::runtime::CombiningRuntimeContract {
        &self.combining_runtime
    }

    /// 返回 mark 契约指纹。
    pub fn mark_contract_fingerprint(&self) -> [u8; 32] {
        self.mark_contract_fingerprint
    }

    /// 返回 mark 需求视图。
    pub fn mark_demand(&self) -> crate::runtime::MarkDemand {
        self.mark_demand
    }

    /// 返回 mark cycle 状态数量。
    pub fn mark_cycle_state_count(&self) -> u32 {
        self.mark_cycle_state_count
    }

    /// 返回收敛条件数量。
    pub fn mark_condition_count(&self) -> u32 {
        self.mark_condition_count
    }

    /// 返回 root snapshot 参与者数量。
    pub fn mark_snapshot_participant_count(&self) -> u32 {
        self.mark_snapshot_participant_count
    }

    /// 返回 cycle credit 池上界。
    pub fn mark_credit_pool(&self) -> u64 {
        self.mark_credit_pool
    }

    /// 返回 MarkMailbox consumer 数量。
    pub fn mark_mailbox_consumer_count(&self) -> u32 {
        self.mark_mailbox_consumer_count
    }

    /// 返回 `MarkTicket` 字段数量。
    pub fn mark_ticket_field_count(&self) -> u32 {
        self.mark_ticket_field_count
    }

    /// 返回 mark record 数量。
    pub fn mark_record_count(&self) -> u32 {
        self.mark_record_count
    }

    /// 返回 mark 契约段。
    pub fn mark_runtime(&self) -> &crate::runtime::MarkRuntimeContract {
        &self.mark_runtime
    }
    /// 返回 GC debt、credit、pacing 与 pressure 契约指纹。
    pub fn pacing_contract_fingerprint(&self) -> [u8; 32] {
        self.pacing_contract_fingerprint
    }
    /// 返回内建 pacing profile 名。
    pub fn pacing_profile(&self) -> &str {
        &self.pacing_profile
    }
    /// 返回 pacing profile revision。
    pub fn pacing_profile_revision(&self) -> u32 {
        self.pacing_profile_revision
    }
    /// 返回增长预算下限。
    pub fn pacing_min_growth_budget(&self) -> u64 {
        self.pacing_min_growth_budget
    }
    /// 返回 assist 触发阈值。
    pub fn pacing_assist_threshold(&self) -> u64 {
        self.pacing_assist_threshold
    }
    /// 返回单次 assist 的偿还上界。
    pub fn pacing_assist_quantum(&self) -> u64 {
        self.pacing_assist_quantum
    }
    /// 返回每 byte 折算的 mark cost unit。
    pub fn pacing_mark_cost_per_byte(&self) -> u32 {
        self.pacing_mark_cost_per_byte
    }
    /// 返回 GC CPU 比例。
    pub fn pacing_gc_cpu_fraction(&self) -> u32 {
        self.pacing_gc_cpu_fraction
    }
    /// 返回滑动 cost window 容量。
    pub fn pacing_gc_cpu_window_cost(&self) -> u64 {
        self.pacing_gc_cpu_window_cost
    }
    /// 返回 remark cost 上界。
    pub fn pacing_remark_cost_budget(&self) -> u64 {
        self.pacing_remark_cost_budget
    }
    /// 返回 evacuation payload 上界。
    pub fn pacing_evacuation_pause_bytes(&self) -> u64 {
        self.pacing_evacuation_pause_bytes
    }
    /// 返回 evacuation root 上界。
    pub fn pacing_evacuation_pause_roots(&self) -> u32 {
        self.pacing_evacuation_pause_roots
    }
    /// 返回 evacuation 字段上界。
    pub fn pacing_evacuation_pause_fields(&self) -> u32 {
        self.pacing_evacuation_pause_fields
    }
    /// 返回 episode 开启比例。
    pub fn pacing_pressure_enter_ratio(&self) -> u32 {
        self.pacing_pressure_enter_ratio
    }
    /// 返回 episode 结束比例。
    pub fn pacing_pressure_clear_ratio(&self) -> u32 {
        self.pacing_pressure_clear_ratio
    }
    /// 返回 owner credit 来源目录长度。
    pub fn pacing_credit_source_count(&self) -> u32 {
        self.pacing_credit_source_count
    }
    /// 返回 committed 快照的刷新间隔。
    pub fn pacing_pressure_poll_bytes(&self) -> u64 {
        self.pacing_pressure_poll_bytes
    }
    /// 返回有界 owner drain 的单 shard item 预算。
    pub fn pacing_owner_drain_items(&self) -> u32 {
        self.pacing_owner_drain_items
    }
    /// 返回有界 owner drain 的单 shard byte 预算。
    pub fn pacing_owner_drain_bytes(&self) -> u64 {
        self.pacing_owner_drain_bytes
    }
    /// 返回两次有界 owner drain 之间的分配字节间隔。
    pub fn pacing_owner_drain_interval_bytes(&self) -> u64 {
        self.pacing_owner_drain_interval_bytes
    }

    /// 返回已建立计划的 TurnRegion 数量。
    pub fn turn_region_sites(&self) -> u32 {
        self.turn_region_sites
    }

    /// 返回 `RegionPublish` 站点数。
    pub fn turn_region_publish_sites(&self) -> u32 {
        self.turn_region_publish_sites
    }

    /// 返回 `RegionReset` 站点数。
    pub fn turn_region_reset_sites(&self) -> u32 {
        self.turn_region_reset_sites
    }

    /// 返回 `PromoteManaged` 站点数。
    pub fn turn_region_promote_sites(&self) -> u32 {
        self.turn_region_promote_sites
    }

    /// 返回 `RegionTransfer` 站点数。
    pub fn turn_region_transfer_sites(&self) -> u32 {
        self.turn_region_transfer_sites
    }

    /// 返回容量 class 阶梯长度。
    pub fn turn_region_capacity_class_count(&self) -> u32 {
        self.turn_region_capacity_class_count
    }

    /// 返回单 region 对象数上界。
    pub fn turn_region_object_limit(&self) -> u32 {
        self.turn_region_object_limit
    }

    /// 返回单个 region 的最大 payload 字节上界。
    pub fn turn_region_max_bytes(&self) -> u64 {
        self.turn_region_max_bytes
    }

    /// 返回全部 region 的 payload 字节总和。
    pub fn turn_region_total_bytes(&self) -> u64 {
        self.turn_region_total_bytes
    }

    /// 返回 TurnRegion 契约指纹。
    pub fn turn_region_contract_fingerprint(&self) -> [u8; 32] {
        self.turn_region_contract_fingerprint
    }

    /// 返回 TurnRegion 需求视图。
    pub fn turn_region_demand(&self) -> crate::runtime::TurnRegionDemand {
        self.turn_region_demand
    }

    /// 返回 TurnRegion runtime 契约。
    pub fn turn_region_runtime(&self) -> &crate::runtime::TurnRegionRuntimeContract {
        &self.turn_region_runtime
    }
    /// 返回 pacing 需求视图。
    pub fn pacing_demand(&self) -> crate::runtime::GcPacingDemand {
        self.pacing_demand
    }
    /// 返回已验证的 pacing 契约段。
    pub fn pacing_runtime(&self) -> &crate::runtime::GcPacingRuntimeContract {
        &self.pacing_runtime
    }
    /// 返回真实 `.gugu.types` section。
    pub fn gc_type_section(&self) -> &[u8] {
        &self.gc_type_section
    }
    /// 返回真实 `.gugu.meta` section。
    pub fn gc_metadata_section(&self) -> &[u8] {
        &self.gc_metadata_section
    }
    /// 返回 type section 字节数。
    pub fn gc_type_section_bytes(&self) -> u32 {
        self.gc_type_section.len() as u32
    }
    /// 返回 metadata section 字节数。
    pub fn gc_metadata_section_bytes(&self) -> u32 {
        self.gc_metadata_section.len() as u32
    }
    /// 返回 type section 内容指纹。
    pub fn gc_type_section_fingerprint(&self) -> [u8; 32] {
        self.gc_type_section_fingerprint
    }
    /// 返回 metadata section 内容指纹。
    pub fn gc_metadata_section_fingerprint(&self) -> [u8; 32] {
        self.gc_metadata_section_fingerprint
    }
    /// 返回调度需求视图。
    pub fn scheduler_demand(&self) -> crate::runtime::SchedulerDemand {
        self.scheduler_runtime.demand
    }
    /// 返回 ResourceCell header 字节数。
    pub fn resource_cell_header_bytes(&self) -> u32 {
        self.resource_cell_header_bytes
    }
    /// 返回 Resource domain 的 class 数量。
    pub fn resource_class_count(&self) -> u32 {
        self.resource_class_count
    }
    /// 返回登记的资源种类数量。
    pub fn resource_kind_count(&self) -> u32 {
        self.resource_kind_count
    }
    /// 返回 release 描述符字段数量。
    pub fn release_descriptor_count(&self) -> u32 {
        self.release_descriptor_count
    }
    /// 返回资源分配点数量。
    pub fn resource_sites(&self) -> u32 {
        self.resource_sites
    }
    /// 返回 lease 结束动作数量。
    pub fn release_sites(&self) -> u32 {
        self.release_sites
    }
}

impl frontend::FrontendOutput {
    fn detail(&self) -> String {
        match &self.path {
            Some(path) => format!(
                "{}，{} 个模块 / {} 字节，{} 个记号，{} 个 AST 项 / {} 个节点，{} 个定义 / {} 个导入，{} 个类型布局",
                path.display(),
                self.modules.len(),
                self.source_len,
                self.token_count,
                self.item_count,
                self.node_count,
                self.names.definitions.len(),
                self.names.imports.len(),
                self.types.len(),
            ),
            None => "空模块".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    };

    use super::{
        ActionKind, ActionStatus, CompileRequest, Compiler, DiagnosticCode, Project, TargetKind,
        TargetName, logical_input_path,
    };

    #[test]
    fn empty_package_has_complete_graph_without_image() {
        let compilation =
            Compiler::new().compile(CompileRequest::empty_package(TargetName::X86_64Linux));

        assert!(compilation.is_success());
        assert!(compilation.image_plan().is_none());
        assert!(
            compilation
                .action_graph()
                .nodes()
                .iter()
                .all(|node| node.status() != ActionStatus::Pending)
        );
        assert_eq!(
            compilation.action_graph().nodes()[2].kind(),
            ActionKind::Frontend
        );
    }

    #[test]
    fn simple_main_reaches_runtime_and_image_plan() {
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            "fn main() {}",
            TargetName::X86_64Linux,
        ));

        assert!(compilation.is_success());
        let plan = compilation.image_plan().expect("simple main has a plan");
        assert_eq!(plan.entry(), "main");
        assert_eq!(plan.rt0(), super::Rt0Boundary::LinuxSyscall);
        // 入口符号必须是 `main` 自己的片段：片段顺序按实例键升序，runtime helper 会排在前
        // 面，所以这里比对 LIR dump 里 `fn main instance=<hex>` 的实例键。
        let lir = compilation.dump_lir().expect("入口有 LIR");
        let entry_symbol = plan.x64_entry_symbol();
        let instance = entry_symbol
            .strip_prefix("__gugu_fn_")
            .expect("入口符号是内部 fn mangling");
        assert_eq!(instance.len(), 64, "{entry_symbol}");
        assert!(
            lir.contains(&format!("fn main instance={instance}")),
            "入口符号没有指向 main 片段：{entry_symbol}"
        );
        assert_eq!(
            compilation
                .action_graph()
                .nodes()
                .iter()
                .find(|node| node.kind() == ActionKind::EmitImage)
                .map(|node| node.status()),
            Some(ActionStatus::Skipped)
        );
    }

    #[test]
    fn malformed_source_never_reaches_image_plan() {
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "broken.gg",
            "fn main( {}",
            TargetName::X86_64Linux,
        ));

        assert!(!compilation.is_success());
        assert!(compilation.image_plan().is_none());
        assert!(!compilation.diagnostics().items().is_empty());
        assert_eq!(
            compilation.action_graph().nodes()[2].status(),
            ActionStatus::Failed
        );
        assert_eq!(
            compilation.action_graph().nodes()[7].status(),
            ActionStatus::Skipped
        );
    }

    #[test]
    fn registered_targets_have_distinct_image_boundaries() {
        assert_eq!(
            TargetName::parse("x86_64-linux")
                .expect("registered target")
                .descriptor()
                .rt0,
            super::Rt0Kind::LinuxSyscall
        );
        assert_eq!(
            TargetName::parse("x86_64-windows")
                .expect("registered target")
                .descriptor()
                .rt0,
            super::Rt0Kind::WindowsThinImport
        );
        assert!(TargetName::parse("x86_64-freebsd").is_err());
    }

    #[test]
    fn library_file_compiles_without_requiring_main() {
        let root = tempfile::tempdir().expect("tempdir");
        let entry = root.path().join("src/lib.gg");
        std::fs::create_dir_all(entry.parent().expect("parent exists")).expect("create src");
        std::fs::write(&entry, "fn util() int { 1 }\n").expect("write lib");
        let compilation = Compiler::new().compile(CompileRequest::library_file(
            &entry,
            TargetName::X86_64Linux,
        ));

        // 库 target 没有 main：前端通过，但没有可执行入口，也不挂 runtime。
        assert!(compilation.is_success());
        assert!(compilation.image_plan().is_none());
        assert_eq!(
            compilation
                .action_graph()
                .nodes()
                .iter()
                .find(|node| node.kind() == ActionKind::PlanBackend)
                .map(|node| node.status()),
            Some(ActionStatus::Complete)
        );
        let source_map = compilation.source_map();
        // 用户源码之后追加 compiler 内建的 std/runtime 源单元。
        assert_eq!(
            source_map.snapshots().len(),
            1 + super::runtime::RuntimeResources::builtin().sources().len()
        );
        // 内建源单元的路径排在用户源码之前；库 target 仍然没有可执行入口。
        assert!(
            source_map
                .snapshots()
                .iter()
                .any(|snapshot| snapshot.logical_path() == "src/lib.gg")
        );
        assert!(compilation.image_plan().is_none());
        assert!(
            source_map
                .snapshots()
                .iter()
                .any(|snapshot| snapshot.logical_path() == "std/runtime/platform.gg"),
            "内建平台源单元必须进入源码表"
        );
    }

    #[test]
    fn project_target_snapshots_and_analyzes_its_module_tree() {
        let root = tempfile::tempdir().expect("tempdir");
        let src = root.path().join("src");
        std::fs::create_dir(&src).expect("create src");
        std::fs::write(
            root.path().join("gugu.toml"),
            "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )
        .expect("write manifest");
        std::fs::write(src.join("main.gg"), "use platform.{boot}\nfn main() {}\n")
            .expect("write entry");
        std::fs::write(
            src.join("platform.gg"),
            "#[cfg(os = \"linux\")] pub fn boot() {}\n",
        )
        .expect("write module");
        std::fs::write(root.path().join("build.gg"), "fn configure() {}\n")
            .expect("write build task");
        let project = Project::discover(root.path()).expect("discover project");
        let package = project.current_package().expect("current package");
        let target = package
            .targets()
            .iter()
            .find(|target| target.kind() == TargetKind::Bin)
            .expect("bin target");
        let build_target = package
            .targets()
            .iter()
            .find(|target| target.kind() == TargetKind::Build)
            .expect("build target");
        let build_request = CompileRequest::project_target(
            package,
            build_target,
            "demo@1.0.0 (path+.)",
            TargetName::X86_64Windows,
            vec!["default".to_owned()],
            BTreeSet::new(),
        );
        assert_eq!(
            build_request.target,
            TargetName::host().unwrap_or(TargetName::X86_64Windows)
        );
        let compilation = Compiler::new().compile(CompileRequest::project_target(
            package,
            target,
            "demo@1.0.0 (path+.)",
            TargetName::X86_64Linux,
            vec!["default".to_owned()],
            BTreeSet::new(),
        ));

        assert!(compilation.is_success());
        assert_eq!(
            compilation.source_map().snapshots().len(),
            2 + super::runtime::RuntimeResources::builtin().sources().len()
        );
        assert!(compilation.image_plan().is_some());
    }

    #[test]
    fn bom_source_fails_at_load_sources_with_stable_code() {
        let root = tempfile::tempdir().expect("tempdir");
        let entry = root.path().join("src/main.gg");
        std::fs::create_dir_all(entry.parent().expect("parent exists")).expect("create src");
        std::fs::write(&entry, b"\xef\xbb\xbffn main() {}").expect("write bom source");
        let compilation = Compiler::new().compile(CompileRequest::single_file_path(
            &entry,
            TargetName::X86_64Linux,
        ));

        assert!(!compilation.is_success());
        assert!(compilation.image_plan().is_none());
        assert!(compilation.source_map().snapshots().is_empty());
        let diagnostic = &compilation.diagnostics().items()[0];
        assert_eq!(diagnostic.code(), DiagnosticCode::SourceBom);
        assert!(
            compilation
                .action_graph()
                .nodes()
                .iter()
                .any(|node| node.kind() == ActionKind::LoadSources
                    && node.status() == ActionStatus::Failed)
        );
    }

    #[test]
    fn image_plan_reports_runtime_raw_contract() {
        let compiler = Compiler::new();
        let request = || {
            CompileRequest::single_file(
                "main.gg",
                "fn main() { let first = 1\n let second = 2\n _ = first + second }",
                TargetName::X86_64Linux,
            )
        };
        let cold = compiler.compile(request());
        let warm = compiler.compile(request());
        assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
        let plan = cold.image_plan().expect("可执行入口必须有镜像计划");
        assert!(plan.raw_size_class_count() > 0);
        assert_eq!(plan.raw_shard_count(), 8);
        assert!(plan.raw_batch_max_items() > 0);
        assert!(plan.raw_batch_soft_bytes() > 0);
        assert!(plan.raw_message_node_capacity() >= 8 * plan.raw_batch_max_items());
        assert_eq!(plan.platform_profile(), "linux");
        assert_eq!(plan.platform_op_count(), 13);
        assert_eq!(
            plan.platform_range_class_count(),
            super::runtime::EXTENT_CLASS_LADDER.len() as u32
        );
        assert_eq!(plan.ledger_category_count(), 5);
        let demand = plan.platform_range_demand();
        assert!(demand.payload_extents >= demand.owners);
        assert!(demand.guard_extents >= demand.stack_extents);
        assert_eq!(plan.resource_cell_header_bytes(), 64);
        assert!(plan.resource_class_count() > 0);
        assert_eq!(plan.resource_kind_count(), 5);
        assert!(plan.release_descriptor_count() >= 4);
        let dump = cold.dump_runtime().expect("契约 dump");
        assert_eq!(
            Some(dump.clone()),
            warm.dump_runtime(),
            "冷热 dump 必须一致"
        );
        assert!(dump.contains("wait schema=1"));
        assert_eq!(plan.wait_inline_select_cases(), 8);
        assert_eq!(plan.wait_scratch_class_count(), 11);
        assert_eq!(plan.wait_node_class_count(), 2);
        assert_ne!(plan.wait_contract_fingerprint(), [0_u8; 32]);
        assert!(plan.stackmap_function_count() > 0);
        assert!(plan.stackmap_safepoint_count() > 0);
        assert!(plan.stackmap_map_count() > 0);
        assert!(plan.stackmap_map_count() <= plan.stackmap_safepoint_count());
        assert_ne!(plan.stackmap_contract_fingerprint(), [0_u8; 32]);
        let stackmap = plan.stackmap_demand();
        assert_eq!(
            stackmap.call_return
                + stackmap.poll_resume
                + stackmap.suspend_resume
                + stackmap.foreign_bridge
                + stackmap.morestack_entry,
            stackmap.safepoints,
            "栈图 kind 分类必须求和为安全点总数"
        );
        assert!(dump.contains("stackmap schema=1"));
        assert!(dump.contains("stackmap-root-kinds heap-direct,heap-interior,shared-handle,compressed-ref,stack-interior"));
        assert_eq!(
            cold.runtime_raw_fingerprint(),
            warm.runtime_raw_fingerprint()
        );
        assert_eq!(cold.action_key(), warm.action_key());
        assert_eq!(cold.exit_code(), 0);
    }

    #[test]
    fn image_plan_reports_resource_lease_sites() {
        let source = "struct ResourceCell { id: uint }\nfn main() {\n let a = ResourceCell { id: 1 }\n let b = a\n b = ResourceCell { id: 2 }\n _ = b\n}";
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
        let plan = compilation.image_plan().expect("镜像计划");
        assert!(
            plan.resource_sites() > 0,
            "placement 必须报告 resource 站点"
        );
        assert!(plan.release_sites() > 0, "GIR 必须报告 release 站点");
        assert_eq!(plan.resource_class_count(), 7);
        assert_eq!(plan.platform_profile(), "linux");
        assert_eq!(plan.platform_op_count(), 13);
        assert_eq!(
            plan.platform_range_class_count(),
            super::runtime::EXTENT_CLASS_LADDER.len() as u32
        );
        assert_eq!(plan.ledger_category_count(), 5);
        let demand = plan.platform_range_demand();
        assert!(demand.payload_extents >= demand.owners);
        assert!(demand.guard_extents >= demand.stack_extents);
        assert_eq!(plan.resource_cell_header_bytes(), 64);
    }

    #[test]
    fn configured_target_changes_raw_contract_fingerprint() {
        let source = "fn main() { let value = 1\n _ = value }";
        let linux = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        let windows = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Windows,
        ));
        assert_eq!(linux.exit_code(), 0);
        assert_eq!(windows.exit_code(), 0);
        assert_ne!(
            linux.runtime_raw_fingerprint(),
            windows.runtime_raw_fingerprint(),
            "目标语义必须进入 raw 契约身份"
        );
    }

    #[test]
    fn image_plan_reports_rt0_startup_contract() {
        let source = "fn main() { let value = 1\n _ = value }";
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
        let plan = compilation.image_plan().expect("镜像计划");
        assert_eq!(plan.rt0_step_count(), 5);
        assert_eq!(plan.rt0_lifecycle_count(), 4);
        assert_eq!(plan.startup_config_var_count(), 7);
        assert_eq!(plan.startup_fatal_count(), 7);
        assert_eq!(plan.report_reason_count(), 15);
        assert_eq!(plan.rt0_emergency_buffer_bytes(), 4096);
        assert!(plan.rt0_entry_present());
        assert!(!plan.rt0_main_returns_result());
        let dump = compilation.dump_runtime().expect("runtime dump");
        assert!(dump.contains("rt0-state booting"));
        assert!(dump.contains("rt0-transition waiting -> terminating on termination-started"));
        assert!(dump.contains("startup-var GUGU_RUNTIME_STACK_MAX grammar=stack-max default=1GiB"));
        assert!(dump.contains("report-schema gugu-runtime-report-v1"));
        assert!(dump.contains("rt0-facility coroutine-slot-slab"));
        assert!(dump.contains("rt0-demand entry=true main-result=false"));
    }

    #[test]
    fn main_result_type_enters_rt0_demand_and_contract() {
        let unit = "fn main() { let value = 1\n _ = value }";
        let result = "fn main() Result[(), string] {\n Err(\"e\")\n}";
        let unit_compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            unit,
            TargetName::X86_64Linux,
        ));
        let unit_plan = unit_compilation.image_plan().expect("unit main 的镜像计划");
        assert!(unit_plan.rt0_entry_present());
        assert!(!unit_plan.rt0_main_returns_result());
        let result_compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            result,
            TargetName::X86_64Linux,
        ));
        assert!(
            result_compilation.is_success(),
            "{:?}",
            result_compilation.diagnostics().items()
        );
        let result_plan = result_compilation
            .image_plan()
            .expect("result main 的镜像计划");
        assert!(result_plan.rt0_main_returns_result());
        assert_ne!(
            unit_plan.rt0_contract_fingerprint(),
            result_plan.rt0_contract_fingerprint(),
            "main 返回类型改变 rt0 契约身份"
        );
    }

    #[test]
    fn rt0_contract_fingerprint_is_deterministic_across_compilations() {
        let source = "fn main() { let value = 1\n _ = value }";
        let first = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        let second = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert_eq!(
            first.image_plan().expect("计划").rt0_contract_fingerprint(),
            second
                .image_plan()
                .expect("计划")
                .rt0_contract_fingerprint()
        );
    }

    #[test]
    fn image_plan_reports_gc_metadata_contract() {
        let source = "struct ResourceCell { id: uint }\nfn main() {\n let value = ResourceCell { id: 1 }\n _ = value\n }";
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
        let plan = compilation.image_plan().expect("镜像计划");
        // 类型表至少含 int/bool/Unit 等基本类型与 fn main 的签名。
        assert!(plan.gc_metadata_type_count() > 0);
        assert!(plan.gc_metadata_trace_bytes() > 0);
        assert!(plan.gc_metadata_value_bytes() > 0);
        assert!(plan.gc_type_section_bytes() > 0);
        assert!(plan.gc_metadata_section_bytes() > 0);
        assert_eq!(&plan.gc_type_section()[..8], b"GUGUTY01");
        assert_eq!(&plan.gc_metadata_section()[..8], b"GUGUMT01");
        assert_ne!(plan.gc_type_section_fingerprint(), [0_u8; 32]);
        assert_ne!(plan.gc_metadata_section_fingerprint(), [0_u8; 32]);
        // 根范围只登记具体 GIR 中实际出现的 managed local。
        assert!(plan.gc_metadata_root_count() > 0);
        // arena/block/line 与契约常量一致。
        assert_eq!(plan.gc_metadata_arena_bytes(), 2 * 1024 * 1024);
        assert_eq!(plan.gc_metadata_block_bytes(), 32 * 1024);
        assert_eq!(plan.gc_metadata_line_bytes(), 128);
        // 指纹必须非零且跨编译稳定。
        assert_ne!(plan.gc_metadata_contract_fingerprint(), [0_u8; 32]);
        let dump = compilation.dump_runtime().expect("runtime dump");
        assert!(dump.contains("gc-metadata schema="));
        assert!(dump.contains("gc-metadata-types"));
        assert!(dump.contains("gc-metadata-sections"));
        assert!(dump.contains("gc-metadata-fingerprint"));
        // `ResourceCell` 是含 identity/passing 语义的聚合，它的 trace program
        // 必须携带真实 DIRECT/INTERIOR 指令，而不是只有单字节 END。
        assert!(
            plan.gc_metadata_trace_bytes() > plan.gc_metadata_type_count(),
            "trace program 必须包含逐类型指令而非单字节 END"
        );
        // 冷/热编译指纹一致。
        let warm = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert_eq!(
            plan.gc_metadata_contract_fingerprint(),
            warm.image_plan()
                .expect("warm plan")
                .gc_metadata_contract_fingerprint()
        );
        assert_eq!(
            plan.gc_type_section_fingerprint(),
            warm.image_plan()
                .expect("warm plan")
                .gc_type_section_fingerprint()
        );
        assert_eq!(
            plan.gc_metadata_section_fingerprint(),
            warm.image_plan()
                .expect("warm plan")
                .gc_metadata_section_fingerprint()
        );
        // 类型表变化时 GC metadata 指纹必须变化。
        let bigger = "struct ResourceCell { id: uint }\nstruct Extra { id: uint }\nfn helper(x: Extra) Extra = x\nfn main() {\n let value = ResourceCell { id: 1 }\n let extra = Extra { id: 2 }\n _ = helper(extra)\n _ = value\n }";
        let bigger_compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            bigger,
            TargetName::X86_64Linux,
        ));
        assert!(
            bigger_compilation.is_success(),
            "{:?}",
            bigger_compilation.diagnostics().items()
        );
        let bigger_plan = bigger_compilation.image_plan().expect("bigger plan");
        assert!(
            bigger_plan.gc_metadata_type_count() > plan.gc_metadata_type_count(),
            "helper 加入后 GC 类型表必须扩张"
        );
        assert_ne!(
            bigger_plan.gc_metadata_contract_fingerprint(),
            plan.gc_metadata_contract_fingerprint(),
            "GC metadata 指纹必须随类型表变化"
        );
    }

    #[test]
    fn image_plan_reports_turn_region_contract() {
        let reset_source = "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }";
        let transfer_source = "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n }";
        let reset = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            reset_source,
            TargetName::X86_64Linux,
        ));
        assert!(reset.is_success(), "{:?}", reset.diagnostics().items());
        let plan = reset.image_plan().expect("镜像计划");
        // 闭包环境是唯一的 region 站点：建立计划、发布一次、重置一次，没有移交。
        assert!(plan.turn_region_sites() >= 1);
        assert!(plan.turn_region_publish_sites() >= 1);
        assert!(plan.turn_region_reset_sites() >= 1);
        assert_eq!(plan.turn_region_transfer_sites(), 0);
        assert_eq!(plan.turn_region_promote_sites(), 0);
        assert_eq!(
            plan.turn_region_capacity_class_count(),
            crate::runtime::region_schema::REGION_CAPACITY_CLASSES.len() as u32
        );
        assert_eq!(
            plan.turn_region_object_limit(),
            crate::runtime::region_schema::REGION_OBJECT_LIMIT
        );
        assert!(plan.turn_region_max_bytes() > 0);
        assert_eq!(
            plan.turn_region_total_bytes(),
            plan.turn_region_demand().total_bytes
        );
        assert_ne!(plan.turn_region_contract_fingerprint(), [0_u8; 32]);
        assert_eq!(
            plan.turn_region_runtime().capacity_class_count(),
            plan.turn_region_capacity_class_count()
        );
        assert_eq!(
            plan.turn_region_runtime().transfer_field_count(),
            13,
            "RegionTransfer 字段目录必须进入契约"
        );
        let dump = reset.dump_runtime().expect("runtime dump");
        assert!(dump.contains("region schema=1 object-limit=64 max-active=64 transfer-lease=1"));
        assert!(dump.contains("region-states private,publishing,reset-pending,reset,local-promote,region-transfer,received"));
        assert!(dump.contains("region-transfer-fields"));
        assert!(dump.contains("region-demand"));
        assert!(dump.contains("region-fingerprint"));

        // sender 之后不再使用闭包时整区移交：transfer 需求来自优化后的 LIR。
        let transfer = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            transfer_source,
            TargetName::X86_64Linux,
        ));
        assert!(
            transfer.is_success(),
            "{:?}",
            transfer.diagnostics().items()
        );
        let plan = transfer.image_plan().expect("镜像计划");
        assert_eq!(plan.turn_region_transfer_sites(), 1);
        assert_eq!(plan.turn_region_reset_sites(), 0);
        assert_eq!(plan.turn_region_publish_sites(), 1);
        assert_eq!(plan.turn_region_sites(), 1);
        assert_ne!(
            plan.turn_region_contract_fingerprint(),
            reset
                .image_plan()
                .expect("镜像计划")
                .turn_region_contract_fingerprint(),
            "需求变化必须改变 TurnRegion 契约指纹"
        );
    }

    #[test]
    fn image_plan_reports_barrier_contract_for_managed_writes() {
        let source = "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }";
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
        let plan = compilation.image_plan().expect("镜像计划");
        assert_eq!(plan.barrier_card_granularity_bytes(), 512);
        assert_eq!(plan.barrier_card_mark_buffer_entries(), 256);
        assert_eq!(plan.barrier_card_mark_stamp_entries(), 256);
        assert_eq!(plan.barrier_flush_reason_count(), 6);
        assert_eq!(plan.barrier_card_mark_batch_fields(), 13);
        assert_eq!(plan.barrier_record_count(), 4);
        assert_ne!(plan.barrier_contract_fingerprint(), [0_u8; 32]);
        // managed store 存在时 barrier 需求非零；每一处写入都被同一站点集合覆盖。
        let demand = plan.barrier_demand();
        assert!(demand.barrier_sites() > 0);
        assert_eq!(demand.card_mark_sites, demand.barrier_sites());
        assert_eq!(demand.edge_summary_sites, demand.barrier_sites());
        assert_eq!(
            plan.barrier_runtime().card_granularity_bytes(),
            plan.barrier_card_granularity_bytes()
        );
        let dump = compilation.dump_runtime().expect("runtime dump");
        assert!(dump.contains("barrier schema=3 card=512 buffer=256 stamps=256"));
        assert!(dump.contains("barrier-steps read-old -> shade-old-deleted -> shade-new-inserted -> store -> card-mark -> edge-summary"));
        assert!(dump.contains("barrier-flush-reasons buffer-full,processor-handoff,foreign-bridge,memory-pressure,minor-stop,producer-stop-gate"));
        assert!(dump.contains(
            "barrier-kinds yuasa-deletion,dijkstra-insertion,direct-field,edge-add,edge-drop"
        ));
        assert!(dump.contains("barrier-record CardMarkEntry bytes=32 align=16"));
        assert!(dump.contains("barrier-field CardMarkStamp.entry offset=8"));
        assert!(dump.contains("barrier-demand"));
        assert!(dump.contains("barrier-pressure"));
        assert!(dump.contains("barrier-fingerprint"));
        // LocalHeap 的 arena/block/line 与 GC metadata 契约同源，位图、TLAB 与记录布局
        // 只由这一组参数推导；需求视图则与 barrier/gc metadata 的站点口径一致。
        let local_heap = plan.local_heap_runtime();
        assert_eq!(local_heap.arena_bytes(), plan.gc_metadata_arena_bytes());
        assert_eq!(local_heap.block_bytes(), plan.gc_metadata_block_bytes());
        assert_eq!(local_heap.line_bytes(), plan.gc_metadata_line_bytes());
        assert_eq!(
            local_heap.tlab_span_bytes(),
            8 * u64::from(plan.gc_metadata_block_bytes())
        );
        assert_eq!(
            local_heap.object_start_bits,
            (plan.gc_metadata_arena_bytes() / 16) as u32
        );
        assert_eq!(local_heap.bitmap_bytes, local_heap.mark_bits / 8);
        assert_eq!(local_heap.page_cover_entries, 512);
        assert_eq!(local_heap.card_bytes, 4096);
        assert_eq!(
            plan.local_heap_demand().barrier_sites,
            demand.barrier_sites()
        );
        assert_eq!(
            plan.local_heap_demand().managed_types,
            plan.gc_metadata_type_count()
        );
        assert!(plan.local_heap_demand().large_types >= 1);
        assert_ne!(plan.local_heap_contract_fingerprint(), [0_u8; 32]);
        assert!(dump.contains("local-heap schema=4 arena=2097152 block=32768 line=128"));
        assert!(dump.contains("block-return schema=1"));
        assert!(dump.contains("local-heap-bitmaps object-start-bits=131072 mark-bits=131072"));
        assert!(dump.contains("local-heap-record HeapArenaMetadata bytes=55576"));
        assert!(dump.contains("local-heap-trigger revision=1"));
        assert!(dump.contains("local-heap-demand"));
        assert!(dump.contains("local-heap-fingerprint"));
        // 边契约：候选回收相位、block 状态与 `EdgeDelta` 字段集合都进入镜像计划。
        assert_ne!(plan.edge_contract_fingerprint(), [0_u8; 32]);
        assert_eq!(plan.edge_runtime().candidate_quantum, 4096);
        assert_eq!(plan.edge_runtime().phases.len(), 10);
        assert_eq!(plan.edge_runtime().phases[0], "discover");
        assert_eq!(
            plan.edge_runtime().states,
            [
                "allocating",
                "candidate",
                "sweeping",
                "evacuating",
                "return-pending",
                "owned-free",
                "free"
            ]
        );
        // `EdgeDelta` 的规范字段集合：18 个字段，全部不带地址（schema 自带校验）。
        assert_eq!(plan.edge_runtime().edge_delta_field_count(), 18);
        // 边需求必须与 barrier/mark 的同一组站点计数一致：三份契约不允许各自记账。
        assert_eq!(plan.edge_demand().edge_sites, demand.edge_summary_sites);
        assert_eq!(plan.edge_demand().reserve_slots, demand.shade_slots);
        assert_eq!(
            plan.edge_demand().edge_sites,
            plan.mark_demand().edge_delta_sites
        );
        assert!(dump.contains("edge schema="));
        assert!(dump.contains("edge-demand "));
        assert!(dump.contains("edge-phases "));
        assert!(dump.contains("edge-states "));
        assert!(dump.contains("edge-fingerprint "));
        // mark 契约：每 owner 单 consumer、七个收敛条件、六类参与者与三条 record 布局。
        assert_eq!(plan.mark_mailbox_consumer_count(), 1);
        assert_eq!(plan.mark_condition_count(), 7);
        assert_eq!(plan.mark_snapshot_participant_count(), 6);
        assert_eq!(plan.mark_cycle_state_count(), 6);
        assert_eq!(plan.mark_ticket_field_count(), 18);
        assert_eq!(plan.mark_record_count(), 5);
        assert!(plan.mark_credit_pool() > 0);
        // mark 需求覆盖的站点集合与 gc metadata/barrier/SharedHeap 三份需求一致。
        assert_eq!(
            plan.mark_demand().root_sites,
            plan.gc_metadata_demand().root_range_count
        );
        assert_eq!(plan.mark_demand().barrier_sites, demand.card_mark_sites);
        assert_eq!(
            plan.mark_demand().ticket_sites,
            plan.shared_heap_demand().mark_sites
        );
        assert_eq!(
            plan.mark_demand().edge_delta_sites,
            demand.edge_summary_sites
        );
        assert_ne!(plan.mark_contract_fingerprint(), [0_u8; 32]);
        assert!(dump.contains("mark schema=4 profile=mosaic-mark revision=2"));
        assert!(dump.contains(
            "mark-conditions local-worklist,published-batch,mailbox,barrier-buffer,producer-epoch,forwarding-work,pending-credit"
        ));
        assert!(dump.contains("mark-record MarkMailboxHead bytes=64 align=64"));
        assert!(dump.contains("mark-credit-sources barrier-buffer,card-mark-batch,edge-delta,pending-return,producer-staging,mark-credit,mark-mailbox,mark-worklist,forwarding-work"));
        assert!(dump.contains("mark-fingerprint"));
        // pacing 契约与需求同样进入镜像计划、dump 与指纹身份。
        assert_eq!(plan.pacing_profile(), "mosaic-default");
        assert_eq!(plan.pacing_profile_revision(), 3);
        assert_eq!(plan.pacing_assist_quantum(), 1 << 16);
        assert_eq!(plan.pacing_gc_cpu_fraction(), 25);
        assert_eq!(plan.pacing_credit_source_count(), 9);
        assert_eq!(plan.pacing_pressure_poll_bytes(), 1 << 20);
        assert_eq!(plan.pacing_owner_drain_items(), 64);
        assert_eq!(plan.pacing_owner_drain_bytes(), 1 << 16);
        assert_eq!(plan.pacing_owner_drain_interval_bytes(), 1 << 20);
        assert_ne!(plan.pacing_contract_fingerprint(), [0_u8; 32]);
        assert_eq!(
            plan.pacing_runtime().pressure_enter_ratio(),
            plan.pacing_pressure_enter_ratio()
        );
        // pacing 需求从优化后 LIR 推导：分配站点与屏障站点必须与需求视图一致。
        let pacing_demand = plan.pacing_demand();
        assert!(pacing_demand.alloc_sites > 0, "闭包捕获必须产生分配站点");
        assert_eq!(pacing_demand.barrier_sites, demand.barrier_sites());
        assert_eq!(
            pacing_demand.slow_edges,
            pacing_demand.alloc_sites + 1,
            "slow edge 等于分配站点加显式 safepoint"
        );
        assert!(pacing_demand.managed_types > 0);
        assert!(dump.contains("pacing schema=3 profile=mosaic-default revision=3"));
        assert!(dump.contains("pacing-drain poll=1048576 items=64 bytes=65536 interval=1048576"));
        assert!(dump.contains("pacing-pressure enter=85 clear=70 states=steady,drain,emergency"));
        assert!(dump.contains("pacing-drain-classes owner-cache-bytes,pending-return-bytes,reclaimable-bytes partition=runtime-committed-bytes"));
        assert!(dump.contains("pacing-credit-sources barrier-buffer,card-mark-batch,edge-delta,pending-return,producer-staging,mark-credit,mark-mailbox,mark-worklist,forwarding-work"));
        assert!(dump.contains("pacing-fingerprint"));
        // 冷/热编译指纹一致，且 dump 逐字节相同。
        let warm = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        let warm_plan = warm.image_plan().expect("warm plan");
        assert_eq!(
            plan.barrier_contract_fingerprint(),
            warm_plan.barrier_contract_fingerprint()
        );
        assert_eq!(dump, warm.dump_runtime().expect("warm dump"));
        // 站点数变化必须改变 barrier 指纹。
        let bigger = "fn main() {\n let value = 1\n let other = 2\n let closure = fn() int { return value + other }\n _ = closure()\n }";
        let bigger_compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            bigger,
            TargetName::X86_64Linux,
        ));
        assert!(
            bigger_compilation.is_success(),
            "{:?}",
            bigger_compilation.diagnostics().items()
        );
        let bigger_plan = bigger_compilation.image_plan().expect("bigger plan");
        assert!(
            bigger_plan.barrier_demand().barrier_sites() > demand.barrier_sites(),
            "捕获两个 managed local 后 barrier 站点必须增多"
        );
        assert_ne!(
            bigger_plan.barrier_contract_fingerprint(),
            plan.barrier_contract_fingerprint(),
            "barrier 指纹必须随站点需求变化"
        );
        // Windows 目标同样形成契约，只是 profile 不同。
        let windows = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Windows,
        ));
        assert!(windows.is_success(), "{:?}", windows.diagnostics().items());
        let windows_plan = windows.image_plan().expect("windows plan");
        assert_eq!(
            windows_plan.barrier_card_granularity_bytes(),
            plan.barrier_card_granularity_bytes()
        );
        // barrier 协议本身不含平台差异：card 粒度、buffer 容量与六步序列在两个目标上
        // 必须相同，平台差异只体现在 raw 契约的其余分段。
        assert_eq!(
            windows_plan.barrier_contract_fingerprint(),
            plan.barrier_contract_fingerprint(),
            "barrier 协议不得随目标漂移"
        );
        assert_ne!(
            compilation.runtime_raw_fingerprint(),
            windows.runtime_raw_fingerprint(),
            "raw 契约整体仍必须随目标分离"
        );
    }

    #[test]
    fn large_type_without_allocation_keeps_demand_bounded() {
        // 冻结类型表里出现、却没有分配站点的大类型不得让 large 上界越过分配站点上界：
        // 一个只有类型引用、没有任何分配的程序曾经被 E0058 拒绝。
        let source = "#[repr(C, align(64))] struct Large { head: uint, tail: [uint; 4096] }\n\
                      #[used] fn field(value: &Large) &uint = &value.head\n\
                      fn main() {}\n";
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
        let plan = compilation.image_plan().expect("镜像计划");
        let demand = plan.block_return_demand();
        assert!(
            demand.large_sites <= demand.block_sites,
            "large 上界必须被分配站点上界夹住：large {} vs block {}",
            demand.large_sites,
            demand.block_sites
        );
    }

    #[test]
    fn logical_input_path_normalizes_to_forward_slashes() {
        assert_eq!(
            logical_input_path(Path::new("src/main.gg")),
            PathBuf::from("src/main.gg")
        );
        assert_eq!(
            logical_input_path(Path::new("./src/main.gg")),
            PathBuf::from("src/main.gg")
        );
    }
}
