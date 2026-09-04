use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::error::ProjectError;
use super::{
    DependencySource, DependencySpec, LockGraph, PackageId, PackageMetadata, PackageSource,
    TargetCondition, VersionReq,
};
use package_files::{parse_archive, validate_file_path};

const CACHE_VERSION: u32 = 1;
const VENDOR_VERSION: u32 = 1;
const PACKAGE_RECORD: &str = "record.toml";
const VENDOR_RECORD: &str = ".gugu-vendor.toml";

const MAX_CACHE_PATH_BYTES: usize = 4096;
#[path = "action_key.rs"]
mod action_key;
#[path = "package_files.rs"]
mod package_files;
#[path = "target_view.rs"]
mod target_view;

pub use action_key::{ActionInputs, ActionKey};
pub use package_files::PackageFiles;
pub use target_view::{TargetArtifact, TargetView};
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// 依赖输入的来源选择策略。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachePolicy {
    /// 是否禁止网络和未登记的外部输入。
    pub offline: bool,
    /// 是否要求调用方保留并验证既有锁图。
    pub locked: bool,
    /// 是否只从 workspace vendor 树读取外部依赖。
    pub vendor: bool,
}

impl CachePolicy {
    /// 返回同时启用锁定和离线的策略。
    pub const fn frozen() -> Self {
        Self {
            offline: true,
            locked: true,
            vendor: false,
        }
    }
}
/// 一个已通过 checksum 或本地路径校验的依赖输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyInput {
    package: PackageId,
    files: PackageFiles,
}
impl DependencyInput {
    /// 创建 package 输入并校验所有逻辑路径。
    pub fn new(package: PackageId, files: PackageFiles) -> Self {
        Self { package, files }
    }

    /// 返回 package 身份。
    pub fn package(&self) -> &PackageId {
        &self.package
    }

    /// 返回 package 文件集合。
    pub fn files(&self) -> &PackageFiles {
        &self.files
    }

    /// 返回 package 内容 checksum。
    pub fn checksum(&self) -> String {
        self.files.checksum()
    }
}

/// 依赖缓存、归档输入与 vendor 验证的错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheError {
    /// 文件系统操作失败。
    Io {
        /// 相关路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
    /// 缓存或 vendor 结构无效。
    Invalid {
        /// 相关路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
    /// 所需 package 不在允许的输入源中。
    Missing {
        /// 缺少的 package 身份。
        package: String,
    },
    /// package checksum 不匹配。
    Checksum {
        /// 相关 package 身份。
        package: String,
        /// 期望 checksum。
        expected: String,
        /// 实际 checksum。
        actual: String,
    },
    /// vendor 映射和锁图不一致。
    VendorMismatch {
        /// 相关路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
    /// 缓存条目损坏并已尝试隔离。
    Corrupt {
        /// 原缓存路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => {
                write!(formatter, "缓存 I/O `{}` 失败：{message}", path.display())
            }
            Self::Invalid { path, message } => {
                write!(formatter, "缓存输入 `{}` 无效：{message}", path.display())
            }
            Self::Missing { package } => write!(formatter, "缺少已验证的依赖输入 `{package}`"),
            Self::Checksum {
                package,
                expected,
                actual,
            } => write!(
                formatter,
                "package `{package}` checksum 不匹配：期望 {expected}，实际 {actual}"
            ),
            Self::VendorMismatch { path, message } => {
                write!(formatter, "vendor `{}` 不一致：{message}", path.display())
            }
            Self::Corrupt { path, message } => {
                write!(formatter, "缓存条目 `{}` 已损坏：{message}", path.display())
            }
        }
    }
}

impl std::error::Error for CacheError {}

/// 全局依赖源码缓存。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyCache {
    root: PathBuf,
}

impl DependencyCache {
    /// 创建指向缓存根的句柄；目录在第一次写入时创建。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回缓存根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把已验证的 package 文件以内容寻址条目写入缓存。
    pub fn store(&self, input: &DependencyInput) -> Result<(), CacheError> {
        let key = package_key(input.package());
        let entries = self.package_entries_dir();
        fs::create_dir_all(&entries).map_err(|error| io_error(&entries, error))?;
        let destination = entries.join(&key);
        if destination.exists() {
            let expected = if matches!(input.package().source(), PackageSource::Registry { .. }) {
                Some(input.checksum())
            } else {
                None
            };
            match self.load(input.package(), expected.as_deref()) {
                Ok(_) => return Ok(()),
                Err(CacheError::Corrupt { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let temporary = self.create_temp_dir("package")?;
        let result = self.write_entry(input, &temporary).and_then(|()| {
            match fs::rename(&temporary, &destination) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    remove_path(&temporary)?;
                    let expected =
                        if matches!(input.package().source(), PackageSource::Registry { .. }) {
                            Some(input.checksum())
                        } else {
                            None
                        };
                    self.load(input.package(), expected.as_deref()).map(|_| ())
                }
                Err(error) => Err(io_error(&destination, error)),
            }
        });
        if result.is_err() {
            let _ = remove_path(&temporary);
        }
        result
    }

    /// 解包并验证 gzip tar package archive，再存入依赖缓存。
    pub fn store_archive(
        &self,
        package: PackageId,
        expected_checksum: &str,
        archive: impl AsRef<[u8]>,
    ) -> Result<DependencyInput, CacheError> {
        if !matches!(package.source(), PackageSource::Registry { .. }) {
            return Err(invalid_error(
                Path::new("<archive>"),
                "归档输入必须属于 registry package",
            ));
        }
        validate_checksum_text(expected_checksum)?;
        let files = parse_archive(archive.as_ref())?;
        let actual = files.checksum();
        if actual != expected_checksum {
            return Err(CacheError::Checksum {
                package: package.to_string(),
                expected: expected_checksum.to_owned(),
                actual,
            });
        }
        let input = DependencyInput::new(package, files);
        self.store(&input)?;
        Ok(input)
    }

    /// 从缓存读取 package，并在 checksum 变化时隔离损坏条目。
    pub fn load(
        &self,
        package: &PackageId,
        expected_checksum: Option<&str>,
    ) -> Result<DependencyInput, CacheError> {
        let destination = self.package_entries_dir().join(package_key(package));
        if !destination.is_dir() {
            if destination.exists() {
                return Err(self.quarantine(
                    &destination,
                    invalid_error(&destination, "缓存条目不是目录"),
                ));
            }
            return Err(CacheError::Missing {
                package: package.to_string(),
            });
        }
        match self.read_entry(package, &destination, expected_checksum) {
            Ok(input) => Ok(input),
            Err(error) => Err(self.quarantine(&destination, error)),
        }
    }

    fn write_entry(&self, input: &DependencyInput, directory: &Path) -> Result<(), CacheError> {
        let files_directory = directory.join("files");
        fs::create_dir_all(&files_directory).map_err(|error| io_error(&files_directory, error))?;
        let mut records = Vec::with_capacity(input.files.files().len());
        for (path, bytes) in input.files.files() {
            let file = files_directory.join(path);
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
            write_file(&file, bytes)?;
            records.push(CacheFileRecord {
                path: path.clone(),
                length: u64::try_from(bytes.len()).expect("file length fits u64"),
                hash: hex_encode(blake3::hash(bytes).as_bytes()),
            });
        }
        let record = CacheRecord {
            version: CACHE_VERSION,
            name: input.package.name().to_owned(),
            package_version: input.package.version().to_string(),
            source: input.package.source().to_string(),
            checksum: match input.package.source() {
                PackageSource::Registry { .. } => Some(input.checksum()),
                _ => None,
            },
            files: records,
        };
        let text = toml::to_string(&record).map_err(|error| invalid_error(directory, error))?;
        write_file(&directory.join(PACKAGE_RECORD), text.as_bytes())?;
        Ok(())
    }

    fn read_entry(
        &self,
        package: &PackageId,
        directory: &Path,
        expected_checksum: Option<&str>,
    ) -> Result<DependencyInput, CacheError> {
        validate_cache_layout(directory)?;
        let record_path = directory.join(PACKAGE_RECORD);
        let record_text =
            fs::read_to_string(&record_path).map_err(|error| invalid_error(&record_path, error))?;
        let record = toml::from_str::<CacheRecord>(&record_text)
            .map_err(|error| invalid_error(&record_path, error))?;
        validate_record(package, &record, &record_path)?;
        if expected_checksum.is_some_and(|checksum| record.checksum.as_deref() != Some(checksum)) {
            return Err(invalid_error(
                &record_path,
                "锁图 checksum 与缓存记录不一致",
            ));
        }
        let mut files = BTreeMap::new();
        let files_directory = directory.join("files");
        for file_record in &record.files {
            validate_file_path(&file_record.path)?;
            if files.contains_key(&file_record.path) {
                return Err(invalid_error(&record_path, "缓存记录包含重复文件"));
            }
            let path = files_directory.join(&file_record.path);
            let bytes = fs::read(&path).map_err(|error| invalid_error(&path, error))?;
            if u64::try_from(bytes.len()).expect("file length fits u64") != file_record.length
                || hex_encode(blake3::hash(&bytes).as_bytes()) != file_record.hash
            {
                return Err(invalid_error(&path, "文件长度或 BLAKE3 摘要不一致"));
            }
            files.insert(file_record.path.clone(), bytes);
        }
        let actual_files = PackageFiles::from_directory(&files_directory)?;
        if actual_files.files() != &files {
            return Err(invalid_error(&files_directory, "缓存目录包含未登记文件"));
        }
        let actual_checksum = actual_files.checksum();
        if let Some(record_checksum) = record.checksum.as_deref() {
            if record_checksum != actual_checksum {
                return Err(CacheError::Checksum {
                    package: package.to_string(),
                    expected: record_checksum.to_owned(),
                    actual: actual_checksum,
                });
            }
        }
        if let Some(expected) = expected_checksum {
            if expected != actual_checksum {
                return Err(CacheError::Checksum {
                    package: package.to_string(),
                    expected: expected.to_owned(),
                    actual: actual_checksum,
                });
            }
        }
        Ok(DependencyInput::new(package.clone(), actual_files))
    }

    fn quarantine(&self, entry: &Path, error: CacheError) -> CacheError {
        let quarantine = self.root.join("dependencies/v1/quarantine");
        if fs::create_dir_all(&quarantine).is_ok() {
            let name = entry.file_name().unwrap_or_default().to_string_lossy();
            let destination = quarantine.join(format!("{}-{}", name, temporary_suffix()));
            if fs::rename(entry, &destination).is_ok() {
                return CacheError::Corrupt {
                    path: entry.to_path_buf(),
                    message: format!("{error}；已隔离至 `{}`", destination.display()),
                };
            }
        }
        CacheError::Corrupt {
            path: entry.to_path_buf(),
            message: format!("{error}；无法隔离损坏条目"),
        }
    }

    fn package_entries_dir(&self) -> PathBuf {
        self.root.join("dependencies/v1/packages")
    }

    fn create_temp_dir(&self, kind: &str) -> Result<PathBuf, CacheError> {
        let temporary_root = self.root.join("dependencies/v1/tmp");
        fs::create_dir_all(&temporary_root).map_err(|error| io_error(&temporary_root, error))?;
        let path = temporary_root.join(format!("{kind}-{}", temporary_suffix()));
        fs::create_dir(&path).map_err(|error| io_error(&path, error))?;
        Ok(path)
    }
}

/// 按当前锁图读取所有可执行依赖输入。
pub fn prepare_dependency_inputs(
    lock: &LockGraph,
    cache: &DependencyCache,
    workspace_root: &Path,
    vendor_root: &Path,
    policy: CachePolicy,
) -> Result<Vec<DependencyInput>, CacheError> {
    let external = lock
        .packages
        .iter()
        .filter(|package| !matches!(package.id.source(), PackageSource::Path { .. }))
        .collect::<Vec<_>>();
    let mut inputs = Vec::with_capacity(lock.packages.len());
    if policy.vendor && !external.is_empty() {
        inputs.extend(load_vendor_inputs(lock, vendor_root)?);
        for package in lock
            .packages
            .iter()
            .filter(|package| matches!(package.id.source(), PackageSource::Path { .. }))
        {
            inputs.push(load_path_input(package, workspace_root)?);
        }
    } else {
        for package in &lock.packages {
            let input = match package.id.source() {
                PackageSource::Path { .. } => load_path_input(package, workspace_root)?,
                PackageSource::Registry { .. } => {
                    let checksum =
                        package
                            .checksum
                            .as_deref()
                            .ok_or_else(|| CacheError::Invalid {
                                path: cache.root().to_path_buf(),
                                message: format!(
                                    "registry package `{}` 缺少锁定 checksum",
                                    package.id
                                ),
                            })?;
                    cache.load(&package.id, Some(checksum))?
                }
                PackageSource::Git { .. } => cache.load(&package.id, None)?,
            };
            inputs.push(input);
        }
    }
    inputs.sort_by(|left, right| left.package.cmp(&right.package));
    Ok(inputs)
}

fn load_path_input(
    package: &super::LockedPackage,
    workspace_root: &Path,
) -> Result<DependencyInput, CacheError> {
    let PackageSource::Path { path } = package.id.source() else {
        unreachable!();
    };
    let root = workspace_root.join(path);
    let files = PackageFiles::from_directory(&root)?;
    Ok(DependencyInput::new(package.id.clone(), files))
}

fn load_vendor_inputs(
    lock: &LockGraph,
    vendor_root: &Path,
) -> Result<Vec<DependencyInput>, CacheError> {
    let record_path = vendor_root.join(VENDOR_RECORD);
    let text = fs::read_to_string(&record_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CacheError::Missing {
                package: format!("vendor `{}`", vendor_root.display()),
            }
        } else {
            io_error(&record_path, error)
        }
    })?;
    let manifest = toml::from_str::<VendorDocument>(&text)
        .map_err(|error| vendor_error(vendor_root, error.to_string()))?;
    if manifest.version != VENDOR_VERSION {
        return Err(vendor_error(vendor_root, "vendor manifest 版本不受支持"));
    }
    validate_vendor_layout(vendor_root, &manifest.package)?;
    let expected = lock
        .packages
        .iter()
        .filter(|package| !matches!(package.id.source(), PackageSource::Path { .. }))
        .collect::<Vec<_>>();
    if manifest.package.len() != expected.len() {
        return Err(vendor_error(vendor_root, "vendor package 数量与锁图不一致"));
    }
    let mut seen = BTreeSet::new();
    let mut inputs = Vec::with_capacity(manifest.package.len());
    for record in manifest.package {
        let Some(package) = expected.iter().find(|package| {
            package.id.name() == record.name
                && package.id.version().to_string() == record.version
                && package.id.source().to_string() == record.source
        }) else {
            return Err(vendor_error(
                vendor_root,
                format!("vendor 中存在锁图外 package `{}`", record.name),
            ));
        };
        if !seen.insert(package.id.clone()) {
            return Err(vendor_error(
                vendor_root,
                "vendor manifest 包含重复 package",
            ));
        }
        if !is_vendor_directory(&record.directory) {
            return Err(vendor_error(
                vendor_root,
                format!("vendor 目录名 `{}` 不安全", record.directory),
            ));
        }
        let directory = vendor_root.join(&record.directory);
        let files = PackageFiles::from_directory(&directory)?;
        let actual = files.checksum();
        validate_checksum_text(&record.content_hash)?;
        if record.content_hash != actual {
            return Err(CacheError::Checksum {
                package: package.id.to_string(),
                expected: record.content_hash.clone(),
                actual,
            });
        }
        if let Some(expected_checksum) = record.checksum.as_deref() {
            if expected_checksum != actual {
                return Err(CacheError::Checksum {
                    package: package.id.to_string(),
                    expected: expected_checksum.to_owned(),
                    actual,
                });
            }
        }
        inputs.push(DependencyInput::new(package.id.clone(), files));
    }
    if seen.len() != expected.len() {
        return Err(vendor_error(vendor_root, "vendor 缺少锁图中的 package"));
    }
    Ok(inputs)
}

fn validate_vendor_layout(root: &Path, records: &[VendorPackageRecord]) -> Result<(), CacheError> {
    let declared = records
        .iter()
        .map(|record| record.directory.as_str())
        .collect::<BTreeSet<_>>();
    if declared.len() != records.len() {
        return Err(vendor_error(root, "vendor manifest 包含重复目录"));
    }
    let entries = fs::read_dir(root).map_err(|error| io_error(root, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error(root, error))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| vendor_error(root, "vendor 文件名不是 UTF-8"))?;
        if name == VENDOR_RECORD {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| io_error(&entry.path(), error))?;
        if !file_type.is_dir() || !declared.contains(name) {
            return Err(vendor_error(
                root,
                format!("vendor 包含未登记条目 `{name}`"),
            ));
        }
    }
    Ok(())
}

/// 根据锁图和已经验证的依赖输入生成 workspace vendor 树。
pub fn materialize_vendor(
    vendor_root: &Path,
    lock: &LockGraph,
    inputs: &[DependencyInput],
) -> Result<(), CacheError> {
    let expected = lock
        .packages
        .iter()
        .filter(|package| !matches!(package.id.source(), PackageSource::Path { .. }))
        .map(|package| package.id.clone())
        .collect::<BTreeSet<_>>();
    let provided = inputs
        .iter()
        .map(|input| input.package.clone())
        .collect::<BTreeSet<_>>();
    if inputs.len() != provided.len() || expected != provided {
        return Err(vendor_error(vendor_root, "输入集合与锁图不一致"));
    }
    for package in lock
        .packages
        .iter()
        .filter(|package| matches!(package.id.source(), PackageSource::Registry { .. }))
    {
        let input = inputs
            .iter()
            .find(|input| input.package == package.id)
            .expect("provided package set was checked");
        let expected_checksum = package.checksum.as_deref().ok_or_else(|| {
            vendor_error(
                vendor_root,
                format!("registry package `{}` 缺少 checksum", package.id),
            )
        })?;
        let actual = input.checksum();
        if actual != expected_checksum {
            return Err(CacheError::Checksum {
                package: package.id.to_string(),
                expected: expected_checksum.to_owned(),
                actual,
            });
        }
    }
    let parent = vendor_root.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    let temporary = parent.join(format!(".gugu-vendor-{}", temporary_suffix()));
    fs::create_dir(&temporary).map_err(|error| io_error(&temporary, error))?;
    let result = write_vendor_directory(&temporary, inputs).and_then(|()| {
        if !vendor_root.exists() {
            return fs::rename(&temporary, vendor_root)
                .map_err(|error| io_error(vendor_root, error));
        }
        let old = parent.join(format!(".gugu-vendor-old-{}", temporary_suffix()));
        fs::rename(vendor_root, &old).map_err(|error| io_error(vendor_root, error))?;
        match fs::rename(&temporary, vendor_root) {
            Ok(()) => remove_path(&old),
            Err(error) => {
                let _ = fs::rename(&old, vendor_root);
                Err(io_error(vendor_root, error))
            }
        }
    });
    if result.is_err() {
        let _ = remove_path(&temporary);
    }
    result
}

fn write_vendor_directory(root: &Path, inputs: &[DependencyInput]) -> Result<(), CacheError> {
    let mut records = Vec::new();
    for input in inputs {
        let directory = format!("pkg-{}", package_key(&input.package)[..16].to_owned());
        if !is_vendor_directory(&directory) {
            return Err(vendor_error(root, "生成的 vendor 目录名无效"));
        }
        let package_directory = root.join(&directory);
        write_package_files(&package_directory, &input.files)?;
        records.push(VendorPackageRecord {
            name: input.package.name().to_owned(),
            version: input.package.version().to_string(),
            source: input.package.source().to_string(),
            checksum: match input.package.source() {
                PackageSource::Registry { .. } => Some(input.checksum()),
                _ => None,
            },
            content_hash: input.checksum(),
            directory,
        });
    }
    records.sort_by(|left, right| {
        (&left.name, &left.version, &left.source).cmp(&(&right.name, &right.version, &right.source))
    });
    let document = VendorDocument {
        version: VENDOR_VERSION,
        package: records,
    };
    let text = toml::to_string(&document).map_err(|error| vendor_error(root, error.to_string()))?;
    write_file(&root.join(VENDOR_RECORD), text.as_bytes())?;
    Ok(())
}

fn write_package_files(root: &Path, files: &PackageFiles) -> Result<(), CacheError> {
    fs::create_dir_all(root).map_err(|error| io_error(root, error))?;
    for (path, bytes) in files.files() {
        let file = root.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        write_file(&file, bytes)?;
    }
    Ok(())
}

/// 用锁图内容重建 registry/Git source index 的确定性候选。
pub fn candidates_from_lock(lock: &LockGraph) -> (Vec<PackageMetadata>, Vec<PackageMetadata>) {
    let mut registry = Vec::new();
    let mut git = Vec::new();
    for package in &lock.packages {
        let source = package.id.source();
        if !matches!(
            source,
            PackageSource::Registry { .. } | PackageSource::Git { .. }
        ) {
            continue;
        }
        let mut metadata = PackageMetadata::new(package.id.clone());
        metadata.checksum = package.checksum.clone();
        for feature in package.features.values().flatten() {
            metadata.features.entry(feature.clone()).or_default();
        }
        metadata.dependencies = package
            .dependencies
            .iter()
            .map(|dependency| DependencySpec {
                alias: dependency.alias.clone(),
                package: dependency.package.name().to_owned(),
                version: VersionReq::parse(&format!("={}", dependency.package.version()))
                    .expect("lock version is a valid exact requirement"),
                source: dependency_source(dependency.package.source()),
                features: dependency.features.clone(),
                default_features: dependency.default_features,
                optional: false,
                domain: dependency.domain,
                target: dependency
                    .target
                    .as_deref()
                    .map(TargetCondition::parse)
                    .transpose()
                    .expect("lock target condition is valid"),
            })
            .collect();
        match source {
            PackageSource::Registry { .. } => registry.push(metadata),
            PackageSource::Git { .. } => git.push(metadata),
            PackageSource::Path { .. } => unreachable!(),
        }
    }
    (registry, git)
}

fn dependency_source(source: &PackageSource) -> DependencySource {
    match source {
        PackageSource::Path { path } => DependencySource::path(path),
        PackageSource::Git { url, commit, .. } => DependencySource::Git {
            url: url.clone(),
            rev: Some(commit.clone()),
            tag: None,
            branch: None,
        },
        PackageSource::Registry { registry } => DependencySource::registry(registry),
    }
}

/// 返回当前平台的默认 Gugu 全局缓存根。
pub fn default_cache_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        return env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("gugu");
    }
    #[cfg(not(target_os = "windows"))]
    {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .map(|path| path.join("gugu"))
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".cache").join("gugu"))
            })
            .unwrap_or_else(|| PathBuf::from(".gugu-cache"))
    }
}

fn validate_cache_layout(directory: &Path) -> Result<(), CacheError> {
    let entries = fs::read_dir(directory).map_err(|error| io_error(directory, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error(directory, error))?;
        let name = entry.file_name();
        let file_type = entry
            .file_type()
            .map_err(|error| io_error(&entry.path(), error))?;
        let valid = match name.to_str() {
            Some(PACKAGE_RECORD) => file_type.is_file(),
            Some("files") => file_type.is_dir(),
            _ => false,
        };
        if !valid {
            return Err(invalid_error(directory, "缓存条目包含未登记文件或符号链接"));
        }
    }
    Ok(())
}

fn validate_record(
    package: &PackageId,
    record: &CacheRecord,
    path: &Path,
) -> Result<(), CacheError> {
    if record.version != CACHE_VERSION
        || record.name != package.name()
        || record.package_version != package.version().to_string()
        || record.source != package.source().to_string()
        || record.files.is_empty() && matches!(package.source(), PackageSource::Registry { .. })
    {
        return Err(invalid_error(path, "缓存记录身份或版本不一致"));
    }
    if let Some(checksum) = record.checksum.as_deref() {
        validate_checksum_text(checksum)?;
        if !matches!(package.source(), PackageSource::Registry { .. }) {
            return Err(invalid_error(path, "非 registry package 不应包含 checksum"));
        }
    } else if matches!(package.source(), PackageSource::Registry { .. }) {
        return Err(invalid_error(path, "registry package 缺少 checksum"));
    }
    Ok(())
}

fn is_vendor_directory(directory: &str) -> bool {
    directory.starts_with("pkg-")
        && directory.len() == 20
        && directory[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_checksum_text(checksum: &str) -> Result<(), CacheError> {
    if checksum.len() != 64
        || !checksum
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_error(
            Path::new("<checksum>"),
            "checksum 必须是 64 位小写十六进制",
        ));
    }
    Ok(())
}

fn package_key(package: &PackageId) -> String {
    hex_encode(blake3::hash(format!("gugu-package-cache-v1\0{}", package).as_bytes()).as_bytes())
}

fn update_u64_be(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_be_bytes());
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", temporary_suffix()));
    let result = (|| {
        let mut file = File::create(&temporary).map_err(|error| io_error(&temporary, error))?;
        file.write_all(bytes)
            .map_err(|error| io_error(&temporary, error))?;
        file.sync_all()
            .map_err(|error| io_error(&temporary, error))?;
        fs::rename(&temporary, path).map_err(|error| io_error(path, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_path(path: &Path) -> Result<(), CacheError> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|error| io_error(path, error))
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| io_error(path, error))
    } else {
        Ok(())
    }
}

fn next_serial() -> u64 {
    TEMP_SERIAL.fetch_add(1, Ordering::Relaxed)
}

fn temporary_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{nanos}-{}", std::process::id(), next_serial())
}
fn io_error(path: &Path, error: io::Error) -> CacheError {
    CacheError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn invalid_error(path: &Path, error: impl std::fmt::Display) -> CacheError {
    CacheError::Invalid {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn vendor_error(path: &Path, message: impl Into<String>) -> CacheError {
    CacheError::VendorMismatch {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheRecord {
    version: u32,
    name: String,
    #[serde(rename = "package-version")]
    package_version: String,
    source: String,
    checksum: Option<String>,
    files: Vec<CacheFileRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheFileRecord {
    path: String,
    length: u64,
    hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VendorDocument {
    version: u32,
    package: Vec<VendorPackageRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VendorPackageRecord {
    name: String,
    version: String,
    source: String,
    checksum: Option<String>,
    #[serde(rename = "content-hash")]
    content_hash: String,
    directory: String,
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;

impl From<CacheError> for ProjectError {
    fn from(error: CacheError) -> Self {
        ProjectError::DependencyResolution {
            package: "cache".to_owned(),
            message: error.to_string(),
        }
    }
}
