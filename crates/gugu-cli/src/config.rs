use std::collections::BTreeMap;
use std::{env, fs, path::PathBuf};

use serde::Deserialize;

/// `[registries.<name>]` 定义：index 是规范身份来源，token 只允许用户配置写入。
#[derive(Clone, Debug, Default)]
pub(crate) struct RegistryDef {
    pub(crate) index: Option<String>,
    pub(crate) api: Option<String>,
    pub(crate) mirror: Option<String>,
    pub(crate) token: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigValues {
    pub(crate) target: Option<String>,
    pub(crate) cache_dir: Option<PathBuf>,
    pub(crate) target_dir: Option<PathBuf>,
    pub(crate) require_signature: Option<bool>,
    pub(crate) deny_yanked: Option<bool>,
    /// `registry.default`：未显式写 `registry` 的依赖使用的 registry 名。
    pub(crate) registry_default: Option<String>,
    /// `registry.allowed`：非空时限制可用 registry 名集合。
    pub(crate) registry_allowed: Vec<String>,
    /// `registry.trusted-keys`：key_id -> base64url 公钥。
    pub(crate) trusted_keys: BTreeMap<String, String>,
    /// `[registries.<name>]` 定义。
    pub(crate) registries: BTreeMap<String, RegistryDef>,
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
    registries: BTreeMap<String, RegistryDefConfig>,
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
    #[serde(rename = "require-signature")]
    require_signature: Option<bool>,
    #[serde(rename = "deny-yanked")]
    deny_yanked: Option<bool>,
    default: Option<String>,
    allowed: Option<Vec<String>>,
    #[serde(rename = "trusted-keys")]
    trusted_keys: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RegistryDefConfig {
    index: Option<String>,
    api: Option<String>,
    mirror: Option<String>,
    token: Option<String>,
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
    /// `allow_token`：只有用户配置文件允许携带 `token`。
    pub(crate) fn apply(&mut self, config: ConfigFile, allow_token: bool) {
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
        if config.registry.default.is_some() {
            self.registry_default = config.registry.default;
        }
        if let Some(allowed) = config.registry.allowed {
            self.registry_allowed = allowed;
        }
        if let Some(keys) = config.registry.trusted_keys {
            self.trusted_keys.extend(keys);
        }
        for (name, def) in config.registries {
            let entry = self.registries.entry(name).or_default();
            if def.index.is_some() {
                entry.index = def.index;
            }
            if def.api.is_some() {
                entry.api = def.api;
            }
            if def.mirror.is_some() {
                entry.mirror = def.mirror;
            }
            if allow_token && def.token.is_some() {
                entry.token = def.token;
            }
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

impl ConfigValues {
    /// 兼容旧调用：等价于 `apply(config, false)`。
    #[cfg(test)]
    fn apply_test(&mut self, config: ConfigFile) {
        self.apply(config, false);
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
    let user_path = user_config_file();
    for path in &paths {
        let source = fs::read_to_string(path)
            .map_err(|error| format!("无法读取配置文件 `{}`：{error}", path.display()))?;
        let config = toml::from_str::<ConfigFile>(&source)
            .map_err(|error| format!("配置文件 `{}` 格式错误：{error}", path.display()))?;
        if config.registries.values().any(|def| def.token.is_some())
            && user_path.as_ref() != Some(path)
        {
            return Err(format!(
                "配置文件 `{}` 包含 registry token；token 只能写在用户配置",
                path.display()
            ));
        }
        values.apply(config, user_path.as_ref() == Some(path));
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

pub(crate) fn user_config_file() -> Option<PathBuf> {
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
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

pub(crate) fn environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::{ConfigFile, ConfigValues, environment_flag};
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        _lock: MutexGuard<'static, ()>,
        name: String,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &str, value: Option<&str>) -> Self {
            let lock = ENV_LOCK.lock().expect("environment test lock");
            let previous = std::env::var_os(name);
            unsafe { std::env::set_var(name, value.unwrap_or_default()) };
            Self {
                _lock: lock,
                name: name.to_owned(),
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(&self.name, value) },
                None => unsafe { std::env::remove_var(&self.name) },
            }
        }
    }

    #[test]
    fn registry_config_deserializes_kebab_case_keys() {
        let config = toml::from_str::<ConfigFile>(
            "[registry]\nrequire-signature = true\ndeny-yanked = true\n",
        )
        .expect("registry config parses");
        let mut values = ConfigValues::default();
        values.apply_test(config);
        assert_eq!(values.require_signature, Some(true));
        assert_eq!(values.deny_yanked, Some(true));
    }

    #[test]
    fn environment_flag_is_enabled_for_any_non_empty_value() {
        {
            let _guard = EnvVarGuard::set("GUGU_OFFLINE_TEST", Some("yes"));
            assert!(environment_flag("GUGU_OFFLINE_TEST"));
        }
        {
            let _guard = EnvVarGuard::set("GUGU_OFFLINE_TEST", Some(""));
            assert!(!environment_flag("GUGU_OFFLINE_TEST"));
        }
    }
}
