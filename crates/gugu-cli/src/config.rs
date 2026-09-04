use std::{env, fs, path::PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigValues {
    pub(crate) target: Option<String>,
    pub(crate) cache_dir: Option<PathBuf>,
    pub(crate) target_dir: Option<PathBuf>,
    pub(crate) require_signature: Option<bool>,
    pub(crate) deny_yanked: Option<bool>,
    pub(crate) permission: Option<bool>,
    pub(crate) read_allows: Vec<PathBuf>,
    pub(crate) write_allows: Vec<PathBuf>,
    pub(crate) env_allows: Vec<String>,
    pub(crate) net_allows: Vec<String>,
    pub(crate) run_allows: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ConfigFile {
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
    pub(crate) fn apply(&mut self, config: ConfigFile) {
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

pub(crate) fn load_config(explicit: &[PathBuf]) -> Result<(ConfigValues, Vec<PathBuf>), String> {
    let mut paths = Vec::new();
    if let Some(path) = user_config_file() {
        append_optional_config(&mut paths, path)?;
    }
    if let Some(path) = workspace_config_file() {
        append_optional_config(&mut paths, path)?;
    }
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

/// 按 workspace 根读取本地配置：从当前目录向父目录找最近的带
/// `[workspace]` 的 `gugu.toml`，用其目录下的 `.gugu/config.toml`；
/// 没有 workspace 时退回最近的 `gugu.toml` 所在目录。
fn workspace_config_file() -> Option<PathBuf> {
    let start = env::current_dir().ok()?;
    let mut directory = start.as_path();
    let mut nearest_manifest = None;
    loop {
        let candidate = directory.join("gugu.toml");
        if candidate.is_file() {
            let has_workspace = fs::read_to_string(&candidate)
                .ok()
                .and_then(|source| toml::from_str::<toml::Value>(&source).ok())
                .is_some_and(|table| table.get("workspace").is_some());
            if has_workspace {
                return Some(directory.join(".gugu").join("config.toml"));
            }
            if nearest_manifest.is_none() {
                nearest_manifest = Some(directory.to_path_buf());
            }
        }
        if !directory.parent().is_some_and(|parent| {
            directory = parent;
            true
        }) {
            break;
        }
    }
    nearest_manifest.map(|directory| directory.join(".gugu").join("config.toml"))
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

pub(crate) fn environment_text(name: &str) -> Result<Option<String>, String> {
    match env::var_os(name) {
        Some(value) => value
            .into_string()
            .map(Some)
            .map_err(|_| format!("环境变量 `{name}` 不是有效 UTF-8")),
        None => Ok(None),
    }
}

pub(crate) fn environment_flag(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| {
        let string = value.to_string_lossy();
        string == "1" || string.eq_ignore_ascii_case("true")
    })
}

pub(crate) fn environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
