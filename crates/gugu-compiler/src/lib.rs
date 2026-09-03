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
mod runtime;
mod target;

pub use action::{ActionGraph, ActionKind, ActionNode, ActionStatus};
pub use diagnostics::{Diagnostic, DiagnosticCode, Diagnostics, Severity, Span};
pub use runtime::{
    IntrinsicBoundary, Rt0Boundary, RuntimeResources, RuntimeSource, RuntimeSourceRole,
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

    /// 创建从文件系统读取的单文件请求。
    pub fn single_file_path(path: impl Into<PathBuf>, target: TargetName) -> Self {
        Self {
            target,
            input: CompileInput::SingleFilePath(path.into()),
        }
    }
}

#[derive(Clone, Debug)]
enum CompileInput {
    EmptyPackage,
    SingleFileSource { path: PathBuf, source: String },
    SingleFilePath(PathBuf),
}

/// compiler 阶段 1 返回的内存结果。
#[derive(Clone, Debug)]
pub struct Compilation {
    graph: ActionGraph,
    diagnostics: Diagnostics,
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

    /// 返回成功时的内存镜像计划。
    pub fn image_plan(&self) -> Option<&ImagePlan> {
        self.image_plan.as_ref()
    }

    /// 判断本次 action 是否成功且没有错误诊断。
    pub fn is_success(&self) -> bool {
        !self.diagnostics.has_errors()
    }
}

/// 阶段 1 的编译器入口。
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
            Err((path, error)) => {
                diagnostics.push(Diagnostic::source_read(&path, &error));
                graph.fail(ActionKind::LoadSources, "源文件读取失败");
                graph.skip_after(ActionKind::LoadSources, "前置 action 失败");
                diagnostics.sort();
                return Compilation {
                    graph,
                    diagnostics,
                    image_plan: None,
                };
            }
        };
        graph.complete(ActionKind::LoadSources, loaded.detail());

        let frontend_input = loaded.as_source_input();
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
            image_plan,
        }
    }
}

#[derive(Clone, Debug)]
enum LoadedInput {
    EmptyPackage,
    SingleFile { path: PathBuf, source: String },
}

impl LoadedInput {
    fn as_source_input(&self) -> SourceInput<'_> {
        match self {
            Self::EmptyPackage => SourceInput::EmptyPackage,
            Self::SingleFile { path, source } => SourceInput::SingleFile { path, source },
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::EmptyPackage => "空 package".to_owned(),
            Self::SingleFile { path, .. } => format!("{}", path.display()),
        }
    }
}

fn load_input(input: &CompileInput) -> Result<LoadedInput, (PathBuf, std::io::Error)> {
    match input {
        CompileInput::EmptyPackage => Ok(LoadedInput::EmptyPackage),
        CompileInput::SingleFileSource { path, source } => Ok(LoadedInput::SingleFile {
            path: path.clone(),
            source: source.clone(),
        }),
        CompileInput::SingleFilePath(path) => fs::read_to_string(path)
            .map(|source| LoadedInput::SingleFile {
                path: path.clone(),
                source,
            })
            .map_err(|error| (path.clone(), error)),
    }
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
    use super::{ActionKind, ActionStatus, CompileRequest, Compiler, TargetName};

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
}
