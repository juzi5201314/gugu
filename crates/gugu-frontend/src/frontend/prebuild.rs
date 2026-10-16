//! 构建期物化：把内建块的派生做一次并编码成 artifact 表。
//!
//! 物化只覆盖与用户无关的层：名称解析、身份、早期常量、类型检查、冻结 HIR、泛型 GIR 与
//! record 类型目录。用户相关层（单态化实例、具体体、GIR pass、LIR、机器码）仍每次现算。
//!
//! 表内容全部是前端 canonical 编码；加载端按同一规则安装到编号前缀并重算指纹比对，
//! 序列化里的指纹只作断言，不作为信任来源。

use std::fmt;

use gugu_compiler_core::{
    PREBUILT_SCHEMA, RuntimeArtifactError, RuntimeArtifactV1, RuntimePrebuiltV1,
};

use crate::frontend::{cfg::CfgContext, derive_builtin, gir, semantics};
use crate::query::QueryEngine;
use crate::runtime::RuntimeResources;
use crate::source::SourceMap;
use crate::{TargetName, canonical};

/// 物化失败：构建期必须直接失败，不做降级。
#[derive(Debug)]
pub enum MaterializeError {
    /// 源 artifact 不可用。
    Artifact(RuntimeArtifactError),
    /// 内建块派生失败。
    Derive(Vec<crate::Diagnostic>),
    /// 内建源使用了物化不覆盖的 cfg 谓词：内建块必须对用户 cfg 保持不变。
    CfgDependentSource {
        /// 内建源逻辑路径。
        path: String,
        /// 被拒绝的谓词原文。
        predicate: String,
    },
}

impl fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact(error) => write!(f, "runtime artifact 无效：{error}"),
            Self::Derive(_) => f.write_str("内建块派生失败"),
            Self::CfgDependentSource { path, predicate } => write!(
                f,
                "内建源 `{path}` 的 cfg 谓词 `{predicate}` 会随用户 cfg 改变内建块；\
                 物化表只覆盖 target 谓词"
            ),
        }
    }
}

impl std::error::Error for MaterializeError {}

impl From<RuntimeArtifactError> for MaterializeError {
    fn from(error: RuntimeArtifactError) -> Self {
        Self::Artifact(error)
    }
}

/// 把 source-only artifact 物化成带内建块派生表的 artifact。
///
/// 派生用 `CfgContext::target_only`：内建块必须对用户 cfg 保持不变，这一点由
/// [`check_cfg_independence`] 在构建期钉住。
pub fn materialize(
    target: TargetName,
    source: RuntimeArtifactV1,
) -> Result<RuntimeArtifactV1, MaterializeError> {
    let resources = RuntimeResources::new(&source)?;
    check_cfg_independence(&source, resources)?;
    let mut source_map = SourceMap::empty();
    let cfg = CfgContext::target_only(target);
    let derivation = derive_builtin(&mut source_map, &QueryEngine::new(), resources, &cfg)
        .map_err(MaterializeError::Derive)?;
    let names = derivation.names;
    let identities = derivation.identities;
    let checked = derivation.checked;
    let early_constants = derivation.early_constants;
    let hir = derivation.hir;
    let record_types = derivation.record_types;
    let gir_bodies = derivation.gir_bodies;
    let mut bodies_slices: Vec<&[u8]> = gir_bodies.iter().map(Vec::as_slice).collect();
    bodies_slices.insert(0, &[]);
    let fingerprints = vec![
        table_fingerprint("names", &[&names]),
        table_fingerprint("identities", &[&identities]),
        table_fingerprint("checked.semantics", &[&checked]),
        table_fingerprint("early.constants", &[&early_constants]),
        table_fingerprint("hir.module", &[&hir]),
        table_fingerprint("gir.bodies", &bodies_slices),
        table_fingerprint("record.types", &[&record_types]),
    ];
    let prebuilt = RuntimePrebuiltV1 {
        schema: PREBUILT_SCHEMA,
        revisions: layer_revisions(),
        module_order: derivation.module_order,
        names,
        identities,
        checked,
        early_constants,
        hir,
        gir_bodies,
        record_types,
        fingerprints,
    };
    let mut artifact = source;
    artifact.payload.prebuilt = Some(prebuilt);
    artifact.validate()?;
    Ok(artifact)
}

/// 各派生层在物化表里的 revision。
///
/// 只登记已有版本常量的层：其余层的失效由表内容指纹（加载时重算比对）与容器
/// [`PREBUILT_SCHEMA`] 共同承担，不在这里编造版本号。
fn layer_revisions() -> Vec<(String, u32)> {
    let mut layers = vec![
        ("canonical".to_owned(), canonical::REVISION),
        (
            "checked.semantics".to_owned(),
            semantics::CHECKED_SCHEMA_VERSION,
        ),
        (
            "early.constants".to_owned(),
            semantics::comptime::EARLY_SCHEMA_VERSION,
        ),
        ("gir.body".to_owned(), gir::body::GIR_REVISION),
        ("gir.build".to_owned(), gir::BUILD_SCHEMA),
        ("record.types".to_owned(), gir::concrete::SCHEMA),
    ];
    layers.sort();
    layers
}

/// 表指纹：(标识域摘要, 内容摘要)。
fn table_fingerprint(layer: &str, content: &[&[u8]]) -> ([u8; 32], [u8; 32]) {
    (
        keyed_digest("gugu-prebuilt-table-domain-v1", &[layer.as_bytes()]),
        keyed_digest("gugu-prebuilt-table-content-v1", content),
    )
}

/// 域隔离摘要：分片按长度前缀拼接，避免不同分片边界得到同一摘要。
fn keyed_digest(domain: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    for part in parts {
        hasher.update(
            &u64::try_from(part.len())
                .expect("摘要分片长度必须落在 u64")
                .to_le_bytes(),
        );
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
}

/// 内建源只允许 target 派生的 cfg 键。
///
/// 物化表在构建期用 `target_only` 派生一次，加载端不会重算内建块；内建源一旦引入
/// `feature`/`test`/`bench`/自定义键，物化表就会悄悄偏离用户 cfg 下的真实派生，因此这里
/// 直接拒绝。可用的键集就是 `CfgContext` 里由目标描述派生的 `os` 与 `arch`。
const ALLOWED_CFG_KEYS: [&str; 2] = ["os", "arch"];

/// 扫描内建源的 `cfg` 谓词；探针源只在探针编译里安装，不参与物化。
fn check_cfg_independence(
    source: &RuntimeArtifactV1,
    resources: RuntimeResources<'_>,
) -> Result<(), MaterializeError> {
    for blob in resources.source_blobs() {
        if crate::frontend::is_probe_builtin_source(&blob.logical_path) {
            continue;
        }
        for predicate in cfg_predicates(&blob.source) {
            let key = predicate
                .split(['(', '=', ',', ')', ' '])
                .find(|token| !token.is_empty())
                .unwrap_or_default();
            if !ALLOWED_CFG_KEYS.contains(&key) {
                return Err(MaterializeError::CfgDependentSource {
                    path: blob.logical_path.clone(),
                    predicate: predicate.trim().to_owned(),
                });
            }
        }
    }
    let _ = source;
    Ok(())
}

/// 返回源文本里 `cfg(...)` 的括号内谓词。
fn cfg_predicates(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    while let Some(offset) = source[index..].find("cfg(") {
        let start = index + offset + "cfg(".len();
        let mut depth = 1_u32;
        let mut cursor = start;
        while cursor < bytes.len() && depth > 0 {
            match bytes[cursor] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            cursor += 1;
        }
        let end = cursor.saturating_sub(1);
        found.push(source[start..end].to_owned());
        index = cursor;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_predicates_extract_balanced_arguments() {
        let source = "#[cfg(target = \"x86_64-linux\")]\n#[cfg(all(unix, not(feature = \"x\")))]\n";
        assert_eq!(
            cfg_predicates(source),
            vec![
                "target = \"x86_64-linux\"".to_owned(),
                "all(unix, not(feature = \"x\"))".to_owned(),
            ]
        );
    }

    #[test]
    fn cfg_independence_accepts_only_target_derived_keys() {
        assert!(ALLOWED_CFG_KEYS.contains(&"os"));
        assert!(ALLOWED_CFG_KEYS.contains(&"arch"));
        for rejected in ["feature", "test", "bench", "target", "unix", "windows"] {
            assert!(!ALLOWED_CFG_KEYS.contains(&rejected), "{rejected} 不得通过");
        }
    }
}
