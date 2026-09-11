#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Gugu compiler 的阶段化 bootstrap 接口。
//!
//! compiler 前端：源码快照、词法及 AST、名称/类型/控制流检查、版本化查询与冻结 HIR。
//! 后端只消费已验证 LIR 形成内存 image plan；机器代码生成由后续后端阶段实现。

mod action;
mod backend;
mod diagnostics;
mod frontend;
mod lir;
mod project;
mod query;
mod runtime;
mod source;
mod target;
pub use query::{
    DependencyFingerprint, ObjectCache, ObjectError, ObjectKey, QueryContext, QueryEngine,
    QueryError, QueryKey, QueryKind, QueryResult, QueryState,
};

pub use action::{ActionGraph, ActionKind, ActionNode, ActionStatus};
pub use diagnostics::{Diagnostic, DiagnosticCode, Diagnostics, Severity};
pub use frontend::format::format_source;
pub use project::{
    ActionInputs, ActionKey, CacheError, CachePolicy, DependencyCache, DependencyDomain,
    DependencyInput, DependencySource, DependencySpec, LockGraph, LockedDependency, LockedPackage,
    Package, PackageFiles, PackageId, PackageMetadata, PackageSource, Project, ProjectError,
    ResolveOptions, Target, TargetArtifact, TargetCondition, TargetKind, TargetSelection,
    TargetView, Version, VersionReq, Workspace, candidates_from_lock, default_cache_root,
    materialize_vendor, prepare_dependency_inputs,
};
pub use runtime::{
    HarnessReport, IntrinsicBoundary, OwnerReturnHarness, PlatformRangeDemand,
    ResourceReleaseHarness, ResourceReleaseReport, Rt0Boundary, RuntimeResources, RuntimeSource,
    RuntimeSourceRole,
};
pub use source::{
    ExpansionId, ExpansionInput, ExpansionRecord, LineColumn, SourceError, SourceFileId, SourceMap,
    SourceMapError, SourceSlot, SourceSnapshot, SourceTableId, Span, SpanError,
    normalize_logical_path,
};
pub use target::{
    Architecture, BackendCostProfile, ObjectFormat, OperatingSystem, Rt0Kind, TargetDescriptor,
    TargetName, TargetParseError, baseline_cost_profile,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use backend::BackendPlan;
use frontend::{SourceInput, cfg::CfgContext};
use runtime::{
    RawModelInputs, RawPlaneDemand, RawPlanePolicyV1, RawResourceDemand, RuntimeRawContractV1,
};

/// 一次 bootstrap 编译请求。
#[derive(Clone, Debug)]
pub struct CompileRequest {
    target: TargetName,
    input: CompileInput,
}

impl CompileRequest {
    /// 创建空 package 请求。
    pub fn empty_package(target: TargetName) -> Self {
        Self {
            target,
            input: CompileInput::EmptyPackage,
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
        }
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

    /// 返回规范退出码；内部 IR 不变量失败与用户源码错误分开报告。
    pub fn exit_code(&self) -> i32 {
        if self.diagnostics.items().iter().any(|diagnostic| {
            matches!(
                diagnostic.code(),
                DiagnosticCode::LirInvariant
                    | DiagnosticCode::RuntimeRawInvariant
                    | DiagnosticCode::ResourceInvariant
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
        let CompileRequest { target, input } = request;
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
                    action_key: None,
                };
            }
        };
        // runtime raw 平面契约：输入来自冻结前端产物与目标描述，与 LIR 一起构成内部表示。
        let demand = RawPlaneDemand {
            coroutine_sites: frontend.gir.coroutine_site_count(),
            resource_sites: frontend.gir.placement.counts().resource,
            runtime_raw_sites: frontend.gir.placement.counts().runtime_raw,
            owners: 0,
            message_nodes: 0,
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
        let raw_contract = match runtime::run(
            RawModelInputs {
                target,
                policy: RawPlanePolicyV1::default(),
                demand,
                resource_demand,
                profile: runtime::PlatformProfile::from(target),
                lir_fingerprint: lir.fingerprint(),
                placement_fingerprint: frontend.gir.placement.fingerprint,
                sources: &source_map,
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
                    action_key: None,
                };
            }
        };
        let action_key = Some(compilation_action_key(
            target,
            &source_map,
            &frontend,
            &lir,
            &raw_contract,
            loaded
                .plan
                .as_ref()
                .map(|plan| (plan.require_main, &plan.cfg)),
        ));

        let hir = frontend.hir;
        let gir = frontend.gir;
        let gir_blocks = gir
            .bodies
            .iter()
            .map(|body| body.blocks.len())
            .sum::<usize>();
        let gir_stmts = gir
            .bodies
            .iter()
            .map(|body| body.statements.len())
            .sum::<usize>();
        graph.complete(
            ActionKind::BuildIr,
            format!(
                "{} 个定义，{} 个已冻结 HIR owner，{} 个 GIR body / {} 个 block / {} 条语句，{} 个 LIR body / {} 条指令 / {} 个 Mem effect",
                hir.module().definitions.len(),
                hir.module().owners.len(),
                gir.bodies.len(),
                gir_blocks,
                gir_stmts,
                lir.bodies(), lir.instructions(), lir.memory_operations()
            ),
        );

        let Some(backend_plan) = backend::plan(
            target,
            &hir,
            &frontend.mono,
            &gir,
            &lir,
            &raw_contract,
            frontend.analysis.runtime_checks_elided_count,
        ) else {
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
                raw_contract: None,
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
            action_key,
        }
    }
}

/// 前端 action 的完整输入集合：identity、host/target、源码摘要、cfg 与 registry 摘要。
fn compilation_action_key(
    target: TargetName,
    source_map: &SourceMap,
    frontend: &frontend::FrontendOutput,
    lir: &lir::Validated,
    raw_contract: &RuntimeRawContractV1,
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
            return Err(LoadInputError::ModuleTree {
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
    placement_count: u32,
    turn_region_count: u32,
    local_heap_count: u32,
    shared_heap_count: u32,
    placement_fingerprint: [u8; 32],
    rt0: Rt0Boundary,
    semantic_fingerprint: [u8; 32],
}

impl ImagePlan {
    fn new(
        plan: BackendPlan,
        attachment: runtime::RuntimeAttachment,
        raw: &RuntimeRawContractV1,
    ) -> Self {
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
            placement_count: plan.placement_count,
            turn_region_count: plan.turn_region_count,
            local_heap_count: plan.local_heap_count,
            shared_heap_count: plan.shared_heap_count,
            placement_fingerprint: plan.placement_fingerprint,
            rt0: attachment.rt0,
            semantic_fingerprint: plan.semantic_fingerprint,
        }
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
        assert_eq!(plan.runtime_source_count(), 3);
        assert_eq!(plan.rt0(), super::Rt0Boundary::LinuxSyscall);
        assert_eq!(
            compilation.action_graph().nodes()[7].status(),
            ActionStatus::Skipped
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
        assert!(dump.contains("runtime-raw schema=3"));
        assert!(dump.contains("platform schema=1 profile=linux"));
        assert!(dump.contains("range-op commit mutating=true blocking=false"));
        assert!(dump.contains("extent-class bytes=2097152 align=2097152 huge-page=true"));
        assert!(dump.contains("range-state committed rule=committed-bytes split-by-commit=true"));
        assert!(dump.contains("range-trim grace-steps=4 leases=allocator,scanner,forwarder"));
        assert!(dump.contains("range-fault linux out-of-space -> OutOfMemory"));
        assert!(dump.contains("range-fault windows out-of-space -> OutOfMemory"));
        assert!(dump.contains("ledger-partition runtime-committed-bytes plane=physical"));
        assert!(dump.contains("ledger-partition address-space-reserved-bytes plane=virtual"));
        assert!(dump.contains("ledger reserved-bytes partition=address-space-reserved-bytes"));
        assert!(dump.contains("message integrity integrity"));
        assert!(dump.contains("resource-cell leases offset=0 bytes=8"));
        assert!(dump.contains("resource-kind 0 File entry=std.resource.release"));
        assert!(dump.contains("release-entry=std.resource.release"));
        assert_eq!(Some(dump), warm.dump_runtime(), "冷热 dump 必须一致");
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
