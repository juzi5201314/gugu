use std::{
    env,
    ffi::OsString,
    fmt, fs,
    io::IsTerminal,
    path::{Component, Path, PathBuf},
};

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use gugu_compiler::{
    Compilation, CompileRequest, Compiler, Project, TargetKind, TargetName, TargetSelection,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum OutputFormat {
    #[default]
    Text,
    Json,
    JsonDiagnosticShort,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::JsonDiagnosticShort => "json-diagnostic-short",
        })
    }
}

impl OutputFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "json-diagnostic-short" => Ok(Self::JsonDiagnosticShort),
            _ => Err(format!(
                "未知输出格式 `{value}`，可选值为 text、json、json-diagnostic-short"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lower")]
enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Default, Args)]
struct GlobalArgs {
    /// 输出格式。
    #[arg(long, value_enum, global = true)]
    format: Option<OutputFormat>,
    /// 颜色输出策略。
    #[arg(long, value_enum, global = true)]
    color: Option<ColorMode>,
    /// 只输出错误与最终结果。
    #[arg(
        short = 'q',
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with = "verbose"
    )]
    quiet: bool,
    /// 输出详细进度。
    #[arg(
        short = 'v',
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with = "quiet"
    )]
    verbose: bool,
    /// 禁止工具链网络访问。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    offline: bool,
    /// 要求锁文件保持最新。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    locked: bool,
    /// 等价于 --locked --offline。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    frozen: bool,
    /// 从 workspace vendor 目录读取依赖。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    vendor: bool,
    /// 要求 registry package 使用有效签名。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    require_signature: bool,
    /// 拒绝锁图中的 yanked package。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    deny_yanked: bool,
    /// 目标名称。
    #[arg(long, global = true)]
    target: Option<String>,
    /// 选择 package。
    #[arg(short = 'p', long, value_name = "owner/name", global = true)]
    package: Option<String>,
    /// 选择整个 workspace。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    workspace: bool,
    /// 选择 lib target。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with_all = ["bin", "test", "bench", "example", "all_targets"]
    )]
    lib: bool,
    /// 选择 bin target；在 new/init 中表示创建 bin package。
    #[arg(
        long,
        value_name = "name",
        num_args = 0..=1,
        default_missing_value = "",
        global = true,
        conflicts_with_all = ["lib", "test", "bench", "example", "all_targets"]
    )]
    bin: Option<Option<String>>,
    /// 选择 test target。
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["lib", "bin", "bench", "example", "all_targets"]
    )]
    test: Option<String>,
    /// 选择 bench target。
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["lib", "bin", "test", "example", "all_targets"]
    )]
    bench: Option<String>,
    /// 选择 example target。
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["lib", "bin", "test", "bench", "all_targets"]
    )]
    example: Option<String>,
    /// 选择所有 target。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with_all = ["lib", "bin", "test", "bench", "example"]
    )]
    all_targets: bool,
    /// 启用逗号分隔的 feature。
    #[arg(long, global = true, conflicts_with = "all_features")]
    features: Option<String>,
    /// 禁用默认 feature。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with = "all_features"
    )]
    no_default_features: bool,
    /// 启用全部 feature。
    #[arg(
        long,
        action = ArgAction::SetTrue,
        global = true,
        conflicts_with_all = ["features", "no_default_features"]
    )]
    all_features: bool,
    /// 对最终镜像执行 strip。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    strip: bool,
    /// 启用 build.gg 权限门。
    #[arg(long, action = ArgAction::SetTrue, global = true)]
    permission: bool,
    /// 允许 build.gg 执行全部操作。
    #[arg(short = 'A', action = ArgAction::SetTrue, global = true)]
    allow_all: bool,
    /// 预授权读路径。
    #[arg(long, action = ArgAction::Append, global = true)]
    read_allows: Vec<PathBuf>,
    /// 预授权写路径。
    #[arg(long, action = ArgAction::Append, global = true)]
    write_allows: Vec<PathBuf>,
    /// 预授权环境变量。
    #[arg(long, action = ArgAction::Append, global = true)]
    env_allows: Vec<String>,
    /// 预授权网络主机。
    #[arg(long, action = ArgAction::Append, global = true)]
    net_allows: Vec<String>,
    /// 预授权进程执行路径。
    #[arg(long, action = ArgAction::Append, global = true)]
    run_allows: Vec<String>,
    /// 追加配置文件。
    #[arg(long, action = ArgAction::Append, global = true)]
    config: Vec<PathBuf>,
    /// 覆盖缓存目录。
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    /// 覆盖 target 目录。
    #[arg(long, global = true)]
    target_dir: Option<PathBuf>,
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

#[derive(Clone, Debug, Default)]
struct ConfigValues {
    target: Option<String>,
    cache_dir: Option<PathBuf>,
    target_dir: Option<PathBuf>,
    require_signature: Option<bool>,
    deny_yanked: Option<bool>,
    permission: Option<bool>,
    read_allows: Vec<PathBuf>,
    write_allows: Vec<PathBuf>,
    env_allows: Vec<String>,
    net_allows: Vec<String>,
    run_allows: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    build: BuildConfig,
    cache: CacheConfig,
    registry: RegistryConfig,
    permission: PermissionConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BuildConfig {
    target: Option<String>,
    #[serde(rename = "target-dir")]
    target_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CacheConfig {
    dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RegistryConfig {
    require_signature: Option<bool>,
    deny_yanked: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PermissionConfig {
    enabled: Option<bool>,
    #[serde(rename = "read-allows")]
    read_allows: Option<Vec<PathBuf>>,
    #[serde(rename = "write-allows")]
    write_allows: Option<Vec<PathBuf>>,
    #[serde(rename = "env-allows")]
    env_allows: Option<Vec<String>>,
    #[serde(rename = "net-allows")]
    net_allows: Option<Vec<String>>,
    #[serde(rename = "run-allows")]
    run_allows: Option<Vec<String>>,
}

impl ConfigValues {
    fn apply(&mut self, config: ConfigFile) {
        if config.build.target.is_some() {
            self.target = config.build.target;
        }
        if config.build.target_dir.is_some() {
            self.target_dir = config.build.target_dir;
        }
        if config.cache.dir.is_some() {
            self.cache_dir = config.cache.dir;
        }
        if config.registry.require_signature.is_some() {
            self.require_signature = config.registry.require_signature;
        }
        if config.registry.deny_yanked.is_some() {
            self.deny_yanked = config.registry.deny_yanked;
        }
        if config.permission.enabled.is_some() {
            self.permission = config.permission.enabled;
        }
        if let Some(values) = config.permission.read_allows {
            self.read_allows = values;
        }
        if let Some(values) = config.permission.write_allows {
            self.write_allows = values;
        }
        if let Some(values) = config.permission.env_allows {
            self.env_allows = values;
        }
        if let Some(values) = config.permission.net_allows {
            self.net_allows = values;
        }
        if let Some(values) = config.permission.run_allows {
            self.run_allows = values;
        }
    }
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

fn resolve_global(raw: &GlobalArgs) -> Result<GlobalArgs, String> {
    let (config, config_files) = load_config(&raw.config)?;
    let mut options = raw.clone();
    options.format = Some(resolve_format(raw)?);
    options.color = Some(if let Some(color) = raw.color {
        color
    } else {
        match environment_text("GUGU_COLOR")? {
            Some(value) => parse_color(&value)?,
            None => ColorMode::default(),
        }
    });
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
    options.target = raw.target.clone().or(environment_target).or(config.target);
    options.bin = raw
        .bin
        .as_ref()
        .and_then(|value| value.as_ref().cloned())
        .filter(|value| !value.is_empty())
        .map(Some);
    options.permission = raw.permission || config.permission.unwrap_or(false);
    options.read_allows = if raw.read_allows.is_empty() {
        config.read_allows
    } else {
        raw.read_allows.clone()
    };
    options.write_allows = if raw.write_allows.is_empty() {
        config.write_allows
    } else {
        raw.write_allows.clone()
    };
    options.env_allows = if raw.env_allows.is_empty() {
        config.env_allows
    } else {
        raw.env_allows.clone()
    };
    options.net_allows = if raw.net_allows.is_empty() {
        config.net_allows
    } else {
        raw.net_allows.clone()
    };
    options.run_allows = if raw.run_allows.is_empty() {
        config.run_allows
    } else {
        raw.run_allows.clone()
    };
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

fn load_config(explicit: &[PathBuf]) -> Result<(ConfigValues, Vec<PathBuf>), String> {
    let mut paths = Vec::new();
    if let Some(path) = user_config_file() {
        append_optional_config(&mut paths, path)?;
    }
    append_optional_config(&mut paths, PathBuf::from(".gugu/config.toml"))?;
    paths.extend(explicit.iter().cloned());

    let mut values = ConfigValues::default();
    for path in &paths {
        let source = fs::read_to_string(path)
            .map_err(|error| format!("无法读取配置文件 `{}`：{error}", path.display()))?;
        let config = toml::from_str::<ConfigFile>(&source)
            .map_err(|error| format!("配置文件 `{}` 格式错误：{error}", path.display()))?;
        values.apply(config);
    }
    Ok((values, paths))
}

fn append_optional_config(paths: &mut Vec<PathBuf>, path: PathBuf) -> Result<(), String> {
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => paths.push(path),
        Ok(_) => return Err(format!("配置路径 `{}` 不是文件", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("无法读取配置路径 `{}`：{error}", path.display()));
        }
    }
    Ok(())
}

fn user_config_file() -> Option<PathBuf> {
    let directory = env::var_os("GUGU_CONFIG_DIR").map(PathBuf::from);
    #[cfg(target_os = "windows")]
    let directory =
        directory.or_else(|| env::var_os("APPDATA").map(|path| PathBuf::from(path).join("gugu")));
    #[cfg(not(target_os = "windows"))]
    let directory = directory
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("gugu"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".config").join("gugu"))
        });
    directory.map(|path| path.join("config.toml"))
}

fn environment_text(name: &str) -> Result<Option<String>, String> {
    match env::var_os(name) {
        Some(value) => value
            .into_string()
            .map(Some)
            .map_err(|_| format!("环境变量 `{name}` 不是有效 UTF-8")),
        None => Ok(None),
    }
}

fn environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

fn environment_flag(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn parse_color(value: &str) -> Result<ColorMode, String> {
    match value {
        "auto" => Ok(ColorMode::Auto),
        "always" => Ok(ColorMode::Always),
        "never" => Ok(ColorMode::Never),
        _ => Err(format!(
            "未知颜色策略 `{value}`，可选值为 auto、always、never"
        )),
    }
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

fn print_version(format: OutputFormat) {
    match format {
        OutputFormat::Text => println!("gugu {}", env!("CARGO_PKG_VERSION")),
        OutputFormat::Json => println!(
            "{}",
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "commit": option_env!("GUGU_COMMIT").unwrap_or("unknown"),
                "commit-date": option_env!("GUGU_COMMIT_DATE").unwrap_or("unknown"),
                "host": TargetName::host()
                    .map(|target| target.to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
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
    match options.bin.clone().flatten() {
        Some(name) => TargetSelection::Bin(Some(name.to_owned())),
        None => TargetSelection::DefaultBuild,
    }
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
    let selected = match project.select_targets(
        options.package.as_deref(),
        options.workspace,
        &target_selection(options),
    ) {
        Ok(selected) => selected,
        Err(error) => {
            emit_cli_error(format, &error.to_string());
            return 2;
        }
    };

    let compiler = Compiler::new();
    let mut failed = false;
    for (package, package_target) in selected {
        let requires_main = matches!(package_target.kind(), TargetKind::Bin | TargetKind::Example)
            || (package_target.kind() == TargetKind::Bench && !package_target.harness());
        // 逻辑路径按 package root 推导，保证诊断位置与工作目录无关。
        let Some(logical_path) = package_target
            .entry()
            .strip_prefix(package.root())
            .ok()
            .map(PathBuf::from)
        else {
            emit_cli_error(format, "target 入口不在 package 根内");
            return 2;
        };
        let request = CompileRequest::project_entry(
            package_target.entry().to_path_buf(),
            logical_path,
            target,
            requires_main,
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
    i32::from(failed)
}

fn print_compilation(
    compilation: &Compilation,
    check_only: bool,
    options: &GlobalArgs,
    target: TargetName,
) {
    match options.format.unwrap_or_default() {
        OutputFormat::Text => print_compilation_text(compilation, check_only, options, target),
        OutputFormat::Json => print_compilation_json(compilation, check_only, options, target),
        OutputFormat::JsonDiagnosticShort => {
            for diagnostic in compilation.diagnostics().items() {
                emit_diagnostic(diagnostic, OutputFormat::JsonDiagnosticShort);
            }
        }
    }
}

fn print_compilation_text(
    compilation: &Compilation,
    check_only: bool,
    options: &GlobalArgs,
    target: TargetName,
) {
    if options.verbose {
        println!("target: {target}");
    }
    if !options.quiet {
        for node in compilation.action_graph().nodes() {
            println!(
                "action {:02} {:<16} {}",
                node.id(),
                node.kind(),
                node.status()
            );
        }
    }
    for diagnostic in compilation.diagnostics().items() {
        let rendered = diagnostic.render_text();
        if should_color(options.color.unwrap_or_default()) {
            eprintln!("\x1b[1;31m{rendered}\x1b[0m");
        } else {
            eprintln!("{rendered}");
        }
    }
    if let Some(plan) = compilation.image_plan() {
        if check_only {
            println!("check succeeded: {}", plan.target());
        } else {
            println!(
                "bootstrap plan ready: target={}, entry={}, image not emitted",
                plan.target(),
                plan.entry()
            );
        }
    } else if compilation.diagnostics().has_errors() {
        println!("{} failed", if check_only { "check" } else { "build" });
    } else if options.quiet {
        println!(
            "{} succeeded: no executable entry",
            if check_only { "check" } else { "build" }
        );
    }
}

fn print_compilation_json(
    compilation: &Compilation,
    check_only: bool,
    options: &GlobalArgs,
    target: TargetName,
) {
    let command = if check_only { "check" } else { "build" };
    if !options.quiet {
        emit_event(
            "build-start",
            json!({ "command": command, "target": target.to_string() }),
        );
    }
    for diagnostic in compilation.diagnostics().items() {
        emit_diagnostic(diagnostic, OutputFormat::Json);
    }
    let image_plan = compilation.image_plan().map(|plan| {
        json!({
            "target": plan.target().to_string(),
            "entry": plan.entry(),
            "function-count": plan.function_count(),
            "runtime-source-count": plan.runtime_source_count(),
            "rt0": plan.rt0().to_string()
        })
    });
    emit_event(
        "build-finish",
        json!({
            "command": command,
            "target": target.to_string(),
            "success": compilation.is_success(),
            "check-only": check_only,
            "image-plan": image_plan
        }),
    );
}

fn emit_diagnostic(diagnostic: &gugu_compiler::Diagnostic, format: OutputFormat) {
    let span = diagnostic.span();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let payload = json!({
        "file": span.map(|span| sanitize_path(span.path(), &cwd)),
        "line": span.map_or(0, |span| span.line()),
        "column": span.map_or(0, |span| span.column()),
        "severity": diagnostic.severity().to_string(),
        "code": diagnostic.code().to_string(),
        "message": sanitize_message(diagnostic.message(), span.map(|span| span.path()), &cwd),
        "suggestion": Value::Null
    });
    if format == OutputFormat::Json || format == OutputFormat::JsonDiagnosticShort {
        emit_event("compiler-diagnostic", payload);
    }
}

fn emit_cli_error(format: OutputFormat, message: &str) {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let message = sanitize_message(message, None, &cwd);
    match format {
        OutputFormat::Text => eprintln!("error: {message}"),
        OutputFormat::Json | OutputFormat::JsonDiagnosticShort => {
            emit_event(
                "cli-error",
                json!({ "message": message, "suggestion": Value::Null }),
            );
        }
    }
}

fn emit_event(reason: &str, payload: Value) {
    println!("{}", json!({ "reason": reason, "payload": payload }));
}

fn should_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Auto => std::io::stderr().is_terminal(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
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

fn sanitize_path(path: &Path, cwd: &Path) -> String {
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(cwd) {
            return normalize_logical_path(relative);
        }
        return external_path(path);
    }
    normalize_logical_path(path)
}

fn normalize_logical_path(path: &Path) -> String {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if components.pop().is_none() {
                    return external_path(path);
                }
            }
            Component::CurDir | Component::Prefix(_) | Component::RootDir => {}
        }
    }
    if components.is_empty() {
        "<source>".to_owned()
    } else {
        components.join("/")
    }
}

fn external_path(path: &Path) -> String {
    path.file_name()
        .map(|name| format!("<external>/{}", name.to_string_lossy()))
        .unwrap_or_else(|| "<external>".to_owned())
}

fn sanitize_message(message: &str, source_path: Option<&Path>, cwd: &Path) -> String {
    let mut clean = message.to_owned();
    if let Some(path) = source_path {
        let displayed_path = path.display().to_string();
        clean = clean.replace(&displayed_path, &sanitize_path(path, cwd));
    }
    let displayed_cwd = cwd.display().to_string();
    clean = clean.replace(&displayed_cwd, ".");
    redact_absolute_path_tokens(&redact_secrets(&clean))
}

fn redact_absolute_path_tokens(message: &str) -> String {
    message
        .split_whitespace()
        .map(|token| {
            let trimmed = token.trim_matches(|character: char| "`'\"(),;".contains(character));
            if trimmed.starts_with('/') || is_windows_absolute(trimmed) {
                "<path>".to_owned()
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn redact_secrets(message: &str) -> String {
    const SECRET_KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "authorization",
        "api_key",
        "apikey",
        "password",
        "secret",
        "token",
    ];
    let lowercase = message.to_ascii_lowercase();
    let bytes = lowercase.as_bytes();
    let mut ranges = Vec::new();
    for key in SECRET_KEYS {
        let mut search_start = 0;
        while let Some(relative) = lowercase[search_start..].find(key) {
            let start = search_start + relative;
            let end_key = start + key.len();
            let boundary_before =
                start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
            let boundary_after = end_key == bytes.len()
                || !bytes[end_key].is_ascii_alphanumeric() && bytes[end_key] != b'_';
            if boundary_before && boundary_after {
                let mut cursor = end_key;
                while bytes.get(cursor) == Some(&b' ') || bytes.get(cursor) == Some(&b'\t') {
                    cursor += 1;
                }
                if matches!(bytes.get(cursor), Some(b'=') | Some(b':')) {
                    cursor += 1;
                    while bytes.get(cursor) == Some(&b' ') || bytes.get(cursor) == Some(&b'\t') {
                        cursor += 1;
                    }
                    if cursor < bytes.len() {
                        let value_start = cursor;
                        if bytes[cursor] == b'\'' || bytes[cursor] == b'\"' {
                            let quote = bytes[cursor];
                            cursor += 1;
                            while cursor < bytes.len() && bytes[cursor] != quote {
                                cursor += 1;
                            }
                            if cursor < bytes.len() {
                                cursor += 1;
                            }
                        } else {
                            while cursor < bytes.len()
                                && !bytes[cursor].is_ascii_whitespace()
                                && !matches!(bytes[cursor], b',' | b';' | b'}')
                            {
                                cursor += 1;
                            }
                        }
                        if value_start < cursor {
                            ranges.push((value_start, cursor));
                        }
                    }
                }
            }
            search_start = end_key;
        }
    }
    ranges.sort_unstable_by_key(|(start, _)| *start);
    ranges.dedup();
    let mut redacted = message.to_owned();
    for (start, end) in ranges.into_iter().rev() {
        redacted.replace_range(start..end, "<redacted>");
    }
    redacted
}

#[cfg(test)]
fn global_from_values(mut raw: GlobalArgs, config: ConfigValues) -> Result<GlobalArgs, String> {
    raw.format = Some(raw.format.unwrap_or_default());
    raw.color = Some(raw.color.unwrap_or_default());
    raw.offline = raw.frozen || raw.offline;
    raw.locked = raw.frozen || raw.locked;
    raw.require_signature = raw.require_signature || config.require_signature.unwrap_or(false);
    raw.deny_yanked = raw.deny_yanked || config.deny_yanked.unwrap_or(false);
    raw.target = raw.target.or(config.target);
    raw.bin = raw
        .bin
        .and_then(|value| value)
        .filter(|value| !value.is_empty())
        .map(Some);
    raw.permission = raw.permission || config.permission.unwrap_or(false);
    raw.read_allows = if raw.read_allows.is_empty() {
        config.read_allows
    } else {
        raw.read_allows
    };
    raw.write_allows = if raw.write_allows.is_empty() {
        config.write_allows
    } else {
        raw.write_allows
    };
    raw.env_allows = if raw.env_allows.is_empty() {
        config.env_allows
    } else {
        raw.env_allows
    };
    raw.net_allows = if raw.net_allows.is_empty() {
        config.net_allows
    } else {
        raw.net_allows
    };
    raw.run_allows = if raw.run_allows.is_empty() {
        config.run_allows
    } else {
        raw.run_allows
    };
    raw.cache_dir = raw.cache_dir.or(config.cache_dir);
    raw.target_dir = raw.target_dir.or(config.target_dir);
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, ConfigFile, ConfigValues, GlobalArgs, OutputFormat, format_hint, redact_secrets,
        sanitize_path,
    };
    use clap::Parser;
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
    };

    #[test]
    fn no_subcommand_and_version_are_registered() {
        assert!(
            Cli::try_parse_from(["gugu"])
                .expect("root help is handled at runtime")
                .command
                .is_none()
        );
        assert!(matches!(
            Cli::try_parse_from(["gugu", "version"])
                .expect("version command parses")
                .command,
            Some(super::Command::Version)
        ));
    }

    #[test]
    fn version_flag_is_parsed_without_clap_early_exit() {
        let cli = Cli::try_parse_from(["gugu", "--version"]).expect("version flag parses");
        assert!(cli.version_flag);
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_command_table_accepts_registered_commands() {
        let cases = [
            vec!["gugu", "new", "demo"],
            vec!["gugu", "new", "demo", "--bin"],
            vec!["gugu", "init"],
            vec!["gugu", "build"],
            vec!["gugu", "check"],
            vec!["gugu", "run"],
            vec!["gugu", "test"],
            vec!["gugu", "bench"],
            vec!["gugu", "fmt"],
            vec!["gugu", "doc"],
            vec!["gugu", "clean"],
            vec!["gugu", "add", "acme/json"],
            vec!["gugu", "remove", "acme/json"],
            vec!["gugu", "update"],
            vec!["gugu", "tree"],
            vec!["gugu", "vendor"],
            vec!["gugu", "package"],
            vec!["gugu", "publish"],
            vec!["gugu", "yank", "acme/json", "--version", "1.0.0"],
            vec!["gugu", "login", "public"],
            vec!["gugu", "cache", "dir"],
            vec!["gugu", "explain", "E0001"],
            vec!["gugu", "version"],
            vec!["gugu", "help", "build"],
        ];
        for arguments in cases {
            Cli::try_parse_from(arguments).expect("command table entry parses");
        }
    }

    #[test]
    fn global_options_parse_before_the_subcommand() {
        let cli = Cli::try_parse_from([
            "gugu",
            "--format",
            "json",
            "--color",
            "never",
            "--offline",
            "--locked",
            "--frozen",
            "--vendor",
            "--require-signature",
            "--deny-yanked",
            "--target",
            "x86_64-linux",
            "--package",
            "acme/app",
            "--workspace",
            "--lib",
            "--strip",
            "--permission",
            "-A",
            "--read-allows",
            "src/**",
            "--write-allows",
            "out/**",
            "--env-allows",
            "HOME",
            "--net-allows",
            "registry.example",
            "--run-allows",
            "tool",
            "--config",
            "config.toml",
            "--cache-dir",
            "cache",
            "--target-dir",
            "target",
            "build",
        ])
        .expect("global options parse");
        assert_eq!(cli.global.format, Some(OutputFormat::Json));
        assert_eq!(cli.global.color, Some(super::ColorMode::Never));
        assert!(cli.global.offline);
        assert!(cli.global.locked);
        assert!(cli.global.frozen);
        assert_eq!(cli.global.target.as_deref(), Some("x86_64-linux"));
        assert_eq!(cli.global.package.as_deref(), Some("acme/app"));
        assert!(cli.global.workspace);
        assert!(cli.global.lib);
        assert!(cli.global.allow_all);
        assert!(matches!(
            cli.command,
            Some(super::Command::Build { file: None })
        ));
    }

    #[test]
    fn global_options_parse_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "gugu",
            "build",
            "--format",
            "json",
            "--target",
            "x86_64-windows",
            "--quiet",
        ])
        .expect("trailing global options parse");
        assert_eq!(cli.global.format, Some(OutputFormat::Json));
        assert_eq!(cli.global.target.as_deref(), Some("x86_64-windows"));
        assert!(cli.global.quiet);
    }

    #[test]
    fn global_cli_values_override_config_values() {
        let mut config = ConfigValues::default();
        config.apply(ConfigFile {
            build: super::BuildConfig {
                target: Some("x86_64-windows".to_owned()),
                target_dir: Some(PathBuf::from("config-target")),
            },
            cache: super::CacheConfig {
                dir: Some(PathBuf::from("config-cache")),
            },
            ..ConfigFile::default()
        });
        let raw = GlobalArgs {
            target: Some("x86_64-linux".to_owned()),
            cache_dir: Some(PathBuf::from("cli-cache")),
            ..GlobalArgs::default()
        };
        let options = super::global_from_values(raw, config).expect("options resolve");
        assert_eq!(options.target.as_deref(), Some("x86_64-linux"));
        assert_eq!(options.cache_dir, Some(PathBuf::from("cli-cache")));
        assert_eq!(options.target_dir, Some(PathBuf::from("config-target")));
    }

    #[test]
    fn config_toml_fields_map_and_preserve_lower_layers() {
        let config = toml::from_str::<ConfigFile>(
            "[build]\ntarget = \"x86_64-linux\"\n[permission]\nread-allows = [\"src/**\"]\n",
        )
        .expect("config TOML parses");
        let mut values = ConfigValues::default();
        values.apply(config);
        values.apply(ConfigFile {
            build: super::BuildConfig {
                target: Some("x86_64-windows".to_owned()),
                ..super::BuildConfig::default()
            },
            ..ConfigFile::default()
        });
        assert_eq!(values.target.as_deref(), Some("x86_64-windows"));
        assert_eq!(values.read_allows, [PathBuf::from("src/**")]);
    }

    #[test]
    fn json_path_and_secret_redaction_are_not_absolute_or_sensitive() {
        let cwd = Path::new("/workspace/project");
        assert_eq!(
            sanitize_path(Path::new("/workspace/project/src/main.gg"), cwd),
            "src/main.gg"
        );
        let message = redact_secrets("token=abc123 password: \"hunter2\"");
        assert_eq!(message, "token=<redacted> password: <redacted>");
    }

    #[test]
    fn output_format_values_are_closed() {
        assert_eq!(
            OutputFormat::parse("json").expect("json format"),
            OutputFormat::Json
        );
        assert!(OutputFormat::parse("yaml").is_err());
    }

    #[test]
    fn format_hint_follows_explicit_format() {
        let arguments = ["gugu", "--format", "json", "build"].map(OsString::from);
        assert_eq!(format_hint(&arguments), OutputFormat::Json);
    }

    #[test]
    fn resolve_global_reads_cli_precedence_without_environment() {
        let raw = GlobalArgs {
            format: Some(OutputFormat::Json),
            ..GlobalArgs::default()
        };
        let options =
            super::global_from_values(raw, ConfigValues::default()).expect("options resolve");
        assert_eq!(options.format, Some(OutputFormat::Json));
    }

    #[test]
    fn single_file_mode_rejects_project_selectors() {
        let base = || GlobalArgs {
            format: Some(OutputFormat::Text),
            ..GlobalArgs::default()
        };

        // 纯单文件输入不触发任何冲突。
        assert!(super::single_file_mode_conflict(&base()).is_none());

        // 项目专属参数与单文件互斥。
        let mut with_package = base();
        with_package.package = Some("demo".to_owned());
        assert_eq!(
            super::single_file_mode_conflict(&with_package).as_deref(),
            Some("单文件编译模式不支持参数：-p/--package")
        );

        let mut with_workspace = base();
        with_workspace.workspace = true;
        assert_eq!(
            super::single_file_mode_conflict(&with_workspace).as_deref(),
            Some("单文件编译模式不支持参数：--workspace")
        );

        // 多个冲突参数按声明顺序聚合列出。
        let mut combined = base();
        combined.lib = true;
        combined.all_targets = true;
        assert_eq!(
            super::single_file_mode_conflict(&combined).as_deref(),
            Some("单文件编译模式不支持参数：--lib、--all-targets")
        );
    }
}
