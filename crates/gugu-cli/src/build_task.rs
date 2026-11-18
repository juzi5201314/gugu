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

/// build task 目录的进程互斥：同包并发构建共享 record/inputs/out，
/// 无锁会混读另一进程的产物。锁文件记录宿主与 build task 子进程 pid，
/// 全部已退出（Ctrl+C 等路径不经过 Drop）时后到的进程回收锁而不是等死；
/// 回收用原子 rename 认领，不删他人文件；存活持有者下有界等待。
struct TaskLock {
    path: PathBuf,
    pid: u32,
}

impl TaskLock {
    fn acquire(dir: &Path, format: OutputFormat) -> Result<Self, ()> {
        let path = dir.join("lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let pid = std::process::id();
        loop {
            // 先把 pid 写进临时文件再 hard_link 进锁位：锁文件落盘即完整，
            // 不存在“已创建未写 pid”的空锁窗口。
            let tmp = path.with_extension(format!(
                "claim-{pid}-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|span| span.as_nanos())
                    .unwrap_or(0)
            ));
            if let Err(error) = fs::write(&tmp, format!("{pid}\n")) {
                emit_cli_error(format, &format!("无法创建 build task 锁文件: {error}"));
                return Err(());
            }
            match fs::hard_link(&tmp, &path) {
                Ok(()) => {
                    let _ = fs::remove_file(&tmp);
                    return Ok(Self {
                        path: path.clone(),
                        pid,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&tmp);
                }
                Err(link_error) => {
                    let _ = fs::remove_file(&tmp);
                    // exFAT/FAT 等不支持 hard_link 的文件系统：退回 create_new + 写
                    // pid，留毫秒级空窗，由残留判定的兜底规则覆盖。
                    match fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                    {
                        Ok(mut file) => {
                            use std::io::Write;
                            if let Err(error) = file.write_all(format!("{pid}\n").as_bytes()) {
                                emit_cli_error(
                                    format,
                                    &format!("无法写入 build task 锁文件: {error}"),
                                );
                                return Err(());
                            }
                            return Ok(Self {
                                path: path.clone(),
                                pid,
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(_) => {
                            emit_cli_error(
                                format,
                                &format!("无法创建 build task 锁文件: {link_error}"),
                            );
                            return Err(());
                        }
                    }
                }
            }
            match fs::metadata(&path) {
                Ok(_) => {
                    if lock_is_stale(&path) {
                        match try_reclaim(&path) {
                            Ok(true) => continue,
                            Ok(false) => {}
                            Err(error) => {
                                emit_cli_error(
                                    format,
                                    &format!("无法回收 build task 残留锁: {error}"),
                                );
                                return Err(());
                            }
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        emit_cli_error(format, "另一个构建进程正在执行该 build task");
                        return Err(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => continue,
            }
        }
    }

    /// build task 子进程与宿主共持锁：宿主退出而子进程存活时锁仍有效。
    /// 先写临时文件再原子 rename，宿主中断不会留下截断的空锁。
    fn watch_child(&self, pid: u32) {
        let content = format!("{}\n{}\n", self.pid, pid);
        let tmp = self.path.with_extension(format!("write-{}", self.pid));
        if fs::write(&tmp, &content).is_ok()
            && (fs::rename(&tmp, &self.path).is_ok()
                || (fs::remove_file(&self.path).is_ok() && fs::rename(&tmp, &self.path).is_ok()))
        {
            return;
        }
        let _ = fs::write(&self.path, content);
    }
}

/// 原子认领残留锁：rename 把文件抢先移出锁位，随后重新读内容——
/// 仍是死 pid 才可删，若捕获的是他人刚写的活锁则尽量链接回原位。
/// 返回 Ok(true) 表示锁位已空可重试 create_new，Ok(false) 表示锁有人持有。
fn try_reclaim(path: &Path) -> Result<bool, std::io::Error> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|span| span.as_nanos())
        .unwrap_or(0);
    let claim = path.with_extension(format!("reclaim-{}-{nanos:x}", std::process::id()));
    match fs::rename(path, &claim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    }
    if lock_is_stale(&claim) {
        let _ = fs::remove_file(&claim);
        Ok(true)
    } else {
        // 原路径已被新持有者占住时链接失败，claim 只是过期副本，删除即可。
        let _ = fs::hard_link(&claim, path);
        let _ = fs::remove_file(&claim);
        Ok(false)
    }
}

/// 锁内所有记录的 pid 都已退出才算残留锁。写不到任何 pid 的文件只可能
/// 是外部残留的垃圾（锁文件落盘即带 pid），超过几秒窗口同样按残留处理，
/// 否则视为持有中。
fn lock_is_stale(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let mut any = false;
    for token in text.split_whitespace() {
        let Ok(pid) = token.parse::<u32>() else {
            continue;
        };
        any = true;
        if pid_alive(pid) {
            return false;
        }
    }
    if any {
        return true;
    }
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age >= std::time::Duration::from_secs(5))
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    let path = format!("/proc/{pid}");
    if !Path::new(&path).exists() {
        return false;
    }
    // 僵尸与退出中的进程虽留 /proc 条目，但不会再写构建产物，不算持有者。
    // comm 允许任意字节（含非 UTF-8），stat 按字节解析最后一个 `)` 后的 state。
    match fs::read(format!("{path}/stat")) {
        Ok(stat) => {
            let tail = stat
                .iter()
                .rposition(|byte| *byte == b')')
                .map(|at| &stat[at + 1..])
                .unwrap_or(&stat[..]);
            let state = tail
                .split(|byte| byte.is_ascii_whitespace())
                .find(|token| !token.is_empty());
            !matches!(state, Some(b"Z") | Some(b"X"))
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    // 无非 libc 的进程存活查询；保守按持有中处理。
    true
}

impl Drop for TaskLock {
    fn drop(&mut self) {
        // 极端竞态下锁文件可能已被新持有者占住；只删仍写着自己 pid 的锁。
        let owned = fs::read_to_string(&self.path)
            .map(|text| text.split_whitespace().any(|t| t == self.pid.to_string()))
            .unwrap_or(false);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
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
            // 路径与内容都进指纹：改名不改内容也必须重跑。
            hasher.extend_from_slice(
                file.strip_prefix(package.root())
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .as_bytes(),
            );
            hasher.extend_from_slice(&hash_file(&file).to_le_bytes());
        }
    } else {
        for pattern in rerun_files {
            let path = package.root().join(pattern);
            if pattern.contains('*') {
                for matched in glob_matches(package.root(), pattern) {
                    hasher.extend_from_slice(
                        matched
                            .strip_prefix(package.root())
                            .unwrap_or(&matched)
                            .to_string_lossy()
                            .as_bytes(),
                    );
                    hasher.extend_from_slice(&hash_file(&matched).to_le_bytes());
                }
            } else {
                hasher.extend_from_slice(pattern.as_bytes());
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

/// 词法归一记录里的生成路径：必须仍在 `out_dir` 下。记录的 `..`/`.`
/// 段先折叠，越界（`out/../x`）或根本不带前缀的记录一律拒收；
/// 相对与绝对写法都归一到记录文本本身，避免与 out_dir 重复拼接。
fn emit_path(out_dir: &Path, recorded: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut cleaned = PathBuf::new();
    for component in Path::new(recorded).components() {
        match component {
            Component::ParentDir => {
                if !cleaned.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => cleaned.push(other.as_os_str()),
        }
    }
    // 两侧同一词法口径：含 `..`/`./` 的 --target-dir 不能令合法记录误判越界。
    let base = lexical_normalize(out_dir);
    if cleaned.strip_prefix(&base).is_ok() {
        Some(cleaned)
    } else {
        None
    }
}

/// 词法折叠 `.`/`..`：不解析符号链接，绝对路径根部之上的 `..` 按根处理。
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                cleaned.pop();
            }
            Component::CurDir => {}
            other => cleaned.push(other.as_os_str()),
        }
    }
    cleaned
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
            ["emit", name, path] => match emit_path(out_dir, path) {
                Some(full) => {
                    record
                        .emits
                        .push(((*name).to_owned(), full.to_string_lossy().into_owned()));
                }
                None => record.errors.push(format!("emit 路径越出 out_dir: {path}")),
            },
            ["define", name] => {
                record.defines.insert((*name).to_owned(), None);
            }
            ["define", name, value] => {
                record
                    .defines
                    .insert((*name).to_owned(), Some((*value).to_owned()));
            }
            ["link", kind, name, _path] => {
                if *kind == "dynamic" {
                    // Library 导入：DT_NEEDED/导入目录条目，不伪造符号引用。
                    record.imports.push(NativeImport::library(*name));
                } else {
                    // 静态 .a 需要搜索路径解析，NativeLink 尚无此通道。
                    record
                        .errors
                        .push(format!("link static 目标暂不支持: {name}"));
                }
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
    let lock = TaskLock::acquire(&dir, format)?;

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
        // 失败任务可能留下半截记录：先作废指纹缓存，半成品才不会被复用。
        let _ = fs::remove_file(&inputs_path);
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
        let mut child = command.spawn().map_err(|error| {
            emit_cli_error(format, &format!("无法执行 build task: {error}"));
        })?;
        lock.watch_child(child.id());
        let status = child.wait().map_err(|error| {
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
