//! `target/` 物化与末端 strip action。
//!
//! 编译成功后把内存镜像计划写成真实文件：`bin/`、`lib/`、`tests/`、
//! `benches/`、`examples/` 承载产物，`generated/` 留给 build.gg 输出，
//! `build-logs/` 写本次 action graph 的可读日志。`--strip` 在最终字节上
//! 做后处理（不重编译），image 与 stripped image 作为两个末端 action
//! 分别记入 action cache。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gugu_compiler::{
    ActionStore, Compilation, Target, TargetArtifact, TargetKind, TargetName, TargetView,
    strip_image,
};
use serde_json::json;

use crate::{GlobalArgs, OutputFormat, emit_cli_error};

const LAYOUT_DIRS: [&str; 2] = ["generated", "build-logs"];

/// package target 编译成功后物化 `target/` 并登记 action record。
pub(crate) fn materialize_target(
    workspace_root: &Path,
    package_target: &Target,
    compilation: &Compilation,
    options: &GlobalArgs,
    target: TargetName,
    check_only: bool,
    format: OutputFormat,
    written: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<(), ()> {
    if check_only || !compilation.is_success() {
        return Ok(());
    }
    let target_root = options
        .target_dir
        .clone()
        .unwrap_or_else(|| workspace_root.join("target"));
    for dir in LAYOUT_DIRS {
        fs::create_dir_all(target_root.join(dir)).map_err(|error| {
            emit_cli_error(format, &format!("创建 `target/{dir}` 失败：{error}"));
        })?;
    }
    if let Some(plan) = compilation.image_plan() {
        let kind = image_kind(target, plan.linux_image_kind(), plan.windows_image_kind());
        let bytes = image_bytes(target, plan);
        let name = format!(
            "{}/{}{}",
            target_dir_name(package_target.kind()),
            package_target.name(),
            artifact_extension(target, kind)
        );
        let bytes = finalize_bytes(target, bytes, &name, compilation, options, format)?;
        let view = TargetView::new(&target_root);
        let artifact = TargetArtifact::new(name.clone(), bytes)
            .map_err(|error| emit_cli_error(format, &error.to_string()))?;
        // `target/<target-triple>/<dir>/<name>`，与规范布局一致。
        let paths = view
            .materialize(&target.to_string(), &[artifact])
            .map_err(|error| emit_cli_error(format, &error.to_string()))?;
        for path in paths {
            // workspace 成员同名产物属于声明冲突，报错而不是静默覆盖。
            if !written.insert(path.clone()) {
                emit_cli_error(
                    format,
                    &format!(
                        "目标产物路径冲突：`{}`（workspace 成员产物重名）",
                        path.display()
                    ),
                );
                return Err(());
            }
            make_executable(&path, package_target.kind(), format)?;
        }
    }
    write_build_log(&target_root, package_target.name(), compilation, format)
}

/// 单文件编译成功后按规范写到当前目录或 `--target-dir`。
pub(crate) fn materialize_single_file(
    file: &Path,
    compilation: &Compilation,
    options: &GlobalArgs,
    target: TargetName,
    check_only: bool,
    format: OutputFormat,
) -> Result<(), ()> {
    if check_only || !compilation.is_success() {
        return Ok(());
    }
    let Some(plan) = compilation.image_plan() else {
        return Ok(());
    };
    let kind = image_kind(target, plan.linux_image_kind(), plan.windows_image_kind());
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_owned().to_string())
        .unwrap_or_else(|| "a".to_owned());
    let name = format!("{stem}{}", artifact_extension(target, kind));
    let bytes = finalize_bytes(
        target,
        image_bytes(target, plan),
        &name,
        compilation,
        options,
        format,
    )?;
    let output = options
        .target_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(name);
    fs::write(&output, bytes).map_err(|error| {
        emit_cli_error(
            format,
            &format!("写出 `{}` 失败：{error}", output.display()),
        );
    })
}

/// `--strip` 时先做末端后处理；image 与 stripped image 都进 action cache。
fn finalize_bytes(
    target: TargetName,
    bytes: &[u8],
    entry: &str,
    compilation: &Compilation,
    options: &GlobalArgs,
    format: OutputFormat,
) -> Result<Vec<u8>, ()> {
    let store = ActionStore::new(crate::cache::cache_root(options));
    publish(&store, compilation, target, "image", entry, bytes, format)?;
    if !options.strip {
        return Ok(bytes.to_vec());
    }
    let stripped = strip_image(target, bytes).map_err(|error| {
        emit_cli_error(format, &format!("strip 失败：{error}"));
    })?;
    publish(
        &store,
        compilation,
        target,
        "stripped-image",
        entry,
        &stripped.bytes,
        format,
    )?;
    Ok(stripped.bytes)
}

fn publish(
    store: &ActionStore,
    compilation: &Compilation,
    target: TargetName,
    kind: &str,
    entry: &str,
    bytes: &[u8],
    format: OutputFormat,
) -> Result<(), ()> {
    let Some(key) = compilation.action_key() else {
        return Ok(());
    };
    store
        .publish(
            &format!("{}.{kind}", key.hex()),
            kind,
            &target.to_string(),
            entry,
            bytes,
        )
        .map(|_| ())
        .map_err(|error| emit_cli_error(format, &error.to_string()))
}

/// 写 `target/build-logs/<epoch>-<target>.jsonl`：每个 action 节点一行。
fn write_build_log(
    target_root: &Path,
    target_name: &str,
    compilation: &Compilation,
    format: OutputFormat,
) -> Result<(), ()> {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = target_root
        .join("build-logs")
        .join(format!("{epoch}-{target_name}.jsonl"));
    let mut text = String::new();
    for node in compilation.action_graph().nodes() {
        let line = json!({
            "kind": node.kind().to_string(),
            "status": node.status().to_string(),
            "detail": node.detail(),
        });
        text.push_str(&line.to_string());
        text.push('\n');
    }
    fs::write(&path, text).map_err(|error| {
        emit_cli_error(format, &format!("写出 `{}` 失败：{error}", path.display()));
    })
}

/// 可执行目录的产物在 Unix 上补执行位；静态/动态库保持普通文件权限。
fn make_executable(path: &Path, kind: TargetKind, format: OutputFormat) -> Result<(), ()> {
    #[cfg(unix)]
    if !matches!(kind, TargetKind::Lib) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|error| {
            emit_cli_error(
                format,
                &format!("设置 `{}` 执行权限失败：{error}", path.display()),
            );
        })?;
    }
    #[cfg(not(unix))]
    let _ = (path, kind, format);
    Ok(())
}

fn target_dir_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Bin => "bin",
        TargetKind::Lib => "lib",
        TargetKind::Test => "tests",
        TargetKind::Bench => "benches",
        TargetKind::Example => "examples",
        TargetKind::Build => "generated",
    }
}

fn image_bytes<'a>(target: TargetName, plan: &'a gugu_compiler::ImagePlan) -> &'a [u8] {
    match target {
        TargetName::X86_64Linux => plan.linux_image(),
        TargetName::X86_64Windows => plan.windows_image(),
    }
}

fn image_kind<'a>(target: TargetName, linux: &'a str, windows: &'a str) -> &'a str {
    match target {
        TargetName::X86_64Linux => linux,
        TargetName::X86_64Windows => windows,
    }
}

fn artifact_extension(target: TargetName, kind: &str) -> &'static str {
    match (target, kind) {
        (TargetName::X86_64Windows, kind) if kind.contains("dll") => ".dll",
        (TargetName::X86_64Windows, kind) if kind.contains("lib") => ".lib",
        (TargetName::X86_64Windows, _) => ".exe",
        (TargetName::X86_64Linux, kind) if kind.contains("dll") || kind.contains("so") => ".so",
        (TargetName::X86_64Linux, kind) if kind.contains("lib") => ".a",
        _ => "",
    }
}
