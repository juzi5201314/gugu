//! build task 宿主：编译 build target 为 host 镜像、执行、回收动作记录。
//!
//! 布局：`target/build/<pkg>-<key>/{bin,record,inputs}`；`out_dir` 是
//! `target/build/<pkg>-<key>/out`。输入指纹命中即复用上轮记录，不再执行。

use std::{
    collections::BTreeMap,
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
};

use gugu_compiler::{
    CompileRequest, Compiler, NativeImport, NativeLink, Package, TargetKind, TargetName,
};

use crate::output::emit_cli_error;
use crate::{GlobalArgs, OutputFormat};

/// FNV-1a 64 位：记录文件指纹只与本二进制写出的值比较，无需密码学强度。
fn hash64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) struct BuildTaskOutput {
    pub defines: BTreeMap<String, Option<String>>,
    pub modules: Vec<(String, String)>,
    pub imports: Vec<NativeImport>,
}

fn build_dir(workspace_root: &Path, options: &GlobalArgs, package: &Package) -> PathBuf {
    options
        .target_dir
        .clone()
        .unwrap_or_else(|| workspace_root.join("target"))
        .join("build")
        .join(format!("{}-{}", package.package_name(), short_key(package)))
}

fn short_key(package: &Package) -> String {
    format!(
        "{:016x}",
        hash64(package.manifest().to_string_lossy().as_bytes())
    )
}

fn collect_inputs(package: &Package, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(package.root()) {
        let mut stack: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        while let Some(path) = stack.pop() {
            let name = path.file_name().unwrap_or_default();
            if name == "target" || name == ".git" {
                continue;
            }
            if path.is_dir() {
                if let Ok(children) = fs::read_dir(&path) {
                    stack.extend(children.flatten().map(|e| e.path()));
                }
            } else if path.is_file() {
                files.push(path);
            }
        }
        files.sort();
    }
}

fn hash_file(path: &Path) -> u64 {
    fs::read(path)
        .map(|bytes| hash64(&bytes))
        .unwrap_or(u64::MAX)
}

/// 上轮声明的 rerun 输入；文件不存在或 env 变化即失配。
fn fingerprint(
    package: &Package,
    features: &[String],
    host: &str,
    target: &str,
    rerun_files: &[String],
    rerun_envs: &[String],
) -> String {
    let mut hasher = Vec::<u8>::new();
    hasher.extend_from_slice(&hash_file(&package.manifest()).to_le_bytes());
    hasher.extend_from_slice(&hash_file(&package.root().join("build.gg")).to_le_bytes());
    hasher.extend_from_slice(features.join(",").as_bytes());
    hasher.extend_from_slice(host.as_bytes());
    hasher.extend_from_slice(target.as_bytes());
    if rerun_files.is_empty() && rerun_envs.is_empty() {
        let mut files = Vec::new();
        collect_inputs(package, &mut files);
        for file in files {
            hasher.extend_from_slice(&hash_file(&file).to_le_bytes());
        }
    } else {
        for pattern in rerun_files {
            let path = package.root().join(pattern);
            if pattern.contains('*') {
                for matched in glob_matches(package.root(), pattern) {
                    hasher.extend_from_slice(&hash_file(&matched).to_le_bytes());
                }
            } else {
                hasher.extend_from_slice(&hash_file(&path).to_le_bytes());
            }
        }
        for name in rerun_envs {
            let value = std::env::var(name).unwrap_or_default();
            hasher.extend_from_slice(name.as_bytes());
            hasher.extend_from_slice(value.as_bytes());
        }
    }
    format!("{:016x}", hash64(&hasher))
}

fn glob_matches(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut all = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        let mut stack: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default();
                if name == "target" || name == ".git" {
                    continue;
                }
                if let Ok(children) = fs::read_dir(&path) {
                    stack.extend(children.flatten().map(|e| e.path()));
                }
            } else if path.is_file() {
                all.push(path);
            }
        }
    }
    all.sort();
    all.into_iter()
        .filter(|path| {
            path.strip_prefix(root)
                .ok()
                .and_then(|rel| rel.to_str())
                .is_some_and(|rel| glob_match(pattern, rel))
        })
        .collect()
}

fn glob_match(pattern: &str, text: &str) -> bool {
    // 单 `*` 通配任意后缀段；与运行时 permission 的 glob 一致。
    match pattern.find('*') {
        None => pattern == text,
        Some(at) => {
            let (prefix, suffix) = pattern.split_at(at);
            let suffix = &suffix[1..];
            text.len() >= prefix.len() + suffix.len()
                && text.starts_with(prefix)
                && text.ends_with(suffix)
                && !suffix.contains('*')
        }
    }
}

struct Record {
    defines: BTreeMap<String, Option<String>>,
    emits: Vec<(String, String)>,
    imports: Vec<NativeImport>,
    warnings: Vec<String>,
    errors: Vec<String>,
    denied: Vec<String>,
    rerun_files: Vec<String>,
    rerun_envs: Vec<String>,
    rerun_always: bool,
}

fn parse_record(text: &str, out_dir: &Path) -> Record {
    let mut record = Record {
        defines: BTreeMap::new(),
        emits: Vec::new(),
        imports: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
        denied: Vec::new(),
        rerun_files: Vec::new(),
        rerun_envs: Vec::new(),
        rerun_always: false,
    };
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["rerun-file", pattern] => record.rerun_files.push((*pattern).to_owned()),
            ["rerun-env", name, ..] => record.rerun_envs.push((*name).to_owned()),
            ["rerun-always"] => record.rerun_always = true,
            ["emit", name, path] => {
                record.emits.push((
                    (*name).to_owned(),
                    out_dir.join(path).to_string_lossy().into_owned(),
                ));
            }
            ["define", name] => {
                record.defines.insert((*name).to_owned(), None);
            }
            ["define", name, value] => {
                record
                    .defines
                    .insert((*name).to_owned(), Some((*value).to_owned()));
            }
            ["link", _kind, name, _path] => {
                // 符号占位导入：只为库名生成 DT_NEEDED 条目；不引用即不解析。
                record.imports.push(NativeImport::data(*name, *name));
            }
            ["link-search", _path] => {}
            ["warning", message] => record.warnings.push((*message).to_owned()),
            ["error", message] => record.errors.push((*message).to_owned()),
            ["denied", op, value] => record.denied.push(format!("{op}: {value}")),
            _ => {}
        }
    }
    record
}

/// 编译、（必要时）执行 build task，回收集合注入后续 target 编译。
///
/// 返回 `None` 表示 package 没有 build target。
pub(crate) fn run_build_task(
    workspace_root: &Path,
    package: &Package,
    compiler: &Compiler,
    options: &GlobalArgs,
    target: TargetName,
    features: &[String],
    format: OutputFormat,
    externals: std::collections::BTreeSet<String>,
    identity: String,
) -> Result<Option<BuildTaskOutput>, ()> {
    let Some(build_target) = package
        .targets()
        .iter()
        .find(|t| t.kind() == TargetKind::Build)
    else {
        return Ok(None);
    };
    let host = TargetName::host().unwrap_or(target);
    let dir = build_dir(workspace_root, options, package);
    let out_dir = dir.join("out");
    let record_path = dir.join("record");
    let inputs_path = dir.join("inputs");

    fs::create_dir_all(&out_dir).map_err(|error| {
        emit_cli_error(format, &format!("无法创建 build task 目录: {error}"));
    })?;

    let record = parse_record(
        &fs::read_to_string(&record_path).unwrap_or_default(),
        &out_dir,
    );
    let current = fingerprint(
        package,
        features,
        &host.to_string(),
        &target.to_string(),
        &record.rerun_files,
        &record.rerun_envs,
    );
    let fresh = !record.rerun_always
        && fs::read_to_string(&inputs_path)
            .map(|stored| stored.trim() == current)
            .unwrap_or(false);

    if !fresh {
        // 编译 build target 为 host 镜像。
        let request = CompileRequest::project_target(
            package,
            build_target,
            identity,
            host,
            features.to_vec(),
            externals,
        );
        let compilation = compiler.compile(request);
        if !compilation.is_success() {
            crate::print_compilation(&compilation, false, options, host);
            emit_cli_error(format, "build task 编译失败");
            return Err(());
        }
        let plan = compilation.image_plan().ok_or_else(|| {
            emit_cli_error(format, "build task 未产生镜像");
        })?;
        let image = if cfg!(windows) {
            plan.windows_image().to_vec()
        } else {
            plan.linux_image().to_vec()
        };
        let binary = dir.join(if cfg!(windows) { "build.exe" } else { "build" });
        fs::write(&binary, &image).map_err(|error| {
            emit_cli_error(format, &format!("无法写出 build task 镜像: {error}"));
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&binary)
                .map(|meta| meta.permissions().mode())
                .unwrap_or(0o644);
            fs::set_permissions(
                &binary,
                std::fs::Permissions::from_mode(mode | ((mode & 0o444) >> 2)),
            )
            .map_err(|error| {
                emit_cli_error(format, &format!("无法设置 build task 可执行位: {error}"));
            })?;
        }
        let _ = fs::remove_file(&record_path);
        let mut command = std::process::Command::new(&binary);
        command.env("GUGU_BUILD_TASK", "1");
        command.env("GUGU_BUILD_OUT_DIR", &out_dir);
        command.env("GUGU_BUILD_RECORD", &record_path);
        command.env("GUGU_BUILD_HOST", host.to_string());
        command.env("GUGU_BUILD_TARGET", target.to_string());
        command.env("GUGU_BUILD_FEATURES", features.join(","));
        command.env("GUGU_PERM_MODE", permission_mode(options));
        command.env("GUGU_PERM_READ", join_paths(&options.read_allows));
        command.env("GUGU_PERM_WRITE", join_paths(&options.write_allows));
        command.env("GUGU_PERM_ENV", options.env_allows.join(";"));
        command.env("GUGU_PERM_NET", options.net_allows.join(";"));
        command.env("GUGU_PERM_RUN", options.run_allows.join(";"));
        if options.allow_all {
            for key in [
                "GUGU_PERM_READ",
                "GUGU_PERM_WRITE",
                "GUGU_PERM_ENV",
                "GUGU_PERM_NET",
                "GUGU_PERM_RUN",
            ] {
                command.env(key, "*");
            }
        }
        if !std::io::stdin().is_terminal() {
            command.env("GUGU_PERM_NONINTERACTIVE", "1");
        }
        let status = command.status().map_err(|error| {
            emit_cli_error(format, &format!("无法执行 build task: {error}"));
        })?;
        if !status.success() {
            emit_cli_error(
                format,
                &format!("build task 退出码 {}", status.code().unwrap_or(-1)),
            );
            return Err(());
        }
        fs::write(&inputs_path, &current).map_err(|error| {
            emit_cli_error(format, &format!("无法记录 build task 指纹: {error}"));
        })?;
    }

    let record = parse_record(
        &fs::read_to_string(&record_path).unwrap_or_default(),
        &out_dir,
    );
    if !record.denied.is_empty() {
        emit_cli_error(
            format,
            &format!("build task 权限门拒绝: {}", record.denied.join(", ")),
        );
        return Err(());
    }
    if !record.errors.is_empty() {
        emit_cli_error(
            format,
            &format!("build task 记录错误: {}", record.errors.join(", ")),
        );
        return Err(());
    }
    for warning in &record.warnings {
        if !options.quiet {
            eprintln!("warning: build.gg: {warning}");
        }
    }
    let mut modules = Vec::with_capacity(record.emits.len());
    for (name, path) in &record.emits {
        let content = fs::read_to_string(path).map_err(|error| {
            emit_cli_error(
                format,
                &format!("无法读取 emit_module `{name}` 生成文件 {path}: {error}"),
            );
        })?;
        modules.push((name.clone(), content));
    }
    Ok(Some(BuildTaskOutput {
        defines: record.defines,
        modules,
        imports: record.imports,
    }))
}

fn permission_mode(options: &GlobalArgs) -> &'static str {
    if options.allow_all {
        "off"
    } else if options.permission {
        "advisory"
    } else {
        "off"
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(";")
}

/// 把记录里的 link 导入组装成 NativeLink（每个 target 请求复用同一份）。
impl BuildTaskOutput {
    pub fn native_link(&self) -> NativeLink {
        NativeLink {
            imports: self.imports.clone(),
            ..NativeLink::default()
        }
    }
}
