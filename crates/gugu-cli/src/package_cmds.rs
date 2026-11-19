//! 依赖、包管理与查询类子命令：explain/tree/update/vendor/package/login/add/remove。
//!
//! 与 `commands.rs` 共享项目装载与退出码约定。

use std::{
    collections::BTreeSet,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use gugu_compiler::{DependencyInput, DiagnosticCode, LockGraph, Project};
use serde_json::json;

use crate::{
    GlobalArgs,
    commands::{discover, fail, format_of, load_project, parse_target_or_exit},
    output::OutputFormat,
};

pub(crate) fn pending_registry(options: &GlobalArgs, name: &str) -> i32 {
    fail(
        format_of(options),
        1,
        format!(
            "`gugu {name}` 需要 registry Protocol v1；归档生成已由 `gugu package` 提供，协议接入尚未完成"
        ),
    )
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

fn explain_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "E0001" => "读取源码文件失败。",
        "E0002" => "缺少 main 入口：bin/test 需要顶层 `fn main` 或等效宏展开入口。",
        "E0003" => "源码不是合法的 Gugu 语法单元。",
        "E0004" => "源码不是 UTF-8。",
        "E0005" => "源码以 BOM 开头，Gugu 源码不允许 BOM。",
        "E0006" => "源码路径无效。",
        "E0007" => "诊断 span 超出源码范围（编译器内部错误）。",
        "E0008" => "单个源码文件超过大小上限。",
        "E0009" => "词法：字符串或字符字面量未闭合。",
        "E0010" => "词法：块注释未闭合。",
        "E0011" => "词法：非法转义序列。",
        "E0012" => "词法：数字字面量格式非法。",
        "E0013" => "词法：无法识别的 token。",
        "E0014" => "词法：未知 attribute 名。",
        "E0015" => "词法：attribute 参数非法。",
        "E0020" => "语法：解析遇到意料之外的 token。",
        "missing_docs" | "E0063" => {
            "lint missing_docs：公开 item 缺少文档注释；级别由 manifest lints 表控制。"
        }
        _ => return None,
    })
}

pub(crate) fn run_explain(code: &str, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let normalized = code.trim().to_ascii_uppercase();
    let registered = DiagnosticCode::ALL
        .iter()
        .find(|entry| entry.to_string() == normalized)
        .map(|entry| (entry.to_string(), format!("{entry:?}")));
    if registered.is_none() && explain_text(&normalized).is_none() {
        return fail(format, 2, format!("未登记的诊断码或 lint 名：`{code}`"));
    }
    match format {
        OutputFormat::Text => {
            if let Some((diag_code, name)) = registered {
                println!("{diag_code} {name}");
            } else {
                println!("{normalized}");
            }
            if let Some(text) = explain_text(&normalized) {
                println!("{text}");
            }
        }
        _ => {
            let (diag_code, name) = registered
                .map(|(c, n)| (c.to_string(), n.to_string()))
                .unwrap_or_else(|| (normalized.clone(), String::new()));
            println!(
                "{}",
                json!({
                    "kind": "explain",
                    "code": diag_code,
                    "name": name,
                    "description": explain_text(&normalized).unwrap_or(""),
                })
            );
        }
    }
    0
}

// ---------------------------------------------------------------------------
// tree / update / vendor
// ---------------------------------------------------------------------------

fn lock_children<'a>(
    lock: &'a LockGraph,
    id: &gugu_compiler::PackageId,
) -> Vec<&'a gugu_compiler::PackageId> {
    lock.packages
        .iter()
        .find(|package| &package.id == id)
        .map(|package| {
            package
                .dependencies
                .iter()
                .map(|dependency| &dependency.package)
                .collect()
        })
        .unwrap_or_default()
}

fn print_tree(
    lock: &LockGraph,
    id: &gugu_compiler::PackageId,
    depth: Option<u32>,
    prefix: &str,
    last: bool,
    expanded: &mut BTreeSet<String>,
    duplicates: bool,
) {
    let marker = if last { "└── " } else { "├── " };
    println!("{prefix}{marker}{id}");
    let key = id.to_string();
    if expanded.contains(&key) && !duplicates {
        return;
    }
    expanded.insert(key);
    let next_depth = depth.map(|d| d.saturating_sub(1));
    if matches!(depth, Some(0)) {
        return;
    }
    let children = lock_children(lock, id);
    let child_prefix = format!("{prefix}{}   ", if last { " " } else { "│" });
    let count = children.len();
    for (index, child) in children.into_iter().enumerate() {
        print_tree(
            lock,
            child,
            next_depth,
            &child_prefix,
            index + 1 == count,
            expanded,
            duplicates,
        );
    }
}

pub(crate) fn run_tree(
    depth: Option<u32>,
    duplicates: bool,
    invert: Option<&str>,
    options: &GlobalArgs,
) -> i32 {
    let format = format_of(options);
    let target = match parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let (context, _packages) = match load_project(options, target) {
        Ok(value) => value,
        Err(code) => return code,
    };
    let lock = &context.lock;
    if let Some(name) = invert {
        // --invert：只展示指定 package 及其反向依赖链（打印其祖先）。
        let Some(needle) = lock
            .packages
            .iter()
            .find(|package| package.id.name() == name)
            .map(|package| package.id.clone())
        else {
            return fail(format, 2, format!("锁图中没有 package `{name}`"));
        };
        for package in &lock.packages {
            if package.id == needle {
                continue;
            }
            if reaches(lock, &package.id, &needle) {
                let mut seen = BTreeSet::new();
                print_tree(lock, &package.id, depth, "", true, &mut seen, duplicates);
            }
        }
        return 0;
    }
    for package in &lock.packages {
        // 只把 workspace 根作为树的根：以 Path source 出现在 packages 中的项。
        if !matches!(
            package.id.source(),
            gugu_compiler::PackageSource::Path { .. }
        ) {
            continue;
        }
        let mut expanded = BTreeSet::new();
        print_tree(
            lock,
            &package.id,
            depth,
            "",
            true,
            &mut expanded,
            duplicates,
        );
    }
    0
}

fn reaches(
    lock: &LockGraph,
    from: &gugu_compiler::PackageId,
    needle: &gugu_compiler::PackageId,
) -> bool {
    let mut stack = vec![from.clone()];
    let mut seen = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if &id == needle {
            return true;
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        stack.extend(lock_children(lock, &id).into_iter().cloned());
    }
    false
}

pub(crate) fn run_update(precise: Option<&str>, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    if precise.is_some() {
        return fail(
            format,
            2,
            "`gugu update --precise` 需要 registry 候选查询，当前 resolver 不支持",
        );
    }
    let target = match parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    // update 允许改写锁文件：屏蔽 --locked 的只读检查。
    let mut relaxed = options.clone();
    relaxed.locked = false;
    match load_project(&relaxed, target) {
        Ok(_) => 0,
        Err(code) => code,
    }
}

pub(crate) fn run_vendor(options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let target = match parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let (context, _packages) = match load_project(options, target) {
        Ok(value) => value,
        Err(code) => return code,
    };
    let vendor_root = context.project.workspace().root().join("vendor");
    // materialize_vendor 的输入集合必须与锁图的外部（非 path）package 一致；
    // workspace 成员自身是 path 输入，不进入 vendor。
    let external: Vec<DependencyInput> = context
        .inputs
        .iter()
        .filter(|input| {
            !matches!(
                input.package().source(),
                gugu_compiler::PackageSource::Path { .. }
            )
        })
        .cloned()
        .collect();
    if let Err(error) = gugu_compiler::materialize_vendor(&vendor_root, &context.lock, &external) {
        return fail(format, 1, error.to_string());
    }
    if format == OutputFormat::Text {
        println!("vendored to {}", vendor_root.display());
    }
    0
}

// ---------------------------------------------------------------------------
// package / login
// ---------------------------------------------------------------------------

pub(crate) fn run_package(options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let (_project, packages) = match discover(options) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if packages.len() != 1 {
        return fail(
            format,
            2,
            "gugu package 需要唯一 package；workspace 请用 -p 指定",
        );
    }
    let package = &packages[0];
    let collected = match gugu_compiler::PackageFiles::from_directory(package.root()) {
        Ok(files) => files,
        Err(error) => return fail(format, 1, error.to_string()),
    };
    let files = match filter_archive_files(package, &collected) {
        Ok(files) => files,
        Err(message) => return fail(format, 1, message),
    };
    let checksum = files.checksum();
    let out_dir = options
        .target_dir
        .clone()
        .unwrap_or_else(|| package.root().join("target"))
        .join("package");
    if let Err(error) = fs::create_dir_all(&out_dir) {
        return fail(
            format,
            1,
            format!("创建 `{}` 失败：{error}", out_dir.display()),
        );
    }
    let file_name = format!("{}-{}.tar.gz", package.name(), package.version());
    let output = out_dir.join(&file_name);
    match write_archive(&files, &output) {
        Ok(()) => {}
        Err(message) => return fail(format, 1, message),
    }
    if format == OutputFormat::Text {
        println!("packaged {} (sha256 {checksum})", output.display());
    } else {
        println!(
            "{}",
            json!({
                "kind": "package",
                "path": output.display().to_string(),
                "sha256": checksum,
            })
        );
    }
    0
}

/// 归档内容规则（publishing-ecosystem 规范）：默认排除产物/凭据/锁文件；
/// `include` 替换候选集合，`exclude` 随后删减，但不能删掉清单、选中 target 或 build task。
fn filter_archive_files(
    package: &gugu_compiler::Package,
    files: &gugu_compiler::PackageFiles,
) -> Result<gugu_compiler::PackageFiles, String> {
    let manifest_path = package.root().join("gugu.toml");
    let raw: toml::Value = toml::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|e| format!("读取 `{}` 失败：{e}", manifest_path.display()))?,
    )
    .map_err(|e| format!("解析 `{}` 失败：{e}", manifest_path.display()))?;
    let read_list = |key: &str| -> Vec<String> {
        raw.get("package")
            .and_then(|package| package.get(key))
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let include = read_list("include");
    let exclude = read_list("exclude");
    // 受保护集合：清单、build task、所有选中 target 的入口与源码根。
    let mut protected: Vec<PathBuf> = vec![PathBuf::from("gugu.toml"), PathBuf::from("build.gg")];
    for target in package.targets() {
        for absolute in [target.entry(), target.source_root()] {
            if let Ok(relative) = absolute.strip_prefix(package.root()) {
                protected.push(relative.to_path_buf());
            }
        }
    }
    let is_protected = |path: &str| {
        protected
            .iter()
            .any(|guard| Path::new(path) == guard || Path::new(path).starts_with(guard))
    };
    let filtered: std::collections::BTreeMap<String, Vec<u8>> = files
        .files()
        .iter()
        .filter(|(path, _)| {
            // 默认排除（锁文件/临时文件/凭据）优先于保护与 include：
            // 宽匹配 include 不算显式收编凭据的意图。
            if default_excluded(path) {
                return false;
            }
            if is_protected(path) {
                return true;
            }
            if !include.is_empty() && !include.iter().any(|p| glob_match(p, path)) {
                return false;
            }
            !exclude.iter().any(|p| glob_match(p, path))
        })
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect();
    gugu_compiler::PackageFiles::new(filtered).map_err(|error| error.to_string())
}

/// 默认排除：锁文件、编辑器临时文件、系统文件与常见凭据文件。
fn default_excluded(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name == "gugu.lock"
        || name == ".DS_Store"
        || name == "Thumbs.db"
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name == "id_rsa"
        || name == "id_dsa"
        || name == "id_ecdsa"
        || name == "id_ed25519"
}

/// 组件级 glob：`**` 匹配任意深度，`*`/`?` 匹配组件内字符；pattern 是路径前缀即命中子树。
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    glob_components(&pattern, &path)
}

fn glob_components(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if path.is_empty() {
        return pattern.iter().all(|component| *component == "**");
    }
    if pattern[0] == "**" {
        return glob_components(&pattern[1..], path) || glob_components(pattern, &path[1..]);
    }
    glob_component(pattern[0], path[0]) && glob_components(&pattern[1..], &path[1..])
}

fn glob_component(pattern: &str, name: &str) -> bool {
    fn inner(pattern: &[u8], name: &[u8]) -> bool {
        match pattern.first() {
            None => name.is_empty(),
            Some(b'*') => {
                inner(&pattern[1..], name) || (!name.is_empty() && inner(pattern, &name[1..]))
            }
            Some(&c) => {
                !name.is_empty() && (c == b'?' || c == name[0]) && inner(&pattern[1..], &name[1..])
            }
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

/// 按规范写规范 tar header：普通文件 0644、mtime 0、无 owner/group。
fn write_archive(files: &gugu_compiler::PackageFiles, output: &Path) -> Result<(), String> {
    use flate2::{Compression, write::GzEncoder};
    let file = fs::File::create(output)
        .map_err(|error| format!("创建 `{}` 失败：{error}", output.display()))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, bytes) in files.files() {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, path, bytes.as_slice())
            .map_err(|error| format!("写归档 `{path}` 失败：{error}"))?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|error| format!("写归档失败：{error}"))?;
    let mut file = encoder
        .finish()
        .map_err(|error| format!("完成 gzip 失败：{error}"))?;
    file.flush().map_err(|error| format!("写归档失败：{error}"))
}

pub(crate) fn run_login(registry: &str, token_stdin: bool, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let token = if token_stdin {
        let mut buffer = String::new();
        match std::io::stdin().read_to_string(&mut buffer) {
            Ok(_) => buffer.trim().to_owned(),
            Err(error) => return fail(format, 101, format!("读取 stdin 失败：{error}")),
        }
    } else {
        match env::var("GUGU_REGISTRY_TOKEN") {
            Ok(token) => token,
            Err(_) => {
                return fail(
                    format,
                    2,
                    "非交互环境请用 `--token-stdin` 或设置 GUGU_REGISTRY_TOKEN",
                );
            }
        }
    };
    if token.is_empty() {
        return fail(format, 2, "token 为空");
    }
    let Some(path) = crate::config::user_config_file() else {
        return fail(format, 101, "无法定位用户配置目录");
    };
    let mut root = if path.is_file() {
        match fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        {
            Some(value) => value,
            None => {
                return fail(format, 1, format!("解析 `{}` 失败", path.display()));
            }
        }
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let table = root
        .as_table_mut()
        .expect("用户配置根必须是 table")
        .entry("registries")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let registries = table.as_table_mut().expect("registries 必须是 table");
    let entry = registries
        .entry(registry.to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    entry
        .as_table_mut()
        .expect("registry 配置必须是 table")
        .insert("token".to_owned(), toml::Value::String(token));
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return fail(
                format,
                1,
                format!("创建 `{}` 失败：{error}", parent.display()),
            );
        }
    }
    match fs::write(&path, toml::to_string(&root).unwrap_or_default()) {
        Ok(()) => {}
        Err(error) => {
            return fail(
                format,
                1,
                format!("写入 `{}` 失败：{error}", path.display()),
            );
        }
    }
    // token 是 bearer 凭据：配置文件只许属主读写。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            return fail(
                format,
                1,
                format!("设置 `{}` 权限失败：{error}", path.display()),
            );
        }
    }
    if format == OutputFormat::Text {
        println!("token saved for `{registry}` in {}", path.display());
    }
    0
}

// ---------------------------------------------------------------------------
// add / remove：manifest 编辑
// ---------------------------------------------------------------------------

fn manifest_path(options: &GlobalArgs) -> Result<PathBuf, i32> {
    let format = format_of(options);
    let start = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = Project::discover(start).map_err(|error| fail(format, 2, error.to_string()))?;
    let packages = project
        .select_packages(options.package.as_deref(), options.workspace)
        .map_err(|error| fail(format, 2, error.to_string()))?;
    if packages.len() != 1 {
        return Err(fail(
            format,
            2,
            "manifest 编辑需要唯一 package；workspace 请用 -p 指定",
        ));
    }
    Ok(packages[0].manifest().to_path_buf())
}

fn deps_table_name(dev: bool, build: bool) -> Result<&'static str, String> {
    match (dev, build) {
        (true, true) => Err("`--dev` 与 `--build` 互斥".to_owned()),
        (true, false) => Ok("test-dependencies"),
        (false, true) => Ok("build.dependencies"),
        (false, false) => Ok("dependencies"),
    }
}

fn nested_table<'a>(
    root: &'a mut toml::map::Map<String, toml::Value>,
    dotted: &str,
) -> &'a mut toml::map::Map<String, toml::Value> {
    let mut current = root;
    for (index, key) in dotted.split('.').enumerate() {
        let _ = index;
        let entry = current
            .entry(key.to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let next = match entry {
            toml::Value::Table(table) => table,
            _ => {
                *entry = toml::Value::Table(toml::map::Map::new());
                match entry {
                    toml::Value::Table(table) => table,
                    _ => unreachable!(),
                }
            }
        };
        current = next;
    }
    current
}

/// 按点分路径返回已存在的子表；任何一段缺失返回 None（不创建）。
fn table_at<'a>(
    root: &'a mut toml::map::Map<String, toml::Value>,
    dotted: &str,
) -> Option<&'a mut toml::map::Map<String, toml::Value>> {
    let mut current = root;
    for key in dotted.split('.') {
        current = current.get_mut(key)?.as_table_mut()?;
    }
    Some(current)
}

fn split_dependency(package: &str) -> (String, String) {
    match package.split_once('@') {
        Some((name, version)) if !version.is_empty() => (name.to_owned(), version.to_owned()),
        _ => (package.to_owned(), "*".to_owned()),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "add 的标志集合与 clap 声明一一对应"
)]
pub(crate) fn run_add(
    package: &str,
    dev: bool,
    build: bool,
    default_features: bool,
    optional: bool,
    path: Option<&Path>,
    git: Option<&str>,
    rev: Option<&str>,
    branch: Option<&str>,
    tag: Option<&str>,
    options: &GlobalArgs,
) -> i32 {
    let format = format_of(options);
    let table_name = match deps_table_name(dev, build) {
        Ok(name) => name,
        Err(message) => return fail(format, 2, message),
    };
    if path.is_some() && git.is_some() {
        return fail(format, 2, "`--path` 与 `--git` 互斥");
    }
    if usize::from(rev.is_some()) + usize::from(branch.is_some()) + usize::from(tag.is_some()) > 1 {
        return fail(format, 2, "`--rev`、`--branch`、`--tag` 最多设置一个");
    }
    let manifest = match manifest_path(options) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let text = match fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) => {
            return fail(
                format,
                2,
                format!("读取 `{}` 失败：{error}", manifest.display()),
            );
        }
    };
    let mut root: toml::Value = match toml::from_str(&text) {
        Ok(root) => root,
        Err(error) => {
            return fail(
                format,
                2,
                format!("解析 `{}` 失败：{error}", manifest.display()),
            );
        }
    };
    let (alias, version) = split_dependency(package);
    let mut entry = toml::map::Map::new();
    if let Some(path) = path {
        entry.insert(
            "path".to_owned(),
            toml::Value::String(path.display().to_string()),
        );
    } else if let Some(url) = git {
        entry.insert("git".to_owned(), toml::Value::String(url.to_owned()));
        for (key, value) in [("rev", rev), ("branch", branch), ("tag", tag)] {
            if let Some(value) = value {
                entry.insert(key.to_owned(), toml::Value::String(value.to_owned()));
            }
        }
    } else {
        entry.insert("version".to_owned(), toml::Value::String(version));
    }
    if default_features {
        entry.insert("default-features".to_owned(), toml::Value::Boolean(true));
    }
    if optional {
        entry.insert("optional".to_owned(), toml::Value::Boolean(true));
    }
    let table = nested_table(
        root.as_table_mut().expect("manifest 根必须是 table"),
        table_name,
    );
    table.insert(alias.clone(), toml::Value::Table(entry));
    if let Err(code) = write_manifest(&manifest, &root, format) {
        return code;
    }
    if format == OutputFormat::Text {
        println!("added `{alias}` to [{table_name}]");
    }
    0
}

pub(crate) fn run_remove(package: &str, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let manifest = match manifest_path(options) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let text = match fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) => {
            return fail(
                format,
                2,
                format!("读取 `{}` 失败：{error}", manifest.display()),
            );
        }
    };
    let mut root: toml::Value = match toml::from_str(&text) {
        Ok(root) => root,
        Err(error) => {
            return fail(
                format,
                2,
                format!("解析 `{}` 失败：{error}", manifest.display()),
            );
        }
    };
    // remove 不创建空表：只在已存在的依赖表里删条目。
    let mut removed = false;
    for table_name in ["dependencies", "test-dependencies", "build.dependencies"] {
        if let Some(table) = table_at(
            root.as_table_mut().expect("manifest 根必须是 table"),
            table_name,
        ) {
            removed |= table.remove(package).is_some();
        }
    }
    if !removed {
        return fail(format, 1, format!("manifest 中没有依赖 `{package}`"));
    }
    if let Err(code) = write_manifest(&manifest, &root, format) {
        return code;
    }
    if format == OutputFormat::Text {
        println!("removed `{package}`");
    }
    0
}

fn write_manifest(manifest: &Path, root: &toml::Value, format: OutputFormat) -> Result<(), i32> {
    let text = toml::to_string_pretty(root)
        .map_err(|error| fail(format, 101, format!("序列化 manifest 失败：{error}")))?;
    fs::write(manifest, text).map_err(|error| {
        fail(
            format,
            1,
            format!("写入 `{}` 失败：{error}", manifest.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deps_table_selects_manifest_section() {
        assert_eq!(deps_table_name(false, false).unwrap(), "dependencies");
        assert_eq!(deps_table_name(true, false).unwrap(), "test-dependencies");
        assert_eq!(deps_table_name(false, true).unwrap(), "build.dependencies");
        assert!(deps_table_name(true, true).is_err());
    }

    #[test]
    fn split_dependency_parses_version_suffix() {
        assert_eq!(
            split_dependency("owner/name@1.2.3"),
            ("owner/name".to_owned(), "1.2.3".to_owned())
        );
        assert_eq!(
            split_dependency("owner/name"),
            ("owner/name".to_owned(), "*".to_owned())
        );
        assert_eq!(
            split_dependency("owner/name@"),
            ("owner/name@".to_owned(), "*".to_owned())
        );
    }

    #[test]
    fn glob_match_prefix_and_star() {
        assert!(glob_match("docs", "docs/guide.md"));
        assert!(glob_match("*.pem", "cert.pem"));
        assert!(glob_match("**/*.pem", "keys/a/cert.pem"));
        assert!(glob_match("src/*.gg", "src/lib.gg"));
        assert!(!glob_match("*.pem", "keys/cert.pem"));
        assert!(!glob_match("a*c", "ax"));
        assert!(glob_match("a*c", "axxxc"));
    }

    #[test]
    fn explain_text_covers_registered_codes() {
        assert!(explain_text("E0001").is_some());
        assert!(explain_text("E0063").is_some());
        assert!(explain_text("E9999").is_none());
    }
}
