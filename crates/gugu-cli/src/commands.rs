//! 其余子命令的端到端编排。
//!
//! 所有命令复用与 `build` 相同的项目发现、锁解析、build task、
//! 权限门与物化路径；退出码遵循规范：0 成功、1 目标操作失败、
//! 2 用法错误、101 内部错误。

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use gugu_compiler::{
    Compiler, DependencyInput, EMBEDDED_ARTIFACTS, LockGraph, Package, Project, Target, TargetKind,
    TargetName, TargetSelection,
};
use serde_json::json;

use crate::{
    Command, GlobalArgs, RunDomain, compile_package, enabled_features, materialize,
    output::{OutputFormat, emit_cli_error},
    resolve_project_lock,
};

/// 子命令统一入口；所有分支要么返回规范退出码，要么提前 emit 错误。
pub(crate) fn dispatch(command: &Command, options: &GlobalArgs) -> i32 {
    match command {
        Command::New { path } => run_new(path, options),
        Command::Init => run_init(options),
        Command::Run { target_name, args } => run_run(target_name.as_deref(), args, options),
        Command::Test { args } => run_test_or_bench(true, args, options),
        Command::Bench { args } => run_test_or_bench(false, args, options),
        Command::Add {
            package_name,
            dev,
            build,
            default_features,
            optional,
            path,
            git,
            rev,
            branch,
            tag,
        } => crate::package_cmds::run_add(
            package_name,
            *dev,
            *build,
            *default_features,
            *optional,
            path.as_deref(),
            git.as_deref(),
            rev.as_deref(),
            branch.as_deref(),
            tag.as_deref(),
            options,
        ),
        Command::Remove { package_name } => crate::package_cmds::run_remove(package_name, options),
        Command::Update { precise } => crate::package_cmds::run_update(precise.as_deref(), options),
        Command::Tree {
            depth,
            duplicates,
            invert,
        } => crate::package_cmds::run_tree(*depth, *duplicates, invert.as_deref(), options),
        Command::Vendor => crate::package_cmds::run_vendor(options),
        Command::Package => crate::package_cmds::run_package(options),
        Command::Publish { dry_run, .. } => {
            let _ = dry_run;
            crate::package_cmds::pending_registry(options, "publish")
        }
        Command::Yank { .. } => crate::package_cmds::pending_registry(options, "yank"),
        Command::Login {
            registry,
            token_stdin,
        } => crate::package_cmds::run_login(registry, *token_stdin, options),
        Command::Explain { code } => crate::package_cmds::run_explain(code, options),
        _ => unreachable!("dispatch 只接收未在 execute 提前分支的命令"),
    }
}

pub(crate) fn format_of(options: &GlobalArgs) -> OutputFormat {
    options.format.unwrap_or_default()
}

pub(crate) fn fail(format: OutputFormat, code: i32, message: impl std::fmt::Display) -> i32 {
    emit_cli_error(format, &message.to_string());
    code
}

// ---------------------------------------------------------------------------
// new / init
// ---------------------------------------------------------------------------

/// 目录名转合法 package 名：小写字母、数字、`-`、`_`。
fn package_name_from(path: &Path) -> Option<String> {
    let base = path.file_name()?.to_str()?;
    let name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_matches('_');
    if name.is_empty() || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(name.to_owned())
}

/// 生成 package 骨架（`--bin`/`--lib`/`--workspace`）。
fn scaffold(dir: &Path, options: &GlobalArgs) -> Result<Vec<PathBuf>, String> {
    let name = package_name_from(dir)
        .ok_or_else(|| format!("无法从目录名推导合法 package 名：`{}`", dir.display()))?;
    let mut written = Vec::new();
    if options.workspace {
        fs::create_dir_all(dir).map_err(|e| format!("创建 `{}` 失败：{e}", dir.display()))?;
        let manifest = dir.join("gugu.toml");
        fs::write(
            &manifest,
            "# Gugu workspace\n[workspace]\nmembers = [\"*\"]\n",
        )
        .map_err(|e| format!("写入 `{}` 失败：{e}", manifest.display()))?;
        written.push(manifest);
        return Ok(written);
    }
    let src = dir.join("src");
    fs::create_dir_all(&src).map_err(|e| format!("创建 `{}` 失败：{e}", src.display()))?;
    let manifest = dir.join("gugu.toml");
    fs::write(
        &manifest,
        format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\n"),
    )
    .map_err(|e| format!("写入 `{}` 失败：{e}", manifest.display()))?;
    written.push(manifest);
    let (entry, body) = if options.lib {
        (src.join("lib.gg"), "fn answer() int {\n    return 42\n}\n")
    } else {
        (src.join("main.gg"), "fn main() {\n}\n")
    };
    fs::write(&entry, body).map_err(|e| format!("写入 `{}` 失败：{e}", entry.display()))?;
    written.push(entry);
    Ok(written)
}

fn run_new(path: &Path, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    if path.exists() && path.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        return fail(
            format,
            2,
            format!("目标目录已存在且非空：`{}`", path.display()),
        );
    }
    match scaffold(path, options) {
        Ok(written) => {
            if format == OutputFormat::Text {
                for file in written {
                    println!("created {}", file.display());
                }
            } else {
                println!("{}", json!({"kind": "scaffold", "result": "ok"}));
            }
            0
        }
        Err(message) => fail(format, 2, message),
    }
}

fn run_init(options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("gugu.toml").exists() {
        return fail(format, 2, "当前目录已存在 `gugu.toml`");
    }
    match scaffold(&cwd, options) {
        Ok(_) => 0,
        Err(message) => fail(format, 2, message),
    }
}

// ---------------------------------------------------------------------------
// 项目装载（run / test / bench / tree / vendor / update 共用）
// ---------------------------------------------------------------------------

pub(crate) struct ProjectContext {
    pub(crate) project: Project,
    pub(crate) lock: LockGraph,
    pub(crate) inputs: Vec<DependencyInput>,
}

pub(crate) fn discover(options: &GlobalArgs) -> Result<(Project, Vec<Package>), i32> {
    let format = format_of(options);
    let start = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = Project::discover(start).map_err(|error| fail(format, 2, error.to_string()))?;
    let selected: Vec<Package> = project
        .select_packages(options.package.as_deref(), options.workspace)
        .map_err(|error| fail(format, 2, error.to_string()))?
        .into_iter()
        .cloned()
        .collect();
    Ok((project, selected))
}

pub(crate) fn load_project(
    options: &GlobalArgs,
    target: TargetName,
) -> Result<(ProjectContext, Vec<Package>), i32> {
    let format = format_of(options);
    let (project, packages) = discover(options)?;
    let refs = packages.iter().collect::<Vec<_>>();
    let (lock, inputs) =
        resolve_project_lock(&project, &refs, options, target, format).map_err(|()| 2)?;
    Ok((
        ProjectContext {
            project,
            lock,
            inputs,
        },
        packages,
    ))
}

pub(crate) fn parse_target_or_exit(options: &GlobalArgs) -> Result<TargetName, i32> {
    let format = format_of(options);
    match options.target.as_deref() {
        Some(name) => TargetName::parse(name).map_err(|e| fail(format, 2, e.to_string())),
        None => TargetName::host().ok_or_else(|| fail(format, 2, "当前宿主不是已登记的 Gugu 目标")),
    }
}

fn host_only(options: &GlobalArgs, target: TargetName) -> Result<(), i32> {
    let host = TargetName::host();
    if Some(target) == host {
        return Ok(());
    }
    Err(fail(
        format_of(options),
        2,
        format!("可执行子命令只能在宿主目标上运行，`{target}` 不是宿主"),
    ))
}

// ---------------------------------------------------------------------------
// run / test / bench：编译 + 执行
// ---------------------------------------------------------------------------

/// 运行编译完成的 image 并透传退出码。
fn spawn_image(path: &Path, args: &[OsString], format: OutputFormat) -> i32 {
    // test/bench 域下 lib target 的产物也是可执行镜像，但物化按 Lib 权限写盘，
    // 执行前在此补执行位。
    if materialize::make_executable(path, format).is_err() {
        return 1;
    }
    let status = std::process::Command::new(path)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => fail(
            format,
            1,
            format!("执行 `{}` 失败：{error}", path.display()),
        ),
    }
}

fn passthrough_args(args: &[OsString]) -> &[OsString] {
    match args.first().and_then(|arg| arg.to_str()) {
        Some("--") => &args[1..],
        _ => args,
    }
}

/// 编译选定 target 集并逐个执行产物；返回最大退出码。
#[expect(clippy::too_many_arguments, reason = "编排需要完整的编译与执行上下文")]
fn compile_and_exec(
    context: &ProjectContext,
    packages: &[Package],
    options: &GlobalArgs,
    target: TargetName,
    selection: &TargetSelection,
    domain: RunDomain,
    exec_kinds: &[TargetKind],
    args: &[OsString],
) -> i32 {
    let format = format_of(options);
    let root = context.project.workspace().root().to_path_buf();
    let target_root = options
        .target_dir
        .clone()
        .unwrap_or_else(|| root.join("target"));
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    let mut written = BTreeSet::new();
    let mut exit_code = 0;
    for package in packages {
        let features = enabled_features(options, package);
        let selected = match package.select_targets_in(selection, &features) {
            Ok(selected) => selected,
            Err(error) => return fail(format, 2, error.to_string()),
        };
        let compiled = compile_package(
            &root,
            package,
            options,
            target,
            false,
            &compiler,
            &context.lock,
            format,
            &mut written,
            selection,
            domain,
        );
        let compile_code = match compiled {
            Ok(code) => code,
            Err(()) => return 2,
        };
        if compile_code != 0 {
            return compile_code;
        }
        for package_target in selected {
            if !exec_kinds.contains(&package_target.kind()) {
                continue;
            }
            // 重新编译结果不在此缓存；artifact_path 只按命名规则推导路径。
            let Some(path) = materialize::artifact_path(&target_root, target, package_target)
            else {
                continue;
            };
            if !path.is_file() {
                return fail(
                    format,
                    1,
                    format!("期望的可执行产物不存在：`{}`", path.display()),
                );
            }
            let child = spawn_image(&path, passthrough_args(args), format);
            exit_code = exit_code.max(child);
        }
    }
    exit_code
}

fn default_run_name(package: &Package) -> Option<String> {
    let text = fs::read_to_string(package.manifest()).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("package")?
        .get("default-run")?
        .as_str()
        .map(str::to_owned)
}

/// 解析 run 的 bin：显式参数 > `--bin` > default-run > 唯一 bin。
fn resolve_run_name(
    package: &Package,
    positional: Option<&str>,
    options: &GlobalArgs,
) -> Result<String, String> {
    if let Some(name) = positional {
        return Ok(name.to_owned());
    }
    if let Some(Some(name)) = &options.bin {
        return Ok(name.clone());
    }
    let bins: Vec<&Target> = package
        .targets()
        .iter()
        .filter(|target| target.kind() == TargetKind::Bin)
        .collect();
    if let Some(name) = default_run_name(package) {
        if bins.iter().any(|target| target.name() == name) {
            return Ok(name);
        }
        return Err(format!("default-run `{name}` 不是已发现的 bin target"));
    }
    match bins.len() {
        1 => Ok(bins[0].name().to_owned()),
        0 => Err(format!(
            "package `{}` 没有 bin target",
            package.package_name()
        )),
        _ => Err(format!(
            "package `{}` 有多个 bin target，请用 `gugu run <name>` 指定",
            package.package_name()
        )),
    }
}

fn run_run(positional: Option<&str>, args: &[OsString], options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let target = match parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if let Err(code) = host_only(options, target) {
        return code;
    }
    let (context, packages) = match load_project(options, target) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if packages.len() != 1 {
        return fail(
            format,
            2,
            "gugu run 需要唯一 package；workspace 请用 -p 指定",
        );
    }
    let name = match resolve_run_name(&packages[0], positional, options) {
        Ok(name) => name,
        Err(message) => return fail(format, 2, message),
    };
    compile_and_exec(
        &context,
        &packages,
        options,
        target,
        &TargetSelection::Bin(Some(name)),
        RunDomain::Default,
        &[TargetKind::Bin],
        args,
    )
}

fn run_test_or_bench(test: bool, args: &[OsString], options: &GlobalArgs) -> i32 {
    let target = match parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if let Err(code) = host_only(options, target) {
        return code;
    }
    let (context, packages) = match load_project(options, target) {
        Ok(value) => value,
        Err(code) => return code,
    };
    let (selection, domain, kinds) = if test {
        if options.lib {
            (TargetSelection::Lib, RunDomain::Test, vec![TargetKind::Lib])
        } else if options.all_targets {
            (
                TargetSelection::All,
                RunDomain::Default,
                vec![TargetKind::Test],
            )
        } else {
            (
                TargetSelection::Test(options.test.clone()),
                RunDomain::Default,
                vec![TargetKind::Test],
            )
        }
    } else if options.lib {
        (
            TargetSelection::Lib,
            RunDomain::Bench,
            vec![TargetKind::Lib],
        )
    } else if options.all_targets {
        (
            TargetSelection::All,
            RunDomain::Default,
            vec![TargetKind::Bench],
        )
    } else {
        (
            TargetSelection::Bench(options.bench.clone()),
            RunDomain::Default,
            vec![TargetKind::Bench],
        )
    };
    compile_and_exec(
        &context, &packages, options, target, &selection, domain, &kinds, args,
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn package_name_sanitizes_dir_basename() {
        assert_eq!(
            package_name_from(Path::new("/tmp/hello-world")),
            Some("hello_world".to_owned())
        );
        assert_eq!(
            package_name_from(Path::new("/tmp/My.Crate")),
            Some("my_crate".to_owned())
        );
        assert_eq!(package_name_from(Path::new("/tmp/9lives")), None);
        assert_eq!(package_name_from(Path::new("/tmp/---")), None);
    }

    #[test]
    fn passthrough_strips_leading_separator() {
        let args: Vec<OsString> = ["--", "-v", "x"].iter().map(OsString::from).collect();
        assert_eq!(
            passthrough_args(&args),
            [OsString::from("-v"), OsString::from("x")]
        );
        let bare: Vec<OsString> = ["x"].iter().map(OsString::from).collect();
        assert_eq!(passthrough_args(&bare), [OsString::from("x")]);
    }

    #[test]
    fn scaffold_writes_bin_manifest() {
        let dir = std::env::temp_dir().join(format!("gugu-scaffold-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut options = GlobalArgs::default();
        options.lib = false;
        options.workspace = false;
        let written = scaffold(&dir, &options).unwrap();
        let manifest = fs::read_to_string(dir.join("gugu.toml")).unwrap();
        assert!(manifest.contains("[package]"));
        assert!(
            manifest.contains(
                &dir.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
                    .replace('-', "_")
                    .to_lowercase()
            )
        );
        assert!(written.iter().any(|path| path.ends_with("main.gg")));
        let _ = fs::remove_dir_all(&dir);
    }
}
