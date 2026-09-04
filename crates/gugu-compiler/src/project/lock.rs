use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use super::super::error::ProjectError;
use super::dependency_model::{
    DependencyDomain, LockGraph, LockedDependency, LockedPackage, PackageId, PackageSource,
};
use super::semver::Version;

impl LockGraph {
    /// 生成规范 TOML 文本。
    pub fn to_toml(&self) -> Result<String, ProjectError> {
        if self.version != 1 {
            return Err(lock_error("锁文件版本必须为 1"));
        }
        let mut packages = self.packages.clone();
        packages.sort_by(|left, right| left.id.cmp(&right.id));
        for package in &mut packages {
            validate_source(package.id.source())?;
            validate_checksum(package.id.source(), package.checksum.as_ref())?;
            for dependency in &package.dependencies {
                validate_source(dependency.package.source())?;
            }
            package.dependencies.sort_by(|left, right| {
                (left.domain, &left.alias, &left.package).cmp(&(
                    right.domain,
                    &right.alias,
                    &right.package,
                ))
            });
            for features in package.features.values_mut() {
                features.sort();
                features.dedup();
            }
        }
        let document = LockDocument {
            version: self.version,
            package: packages.into_iter().map(LockPackageToml::from).collect(),
        };
        toml::to_string(&document).map_err(|error| lock_error(format!("锁文件编码失败：{error}")))
    }

    /// 从 TOML 文本读取并验证锁图。
    pub fn from_toml(source: &str) -> Result<Self, ProjectError> {
        let document = toml::from_str::<LockDocumentOwned>(source)
            .map_err(|error| lock_error(format!("锁文件解析失败：{error}")))?;
        if document.version != 1 {
            return Err(lock_error("不支持的锁文件版本"));
        }
        let mut packages = document
            .package
            .into_iter()
            .map(LockedPackage::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        packages.sort_by(|left, right| left.id.cmp(&right.id));
        if packages.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(lock_error("锁文件包含重复 package ID"));
        }
        for package in &packages {
            validate_source(package.id.source())?;
            validate_checksum(package.id.source(), package.checksum.as_ref())?;
            for dependency in &package.dependencies {
                validate_source(dependency.package.source())?;
            }
        }
        let ids = packages
            .iter()
            .map(|package| package.id.clone())
            .collect::<BTreeSet<_>>();
        for package in &packages {
            for dependency in &package.dependencies {
                if !ids.contains(&dependency.package) {
                    return Err(lock_error(format!(
                        "锁边指向不存在的 package `{}`",
                        dependency.package
                    )));
                }
            }
        }
        Ok(Self {
            version: 1,
            packages,
        })
    }

    /// 从文件读取并验证锁图。
    pub fn read(path: &Path) -> Result<Self, ProjectError> {
        let source = fs::read_to_string(path).map_err(|error| ProjectError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        Self::from_toml(&source).map_err(|error| match error {
            ProjectError::DependencyResolution { package, message } => ProjectError::Lockfile {
                path: path.to_path_buf(),
                message: format!("{package}：{message}"),
            },
            error => error,
        })
    }

    /// 将规范锁图写入文件。
    pub fn write(&self, path: &Path) -> Result<(), ProjectError> {
        let text = self.to_toml()?;
        fs::write(path, text).map_err(|error| ProjectError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
    }
}

fn validate_source(source: &PackageSource) -> Result<(), ProjectError> {
    match source {
        PackageSource::Path { path }
            if path.is_empty()
                || path.starts_with('/')
                || path.contains('\\')
                || path.contains('\0') =>
        {
            Err(lock_error("锁文件 path source 不是规范相对路径"))
        }
        PackageSource::Git { url, commit, tree }
            if url.is_empty()
                || commit.is_empty()
                || tree.is_empty()
                || url.chars().any(char::is_whitespace) =>
        {
            Err(lock_error("锁文件 Git source 缺少规范身份字段"))
        }
        PackageSource::Registry { registry }
            if registry.is_empty() || registry.chars().any(char::is_whitespace) =>
        {
            Err(lock_error("锁文件 registry source 不是规范身份"))
        }
        _ => Ok(()),
    }
}

fn validate_checksum(
    source: &PackageSource,
    checksum: Option<&String>,
) -> Result<(), ProjectError> {
    if let Some(checksum) = checksum {
        if !matches!(source, PackageSource::Registry { .. })
            || checksum.len() != 64
            || !checksum
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(lock_error("registry checksum 必须是 64 位小写十六进制"));
        }
    }
    Ok(())
}

fn lock_error(message: impl Into<String>) -> ProjectError {
    ProjectError::DependencyResolution {
        package: "gugu.lock".to_owned(),
        message: message.into(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockDocument {
    version: u32,
    package: Vec<LockPackageToml>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockPackageToml {
    name: String,
    version: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
    dependencies: Vec<LockDependencyToml>,
    features: LockFeaturesToml,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockDependencyToml {
    alias: String,
    package: String,
    version: String,
    source: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    features: Vec<String>,
    #[serde(rename = "default-features")]
    default_features: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockFeaturesToml {
    normal: Vec<String>,
    test: Vec<String>,
    build: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LockDocumentOwned {
    version: u32,
    package: Vec<LockPackageToml>,
}

impl From<LockedPackage> for LockPackageToml {
    fn from(package: LockedPackage) -> Self {
        Self {
            name: package.id.name().to_owned(),
            version: package.id.version().to_string(),
            source: package.id.source().to_string(),
            checksum: package.checksum,
            dependencies: package
                .dependencies
                .into_iter()
                .map(|dependency| LockDependencyToml {
                    alias: dependency.alias,
                    package: dependency.package.name().to_owned(),
                    version: dependency.package.version().to_string(),
                    source: dependency.package.source().to_string(),
                    kind: dependency.domain.to_string(),
                    target: dependency.target,
                    features: dependency.features,
                    default_features: dependency.default_features,
                })
                .collect(),
            features: LockFeaturesToml {
                normal: package
                    .features
                    .get(&DependencyDomain::Normal)
                    .cloned()
                    .unwrap_or_default(),
                test: package
                    .features
                    .get(&DependencyDomain::Test)
                    .cloned()
                    .unwrap_or_default(),
                build: package
                    .features
                    .get(&DependencyDomain::Build)
                    .cloned()
                    .unwrap_or_default(),
            },
        }
    }
}

impl TryFrom<LockPackageToml> for LockedPackage {
    type Error = ProjectError;

    fn try_from(package: LockPackageToml) -> Result<Self, Self::Error> {
        let id = PackageId::new(
            package.name,
            Version::parse(&package.version).map_err(lock_error)?,
            parse_source(&package.source)?,
        );
        let dependencies = package
            .dependencies
            .into_iter()
            .map(|dependency| {
                Ok(LockedDependency {
                    alias: dependency.alias,
                    package: PackageId::new(
                        dependency.package,
                        Version::parse(&dependency.version).map_err(lock_error)?,
                        parse_source(&dependency.source)?,
                    ),
                    domain: parse_domain(&dependency.kind)?,
                    target: dependency.target,
                    features: dependency.features,
                    default_features: dependency.default_features,
                })
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;
        let mut features = BTreeMap::new();
        features.insert(DependencyDomain::Normal, package.features.normal);
        features.insert(DependencyDomain::Test, package.features.test);
        features.insert(DependencyDomain::Build, package.features.build);
        Ok(Self {
            id,
            checksum: package.checksum,
            dependencies,
            features,
        })
    }
}

fn parse_source(source: &str) -> Result<PackageSource, ProjectError> {
    if let Some(path) = source.strip_prefix("path+") {
        let source = PackageSource::Path {
            path: path.to_owned(),
        };
        validate_source(&source)?;
        return Ok(source);
    }
    if let Some(registry) = source.strip_prefix("registry+") {
        return Ok(PackageSource::Registry {
            registry: registry.to_owned(),
        });
    }
    let Some(git) = source.strip_prefix("git+") else {
        return Err(lock_error(format!("未知 package source `{source}`")));
    };
    let (url, query) = git
        .split_once("?commit=")
        .ok_or_else(|| lock_error("Git source 缺少 commit"))?;
    let (commit, tree) = query
        .split_once("&tree=")
        .ok_or_else(|| lock_error("Git source 缺少 tree hash"))?;
    Ok(PackageSource::Git {
        url: url.to_owned(),
        commit: commit.to_owned(),
        tree: tree.to_owned(),
    })
}

fn parse_domain(value: &str) -> Result<DependencyDomain, ProjectError> {
    match value {
        "normal" => Ok(DependencyDomain::Normal),
        "test" => Ok(DependencyDomain::Test),
        "build" => Ok(DependencyDomain::Build),
        _ => Err(lock_error(format!("未知依赖域 `{value}`"))),
    }
}
