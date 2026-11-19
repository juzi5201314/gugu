//! `gugu publish` / `gugu yank` / `update --precise` 与 registry 依赖解析接入。
//! 实现 publishing-ecosystem 规范的 Protocol v1：索引候选收集、下载入库、
//! 发布签名与 yank；所有网络访问经 `gugu_registry::Client`（HTTPS/loopback）。

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use gugu_compiler::{
    DependencyCache, DependencySource, DependencySpec, LockGraph, Package, PackageMetadata,
    PackageSource, Project, candidates_from_lock,
};
use gugu_registry::{
    Client, IndexRecord, RegistryError, RegistryErrorKind, UreqTransport,
    auth as registry_auth_error, config as registry_config_error, has_trusted_signature,
    normalize_index_url, offline_missing, package_checksum, package_invalid, package_yanked,
    parse_secret_key, sign, signature_message, signature_required, source_not_allowed,
};
use serde_json::json;

use crate::commands::{discover, fail, format_of, parse_target_or_exit};
use crate::config::{ConfigValues, load_config};
use crate::output::{OutputFormat, emit_cli_error, emit_event};
use crate::package_cmds::archive_bytes;
use crate::{GlobalArgs, TargetName};

const TOKEN_ENV: &str = "GUGU_REGISTRY_TOKEN";

/// registry 候选收集的结果：候选列表 + 索引中标记为 yanked 的版本集合。
pub(crate) struct RegistryFetch {
    pub(crate) candidates: Vec<PackageMetadata>,
    /// `(identity, name, version)` 的 yanked 记录；`--deny-yanked` 对锁图逐一检查。
    pub(crate) yanked: BTreeSet<(String, String, String)>,
    /// resolver 使用的默认 registry 身份。
    pub(crate) default_identity: String,
    /// refresh 时被索引重新覆盖的 `(identity, name)`；调用方应移除同键锁候选，
    /// 避免同 ID 两份元数据冲突。
    pub(crate) refreshed: BTreeSet<(String, String)>,
}

/// 一次会话的 registry 视图：配置值 + HTTP 传输；client 按 identity 惰性连接。
pub(crate) struct RegistrySession {
    values: ConfigValues,
    transport: UreqTransport,
}

impl RegistrySession {
    /// 读取全部配置文件（`--config` 显式文件 + 用户/工作区文件）并叠加 CLI 策略开关。
    pub(crate) fn load(options: &GlobalArgs) -> Result<Self, i32> {
        let format = format_of(options);
        let (mut values, _paths) =
            load_config(&options.config).map_err(|message| fail(format, 2, message))?;
        if options.require_signature {
            values.require_signature = Some(true);
        }
        if options.deny_yanked {
            values.deny_yanked = Some(true);
        }
        Ok(Self {
            values,
            transport: UreqTransport::new(),
        })
    }

    fn require_signature(&self) -> bool {
        self.values.require_signature.unwrap_or(false)
    }

    fn deny_yanked(&self) -> bool {
        self.values.deny_yanked.unwrap_or(false)
    }

    /// 逻辑名（`"default"` 或 `[registries.<name>]` 名）→ 定义与规范身份。
    /// 非 allowlist 名或未定义名 → `registry-source-not-allowed`。
    fn resolve_logical(&self, logical: &str) -> Result<(String, String), RegistryError> {
        let name = if logical == "default" && !self.values.registries.contains_key("default") {
            self.values
                .registry_default
                .as_deref()
                .ok_or_else(|| registry_config_error("未配置 registry.default 默认 registry"))?
        } else {
            logical
        };
        self.check_allowed(name)?;
        let def = self
            .values
            .registries
            .get(name)
            .ok_or_else(|| source_not_allowed(format!("registry `{name}` 未在配置中定义")))?;
        let index = def
            .index
            .as_deref()
            .ok_or_else(|| registry_config_error(format!("registry `{name}` 缺少 index")))?;
        Ok((name.to_owned(), normalize_index_url(index)?))
    }

    /// 依赖声明中的 registry 字段（逻辑名或已是身份 URL）→ 规范身份。
    fn identity_of(&self, source_name: &str) -> Result<String, RegistryError> {
        if source_name.contains("://") {
            // 依赖写身份 URL：仍须命中已定义 registry 且其逻辑名过 allowlist。
            for (name, def) in &self.values.registries {
                if let Some(index) = &def.index {
                    if normalize_index_url(index).ok().as_deref() == Some(source_name) {
                        self.check_allowed(name)?;
                        return Ok(source_name.to_owned());
                    }
                }
            }
            return Err(source_not_allowed(format!(
                "registry `{source_name}` 不在已配置 registry 中"
            )));
        }
        self.resolve_logical(source_name).map(|(_, id)| id)
    }

    fn check_allowed(&self, name: &str) -> Result<(), RegistryError> {
        let allowed = &self.values.registry_allowed;
        if !allowed.is_empty() && !allowed.iter().any(|item| item == name) {
            return Err(source_not_allowed(format!(
                "registry `{name}` 不在 registry.allowed 白名单中"
            )));
        }
        Ok(())
    }

    /// resolver 用的逻辑名 → 身份别名表。
    pub(crate) fn aliases(&self) -> BTreeMap<String, String> {
        self.values
            .registries
            .iter()
            .filter_map(|(name, def)| {
                def.index
                    .as_deref()
                    .and_then(|index| normalize_index_url(index).ok())
                    .map(|identity| (name.clone(), identity))
            })
            .collect()
    }

    /// 身份 URL → 逻辑名（反查 client / 签名上下文用）。
    fn logical_for_identity(&self, identity: &str) -> Result<String, RegistryError> {
        for (name, def) in &self.values.registries {
            if let Some(index) = &def.index {
                if normalize_index_url(index).ok().as_deref() == Some(identity) {
                    return Ok(name.clone());
                }
            }
        }
        Err(source_not_allowed(format!(
            "registry `{identity}` 不在已配置 registry 中"
        )))
    }

    /// resolver 的 `default_registry`：有默认配置时为其身份，否则保持字面 `"default"`。
    pub(crate) fn default_identity(&self) -> String {
        self.resolve_logical("default")
            .map(|(_, identity)| identity)
            .unwrap_or_else(|_| "default".to_owned())
    }

    /// token 来源：环境变量优先于用户配置中的 `registries.<name>.token`。
    fn token(&self, name: &str) -> Option<String> {
        env::var(TOKEN_ENV).ok().or_else(|| {
            self.values
                .registries
                .get(name)
                .and_then(|def| def.token.clone())
        })
    }

    /// 连接（含 mirror）：config/index/下载走 mirror 网络位置，身份仍是 `index`。
    fn client_for<'a, 'c>(
        &'a self,
        clients: &'c mut BTreeMap<String, Client<'a, UreqTransport>>,
        logical: &str,
    ) -> Result<&'c Client<'a, UreqTransport>, RegistryError> {
        let (name, identity) = self.resolve_logical(logical)?;
        if clients.contains_key(&identity) {
            return Ok(&clients[&identity]);
        }
        let def = self
            .values
            .registries
            .get(&name)
            .expect("resolve_logical 已确认 registry 存在");
        let token = self.token(&name);
        let client = match &def.mirror {
            Some(mirror) => Client::connect_mirror(&self.transport, mirror, &identity, token)?,
            None => Client::connect(&self.transport, &identity, token)?,
        };
        clients.insert(identity.clone(), client);
        Ok(&clients[&identity])
    }

    /// 校验一条 index 记录的签名策略：有签名则必须可校验；
    /// require-signature 时必须有受信任有效签名，否则未信任只告警。
    fn check_signature(
        &self,
        identity: &str,
        record: &IndexRecord,
        format: OutputFormat,
    ) -> Result<(), RegistryError> {
        if record.signatures.is_empty() {
            if self.require_signature() {
                return Err(signature_required(format!(
                    "registry 记录 `{}@{}` 没有签名",
                    record.name, record.vers
                )));
            }
            return Ok(());
        }
        let mut trusted = false;
        for signature in &record.signatures {
            let message = signature_message(
                identity,
                &record.name,
                &record.vers,
                &record.cksum,
                &signature.role,
            );
            if has_trusted_signature(
                std::slice::from_ref(signature),
                &message,
                &self.values.trusted_keys,
            )? {
                trusted = true;
                break;
            }
        }
        if !trusted {
            if self.require_signature() {
                return Err(signature_required(format!(
                    "registry 记录 `{}@{}` 缺少受信任签名",
                    record.name, record.vers
                )));
            }
            warn_unverified(format, &record.name, &record.vers, identity);
        }
        Ok(())
    }

    /// 按清单依赖声明做不动点索引收集：每条记录的依赖把新 package 加入前沿，
    /// 直到闭包。返回全部候选；`offline` 时跳过网络并校验锁覆盖。
    /// `refresh`：`gugu update` 时总是抓索引提供新版本候选；
    /// 普通构建只在锁未覆盖、`--deny-yanked` 或 `--require-signature` 时抓。
    pub(crate) fn gather_candidates(
        &self,
        specs: &[DependencySpec],
        lock_candidates: &[PackageMetadata],
        offline: bool,
        refresh: bool,
    ) -> Result<RegistryFetch, RegistryError> {
        let default_identity = self.default_identity();
        let mut clients: BTreeMap<String, Client<'_, UreqTransport>> = BTreeMap::new();
        let mut refreshed: BTreeSet<(String, String)> = BTreeSet::new();
        let mut wanted: BTreeSet<(String, String)> = BTreeSet::new();
        for spec in specs {
            if let DependencySource::Registry { registry } = &spec.source {
                wanted.insert((registry.clone(), spec.package.clone()));
            }
        }
        let mut fetched: BTreeSet<(String, String)> = BTreeSet::new();
        let mut candidates: Vec<PackageMetadata> = Vec::new();
        let mut yanked: BTreeSet<(String, String, String)> = BTreeSet::new();
        loop {
            let pending: Vec<(String, String)> = wanted
                .iter()
                .filter(|(registry, package)| {
                    self.identity_of(registry)
                        .map(|identity| !fetched.contains(&(identity, package.clone())))
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            for (registry, package) in pending {
                let identity = self.identity_of(&registry)?;
                fetched.insert((identity.clone(), package.clone()));
                // offline 下 refresh 不取消锁覆盖：update --offline 仍能复用
                // 锁文件与缓存，不误报 offline-missing。
                let covered = (offline || !refresh)
                    && lock_candidates.iter().any(|metadata| {
                        metadata.id.name() == package
                            && matches!(
                                metadata.id.source(),
                                PackageSource::Registry { registry } if registry == &identity
                            )
                    });
                let need_fetch = if offline {
                    // 离线时未覆盖即 registry-offline-missing；已覆盖则不再抓索引。
                    if !covered {
                        return Err(offline_missing(format!(
                            "--offline 且 package `{package}` 不在锁文件中"
                        )));
                    }
                    false
                } else {
                    // --deny-yanked / --require-signature 需要已锁定版本的索引记录。
                    !covered || self.deny_yanked() || self.require_signature()
                };
                if !need_fetch {
                    continue;
                }
                if !covered {
                    refreshed.insert((identity.clone(), package.clone()));
                }
                let client = self.client_for(&mut clients, &registry)?;
                let records = client.index(&package)?;
                for record in &records {
                    if record.yanked {
                        yanked.insert((identity.clone(), record.name.clone(), record.vers.clone()));
                    }
                    // covered 时只入队锁定版本的依赖：锁定包的传递依赖在严格策略下
                    // 仍要检查，但未被锁定版本引用其它 registry 不应阻断构建。
                    let locked_version = covered
                        && lock_candidates.iter().any(|metadata| {
                            metadata.id.name() == package
                                && metadata.id.version().to_string() == record.vers
                                && matches!(
                                    metadata.id.source(),
                                    PackageSource::Registry { registry } if registry == &identity
                                )
                        });
                    if !covered || locked_version {
                        for dep in &record.deps {
                            wanted.insert((
                                dep.registry.clone().unwrap_or_else(|| "default".to_owned()),
                                dep.package.clone(),
                            ));
                        }
                    }
                    if covered {
                        // 锁已覆盖：只收集 yanked 状态，不把其他版本喂给 resolver。
                        continue;
                    }
                    let mut metadata = record.to_metadata(&identity)?;
                    for dep in &mut metadata.dependencies {
                        if let DependencySource::Registry { registry } = &mut dep.source {
                            *registry = self.identity_of(registry)?;
                        }
                    }
                    candidates.push(metadata);
                }
            }
        }
        Ok(RegistryFetch {
            candidates,
            yanked,
            default_identity,
            refreshed,
        })
    }

    /// 校验锁图中某版本的索引记录：checksum 一致、yanked 策略、签名策略。
    /// 只针对最终选中的版本；与版本无关的无签名记录不阻断解析。
    fn verify_locked_record(
        &self,
        identity: &str,
        name: &str,
        version: &str,
        checksum: &str,
        format: OutputFormat,
    ) -> Result<(Client<'_, UreqTransport>, IndexRecord), RegistryError> {
        let logical = self.logical_for_identity(identity)?;
        let mut clients: BTreeMap<String, Client<'_, UreqTransport>> = BTreeMap::new();
        let client = self.client_for(&mut clients, &logical)?;
        let records = client.index(name)?;
        let record = records
            .into_iter()
            .find(|record| record.vers == version)
            .ok_or_else(|| package_invalid(format!("索引中不存在 `{name}@{version}`")))?;
        if record.cksum != checksum {
            return Err(package_checksum(format!(
                "锁定的 checksum 与索引记录不一致：`{name}@{version}`"
            )));
        }
        if record.yanked && self.deny_yanked() {
            return Err(package_yanked(format!(
                "`{name}@{version}` 已被 yank（--deny-yanked）"
            )));
        }
        self.check_signature(identity, &record, format)?;
        Ok((clients.remove(identity).expect("client 已连接"), record))
    }

    /// 下载索引中某版本的归档并写入内容寻址缓存；
    /// 校验锁 checksum 与索引记录一致、`--deny-yanked` 与签名策略，
    /// 并核对归档内清单的 owner/name/version 与锁 ID 一致。
    pub(crate) fn fetch_and_store(
        &self,
        identity: &str,
        name: &str,
        version: &str,
        checksum: &str,
        cache: &DependencyCache,
        format: OutputFormat,
    ) -> Result<(), RegistryError> {
        let (client, record) =
            self.verify_locked_record(identity, name, version, checksum, format)?;
        let archive = client.download(&record)?;
        verify_archive_manifest(&archive, name, version)?;
        cache
            .store_archive(record.to_metadata(identity)?.id, checksum, &archive)
            .map(|_| ())
            .map_err(|error| package_checksum(format!("registry 归档校验失败：{error}")))
    }

    /// 与 `verify_locked_record` 相同的检查，但不下载：
    /// 缓存命中时在 `--deny-yanked`/`--require-signature` 下仍验证索引记录。
    pub(crate) fn verify_locked_package(
        &self,
        identity: &str,
        name: &str,
        version: &str,
        checksum: &str,
        format: OutputFormat,
    ) -> Result<(), RegistryError> {
        self.verify_locked_record(identity, name, version, checksum, format)
            .map(|_| ())
    }
}

/// 核对归档内 `gugu.toml` 的 `owner`/`name`/`version` 与请求 ID 一致；
/// 防止被污染的索引把另一个 package 的内容按所请求依赖入库。
fn verify_archive_manifest(archive: &[u8], name: &str, version: &str) -> Result<(), RegistryError> {
    use flate2::read::GzDecoder;
    let mut manifest_text = None;
    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    for entry in tar
        .entries()
        .map_err(|error| package_invalid(format!("registry 归档不是合法 tar：{error}")))?
    {
        let mut entry =
            entry.map_err(|error| package_invalid(format!("registry 归档读取失败：{error}")))?;
        let path = entry
            .path()
            .map_err(|error| package_invalid(format!("registry 归档路径非法：{error}")))?
            .to_path_buf();
        if path.as_os_str() == "gugu.toml" {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut entry, &mut text)
                .map_err(|error| package_invalid(format!("registry 归档清单读取失败：{error}")))?;
            manifest_text = Some(text);
            break;
        }
    }
    let text = manifest_text.ok_or_else(|| package_invalid("registry 归档缺少 `gugu.toml`"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|error| package_invalid(format!("registry 归档清单解析失败：{error}")))?;
    let package = table
        .get("package")
        .and_then(|value| value.as_table())
        .ok_or_else(|| package_invalid("registry 归档清单缺少 [package]"))?;
    let owner = package
        .get("owner")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let package_name = package
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let package_version = package
        .get("version")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let actual = if owner.is_empty() {
        package_name.to_owned()
    } else {
        format!("{owner}/{package_name}")
    };
    if actual != name || package_version != version {
        return Err(package_invalid(format!(
            "归档清单 `{actual}@{package_version}` 与请求 `{name}@{version}` 不一致"
        )));
    }
    Ok(())
}

fn warn_unverified(format: OutputFormat, name: &str, version: &str, identity: &str) {
    let message =
        format!("registry 记录 `{name}@{version}` 的签名未被信任（registry `{identity}`）");
    match format {
        OutputFormat::Text => eprintln!("warning: signature-unverified: {message}"),
        _ => emit_event(
            "warning",
            json!({ "code": "signature-unverified", "message": message }),
        ),
    }
}

fn registry_fail(format: OutputFormat, error: RegistryError) -> i32 {
    emit_cli_error(format, &error.to_string());
    match error.kind() {
        RegistryErrorKind::SourceNotAllowed
        | RegistryErrorKind::OfflineMissing
        | RegistryErrorKind::SignatureRequired => 2,
        _ => 1,
    }
}

/// `gugu publish`：构建归档 → 校验清单发布约束 → （可选）签名 → POST /publish。
pub(crate) fn run_publish(
    dry_run: bool,
    registry: Option<&str>,
    sign_key: Option<&Path>,
    options: &GlobalArgs,
) -> i32 {
    let format = format_of(options);
    let (project, packages) = match discover(options) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if packages.len() != 1 {
        return fail(
            format,
            2,
            "gugu publish 需要唯一 package；workspace 请用 -p 指定",
        );
    }
    let package = &packages[0];
    let collected = match gugu_compiler::PackageFiles::from_directory(package.root()) {
        Ok(files) => files,
        Err(error) => return fail(format, 1, error.to_string()),
    };
    let files = match crate::package_cmds::filter_archive_files(package, &collected) {
        Ok(files) => files,
        Err(message) => return fail(format, 1, message),
    };
    if let Err(message) = validate_publish_manifest(package) {
        return fail(format, 1, message);
    }
    // 规范步骤 5：以 --locked 等价解析检查清单与锁图一致。
    let target = match parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    // 有锁文件时以 --locked 等价解析检查清单与锁图一致；锁图按整个
    // workspace 解析，publish -p 也不因其他成员存在而误报不一致。
    // 无锁文件时正常解析会生成锁图，但用户显式 --locked/--frozen 保持失败语义。
    let mut check = options.clone();
    check.locked = options.locked || project.lock_path().exists();
    let refs = project.packages().iter().collect::<Vec<_>>();
    if crate::resolve_project_lock(&project, &refs, &check, target, format, false).is_err() {
        return 1;
    }
    let checksum = files.checksum();
    let archive = match archive_bytes(&files) {
        Ok(archive) => archive,
        Err(message) => return fail(format, 1, message),
    };
    let session = match RegistrySession::load(options) {
        Ok(session) => session,
        Err(code) => return code,
    };
    let logical = registry.unwrap_or("default");
    let (name, identity) = match session.resolve_logical(logical) {
        Ok(value) => value,
        Err(error) => return registry_fail(format, error),
    };
    let package_id = match package.owner() {
        Some(owner) => format!("{owner}/{}@{}", package.name(), package.version()),
        None => format!("{}@{}", package.name(), package.version()),
    };
    if dry_run {
        emit_event(
            "publish-result",
            json!({
                "registry": identity,
                "package": package_id,
                "checksum": checksum,
                "archive_bytes": archive.len(),
                "dry_run": true,
                "idempotent": false,
            }),
        );
        return 0;
    }
    let token = match session.token(&name) {
        Some(token) => token,
        None => {
            return registry_fail(
                format,
                registry_auth_error(format!(
                    "registry `{name}` 发布需要 token（{TOKEN_ENV} 或用户配置 token）"
                )),
            );
        }
    };
    let mut clients: BTreeMap<String, Client<'_, UreqTransport>> = BTreeMap::new();
    let client = match session.client_for(&mut clients, logical) {
        Ok(client) => client,
        Err(error) => return registry_fail(format, error),
    };
    let signature = match sign_key {
        Some(path) => match sign_archive(&identity, &package_id, &checksum, path) {
            Ok(record) => Some(record),
            Err(error) => return registry_fail(format, error),
        },
        None => None,
    };
    match client.publish(&archive, &checksum, &token, signature.as_deref()) {
        Ok(outcome) => {
            emit_event(
                "publish-result",
                json!({
                    "registry": identity,
                    "package": package_id,
                    "checksum": outcome.cksum,
                    "archive_bytes": archive.len(),
                    "dry_run": false,
                    "idempotent": outcome.idempotent,
                }),
            );
            0
        }
        Err(error) => registry_fail(format, error),
    }
}

/// 生成 `--sign` 的发布者签名记录 JSON。
fn sign_archive(
    identity: &str,
    package_id: &str,
    checksum: &str,
    key_path: &Path,
) -> Result<String, RegistryError> {
    let encoded = fs::read_to_string(key_path).map_err(|error| {
        package_invalid(format!(
            "读取签名私钥 `{}` 失败：{error}",
            key_path.display()
        ))
    })?;
    let secret = parse_secret_key(&encoded)?;
    let (name, version) = package_id
        .rsplit_once('@')
        .ok_or_else(|| package_invalid("package ID 缺少 `@version`"))?;
    let message = signature_message(identity, name, version, checksum, "publisher");
    let record = sign(&secret, &message)?;
    serde_json::to_string(&record)
        .map_err(|error| RegistryError::new(RegistryErrorKind::Protocol, error.to_string()))
}

/// 清单发布约束：`publish = true`，path 依赖必须同时声明 version 回退，
/// Git 依赖必须锁定 `rev`。
fn validate_publish_manifest(package: &Package) -> Result<(), String> {
    let text = fs::read_to_string(package.manifest())
        .map_err(|error| format!("读取清单 `{}` 失败：{error}", package.manifest().display()))?;
    let value: toml::Table = text
        .parse()
        .map_err(|error| format!("解析清单失败：{error}"))?;
    let package_table = value
        .get("package")
        .and_then(|table| table.as_table())
        .ok_or_else(|| "清单缺少 [package]".to_owned())?;
    if package_table.get("publish").and_then(|v| v.as_bool()) != Some(true) {
        return Err("发布要求 [package] 设置 `publish = true`".to_owned());
    }
    // 与清单解析一致的依赖表：`build` 是子表、`target.<cond>` 是条件表。
    let mut tables: Vec<(String, &toml::Table)> = Vec::new();
    for (label, deps) in [
        ("dependencies", value.get("dependencies")),
        ("test-dependencies", value.get("test-dependencies")),
        (
            "build.dependencies",
            value
                .get("build")
                .and_then(|build| build.get("dependencies")),
        ),
    ] {
        if let Some(table) = deps.and_then(toml::Value::as_table) {
            tables.push((label.to_owned(), table));
        }
    }
    for section in [
        value.get("target"),
        value.get("build").and_then(|build| build.get("target")),
    ] {
        let Some(targets) = section.and_then(toml::Value::as_table) else {
            continue;
        };
        for (condition, table) in targets {
            let Some(table) = table.as_table() else {
                continue;
            };
            for (key, deps) in [
                ("dependencies", table.get("dependencies")),
                ("test-dependencies", table.get("test-dependencies")),
            ] {
                if let Some(deps) = deps.and_then(toml::Value::as_table) {
                    tables.push((format!("target.{condition}.{key}"), deps));
                }
            }
        }
    }
    for (section, deps) in tables {
        for (dep_name, dep) in deps {
            let Some(dep_table) = dep.as_table() else {
                continue;
            };
            if dep_table.contains_key("path") && !dep_table.contains_key("version") {
                return Err(format!(
                    "path 依赖 `{dep_name}`（{section}）发布时必须同时提供 registry version 回退"
                ));
            }
            if dep_table.contains_key("git") && !dep_table.contains_key("rev") {
                return Err(format!(
                    "Git 依赖 `{dep_name}`（{section}）发布时必须锁定 `rev`"
                ));
            }
        }
    }
    Ok(())
}

/// `gugu yank <name> --version <v> [--undo]`。
pub(crate) fn run_yank(package_name: &str, version: &str, undo: bool, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let session = match RegistrySession::load(options) {
        Ok(session) => session,
        Err(code) => return code,
    };
    let (name, identity) = match session.resolve_logical("default") {
        Ok(value) => value,
        Err(error) => return registry_fail(format, error),
    };
    let token = match session.token(&name) {
        Some(token) => token,
        None => {
            return registry_fail(
                format,
                registry_auth_error(format!(
                    "registry `{name}` yank 需要 token（{TOKEN_ENV} 或用户配置 token）"
                )),
            );
        }
    };
    let mut clients: BTreeMap<String, Client<'_, UreqTransport>> = BTreeMap::new();
    let client = match session.client_for(&mut clients, "default") {
        Ok(client) => client,
        Err(error) => return registry_fail(format, error),
    };
    match client.yank(package_name, version, !undo, &token) {
        Ok(outcome) => {
            emit_event(
                "yank-result",
                json!({
                    "registry": identity,
                    "package": format!("{}@{}", outcome.name, outcome.vers),
                    "yanked": outcome.yanked,
                }),
            );
            0
        }
        Err(error) => registry_fail(format, error),
    }
}

/// `gugu update --precise <v>`：把 `-p` 指定 package 的 registry 候选
/// 收窄到精确版本后重解析，写入新锁。
pub(crate) fn run_update_precise(version: &str, options: &GlobalArgs) -> i32 {
    let format = format_of(options);
    let Some(package_name) = options.package.as_deref() else {
        return fail(format, 2, "gugu update --precise 需要 -p 指定 package");
    };
    let target = match crate::commands::parse_target_or_exit(options) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let project = match Project::discover(env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    {
        Ok(project) => project,
        Err(error) => return fail(format, 2, error.to_string()),
    };
    let session = match RegistrySession::load(options) {
        Ok(session) => session,
        Err(code) => return code,
    };
    let path = project.lock_path();
    let existing = if path.exists() {
        match LockGraph::read(&path) {
            Ok(graph) => Some(graph),
            Err(error) => return fail(format, 1, error.to_string()),
        }
    } else {
        None
    };
    let (mut lock_candidates, git_candidates) = existing
        .as_ref()
        .map(candidates_from_lock)
        .unwrap_or_default();
    let specs = match project.dependency_specs() {
        Ok(specs) => specs,
        Err(error) => return fail(format, 1, error.to_string()),
    };
    let fetch = match session.gather_candidates(&specs, &lock_candidates, options.offline, true) {
        Ok(fetch) => fetch,
        Err(error) => return registry_fail(format, error),
    };
    let mut registry_candidates = fetch.candidates;
    // refresh 抓取的索引记录覆盖同键锁候选，避免同 ID 两份元数据冲突。
    lock_candidates.retain(|metadata| {
        !matches!(
            metadata.id.source(),
            PackageSource::Registry { registry }
                if fetch
                    .refreshed
                    .contains(&(registry.clone(), metadata.id.name().to_owned()))
        )
    });
    // 锁定目标 package 到精确版本：其他版本候选全部移除。
    registry_candidates.retain(|metadata| {
        metadata.id.name() != package_name || metadata.id.version().to_string() == version
    });
    lock_candidates.retain(|metadata| {
        metadata.id.name() != package_name || metadata.id.version().to_string() == version
    });
    let available = registry_candidates
        .iter()
        .chain(lock_candidates.iter())
        .any(|metadata| metadata.id.name() == package_name);
    if !available {
        return fail(
            format,
            1,
            format!("registry 中没有 `{package_name}@{version}` 候选"),
        );
    }
    registry_candidates.extend(lock_candidates);
    let roots: Vec<String> = project
        .packages()
        .iter()
        .map(|package| package.package_name())
        .collect();
    let host = TargetName::host().unwrap_or(target);
    let graph = match project.resolve_dependencies(gugu_compiler::ResolveOptions {
        target: target.to_string(),
        host: host.to_string(),
        roots,
        registry_packages: registry_candidates,
        git_packages: git_candidates,
        default_registry: fetch.default_identity.clone(),
        ..Default::default()
    }) {
        Ok(graph) => graph,
        Err(error) => return fail(format, 1, error.to_string()),
    };
    let resolved = graph
        .packages
        .iter()
        .find(|package| package.id.name() == package_name);
    match resolved {
        Some(package) if package.id.version().to_string() == version => {}
        Some(package) => {
            return fail(
                format,
                1,
                format!(
                    "解析结果 `{package_name}@{}` 与 --precise {version} 不一致",
                    package.id.version()
                ),
            );
        }
        None => {
            return fail(format, 1, format!("`{package_name}` 不在解析图中"));
        }
    }
    match graph.write(&path) {
        Ok(()) => 0,
        Err(error) => fail(format, 1, error.to_string()),
    }
}

/// 解析后保证 registry package 已入库：缺失时按锁 checksum 下载。
pub(crate) fn ensure_registry_inputs(
    session: &RegistrySession,
    graph: &LockGraph,
    cache: &DependencyCache,
    yanked: &BTreeSet<(String, String, String)>,
    offline: bool,
    vendor: bool,
    format: OutputFormat,
) -> Result<(), i32> {
    for package in &graph.packages {
        let PackageSource::Registry { registry } = package.id.source() else {
            continue;
        };
        let Some(checksum) = package.checksum.clone() else {
            return Err(registry_fail(
                format,
                package_checksum(format!("registry package `{}` 缺少 checksum", package.id)),
            ));
        };
        // --deny-yanked：锁图引用 yanked 版本即失败。
        if session.deny_yanked()
            && yanked.contains(&(
                registry.clone(),
                package.id.name().to_owned(),
                package.id.version().to_string(),
            ))
        {
            return Err(registry_fail(
                format,
                package_yanked(format!("锁图引用已 yank 的版本 `{}`", package.id)),
            ));
        }
        // 严格策略下先验证索引记录中的 yanked/签名状态：缓存命中与
        // vendor 输入都不豁免（--offline 无网络可验证，由校验和与锁图兜底）。
        if !offline && (session.deny_yanked() || session.require_signature()) {
            session
                .verify_locked_package(
                    registry,
                    package.id.name(),
                    &package.id.version().to_string(),
                    &checksum,
                    format,
                )
                .map_err(|error| registry_fail(format, error))?;
        }
        if cache.load(&package.id, Some(&checksum)).is_ok() {
            continue;
        }
        if vendor {
            // vendor 模式输入全部来自 vendor 目录，不从 registry 下载。
            continue;
        }
        if offline {
            return Err(registry_fail(
                format,
                offline_missing(format!("--offline 且 package `{}` 不在缓存中", package.id)),
            ));
        }
        session
            .fetch_and_store(
                registry,
                package.id.name(),
                &package.id.version().to_string(),
                &checksum,
                cache,
                format,
            )
            .map_err(|error| registry_fail(format, error))?;
    }
    Ok(())
}
