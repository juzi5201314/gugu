//! `gugu doc`：公共 API 文档生成。
//!
//! 每个 package 编译 lib target 得到 `DocWorld`，渲染为 `target/doc/<pkg>/` 下的
//! HTML：模块页列出公共项签名与文档，项链接到带行锚点的源码页。`--no-deps`
//! 只生成当前 package；默认同时生成锁图中依赖 package 的文档。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use gugu_compiler::docs::{DocItem, DocModule, DocWorld};
use gugu_compiler::{
    CachePolicy, Compilation, CompileRequest, Compiler, DependencyCache, DependencyDomain,
    EMBEDDED_ARTIFACTS, LockGraph, Package, PackageFiles, PackageSource, Project, Target,
    TargetName, TargetSelection, default_cache_root, prepare_dependency_inputs,
};
use serde_json::json;

use crate::output::{OutputFormat, emit_event};
use crate::{
    GlobalArgs, RunDomain, emit_cli_error, enabled_features, package_resolution, print_compilation,
    resolve_project_lock,
};

/// `gugu doc [--open] [--no-deps]` 入口。
pub(crate) fn run(options: &GlobalArgs, open: bool, no_deps: bool) -> i32 {
    let format = options.format.unwrap_or_default();
    let target = match options.target.as_deref() {
        Some(name) => match TargetName::parse(name) {
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
    let project = match Project::discover(std::env::current_dir().unwrap_or_else(|_| ".".into())) {
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
    let (lock, _inputs) =
        match resolve_project_lock(&project, &packages, options, target, format, false) {
            Ok(resolved) => resolved,
            Err(()) => return 2,
        };
    let workspace_root = project.workspace().root().to_path_buf();
    let doc_root = options
        .target_dir
        .clone()
        .unwrap_or_else(|| workspace_root.join("target"))
        .join("doc");
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    let mut written: Vec<(String, PathBuf)> = Vec::new();
    let mut exit_code = 0;
    let member_roots: BTreeSet<PathBuf> = packages
        .iter()
        .map(|package| package.root().to_path_buf())
        .collect();
    for package in &packages {
        let features = enabled_features(options, package);
        let Some(target_ref) = doc_target(package, &features) else {
            emit_cli_error(
                format,
                &format!(
                    "package `{}` 没有 lib/bin target，无法生成文档",
                    package.package_name()
                ),
            );
            return 2;
        };
        let (identity, external) = package_resolution(
            &lock,
            &workspace_root,
            package,
            target_ref.kind(),
            RunDomain::Default,
        );
        let request = CompileRequest::project_target(
            package, target_ref, identity, target, features, external,
        );
        let compilation = compiler.compile(request);
        print_compilation(&compilation, true, options, target);
        exit_code = exit_code.max(compilation.exit_code());
        let Some(docs) = compilation.docs() else {
            continue;
        };
        let dir = doc_root.join(sanitize(&package.package_name()));
        match render_package(&dir, docs, &compilation, &package.package_name()) {
            Ok(()) => written.push((package.package_name(), dir)),
            Err(error) => {
                emit_cli_error(format, &error);
                return 2;
            }
        }
    }
    if !no_deps {
        match render_dependencies(
            &lock,
            &workspace_root,
            &member_roots,
            &doc_root,
            &compiler,
            options,
            target,
        ) {
            Ok(mut deps) => written.append(&mut deps),
            Err(()) => return 2,
        }
    }
    if exit_code != 0 {
        return exit_code;
    }
    let index = doc_root.join("index.html");
    if let Err(error) = write_index(&index, &written) {
        emit_cli_error(format, &error);
        return 2;
    }
    match format {
        OutputFormat::Json | OutputFormat::JsonDiagnosticShort => {
            emit_event(
                "doc-generated",
                json!({
                    "dir": doc_root.display().to_string(),
                    "packages": written.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                }),
            );
        }
        OutputFormat::Text => {
            if !options.quiet {
                println!("doc generated: {}", index.display());
            }
        }
    }
    if open && !open_browser(&index) {
        emit_cli_error(format, &format!("无法在浏览器打开 `{}`", index.display()));
        return 1;
    }
    0
}

/// 文档只生成公开 API：lib target 优先，没有 lib 时退回默认构建目标。
fn doc_target<'a>(package: &'a Package, features: &[String]) -> Option<&'a Target> {
    // 无 lib target 时 select_targets_in 返回 Err：不能过早 ok()?，否则纯 bin package 永远走不到回退。
    if let Ok(libs) = package.select_targets_in(&TargetSelection::Lib, features)
        && let Some(target) = libs.first()
    {
        return Some(target);
    }
    package
        .select_targets_in(&TargetSelection::DefaultBuild, features)
        .ok()?
        .into_iter()
        .next()
}

/// 渲染锁图中非 workspace 成员的依赖 package 文档。
fn render_dependencies(
    lock: &LockGraph,
    workspace_root: &Path,
    member_roots: &BTreeSet<PathBuf>,
    doc_root: &Path,
    compiler: &Compiler,
    options: &GlobalArgs,
    target: TargetName,
) -> Result<Vec<(String, PathBuf)>, ()> {
    let format = options.format.unwrap_or_default();
    let cache = DependencyCache::new(options.cache_dir.clone().unwrap_or_else(default_cache_root));
    // 与 build/check 同一条依赖输入路径：离线、locked 与 --vendor 在此统一处理，
    // 文档始终由校验过的锁定源码生成，不读取任何此前生成的文档或 target 产物。
    let policy = CachePolicy {
        offline: options.offline,
        locked: options.locked,
        vendor: options.vendor,
    };
    let inputs = match prepare_dependency_inputs(
        lock,
        &cache,
        workspace_root,
        &workspace_root.join("vendor"),
        policy,
    ) {
        Ok(inputs) => inputs,
        Err(error) => {
            emit_cli_error(format, &error.to_string());
            return Err(());
        }
    };
    let inputs: BTreeMap<String, PackageFiles> = inputs
        .into_iter()
        .map(|input| (input.package().to_string(), input.files().clone()))
        .collect();
    // pid + 纳秒随机后缀：共享 /tmp 上其他本地用户无法预建或冒充固定目录。
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let dep_src_root =
        std::env::temp_dir().join(format!("gugu-doc-src-{}-{nonce:x}", std::process::id()));
    let mut written = Vec::new();
    // 失败也要清理临时依赖源码：把 return Err(()) 改为 dep_error + break。
    let mut dep_error = false;
    for locked in &lock.packages {
        // workspace 成员的文档由上层 package 循环生成，这里跳过。
        if matches!(locked.id.source(), PackageSource::Path { path }
            if member_roots.contains(&workspace_root.join(path)))
        {
            continue;
        }
        let Some(files) = inputs.get(&locked.id.to_string()) else {
            emit_cli_error(
                format,
                &format!("依赖 `{}` 缺少已校验的锁定输入", locked.id),
            );
            dep_error = true;
            break;
        };
        // 物化到本次运行唯一的临时目录而非 doc 输出目录：放在工作区内会让
        // find_workspace_manifest 把依赖清单归进宿主 workspace，导致非成员错误；
        // 共享固定目录则会让并行的 gugu doc 互相清空依赖源码。
        let dep_root = dep_src_root.join(sanitize(&locked.id.to_string()));
        if let Err(message) = materialize_package(&dep_root, files) {
            emit_cli_error(format, &message);
            dep_error = true;
            break;
        }
        let dep_project = match Project::discover(&dep_root) {
            Ok(project) => project,
            Err(error) => {
                emit_cli_error(
                    format,
                    &format!("依赖 `{}` 清单读取失败：{error}", locked.id),
                );
                dep_error = true;
                break;
            }
        };
        let Some(dep_package) = dep_project.packages().iter().find(|package| {
            package.package_name() == locked.id.name() || package.name() == locked.id.name()
        }) else {
            emit_cli_error(
                format,
                &format!(
                    "依赖 `{}` 在 `{}` 中没有对应 package",
                    locked.id,
                    dep_root.display()
                ),
            );
            dep_error = true;
            break;
        };
        // 用锁图记录的真实启用特性：feature 门控的公共 API 才能进入依赖文档。
        let features = locked
            .features
            .get(&DependencyDomain::Normal)
            .cloned()
            .unwrap_or_default();
        let Some(target_ref) = doc_target(dep_package, &features) else {
            continue;
        };
        let external: BTreeSet<String> = locked
            .dependencies
            .iter()
            .filter(|dependency| dependency.domain == DependencyDomain::Normal)
            .map(|dependency| dependency.alias.clone())
            .collect();
        let request = CompileRequest::project_target(
            dep_package,
            target_ref,
            locked.id.to_string(),
            target,
            features,
            external,
        );
        let compilation = compiler.compile(request);
        print_compilation(&compilation, true, options, target);
        if compilation.exit_code() != 0 {
            dep_error = true;
            break;
        }
        let Some(docs) = compilation.docs() else {
            continue;
        };
        let dir = doc_root.join("deps").join(sanitize(&locked.id.to_string()));
        match render_package(&dir, docs, &compilation, &locked.id.to_string()) {
            Ok(()) => written.push((locked.id.to_string(), dir)),
            Err(error) => {
                emit_cli_error(format, &error);
                dep_error = true;
                break;
            }
        }
    }
    let _ = fs::remove_dir_all(&dep_src_root);
    if dep_error {
        return Err(());
    }
    Ok(written)
}

/// 把已校验的依赖文件集物化到本进程的 `gugu-doc-src-<pid>/` 供
/// Project::discover 读取；先清空目标目录，防止已删除的源文件残留参与编译。
fn materialize_package(dir: &Path, files: &PackageFiles) -> Result<(), String> {
    if dir.exists() {
        fs::remove_dir_all(dir)
            .map_err(|error| format!("清理 `{}` 失败：{error}", dir.display()))?;
    }
    for (path, bytes) in files.files() {
        let file = dir.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("物化 `{}` 失败：{error}", file.display()))?;
        }
        fs::write(&file, bytes)
            .map_err(|error| format!("物化 `{}` 失败：{error}", file.display()))?;
    }
    Ok(())
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;margin:2em;line-height:1.5}\
pre{background:#f6f8fa;padding:.6em;overflow-x:auto}\
.sig{font-family:ui-monospace,monospace}\
table{border-collapse:collapse}td,th{padding:.2em .8em .2em 0;text-align:left;vertical-align:top}\
.kind{color:#8250df}nav a{margin-right:1em}\
.src{white-space:pre;font-family:ui-monospace,monospace}\
.src .l{color:#6e7781;display:inline-block;width:4em}\
a{color:#0969da;text-decoration:none}";

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}

/// 文档文本按段落与 ` ``` ` 围栏渲染；gugu 代码块进入 `<pre><code>`。
fn docs_html(docs: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut paragraph = String::new();
    for line in docs.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                out.push_str("</code></pre>");
                in_fence = false;
            } else {
                if !paragraph.is_empty() {
                    out.push_str(&format!("<p>{}</p>", esc(&paragraph)));
                    paragraph.clear();
                }
                out.push_str("<pre><code>");
                in_fence = true;
            }
            continue;
        }
        if in_fence {
            out.push_str(&esc(line));
            out.push('\n');
            continue;
        }
        if line.trim().is_empty() {
            if !paragraph.is_empty() {
                out.push_str(&format!("<p>{}</p>", esc(&paragraph)));
                paragraph.clear();
            }
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(line.trim());
        }
    }
    if in_fence {
        out.push_str("</code></pre>");
    }
    if !paragraph.is_empty() {
        out.push_str(&format!("<p>{}</p>", esc(&paragraph)));
    }
    out
}

fn item_html(item: &DocItem, module: &DocModule) -> String {
    let mut out = format!(
        "<h3 id=\"i-{}\"><span class=\"kind\">{}</span> <code>{}</code> <a href=\"src/{}.html#L{}\">[source]</a></h3>",
        esc(&item.name),
        item.kind,
        esc(&item.name),
        esc(&module.path),
        item.line,
    );
    out.push_str(&format!(
        "<pre class=\"sig\"><code>{}</code></pre>",
        esc(&item.signature)
    ));
    out.push_str(&docs_html(&item.docs));
    for member in &item.members {
        out.push_str(&format!(
            "<h4 id=\"i-{}.{}\"><span class=\"kind\">{}</span> <code>{}</code> <a href=\"src/{}.html#L{}\">[source]</a></h4>",
            esc(&item.name),
            esc(&member.name),
            member.kind,
            esc(&member.name),
            esc(&module.path),
            member.line,
        ));
        out.push_str(&format!(
            "<pre class=\"sig\"><code>{}</code></pre>",
            esc(&member.signature)
        ));
        out.push_str(&docs_html(&member.docs));
    }
    out
}

fn module_html(module: &DocModule, package: &str) -> String {
    let mut body = format!(
        "<nav><a href=\"index.html\">{package}</a> › {}</nav><h1>模块 <code>{}</code></h1>",
        esc(&module.path),
        esc(&module.path)
    );
    body.push_str(&docs_html(&module.docs));
    if !module.items.is_empty() {
        body.push_str("<h2>公共项</h2><table>");
        for item in &module.items {
            body.push_str(&format!(
                "<tr><td class=\"kind\">{}</td><td><a href=\"#i-{}\"><code>{}</code></a></td><td>{}</td></tr>",
                item.kind,
                esc(&item.name),
                esc(&item.name),
                esc(item.docs.lines().next().unwrap_or("")),
            ));
        }
        body.push_str("</table>");
        for item in &module.items {
            body.push_str(&item_html(item, module));
        }
    }
    page(&format!("{package} — {}", module.path), &body)
}

fn index_html(package: &str, world: &DocWorld) -> String {
    let mut body = format!("<h1>{package} API 文档</h1><table>");
    for module in &world.modules {
        body.push_str(&format!(
            "<tr><td><a href=\"{}.html\"><code>{}</code></a></td><td>{}</td></tr>",
            esc(&module.path),
            esc(&module.path),
            esc(module.docs.lines().next().unwrap_or("")),
        ));
    }
    body.push_str("</table>");
    page(&format!("{package} API 文档"), &body)
}

fn source_html(module: &DocModule, package: &str, source: &str) -> String {
    let mut body = format!(
        "<nav><a href=\"{}.html\">{}</a> › 源码</nav><div class=\"src\">",
        esc(&module.path),
        esc(&module.path)
    );
    for (index, line) in source.lines().enumerate() {
        let number = index + 1;
        body.push_str(&format!(
            "<a id=\"L{number}\"><span class=\"l\">{number}</span></a>{}\n",
            esc(line)
        ));
    }
    body.push_str("</div>");
    page(&format!("{package} — {} 源码", module.path), &body)
}

fn render_package(
    dir: &Path,
    world: &DocWorld,
    compilation: &Compilation,
    package: &str,
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| format!("创建 `{}` 失败：{error}", dir.display()))?;
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir)
        .map_err(|error| format!("创建 `{}` 失败：{error}", src_dir.display()))?;
    for module in &world.modules {
        fs::write(
            dir.join(format!("{}.html", module.path)),
            module_html(module, package),
        )
        .map_err(|error| format!("写模块页 `{}` 失败：{error}", module.path))?;
        let source = compilation
            .source_map()
            .snapshots()
            .iter()
            .find(|snapshot| snapshot.logical_path() == module.source_path)
            .map(|snapshot| snapshot.content().to_owned())
            .unwrap_or_default();
        fs::write(
            src_dir.join(format!("{}.html", module.path)),
            source_html(module, package, &source),
        )
        .map_err(|error| format!("写源码页 `{}` 失败：{error}", module.path))?;
    }
    fs::write(dir.join("index.html"), index_html(package, world))
        .map_err(|error| format!("写 index 失败：{error}"))?;
    Ok(())
}

/// doc 根 index：列出本次生成的 package 文档。
fn write_index(path: &Path, written: &[(String, PathBuf)]) -> Result<(), String> {
    let mut body = String::from("<h1>Gugu API 文档</h1><table>");
    for (name, dir) in written {
        let relative = dir
            .strip_prefix(path.parent().unwrap_or(dir))
            .unwrap_or(dir)
            .join("index.html");
        body.push_str(&format!(
            "<tr><td><a href=\"{}\"><code>{}</code></a></td></tr>",
            esc(&relative.display().to_string().replace('\\', "/")),
            esc(name),
        ));
    }
    body.push_str("</table>");
    fs::create_dir_all(path.parent().unwrap_or(path))
        .map_err(|error| format!("创建 doc 目录失败：{error}"))?;
    fs::write(path, page("Gugu API 文档", &body))
        .map_err(|error| format!("写 `{}` 失败：{error}", path.display()))
}

#[cfg(target_os = "macos")]
fn open_browser(path: &Path) -> bool {
    std::process::Command::new("open").arg(path).spawn().is_ok()
}

#[cfg(target_os = "windows")]
fn open_browser(path: &Path) -> bool {
    std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn()
        .is_ok()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn open_browser(path: &Path) -> bool {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .is_ok()
}
