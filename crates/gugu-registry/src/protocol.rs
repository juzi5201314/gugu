use std::collections::BTreeMap;

use gugu_project::{
    DependencyDomain, DependencySource, DependencySpec, PackageId, PackageMetadata, PackageSource,
    TargetCondition, Version, VersionReq,
};
use serde::Deserialize;

use crate::error::{RegistryError, config, protocol};
use crate::identity::normalize_index_url;

/// `config.json` 响应文档。
#[derive(Clone, Debug)]
pub struct ConfigDocument {
    /// 主协议号；只支持 `1`。
    pub protocol: u64,
    /// 服务端声明的规范 index 身份，必须与客户端请求身份一致。
    pub index: String,
    /// 含 `{owner}`/`{name}`/`{version}` 的下载 URL 模板。
    pub download: String,
    /// 发布/yank API 根。
    pub api: Option<String>,
    /// 未认证请求是否被拒绝。
    pub auth_required: bool,
}

#[derive(Deserialize)]
struct RawConfigDocument {
    protocol: u64,
    index: String,
    download: String,
    api: Option<String>,
    #[serde(default)]
    auth_required: bool,
}

/// 解析并验证 `config.json`；服务端 `index` 必须等于请求的规范身份。
pub fn parse_config_document(
    body: &str,
    expected_identity: &str,
) -> Result<ConfigDocument, RegistryError> {
    let raw: RawConfigDocument = serde_json::from_str(body)
        .map_err(|error| config(format!("config.json 不是合法 JSON：{error}")))?;
    if raw.protocol != 1 {
        return Err(config(format!(
            "registry 协议版本 `{}` 不受支持（本工具支持 1）",
            raw.protocol
        )));
    }
    let index = normalize_index_url(&raw.index)?;
    if index != expected_identity {
        return Err(config(format!(
            "registry 身份不一致：请求 `{expected_identity}`，服务端声明 `{index}`"
        )));
    }
    let api = raw
        .api
        .map(|api| normalize_index_url(&api).map(|url| url.trim_end_matches('/').to_owned()))
        .transpose()?;
    if !raw.download.starts_with("https://")
        && !raw.download.starts_with("http://127.")
        && !raw.download.starts_with("http://localhost")
        && !raw.download.starts_with("http://[::1]")
    {
        return Err(config("download 模板必须使用 HTTPS"));
    }
    for marker in ["{owner}", "{name}", "{version}"] {
        if !raw.download.contains(marker) {
            return Err(config(format!("download 模板缺少 `{marker}`")));
        }
    }
    Ok(ConfigDocument {
        protocol: raw.protocol,
        index,
        download: raw.download,
        api,
        auth_required: raw.auth_required,
    })
}

/// 一条索引记录（NDJSON 行）。
#[derive(Clone, Debug)]
pub struct IndexRecord {
    pub name: String,
    pub vers: String,
    pub yanked: bool,
    pub cksum: String,
    pub deps: Vec<IndexDep>,
    pub features: BTreeMap<String, Vec<String>>,
    pub signatures: Vec<SignatureRecord>,
}

/// 索引记录中的依赖声明。
#[derive(Clone, Debug)]
pub struct IndexDep {
    /// 源码别名。
    pub name: String,
    /// 规范 package 名 `owner/name`。
    pub package: String,
    /// SemVer 约束文本。
    pub req: String,
    /// 目标 registry 逻辑名；缺省同发布方 registry。
    pub registry: Option<String>,
    pub features: Vec<String>,
    pub default_features: bool,
    pub optional: bool,
    /// `normal` / `test` / `build`。
    pub kind: String,
    /// cfg 目标条件文本。
    pub target: Option<String>,
}

/// 签名记录。
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct SignatureRecord {
    pub role: String,
    pub algorithm: String,
    pub key_id: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct RawIndexRecord {
    schema: u64,
    name: String,
    vers: String,
    #[serde(default)]
    yanked: bool,
    cksum: String,
    #[serde(default)]
    deps: Vec<RawIndexDep>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    // 保留字段：解析验证存在性即可，当前版本不消费其内容。
    #[serde(default, rename = "metadata")]
    _metadata: Option<serde_json::Value>,
    #[serde(default)]
    signatures: Vec<SignatureRecord>,
}

#[derive(Deserialize)]
struct RawIndexDep {
    name: String,
    package: String,
    req: String,
    registry: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default = "default_true")]
    default_features: bool,
    #[serde(default)]
    optional: bool,
    #[serde(default = "default_normal")]
    kind: String,
    target: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_normal() -> String {
    "normal".to_owned()
}

/// 解析稀疏索引响应（NDJSON）：每行一个版本记录，校验 schema、名称、
/// 版本、checksum 与重复版本。
pub fn parse_index(body: &str, expected_name: &str) -> Result<Vec<IndexRecord>, RegistryError> {
    let mut records = Vec::new();
    for (number, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawIndexRecord = serde_json::from_str(line).map_err(|error| {
            protocol(format!(
                "索引记录第 {} 行不是合法 JSON：{error}",
                number + 1
            ))
        })?;
        if raw.schema != 1 {
            return Err(protocol(format!(
                "索引记录 schema `{}` 不受支持",
                raw.schema
            )));
        }
        if raw.name != expected_name {
            return Err(protocol(format!(
                "索引记录 name `{}` 与请求 `{expected_name}` 不一致",
                raw.name
            )));
        }
        Version::parse(&raw.vers)
            .map_err(|message| protocol(format!("索引记录版本 `{}` 非法：{message}", raw.vers)))?;
        validate_checksum(&raw.cksum)?;
        let deps = raw
            .deps
            .iter()
            .map(|dep| IndexDep {
                name: dep.name.clone(),
                package: dep.package.clone(),
                req: dep.req.clone(),
                registry: dep.registry.clone(),
                features: dep.features.clone(),
                default_features: dep.default_features,
                optional: dep.optional,
                kind: dep.kind.clone(),
                target: dep.target.clone(),
            })
            .collect();
        records.push(IndexRecord {
            name: raw.name,
            vers: raw.vers,
            yanked: raw.yanked,
            cksum: raw.cksum,
            deps,
            features: raw.features,
            signatures: raw.signatures,
        });
    }
    records.sort_by(|left, right| left.vers.cmp(&right.vers));
    let mut previous: Option<&str> = None;
    for record in &records {
        if previous == Some(record.vers.as_str()) {
            return Err(protocol(format!("索引包含重复版本 `{}`", record.vers)));
        }
        previous = Some(&record.vers);
    }
    Ok(records)
}

fn validate_checksum(cksum: &str) -> Result<(), RegistryError> {
    let valid = cksum.len() == 64
        && cksum
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !valid {
        return Err(protocol("索引记录 cksum 不是 64 位小写十六进制"));
    }
    Ok(())
}

impl IndexRecord {
    /// 转换为 resolver 候选元数据；registry identity 必须是规范 URL。
    pub fn to_metadata(&self, identity: &str) -> Result<PackageMetadata, RegistryError> {
        let version = Version::parse(&self.vers)
            .map_err(|message| protocol(format!("索引记录版本非法：{message}")))?;
        let id = PackageId::new(
            self.name.clone(),
            version,
            PackageSource::Registry {
                registry: identity.to_owned(),
            },
        );
        let mut metadata = PackageMetadata::new(id)
            .with_checksum(self.cksum.clone())
            .with_yanked(self.yanked)
            .with_features(self.features.clone());
        let mut dependencies = Vec::with_capacity(self.deps.len());
        for dep in &self.deps {
            dependencies.push(dep.to_spec()?);
        }
        metadata = metadata.with_dependencies(dependencies);
        Ok(metadata)
    }
}

impl IndexDep {
    /// 转换为 resolver 依赖声明；`registry` 缺省时由调用方填默认身份。
    pub fn to_spec(&self) -> Result<DependencySpec, RegistryError> {
        let version = VersionReq::parse(&self.req).map_err(|message| {
            protocol(format!(
                "索引依赖 `{}` 的版本约束非法：{message}",
                self.package
            ))
        })?;
        let domain = match self.kind.as_str() {
            "normal" => DependencyDomain::Normal,
            "test" => DependencyDomain::Test,
            "build" => DependencyDomain::Build,
            other => {
                return Err(protocol(format!(
                    "索引依赖 `{}` 的 kind `{other}` 非法",
                    self.package
                )));
            }
        };
        let target = self
            .target
            .as_deref()
            .map(TargetCondition::parse)
            .transpose()
            .map_err(|error| protocol(format!("索引依赖 target 条件非法：{error}")))?;
        Ok(DependencySpec {
            alias: self.name.clone(),
            package: self.package.clone(),
            version,
            source: DependencySource::Registry {
                registry: self
                    .registry
                    .clone()
                    .unwrap_or_else(|| "default".to_owned()),
            },
            features: self.features.clone(),
            default_features: self.default_features,
            optional: self.optional,
            domain,
            target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: &str = "https://packages.example/index/";

    fn config_json(index: &str, protocol: u64) -> String {
        format!(
            r#"{{"protocol": {protocol}, "index": "{index}", "api": "https://packages.example/api/v1", "download": "https://packages.example/dl/{{owner}}/{{name}}/{{version}}", "auth_required": false}}"#
        )
    }

    #[test]
    fn config_document_accepts_matching_identity() {
        let doc = parse_config_document(&config_json(IDENTITY, 1), IDENTITY).unwrap();
        assert_eq!(doc.index, IDENTITY);
        assert_eq!(doc.api.as_deref(), Some("https://packages.example/api/v1"));
    }

    #[test]
    fn config_document_rejects_identity_mismatch_and_bad_protocol() {
        let err = parse_config_document(&config_json("https://evil.example/index/", 1), IDENTITY)
            .unwrap_err();
        assert_eq!(err.code(), "registry-config");
        let err = parse_config_document(&config_json(IDENTITY, 2), IDENTITY).unwrap_err();
        assert_eq!(err.code(), "registry-config");
    }

    #[test]
    fn index_parses_records_sorted_and_validates_fields() {
        let body = concat!(
            r#"{"schema":1,"name":"pub/foo","vers":"1.2.0","cksum":""#,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            r#"","deps":[],"features":{}}"#,
            "\n",
            r#"{"schema":1,"name":"pub/foo","vers":"1.0.0","cksum":""#,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            r#"","deps":[],"features":{},"yanked":true}"#,
            "\n"
        );
        let records = parse_index(body, "pub/foo").unwrap();
        assert_eq!(records[0].vers, "1.0.0");
        assert!(records[0].yanked);
        assert_eq!(records[1].vers, "1.2.0");
    }

    #[test]
    fn index_rejects_name_mismatch_duplicate_and_bad_checksum() {
        let rec = |name: &str, vers: &str, cksum: &str| {
            format!(
                r#"{{"schema":1,"name":"{name}","vers":"{vers}","cksum":"{cksum}","deps":[],"features":{{}}}}"#
            )
        };
        let good = "a".repeat(64);
        // 名称不符
        let err =
            parse_index(&format!("{}\n", rec("pub/bar", "1.0.0", &good)), "pub/foo").unwrap_err();
        assert_eq!(err.code(), "registry-protocol");
        // 重复版本
        let dup = format!(
            "{}\n{}\n",
            rec("pub/foo", "1.0.0", &good),
            rec("pub/foo", "1.0.0", &good)
        );
        assert_eq!(
            parse_index(&dup, "pub/foo").unwrap_err().code(),
            "registry-protocol"
        );
        // 非法 checksum
        let err =
            parse_index(&format!("{}\n", rec("pub/foo", "1.0.0", "xyz")), "pub/foo").unwrap_err();
        assert_eq!(err.code(), "registry-protocol");
    }
}
