mod config;
mod formatting;
mod output;

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use gugu_compiler::{
    CachePolicy, CompileRequest, Compiler, DependencyCache, DependencyDomain, LockGraph, Package,
    PackageSource, Project, ResolveOptions, TargetKind, TargetName, TargetSelection,
    candidates_from_lock, default_cache_root, prepare_dependency_inputs,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{
    config::{ConfigValues, environment_flag, environment_path, environment_text, load_config},
    output::{ColorMode, OutputFormat, emit_cli_error, parse_color, print_compilation},
};
#[derive(Clone, Debug, Default, Args)]
pub(crate) struct GlobalArgs {
    /// 输出格式。
    #[arg(long, value_enum, global = true)]
    pub(crate) format: Option<OutputFormat>,
    /// 颜色输出策略。
    #[arg(long, value_enum, global = true)]
    pub(crate) color: Option<ColorMode>,
    /// 只输出错误与最终结果。
    #[arg(
        short = 'q',
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with = "verbose"
    )]
    pub(crate) quiet: bool,
    /// 输出详细进度。
    #[arg(
        short = 'v',
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with = "quiet"
    )]
    pub(crate) verbose: bool,
    /// 禁止工具链网络访问。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) offline: bool,
    /// 要求锁文件保持最新。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) locked: bool,
    /// 等价于 --locked --offline。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) frozen: bool,
    /// 从 workspace vendor 目录读取依赖。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) vendor: bool,
    /// 要求 registry package 使用有效签名。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) require_signature: bool,
    /// 拒绝锁图中的 yanked package。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) deny_yanked: bool,
    /// 目标名称。
    #[arg(long, global = true)]
    pub(crate) target: Option<String>,
    /// 选择 package。
    #[arg(short = 'p', long, value_name = "owner/name", global = true)]
    pub(crate) package: Option<String>,
    /// 选择整个 workspace。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) workspace: bool,
    /// 选择 lib target。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with_all = ["bin", "test", "bench", "example", "all_targets"]
    )]
    pub(crate) lib: bool,
    /// 选择 bin target；在 new/init 中表示创建 bin package。
    #[arg(
        long,
        value_name = "name",
        num_args = 0..=1,
        default_missing_value = "",
        global = true,
        conflicts_with_all = ["lib", "test", "bench", "example", "all_targets"]
    )]
    pub(crate) bin: Option<Option<String>>,
    /// 选择 test target。
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["lib", "bin", "bench", "example", "all_targets"]
    )]
    pub(crate) test: Option<String>,
    /// 选择 bench target。
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["lib", "bin", "test", "example", "all_targets"]
    )]
    pub(crate) bench: Option<String>,
    /// 选择 example target。
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["lib", "bin", "test", "bench", "all_targets"]
    )]
    pub(crate) example: Option<String>,
    /// 选择所有 target。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with_all = ["lib", "bin", "test", "bench", "example"]
    )]
    pub(crate) all_targets: bool,
    /// 启用逗号分隔的 feature。
    #[arg(long, global = true, conflicts_with = "all_features")]
    pub(crate) features: Option<String>,
    /// 禁用默认 feature。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with = "all_features"
    )]
    pub(crate) no_default_features: bool,
    /// 启用全部 feature。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with_all = ["features", "no_default_features"]
    )]
    pub(crate) all_features: bool,
    /// 对最终镜像执行 strip。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) strip: bool,
    /// 启用 build.gg 权限门。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    pub(crate) permission: bool,
    /// 允许 build.gg 执行全部操作。
    #[arg(short = 'A', action = ArgAction::SetTrue, global = true)]
    pub(crate) allow_all: bool,
    /// 预授权读路径。
    #[arg(long, action = ArgAction::Append, global = true)]
    pub(crate) read_allows: Vec<PathBuf>,
    /// 预授权写路径。
    #[arg(long, action = ArgAction::Append, global = true)]
    pub(crate) write_allows: Vec<PathBuf>,
    /// 预授权环境变量。
    #[arg(long, action = ArgAction::Append, global = true)]
    pub(crate) env_allows: Vec<String>,
    /// 预授权网络主机。
    #[arg(long, action = ArgAction::Append, global = true)]
    pub(crate) net_allows: Vec<String>,
    /// 预授权进程执行路径。
    #[arg(long, action = ArgAction::Append, global = true)]
    pub(crate) run_allows: Vec<String>,
    /// 追加配置文件。
    #[arg(long, action = ArgAction::Append, global = true)]
    pub(crate) config: Vec<PathBuf>,
    /// 覆盖缓存目录。
    #[arg(long, global = true)]
    pub(crate) cache_dir: Option<PathBuf>,
    /// 覆盖 target 目录。
    #[arg(long, global = true)]
    pub(crate) target_dir: Option<PathBuf>,
}

#[derive(Debug, Parser)]
#[command(
    name = "gugu",
    version,
    about = "Gugu 编译器 bootstrap",
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    /// 打印版本信息并退出。
    #[arg(long = "version", action = ArgAction::SetTrue)]
    version_flag: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 创建新 package 或 workspace。
    New { path: PathBuf },
    /// 在当前目录初始化 package 或 workspace。
    Init,
    /// 编译一个单文件入口或空 package。
    Build { file: Option<PathBuf> },
    /// 执行编译检查但不准备镜像计划。
    Check { file: Option<PathBuf> },
    /// 编译并运行 bin target。
    #[command(trailing_var_arg = true)]
    Run {
        target: Option<String>,
        args: Vec<OsString>,
    },
    /// 编译并运行 test target。
    #[command(trailing_var_arg = true)]
    Test { args: Vec<OsString> },
    /// 编译并运行 bench target。
    #[command(trailing_var_arg = true)]
    Bench { args: Vec<OsString> },
    /// 格式化源码。
    Fmt {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        all: bool,
    },
    /// 生成 API 文档。
    Doc {
        #[arg(long)]
        open: bool,
        #[arg(long)]
        no_deps: bool,
    },
    /// 删除 workspace 或编译缓存。
    Clean {
        #[arg(long)]
        cache: bool,
        #[arg(long)]
        registry: bool,
        #[arg(long)]
        all: bool,
    },
    /// 添加依赖。
    Add {
        package: String,
        #[arg(long)]
        dev: bool,
        #[arg(long)]
        build: bool,
        #[arg(long)]
        default_features: bool,
        #[arg(long)]
        optional: bool,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        git: Option<String>,
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// 移除依赖。
    Remove { package: String },
    /// 更新锁文件。
    Update {
        #[arg(long)]
        precise: Option<String>,
    },
    /// 打印依赖树。
    Tree {
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long)]
        duplicates: bool,
        #[arg(long)]
        invert: Option<String>,
    },
    /// 生成 vendor 目录。
    Vendor,
    /// 生成 package 发布归档。
    Package,
    /// 发布 package。
    Publish {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        sign: Option<PathBuf>,
    },
    /// 撤回 registry 中的精确版本。
    Yank {
        package: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        undo: bool,
    },
    /// 保存 registry 凭据。
    Login {
        registry: String,
        #[arg(long)]
        token_stdin: bool,
    },
    #[command(subcommand, about = "管理编译缓存")]
    Cache(CacheCommand),
    /// 解释诊断码或 lint 名。
    Explain { code: String },
    /// 打印版本信息。
    Version,
    /// 打印帮助。
    Help { command: Option<String> },
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// 清空编译缓存。
    Clean,
    /// 按 LRU 回收编译缓存。
    Gc,
    /// 校验缓存完整性。
    Verify,
    /// 打印缓存目录。
    Dir,
}

fn main() {
    let exit_code = run_process();
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn run_process() -> i32 {
    let arguments = env::args_os().collect::<Vec<_>>();
    let format = format_hint(&arguments);
    match Cli::try_parse_from(arguments) {
        Ok(cli) => execute(cli),
        Err(error) => {
            let exit_code = match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
            if exit_code == 0 || format == OutputFormat::Text {
                if error.print().is_err() {
                    return 101;
                }
            } else {
                emit_cli_error(format, &error.to_string());
            }
            exit_code
        }
    }
}

fn format_hint(arguments: &[OsString]) -> OutputFormat {
    let mut explicit = None;
    let mut iterator = arguments.iter().skip(1);
    while let Some(argument) = iterator.next() {
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        if argument == "--format" {
            explicit = iterator
                .next()
                .map(|value| value.to_string_lossy().into_owned());
        } else if let Some(value) = argument.strip_prefix("--format=") {
            explicit = Some(value.to_owned());
        }
    }
    if let Some(value) = explicit {
        return OutputFormat::parse(&value).unwrap_or_default();
    }
    environment_text("GUGU_FORMAT")
        .ok()
        .flatten()
        .and_then(|value| OutputFormat::parse(&value).ok())
        .unwrap_or_default()
}

fn execute(cli: Cli) -> i32 {
    if cli.version_flag {
        let format = match resolve_format(&cli.global) {
            Ok(format) => format,
            Err(error) => {
                emit_cli_error(OutputFormat::Text, &error);
                return 2;
            }
        };
        print_version(format);
        return 0;
    }
    let Some(command) = cli.command else {
        return print_help(None);
    };

    match command {
        Command::Help { command } => return print_help(command.as_deref()),
        Command::Version => {
            let format = match resolve_format(&cli.global) {
                Ok(format) => format,
                Err(error) => {
                    emit_cli_error(OutputFormat::Text, &error);
                    return 2;
                }
            };
            print_version(format);
            return 0;
        }
        _ => {}
    }

    let options = match resolve_global(&cli.global) {
        Ok(options) => options,
        Err(error) => {
            let format = resolve_format(&cli.global).unwrap_or_default();
            emit_cli_error(format, &error);
            return 2;
        }
    };

    match command {
        Command::Build { file } => run_compile(file, &options, false),
        Command::Check { file } => run_compile(file, &options, true),
        Command::Fmt { check, all } => formatting::run(check, all, &options),
        command => run_registered_command(&command, options.format.unwrap_or_default()),
    }
}

fn resolve_format(raw: &GlobalArgs) -> Result<OutputFormat, String> {
    if let Some(format) = raw.format {
        return Ok(format);
    }
    match environment_text("GUGU_FORMAT")? {
        Some(value) => OutputFormat::parse(&value),
        None => Ok(OutputFormat::default()),
    }
}

fn resolve_color(raw: Option<ColorMode>) -> Result<ColorMode, String> {
    if let Some(color) = raw {
        return Ok(color);
    }
    match environment_text("GUGU_COLOR")? {
        Some(value) => parse_color(&value),
        None => Ok(ColorMode::default()),
    }
}

fn apply_permission_options(options: &mut GlobalArgs, config: &ConfigValues) {
    options.permission = options.permission || config.permission.unwrap_or(false);
    if options.read_allows.is_empty() {
        options.read_allows = config.read_allows.clone();
    }
    if options.write_allows.is_empty() {
        options.write_allows = config.write_allows.clone();
    }
    if options.env_allows.is_empty() {
        options.env_allows = config.env_allows.clone();
    }
    if options.net_allows.is_empty() {
        options.net_allows = config.net_allows.clone();
    }
    if options.run_allows.is_empty() {
        options.run_allows = config.run_allows.clone();
    }
}

fn resolve_global(raw: &GlobalArgs) -> Result<GlobalArgs, String> {
    let (config, config_files) = load_config(&raw.config)?;
    let mut options = raw.clone();
    options.format = Some(resolve_format(raw)?);
    options.color = Some(resolve_color(raw.color)?);
    let environment_target = environment_text("GUGU_BUILD_TARGET")?;
    let environment_cache_dir = environment_path("GUGU_CACHE_DIR");
    let environment_target_dir = environment_path("GUGU_TARGET_DIR");
    let frozen = raw.frozen;

    options.offline = frozen || raw.offline || environment_flag("GUGU_OFFLINE");
    options.locked = frozen || raw.locked || environment_flag("GUGU_LOCKED");
    options.require_signature = raw.require_signature
        || environment_flag("GUGU_REGISTRY_REQUIRE_SIGNATURE")
        || config.require_signature.unwrap_or(false);
    options.deny_yanked = raw.deny_yanked
        || environment_flag("GUGU_REGISTRY_DENY_YANKED")
        || config.deny_yanked.unwrap_or(false);
    apply_permission_options(&mut options, &config);
    options.target = raw.target.clone().or(environment_target).or(config.target);
    // 保留无名的 --bin（选择全部 bin）；只把空的显式名字归一为空。
    options.bin = raw
        .bin
        .as_ref()
        .map(|value| {
            value
                .as_ref()
                .filter(|name| !name.is_empty())
                .map(String::as_str)
        })
        .map(|value| value.map(str::to_owned));
    options.config = config_files;
    options.cache_dir = raw
        .cache_dir
        .clone()
        .or(environment_cache_dir)
        .or(config.cache_dir);
    options.target_dir = raw
        .target_dir
        .clone()
        .or(environment_target_dir)
        .or(config.target_dir);
    Ok(options)
}

fn print_help(command_name: Option<&str>) -> i32 {
    let mut command = Cli::command();
    if let Some(name) = command_name {
        let Some(subcommand) = command.find_subcommand_mut(name) else {
            emit_cli_error(OutputFormat::Text, &format!("未知帮助主题 `{name}`"));
            return 2;
        };
        if subcommand.print_help().is_err() {
            eprintln!("无法输出帮助");
            return 101;
        }
    } else if command.print_help().is_err() {
        eprintln!("无法输出帮助");
        return 101;
    }
    println!();
    0
}

fn version_identity() -> (String, String, String, String) {
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let commit = option_env!("GUGU_COMMIT").unwrap_or("unknown").to_owned();
    let commit_date = option_env!("GUGU_COMMIT_DATE")
        .unwrap_or("unknown")
        .to_owned();
    let host = TargetName::host()
        .map(|target| target.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    (version, commit, commit_date, host)
}

pub(crate) fn version_text_lines() -> Vec<String> {
    let (version, commit, commit_date, host) = version_identity();
    vec![
        format!("gugu {version} (commit {commit} {commit_date})"),
        format!("host: {host}"),
        "llvm: not-used".to_owned(),
    ]
}

fn print_version(format: OutputFormat) {
    let (version, commit, commit_date, host) = version_identity();
    match format {
        OutputFormat::Text => {
            for line in version_text_lines() {
                println!("{line}");
            }
        }
        OutputFormat::Json => println!(
            "{}",
            json!({
                "version": version,
                "commit": commit,
                "commit-date": commit_date,
                "host": host,
                "llvm": "not-used"
            })
        ),
        OutputFormat::JsonDiagnosticShort => {}
    }
}

fn run_compile(file: Option<PathBuf>, options: &GlobalArgs, check_only: bool) -> i32 {
    let format = options.format.unwrap_or_default();
    let target = match options.target.as_deref() {
        Some(target) => match TargetName::parse(target) {
            Ok(target) => target,
            Err(error) => {
                emit_cli_error(format, &error.to_string());
                return 2;
            }
        },
        None => match TargetName::host() {
            Some(target) => target,
            None => {
                emit_cli_error(format, "当前宿主不是已登记的 Gugu 目标");
                return 2;
            }
        },
    };

    let Some(file) = file else {
        return run_project_compile(options, target, check_only);
    };
    if let Some(message) = single_file_mode_conflict(options) {
        emit_cli_error(format, &message);
        return 2;
    }
    let compilation = Compiler::new().compile(CompileRequest::single_file_path(file, target));
    print_compilation(&compilation, check_only, options, target);
    i32::from(!compilation.is_success())
}

fn single_file_mode_conflict(options: &GlobalArgs) -> Option<String> {
    let conflicts = [
        options.package.is_some().then_some("-p/--package"),
        options.workspace.then_some("--workspace"),
        options.lib.then_some("--lib"),
        options.bin.is_some().then_some("--bin"),
        options.test.is_some().then_some("--test"),
        options.bench.is_some().then_some("--bench"),
        options.example.is_some().then_some("--example"),
        options.all_targets.then_some("--all-targets"),
        options.features.is_some().then_some("--features"),
        options
            .no_default_features
            .then_some("--no-default-features"),
        options.all_features.then_some("--all-features"),
    ];
    let names = conflicts
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("、");
    (!names.is_empty()).then(|| format!("单文件编译模式不支持参数：{names}"))
}

fn target_selection(options: &GlobalArgs) -> TargetSelection {
    if options.all_targets {
        return TargetSelection::All;
    }
    if options.lib {
        return TargetSelection::Lib;
    }
    if let Some(name) = options.test.as_deref() {
        return TargetSelection::Test(Some(name.to_owned()));
    }
    if let Some(name) = options.bench.as_deref() {
        return TargetSelection::Bench(Some(name.to_owned()));
    }
    if let Some(name) = options.example.as_deref() {
        return TargetSelection::Example(Some(name.to_owned()));
    }
    match &options.bin {
        // --bin 不带名字表示选择全部 bin。
        Some(None) => return TargetSelection::Bin(None),
        Some(Some(name)) => return TargetSelection::Bin(Some(name.clone())),
        None => {}
    }
    TargetSelection::DefaultBuild
}

/// 根据全局参数与 package 声明解析启用 feature 集合。
///
/// 返回值按名称排序去重；`--all-features` 启用全部声明，
/// `default` 在未禁用默认 feature 时启用，`--features` 追加显式名称。
fn enabled_features(options: &GlobalArgs, package: &Package) -> Vec<String> {
    let mut features = if options.all_features {
        package.declared_features().to_vec()
    } else if options.no_default_features {
        Vec::new()
    } else {
        vec!["default".to_owned()]
    };
    if let Some(list) = options.features.as_deref() {
        for name in list.split(',') {
            let name = name.trim();
            if !name.is_empty() {
                features.push(name.to_owned());
            }
        }
    }
    features.sort();
    features.dedup();
    features
}

fn compile_package(
    project_root: &Path,
    package: &Package,
    options: &GlobalArgs,
    target: TargetName,
    check_only: bool,
    compiler: &Compiler,
    lock: &LockGraph,
    format: OutputFormat,
) -> Result<bool, ()> {
    let features = enabled_features(options, package);
    let selected = match package.select_targets_in(&target_selection(options), &features) {
        Ok(selected) => selected,
        Err(error) => {
            emit_cli_error(format, &error.to_string());
            return Err(());
        }
    };
    let mut failed = false;
    for package_target in selected {
        let (package_identity, external_packages) =
            package_resolution(lock, project_root, package, package_target.kind());
        let request = CompileRequest::project_target(
            package,
            package_target,
            package_identity,
            target,
            features.clone(),
            external_packages,
        );
        let compilation = compiler.compile(request);
        // 头行只属于 text 输出；json 与 json-diagnostic-short 保持纯事件流。
        if !options.quiet && format == OutputFormat::Text {
            println!(
                "package `{}` target `{}` ({})",
                package.package_name(),
                package_target.name(),
                package_target.kind()
            );
        }
        print_compilation(&compilation, check_only, options, target);
        failed |= !compilation.is_success();
    }
    Ok(failed)
}

fn resolve_project_lock(
    project: &Project,
    packages: &[&Package],
    options: &GlobalArgs,
    target: TargetName,
    format: OutputFormat,
) -> Result<LockGraph, ()> {
    let path = project.lock_path();
    let existing = if path.exists() {
        Some(LockGraph::read(&path).map_err(|error| {
            emit_cli_error(format, &error.to_string());
        })?)
    } else {
        if options.locked {
            emit_cli_error(
                format,
                &format!("--locked 要求锁文件存在：`{}`", path.display()),
            );
            return Err(());
        }
        None
    };
    let (registry_packages, git_packages) = existing
        .as_ref()
        .map(candidates_from_lock)
        .unwrap_or_default();
    let mut roots = Vec::with_capacity(packages.len());
    let mut root_features = BTreeMap::new();
    let mut root_default_features = BTreeMap::new();
    for package in packages {
        roots.push(package.package_name());
        let enabled = enabled_features(options, package);
        root_features.insert(
            package.package_name(),
            enabled
                .into_iter()
                .filter(|feature| feature != "default")
                .collect(),
        );
        root_default_features.insert(package.package_name(), !options.no_default_features);
    }
    let host = TargetName::host().unwrap_or(target);
    let graph = project
        .resolve_dependencies(ResolveOptions {
            target: target.to_string(),
            host: host.to_string(),
            roots,
            root_features,
            root_default_features,
            registry_packages,
            git_packages,
            ..ResolveOptions::default()
        })
        .map_err(|error| {
            emit_cli_error(format, &error.to_string());
        })?;
    if let Some(existing) = existing {
        let expected = graph.to_toml().map_err(|error| {
            emit_cli_error(format, &error.to_string());
        })?;
        let actual = existing.to_toml().map_err(|error| {
            emit_cli_error(format, &error.to_string());
        })?;
        if options.locked && actual != expected {
            emit_cli_error(
                format,
                "--locked 要求 gugu.lock 与当前清单、目标和 feature 一致",
            );
            return Err(());
        }
    }
    let cache_root = options.cache_dir.clone().unwrap_or_else(default_cache_root);
    let cache = DependencyCache::new(cache_root);
    let policy = CachePolicy {
        offline: options.offline,
        locked: options.locked,
        vendor: options.vendor,
    };
    prepare_dependency_inputs(
        &graph,
        &cache,
        project.workspace().root(),
        &project.workspace().root().join("vendor"),
        policy,
    )
    .map_err(|error| {
        emit_cli_error(format, &error.to_string());
    })?;
    if !options.locked {
        graph.write(&path).map_err(|error| {
            emit_cli_error(format, &error.to_string());
        })?;
    }
    Ok(graph)
}

fn package_resolution(
    lock: &LockGraph,
    project_root: &Path,
    package: &Package,
    target: TargetKind,
) -> (String, BTreeSet<String>) {
    let include_test = matches!(
        target,
        TargetKind::Test | TargetKind::Bench | TargetKind::Example
    );
    let relative = package
        .root()
        .strip_prefix(project_root)
        .expect("workspace package 必须位于项目根内");
    let locked = lock
        .packages
        .iter()
        .find(|locked| {
            locked.id.name() == package.package_name()
                && locked.id.version().to_string() == package.version()
                && matches!(
                    locked.id.source(),
                    PackageSource::Path { path }
                        if (path == "." && relative.as_os_str().is_empty())
                            || Path::new(path) == relative
                )
        })
        .expect("已解析锁图必须包含当前 package 的精确 source identity");
    let aliases = locked
        .dependencies
        .iter()
        .filter(|dependency| {
            dependency.domain == DependencyDomain::Normal
                || (include_test && dependency.domain == DependencyDomain::Test)
                || (target == TargetKind::Build && dependency.domain == DependencyDomain::Build)
        })
        .map(|dependency| dependency.alias.clone())
        .collect();
    (locked.id.to_string(), aliases)
}

fn run_project_compile(options: &GlobalArgs, target: TargetName, check_only: bool) -> i32 {
    let format = options.format.unwrap_or_default();
    let start = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = match Project::discover(start) {
        Ok(project) => project,
        Err(error) => {
            emit_cli_error(format, &error.to_string());
            return 2;
        }
    };
    let packages = match project.select_packages(options.package.as_deref(), options.workspace) {
        Ok(packages) => packages,
        Err(error) => {
            emit_cli_error(format, &error.to_string());
            return 2;
        }
    };
    let lock = match resolve_project_lock(&project, &packages, options, target, format) {
        Ok(lock) => lock,
        Err(()) => return 2,
    };
    let compiler = Compiler::new();
    let mut failed = false;
    for package in packages {
        match compile_package(
            project.workspace().root(),
            package,
            options,
            target,
            check_only,
            &compiler,
            &lock,
            format,
        ) {
            Ok(pkg_failed) => failed |= pkg_failed,
            Err(()) => return 2,
        }
    }
    i32::from(failed)
}

fn run_registered_command(command: &Command, format: OutputFormat) -> i32 {
    emit_cli_error(
        format,
        &format!(
            "命令 `{}` 已注册，但 bootstrap 尚未接入其执行 action",
            command_name(command)
        ),
    );
    1
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::New { .. } => "new",
        Command::Init => "init",
        Command::Build { .. } => "build",
        Command::Check { .. } => "check",
        Command::Run { .. } => "run",
        Command::Test { .. } => "test",
        Command::Bench { .. } => "bench",
        Command::Fmt { .. } => "fmt",
        Command::Doc { .. } => "doc",
        Command::Clean { .. } => "clean",
        Command::Add { .. } => "add",
        Command::Remove { .. } => "remove",
        Command::Update { .. } => "update",
        Command::Tree { .. } => "tree",
        Command::Vendor => "vendor",
        Command::Package => "package",
        Command::Publish { .. } => "publish",
        Command::Yank { .. } => "yank",
        Command::Login { .. } => "login",
        Command::Cache(..) => "cache",
        Command::Explain { .. } => "explain",
        Command::Version => "version",
        Command::Help { .. } => "help",
    }
}
