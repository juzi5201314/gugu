#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Gugu compiler 的阶段化 bootstrap 接口。
//!
//! 阶段 1 只建立 action graph、目标描述、bootstrap 前端、最小 IR、后端
//! image plan 和 Gugu runtime 源资源登记。它不会执行完整 parser、类型系统、
//! machine encoder 或最终镜像写出。

mod action;
mod backend;
mod diagnostics;
mod frontend;
mod ir;
mod project;
mod runtime;
mod source;
mod target;

pub use action::{ActionGraph, ActionKind, ActionNode, ActionStatus};
pub use diagnostics::{Diagnostic, DiagnosticCode, Diagnostics, Severity};
pub use project::{
    ActionInputs, ActionKey, CacheError, CachePolicy, DependencyCache, DependencyDomain,
    DependencyInput, DependencySource, DependencySpec, LockGraph, LockedDependency, LockedPackage,
    Package, PackageFiles, PackageId, PackageMetadata, PackageSource, Project, ProjectError,
    ResolveOptions, Target, TargetArtifact, TargetCondition, TargetKind, TargetSelection,
    TargetView, Version, VersionReq, Workspace, candidates_from_lock, default_cache_root,
    materialize_vendor, prepare_dependency_inputs,
};
pub use runtime::{
    IntrinsicBoundary, Rt0Boundary, RuntimeResources, RuntimeSource, RuntimeSourceRole,
};
pub use source::{
    ExpansionId, ExpansionInput, ExpansionRecord, LineColumn, SourceError, SourceFileId, SourceMap,
    SourceMapError, SourceSlot, SourceSnapshot, SourceTableId, Span, SpanError,
    normalize_logical_path,
};
pub use target::{
    Architecture, ObjectFormat, OperatingSystem, Rt0Kind, TargetDescriptor, TargetName,
    TargetParseError,
};

use std::{fs, path::PathBuf};

use backend::BackendPlan;
use frontend::SourceInput;

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

    /// 创建项目 target 入口请求；逻辑路径由调用方按 package root 推导。
    pub fn project_entry(
        path: impl Into<PathBuf>,
        logical_path: impl Into<PathBuf>,
        target: TargetName,
        require_main: bool,
    ) -> Self {
        Self {
            target,
            input: CompileInput::File {
                path: path.into(),
                logical_path: Some(logical_path.into()),
                require_main,
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
}

/// compiler 返回的内存结果。
#[derive(Clone, Debug)]
pub struct Compilation {
    graph: ActionGraph,
    diagnostics: Diagnostics,
    source_map: SourceMap,
    image_plan: Option<ImagePlan>,
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
        !self.diagnostics.has_errors()
    }
}

/// 阶段 3/4 的编译器入口。
#[derive(Clone, Copy, Debug, Default)]
pub struct Compiler;

impl Compiler {
    /// 创建 bootstrap compiler。
    pub fn new() -> Self {
        Self
    }

    /// 执行一次确定性的 bootstrap action graph。
    pub fn compile(&self, request: CompileRequest) -> Compilation {
        let mut graph = ActionGraph::new();
        let mut diagnostics = Diagnostics::default();
        let target = request.target;
        let descriptor = target.descriptor();
        graph.complete(ActionKind::ResolveTarget, target.to_string());

        let loaded = match load_input(&request.input) {
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
                };
            }
        };
        let source_map = loaded.source_map();
        graph.complete(ActionKind::LoadSources, loaded.detail());

        let frontend_input = loaded.as_source_input(&source_map);
        let frontend = match frontend::bootstrap(frontend_input) {
            Ok(output) => output,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                graph.fail(ActionKind::Frontend, "bootstrap 前端检查失败");
                graph.skip_after(ActionKind::Frontend, "前置 action 失败");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    source_map,
                    image_plan: None,
                };
            }
        };
        graph.complete(ActionKind::Frontend, frontend.detail());

        let ir = ir::lower(&frontend);
        graph.complete(
            ActionKind::BuildIr,
            format!("{} 个函数", ir.functions.len()),
        );

        let Some(backend_plan) = backend::plan(target, &ir) else {
            graph.complete(ActionKind::PlanBackend, "没有可执行入口");
            graph.skip_after(ActionKind::PlanBackend, "没有可执行入口");
            diagnostics.sort();
            return Compilation {
                graph,
                diagnostics,
                source_map,
                image_plan: None,
            };
        };
        graph.complete(ActionKind::PlanBackend, "内存 image plan");

        let runtime = RuntimeResources::builtin();
        let attachment = runtime.attach(descriptor.name);
        graph.complete(
            ActionKind::AttachRuntime,
            format!(
                "{} 个 Gugu 源单元，rt0={}",
                attachment.source_count, attachment.rt0
            ),
        );
        graph.complete(ActionKind::ValidateImage, "image plan 校验通过");
        graph.skip_after(
            ActionKind::ValidateImage,
            "阶段 1 仅保留内存计划，未写出镜像",
        );

        let image_plan = Some(ImagePlan::new(backend_plan, attachment));
        diagnostics.sort();
        Compilation {
            graph,
            diagnostics,
            source_map,
            image_plan,
        }
    }
}

#[derive(Clone, Debug)]
enum LoadedInput {
    EmptyPackage,
    File {
        snapshot: SourceSnapshot,
        require_main: bool,
    },
}

impl LoadedInput {
    fn as_source_input<'a>(&'a self, source_map: &'a SourceMap) -> SourceInput<'a> {
        match self {
            Self::EmptyPackage => SourceInput::EmptyPackage,
            Self::File {
                snapshot,
                require_main,
            } => {
                if *require_main {
                    SourceInput::SingleFile {
                        snapshot,
                        source_map,
                    }
                } else {
                    SourceInput::LibraryFile { snapshot }
                }
            }
        }
    }

    fn source_map(&self) -> SourceMap {
        let snapshot = match self {
            Self::EmptyPackage => return SourceMap::empty(),
            Self::File { snapshot, .. } => snapshot,
        };
        SourceMap::new(vec![snapshot.clone()]).expect("one source path is unique")
    }

    fn detail(&self) -> String {
        match self {
            Self::EmptyPackage => "空 package".to_owned(),
            Self::File { snapshot, .. } => snapshot.logical_path().to_owned(),
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
}

impl LoadInputError {
    fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::Read { path, error } => Diagnostic::source_read(path, error),
            Self::Snapshot(error) => Diagnostic::source_error(error),
        }
    }
}

fn load_input(input: &CompileInput) -> Result<LoadedInput, LoadInputError> {
    match input {
        CompileInput::EmptyPackage => Ok(LoadedInput::EmptyPackage),
        CompileInput::SingleFileSource { path, source } => {
            SourceSnapshot::from_str(logical_input_path(path), source)
                .map(|snapshot| LoadedInput::File {
                    snapshot,
                    require_main: true,
                })
                .map_err(LoadInputError::Snapshot)
        }
        CompileInput::File {
            path,
            logical_path,
            require_main,
        } => fs::read(path)
            .map_err(|error| LoadInputError::Read {
                path: path.clone(),
                error,
            })
            .and_then(|bytes| {
                let logical_path = logical_path
                    .clone()
                    .unwrap_or_else(|| logical_input_path(path));
                SourceSnapshot::from_bytes(logical_path, bytes)
                    .map(|snapshot| LoadedInput::File {
                        snapshot,
                        require_main: *require_main,
                    })
                    .map_err(LoadInputError::Snapshot)
            }),
    }
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

/// 阶段 1 的内存镜像计划，不是可执行文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImagePlan {
    target: TargetName,
    entry: &'static str,
    function_count: u32,
    runtime_source_count: u32,
    rt0: Rt0Boundary,
}

impl ImagePlan {
    fn new(plan: BackendPlan, attachment: runtime::RuntimeAttachment) -> Self {
        Self {
            target: plan.target,
            entry: plan.entry,
            function_count: plan.function_count,
            runtime_source_count: attachment.source_count,
            rt0: attachment.rt0,
        }
    }

    /// 返回目标名称。
    pub fn target(&self) -> TargetName {
        self.target
    }

    /// 返回入口符号。
    pub fn entry(&self) -> &'static str {
        self.entry
    }

    /// 返回 bootstrap IR 中的函数数量。
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
}

impl frontend::FrontendOutput {
    fn detail(&self) -> String {
        match &self.path {
            Some(path) => format!("{}，{} 字节", path.display(), self.source_len),
            None => "空模块".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        ActionKind, ActionStatus, CompileRequest, Compiler, DiagnosticCode, TargetName,
        logical_input_path,
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
        assert_eq!(plan.runtime_source_count(), 2);
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
        assert_eq!(compilation.diagnostics().items().len(), 1);
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
        let compilation = Compiler::new().compile(CompileRequest::project_entry(
            &entry,
            "src/lib.gg",
            TargetName::X86_64Linux,
            false,
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
        assert_eq!(source_map.snapshots().len(), 1);
        assert_eq!(source_map.snapshots()[0].logical_path(), "src/lib.gg");
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
