//! `gugu clean` 与 `gugu cache`：目标产物与编译缓存的生命周期管理。
//!
//! `gugu clean` 默认只删当前 workspace 的 `target/`，不触碰全局依赖缓存；
//! `--cache` 删除编译缓存（action record store），`--registry` 只删依赖缓存中
//! registry 来源的条目，`--all` 删除全部三类。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use gugu_compiler::{ActionStore, CacheError, DependencyCache, Project, ProjectError};

use crate::{CacheCommand, OutputFormat, emit_cli_error};

/// `gugu clean [--cache|--registry|--all]`。
pub(crate) fn run_clean(
    options: &crate::GlobalArgs,
    cache: bool,
    registry: bool,
    all: bool,
) -> i32 {
    let format = options.format.unwrap_or_default();
    let mut removed: Vec<String> = Vec::new();
    // 默认与 --all 都删除 workspace target/；需要 workspace 发现成功。
    if !cache && !registry || all {
        match workspace_root() {
            Ok(root) => {
                if let Some(target_dir) = clean_target_dir(&root, options, format, &mut removed)
                    && let Err(error) = remove_dir(&target_dir)
                {
                    emit_cli_error(format, &format!("删除 `target/` 失败：{error}"));
                    return 2;
                }
            }
            Err(error) => {
                emit_cli_error(format, &error.to_string());
                return 2;
            }
        }
    }
    if cache || all {
        match ActionStore::new(cache_root(options)).clean() {
            Ok(freed) => removed.push(format!("compile cache（{freed} bytes）")),
            Err(error) => {
                emit_cli_error(format, &error.to_string());
                return 2;
            }
        }
    }
    if registry || all {
        match clean_registry_entries(&cache_root(options)) {
            Ok(count) => removed.push(format!("registry 缓存条目 ×{count}")),
            Err(error) => {
                emit_cli_error(format, &error.to_string());
                return 2;
            }
        }
    }
    if format == OutputFormat::Text && !options.quiet {
        if removed.is_empty() {
            println!("nothing to clean");
        } else {
            println!("cleaned: {}", removed.join("、"));
        }
    }
    0
}

/// `gugu cache <dir|clean|gc|verify>`。
pub(crate) fn run_cache(options: &crate::GlobalArgs, command: &CacheCommand) -> i32 {
    let format = options.format.unwrap_or_default();
    let store = ActionStore::new(cache_root(options));
    match command {
        CacheCommand::Dir => {
            println!("{}", store.root().display());
            0
        }
        CacheCommand::Clean => match store.clean() {
            Ok(freed) => {
                if format == OutputFormat::Text && !options.quiet {
                    println!("cleaned compile cache（{freed} bytes）");
                }
                0
            }
            Err(error) => report(format, &error),
        },
        CacheCommand::Gc => match store.gc(ActionStore::capacity()) {
            Ok(report) => {
                if format == OutputFormat::Text && !options.quiet {
                    println!(
                        "gc: evicted {} entries（{} bytes），remaining {} bytes",
                        report.evicted, report.freed_bytes, report.remaining_bytes
                    );
                }
                0
            }
            Err(error) => report(format, &error),
        },
        CacheCommand::Verify => match store.verify() {
            Ok(report) => {
                if format == OutputFormat::Text && !options.quiet {
                    println!(
                        "verify: checked {} entries，quarantined {}",
                        report.checked, report.quarantined
                    );
                }
                (report.quarantined > 0) as i32
            }
            Err(error) => report(format, &error),
        },
    }
}

fn report(format: OutputFormat, error: &CacheError) -> i32 {
    emit_cli_error(format, &error.to_string());
    2
}

fn workspace_root() -> Result<PathBuf, ProjectError> {
    Project::discover(env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .map(|project| project.workspace().root().to_path_buf())
}

fn clean_target_dir(
    root: &Path,
    options: &crate::GlobalArgs,
    format: OutputFormat,
    removed: &mut Vec<String>,
) -> Option<PathBuf> {
    let target_dir = options
        .target_dir
        .clone()
        .unwrap_or_else(|| root.join("target"));
    if target_dir.exists() {
        removed.push(format!("target/（{}）", target_dir.display()));
        Some(target_dir)
    } else {
        let _ = format;
        None
    }
}

/// 删除依赖缓存中 registry 来源的条目；path/git 来源不动。
fn clean_registry_entries(cache_root: &Path) -> Result<u32, CacheError> {
    let cache = DependencyCache::new(cache_root.to_path_buf());
    let packages = cache.root().join("dependencies/v1/packages");
    let mut count = 0;
    if !packages.is_dir() {
        return Ok(0);
    }
    for entry in fs::read_dir(&packages).map_err(|e| io_cache(&packages, e))? {
        let dir = entry.map_err(|e| io_cache(&packages, e))?.path();
        let record = dir.join("record.toml");
        let text = match fs::read_to_string(&record) {
            Ok(text) => text,
            Err(_) => continue,
        };
        if registry_source(&text) {
            fs::remove_dir_all(&dir).map_err(|e| io_cache(&dir, e))?;
            count += 1;
        }
    }
    Ok(count)
}

fn registry_source(record_text: &str) -> bool {
    record_text
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with("source") && line.contains("\"registry+"))
}

fn io_cache(path: &Path, error: std::io::Error) -> CacheError {
    CacheError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn remove_dir(dir: &Path) -> Result<(), std::io::Error> {
    fs::remove_dir_all(dir)
}

pub(crate) fn cache_root(options: &crate::GlobalArgs) -> PathBuf {
    options
        .cache_dir
        .clone()
        .unwrap_or_else(gugu_compiler::default_cache_root)
}
