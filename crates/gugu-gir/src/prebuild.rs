//! 构建期物化：把内建块的派生做一次并编码成 artifact 表。
//!
//! 本层物化与用户无关的表：名称解析、身份、早期常量、类型检查、冻结 HIR、泛型 GIR 与
//! record 类型目录。函数表要在已物化的前缀上编译 canonical 程序并经过 LIR，由 driver 在
//! [`materialize_tables`] 之后录制（见 [`crate::functions::record`]）。
//!
//! 表内容全部是前端 canonical 编码；加载端按同一规则安装到编号前缀并重算指纹比对，
//! 序列化里的指纹只作断言，不作为信任来源。

use std::collections::BTreeSet;
use std::fmt;

use gugu_compiler_core::{
    BlockRangeV1, ModuleBlockV1, PREBUILT_SCHEMA, RuntimeArtifactError, RuntimeArtifactV1,
    RuntimePrebuiltV1,
};

use crate::query::QueryEngine;
use crate::runtime::RuntimeResources;
use crate::sema_prefix::table_fingerprint;
use crate::source::SourceMap;
use crate::{TargetName, canonical};
use crate::{cfg::CfgContext, derive_builtin, gir, semantics};

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

/// 把 source-only artifact 物化成带内建块派生表的 artifact；函数表留空。
///
/// 派生用 `CfgContext::target_only`：内建块必须对用户 cfg 保持不变，这一点由
/// [`check_cfg_independence`] 在构建期钉住。
pub fn materialize_tables(
    target: TargetName,
    source: RuntimeArtifactV1,
) -> Result<RuntimeArtifactV1, MaterializeError> {
    let resources = RuntimeResources::new(&source);
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
    let hir_module: crate::hir::Module = canonical::from_slice(&hir).expect("物化 HIR 片段可解码");
    let prebuilt = RuntimePrebuiltV1 {
        schema: PREBUILT_SCHEMA,
        revisions: layer_revisions(),
        floor_modules: floor_modules(&derivation.module_order),
        contract_modules: derivation
            .module_order
            .iter()
            .filter(|path| path.starts_with("std.runtime."))
            .cloned()
            .collect(),
        blocks: module_blocks(&hir_module, &derivation.module_order),
        module_order: derivation.module_order,
        names,
        identities,
        checked,
        early_constants,
        hir,
        gir_bodies,
        record_types,
        fingerprints,
        // 函数表在 canonical 程序编译后录制：录制要在已物化的前缀上运行。
        function_revisions: Vec::new(),
        functions: Vec::new(),
    };
    let mut artifact = source;
    artifact.payload.prebuilt = Some(prebuilt);
    artifact.validate()?;
    Ok(artifact)
}

/// 内建编号前缀的定义数：物化块的定义区间之和，探针等未物化的内建块不在其中。
pub fn builtin_definitions(prebuilt: &RuntimePrebuiltV1) -> usize {
    prebuilt
        .blocks
        .iter()
        .map(|block| block.definitions.len as usize)
        .sum()
}

/// 按 `module_order` 顺序算出每个模块占据的定义/owner 区间。
///
/// 定义表按「模块编号」单调排列（`Module::verify` 校验），因此每块就是一段连续区间；
/// owner 表按定义下标递增，归属由 `Owner.definition` 反查定义得到。
fn module_blocks(module: &crate::hir::Module, module_order: &[String]) -> Vec<ModuleBlockV1> {
    let mut definitions = vec![BlockRangeV1 { start: 0, len: 0 }; module_order.len()];
    let mut current: Option<u32> = None;
    for definition in &module.definitions {
        let slot = definition.module as usize;
        if current != Some(definition.module) {
            assert!(
                definitions[slot].len == 0,
                "定义表必须按模块连续排列：模块 {slot} 出现第二次"
            );
            current = Some(definition.module);
        }
        definitions[slot].len += 1;
    }
    let mut owners = vec![BlockRangeV1 { start: 0, len: 0 }; module_order.len()];
    for (index, owner) in module.owners.iter().enumerate() {
        let slot = module.definitions[owner.definition.index()].module as usize;
        if owners[slot].len == 0 {
            owners[slot].start = index as u32;
        }
        owners[slot].len += 1;
    }
    // 空块（模块没有定义或 owner）的起点也要落在前后两段之间，保证整串区间首尾相接。
    let mut running = 0_u32;
    for range in &mut definitions {
        range.start = running;
        running += range.len;
    }
    let mut running = 0_u32;
    for range in &mut owners {
        range.start = running;
        running += range.len;
    }
    module_order
        .iter()
        .enumerate()
        .map(|(index, path)| ModuleBlockV1 {
            path: path.clone(),
            definitions: definitions[index],
            owners: owners[index],
        })
        .collect()
}

/// 固定地板集：rt0 与 machine entry 恒需的内建模块，按逻辑路径升序给出。
///
/// 只在构建期计算一次并写进物化表；加载期直接取用，不再推断。
fn floor_modules(module_order: &[String]) -> Vec<String> {
    let mut floor = [
        "std.prelude",
        "std.runtime.core",
        "std.runtime.platform",
        "std.runtime.scheduler",
    ]
    .into_iter()
    .filter(|path| module_order.iter().any(|module| module == path))
    .map(str::to_owned)
    .collect::<Vec<_>>();
    floor.sort();
    floor
}

/// 内建世界的 import 闭包。
///
/// `std.prelude`、固定地板集与契约集恒在内；其余内建模块只在被用户 `use` 或路径引用触达时进入。
/// 未出现在物化表 `module_order` 里的路径直接忽略，避免用户拼写把非内建路径带进闭包。
pub fn closure_program_modules<'a>(
    imported: impl IntoIterator<Item = &'a str>,
    prebuilt: &RuntimePrebuiltV1,
) -> BTreeSet<String> {
    let known = prebuilt
        .module_order
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut closure = prebuilt
        .floor_modules
        .iter()
        .chain(&prebuilt.contract_modules)
        .cloned()
        .collect::<BTreeSet<_>>();
    closure.extend(
        imported
            .into_iter()
            .filter(|path| known.contains(path))
            .map(str::to_owned),
    );
    closure
}

/// 各派生层在物化表里的 revision；加载端要求物化表声明的 revision 与当前编译器完全一致。
///
/// 只登记已有版本常量的层：其余层的失效由表内容指纹（加载时重算比对）与容器
/// [`PREBUILT_SCHEMA`] 共同承担，不在这里编造版本号。
pub fn layer_revisions() -> Vec<(String, u32)> {
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
        if crate::is_probe_builtin_source(&blob.logical_path) {
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

/// 返回源文本里 `cfg(...)` 属性的括号内谓词。
fn cfg_predicates(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    while let Some(offset) = source[index..].find("cfg(") {
        let hit = index + offset;
        // 只有 `#[cfg(` / `#![cfg(` 是属性；标识符或调用中的 `cfg(` 子串不是谓词。
        if !is_attribute_cfg(bytes, hit) {
            index = hit + "cfg(".len();
            continue;
        }
        let start = hit + "cfg(".len();
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

/// 判断 `cfg(` 命中是否位于 `#[`/`#![` 属性括号内。
fn is_attribute_cfg(bytes: &[u8], hit: usize) -> bool {
    let mut cursor = hit;
    while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    if cursor == 0 || bytes[cursor - 1] != b'[' {
        return false;
    }
    cursor -= 1;
    while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    if cursor > 0 && bytes[cursor - 1] == b'!' {
        cursor -= 1;
        while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
            cursor -= 1;
        }
    }
    cursor > 0 && bytes[cursor - 1] == b'#'
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
    fn cfg_predicates_ignore_identifier_and_call_positions() {
        let source = "fn define_cfg(name: string) {}\n#![cfg(os = \"linux\")]\nlet x = my_cfg(foo)";
        assert_eq!(cfg_predicates(source), vec!["os = \"linux\"".to_owned()]);
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

#[cfg(test)]
mod closure_tests {
    use super::*;

    fn sample_prebuilt() -> RuntimePrebuiltV1 {
        RuntimePrebuiltV1 {
            schema: PREBUILT_SCHEMA,
            revisions: vec![("names".to_owned(), 1)],
            module_order: vec![
                "std.prelude".to_owned(),
                "std.runtime.core".to_owned(),
                "std.runtime.scheduler".to_owned(),
                "std.vec".to_owned(),
            ],
            names: Vec::new(),
            identities: Vec::new(),
            checked: Vec::new(),
            early_constants: Vec::new(),
            hir: Vec::new(),
            gir_bodies: Vec::new(),
            record_types: Vec::new(),
            fingerprints: vec![([0; 32], [0; 32])],
            function_revisions: Vec::new(),
            functions: Vec::new(),
            floor_modules: vec!["std.prelude".to_owned(), "std.runtime.core".to_owned()],
            contract_modules: vec!["std.runtime.scheduler".to_owned()],
            blocks: vec![
                ModuleBlockV1 {
                    path: "std.prelude".to_owned(),
                    definitions: BlockRangeV1 { start: 0, len: 2 },
                    owners: BlockRangeV1 { start: 0, len: 1 },
                },
                ModuleBlockV1 {
                    path: "std.runtime.core".to_owned(),
                    definitions: BlockRangeV1 { start: 2, len: 0 },
                    owners: BlockRangeV1 { start: 1, len: 0 },
                },
                ModuleBlockV1 {
                    path: "std.runtime.scheduler".to_owned(),
                    definitions: BlockRangeV1 { start: 2, len: 1 },
                    owners: BlockRangeV1 { start: 1, len: 2 },
                },
                ModuleBlockV1 {
                    path: "std.vec".to_owned(),
                    definitions: BlockRangeV1 { start: 3, len: 1 },
                    owners: BlockRangeV1 { start: 3, len: 1 },
                },
            ],
        }
    }

    #[test]
    fn closure_keeps_floor_contracts_and_known_imports() {
        let prebuilt = sample_prebuilt();
        prebuilt.validate().expect("样例物化表合法");
        let closure = closure_program_modules(["std.vec", "std.unknown"], &prebuilt);
        assert_eq!(
            closure.into_iter().collect::<Vec<_>>(),
            vec![
                "std.prelude",
                "std.runtime.core",
                "std.runtime.scheduler",
                "std.vec",
            ]
        );
    }

    #[test]
    fn floor_outside_module_order_is_rejected() {
        let mut prebuilt = sample_prebuilt();
        prebuilt.floor_modules = vec!["std.runtime.core".to_owned(), "std.nope".to_owned()];
        assert!(prebuilt.validate().is_err());
    }
}
