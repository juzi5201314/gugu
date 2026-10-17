//! 加载期的内建块前缀：解码物化表、重算指纹、校验编号前缀契约。
//!
//! 内建块在所有稠密编号空间里占据前缀，顺序由 artifact 的 source manifest 固定，因此物化
//! 表的下标可以直接沿用，跨块引用只走稳定键。序列化里的指纹只作断言：加载端重算表内容摘要
//! 并比对，再跑结构校验；缺少物化表、revision 不符或校验失败一律按 compiler invariant 失败，
//! 内建块没有源码回退路径。

use gugu_compiler_core::RuntimePrebuiltV1;
use serde::de::DeserializeOwned;

use crate::frontend::ParsedModule;
use crate::frontend::names;
use crate::frontend::prebuild::{declared_content_digest, layer_revisions, table_fingerprint};
use crate::frontend::semantics::Identities;
use crate::{Diagnostic, DiagnosticCode};

/// names 表的层名：域摘要由层名派生，因此查指纹不依赖表的声明顺序。
const NAMES_TABLE: &str = "names";

/// 身份表的层名：`DefRef` 与 `HIR DefId` 的双向映射。
const IDENTITIES_TABLE: &str = "identities";

/// HIR 片段的层名：内建模块的全部 HIR 表。
const HIR_TABLE: &str = "hir.module";

/// 契约记录类型的层名：七个 runtime layout verifier 的校验对象。
const RECORD_TYPES_TABLE: &str = "record.types";

/// GIR body 表的层名：内建 owner 的 generic GIR，与 owner 顺序一一对应。
const GIR_BODIES_TABLE: &str = "gir.bodies";

/// 早期常量表的层名：内建顶层 const/static 的 comptime 求值结果。
const EARLY_TABLE: &str = "early.constants";

/// 类型检查表的层名：内建每个 body 的推断结果与义务证据。
const CHECKED_TABLE: &str = "checked.semantics";

/// 已校验的内建块前缀：当前会话可直接安装的派生表。
pub(crate) struct BuiltinPrefix {
    names: names::NameResolution,
    identities: Identities,
    /// 内建模块的 HIR 片段：定义、类型与各侧表整段接管，用户侧只追加。已通过 verifier。
    hir: std::sync::Arc<crate::frontend::hir::Validated>,
    /// 内建契约记录的类型目录：layout verifier 在加载期逐字段校验它。
    record_types: Vec<crate::frontend::gir::concrete::RecordTypes>,
    /// 内建 owner 的 generic GIR body：按 owner 顺序排列，装载时逐条校验 owner 与稳定键。
    gir_bodies: Vec<crate::frontend::gir::GirBody>,
    /// 内建顶层 const/static 的 comptime 求值结果；用户定义照旧现算后追加。
    early_constants: crate::frontend::semantics::comptime::EarlyConstTable,
    /// 内建每个 body 的类型检查结果；用户 body 现算后按定义序追加。
    checked: crate::frontend::semantics::CheckedSemantics,
    modules: Vec<String>,
    identity: [u8; 32],
}

impl BuiltinPrefix {
    /// 解码并校验内建块前缀。
    ///
    /// `sources` 是本次会话安装的内建源单元数（文件 ID 前缀长度）：物化表里的位置必须落在
    /// 该前缀内，否则内建定义会引用用户源码。表里只保存稳定字段，`Span` 携带的源码表身份是
    /// 构建期会话的局部编号，因此在加载时按当前源码表重建。
    pub(crate) fn decode(
        prebuilt: Option<&RuntimePrebuiltV1>,
        sources: usize,
        source_map: &crate::SourceMap,
    ) -> Result<Self, Vec<Diagnostic>> {
        let Some(prebuilt) = prebuilt else {
            return Err(invalid("runtime artifact 缺少内建块物化表"));
        };
        prebuilt
            .validate()
            .map_err(|error| invalid(&format!("内建块物化表结构非法：{error}")))?;
        if prebuilt.revisions != layer_revisions() {
            return Err(invalid("内建块物化表的派生层 revision 与当前编译器不符"));
        }
        let declared = declared_content_digest(prebuilt, NAMES_TABLE)
            .ok_or_else(|| invalid("内建块物化表缺少 names 表指纹"))?;
        if table_fingerprint(NAMES_TABLE, &[&prebuilt.names]).1 != declared {
            return Err(invalid("内建块物化表的 names 表内容摘要不符"));
        }
        let mut names: names::NameResolution = crate::canonical::from_slice(&prebuilt.names)
            .map_err(|_| invalid("内建块的名称解析表不能解码"))?;
        if !names::resolution_is_valid(&names) {
            return Err(invalid("内建块的名称解析表结构不自洽"));
        }
        verify_positions(&names, prebuilt.module_order.len(), sources)?;
        rebind_spans(&mut names, source_map)?;
        let identities: Identities =
            decode_table(prebuilt, IDENTITIES_TABLE, &prebuilt.identities)?;
        // HIR 片段在这里跑完整结构 verifier：定义稳定键唯一、侧表与 owner 下标自洽、类型与
        // 声明引用完整。表字节的摘要已在上面比对过，这里钉的是结构。
        let (hir, _) = crate::frontend::hir::Validated::freeze(decode_table(
            prebuilt,
            HIR_TABLE,
            &prebuilt.hir,
        )?)
        .map_err(|error| invalid(&format!("内建块 HIR 片段结构非法：{error:?}")))?;
        if hir.module().definitions.len() != identities.origins().len()
            || hir.module().opaques.len() != identities.opaques.len()
            || hir.module().interfaces.len() != identities.interfaces.len()
        {
            return Err(invalid("内建块物化表的 HIR 与身份表不同源"));
        }
        let record_types: Vec<crate::frontend::gir::concrete::RecordTypes> =
            decode_table(prebuilt, RECORD_TYPES_TABLE, &prebuilt.record_types)?;
        // GIR body 逐条编码，表摘要按「空表头 + 全部 body」一组计算，与构建期一致。
        let mut slices: Vec<&[u8]> = prebuilt.gir_bodies.iter().map(Vec::as_slice).collect();
        slices.insert(0, &[]);
        let declared = declared_content_digest(prebuilt, GIR_BODIES_TABLE)
            .ok_or_else(|| invalid("内建块物化表缺少 gir.bodies 表指纹"))?;
        if table_fingerprint(GIR_BODIES_TABLE, &slices).1 != declared {
            return Err(invalid("内建块物化表的 gir.bodies 表内容摘要不符"));
        }
        let mut gir_bodies = Vec::with_capacity(prebuilt.gir_bodies.len());
        for encoded in &prebuilt.gir_bodies {
            gir_bodies.push(
                crate::canonical::from_slice(encoded)
                    .map_err(|_| invalid("内建块的 gir.bodies 表不能解码"))?,
            );
        }
        let early_constants: crate::frontend::semantics::comptime::EarlyConstTable =
            decode_table(prebuilt, EARLY_TABLE, &prebuilt.early_constants)?;
        let checked: crate::frontend::semantics::CheckedSemantics =
            decode_table(prebuilt, CHECKED_TABLE, &prebuilt.checked)?;
        Ok(Self {
            names,
            identities,
            hir: std::sync::Arc::new(hir),
            record_types,
            gir_bodies,
            early_constants,
            checked,
            modules: prebuilt.module_order.clone(),
            identity: installed_tables_digest(prebuilt),
        })
    }

    /// 内建每个 body 的类型检查结果。
    pub(crate) fn checked(&self) -> &crate::frontend::semantics::CheckedSemantics {
        &self.checked
    }

    /// 内建顶层 const/static 的早期常量表。
    pub(crate) fn early_constants(&self) -> &crate::frontend::semantics::comptime::EarlyConstTable {
        &self.early_constants
    }

    /// 内建 owner 的 generic GIR body；下标与 HIR `owners` 一一对应。
    pub(crate) fn gir_body(&self, index: usize) -> Option<&crate::frontend::gir::GirBody> {
        self.gir_bodies.get(index)
    }

    /// 内建契约记录的类型目录；安装后仍需过七个 layout verifier。
    pub(crate) fn record_types(&self) -> &[crate::frontend::gir::concrete::RecordTypes] {
        &self.record_types
    }

    /// 内建模块的 HIR 片段：已通过 verifier，两条加载路径必须给出同一份。
    pub(crate) fn hir(&self) -> &crate::frontend::hir::Validated {
        &self.hir
    }

    /// 安装表的内容身份：参与 query 键，缓存不能跨 artifact 复用。
    ///
    /// 世界身份（如 HIR `input_fingerprint`）只由源码与依赖决定，不带它；这里只保证会话内的
    /// 记忆化不会把另一份物化表的派生结果当成自己的。
    pub(crate) fn identity(&self) -> [u8; 32] {
        self.identity
    }

    /// 内建身份表：`DefRef` 与 `HIR DefId` 的映射，以及各 owner 的身份桶。
    pub(crate) fn identities(&self) -> &Identities {
        &self.identities
    }

    /// 名称解析前缀的定义数量：用户定义的 `names::DefId` 从它开始。
    pub(crate) fn definition_count(&self) -> u32 {
        u32::try_from(self.names.definitions.len()).expect("内建定义数量必须落在 u32")
    }

    /// 身份前缀的 `HIR DefId` 数量：用户定义的 HIR 编号从它开始。
    pub(crate) fn hir_definition_count(&self) -> u32 {
        u32::try_from(self.identities.origins().len()).expect("内建身份数量必须落在 u32")
    }

    /// 该模块路径是否由物化表覆盖；未覆盖的内建模块（探针源）追加在物化块之后派生。
    pub(crate) fn covers_module(&self, path: &str) -> bool {
        self.modules
            .binary_search_by(|candidate| candidate.as_str().cmp(path))
            .is_ok()
    }

    /// 内建名称解析片段。
    pub(crate) fn names(&self) -> &names::NameResolution {
        &self.names
    }

    /// 校验物化模块恰好占据模块表前缀。
    ///
    /// 前缀位置是物化表下标的全部依据：物化模块一旦被未覆盖的模块插到中间，表里的模块下标
    /// 就会指向别的模块。
    pub(crate) fn verify_installed(&self, modules: &[ParsedModule]) -> Result<(), Vec<Diagnostic>> {
        if modules.len() < self.modules.len() {
            return Err(invalid("模块表比内建块物化表短"));
        }
        for (index, path) in self.modules.iter().enumerate() {
            if &modules[index].path != path {
                return Err(invalid(&format!(
                    "物化模块 `{path}` 不在模块表前缀的第 {index} 位"
                )));
            }
        }
        // 身份桶按会话 AST 的 arena 尺寸取值：长度不合说明物化表与本次源码树不是同一份。
        for (index, parsed) in modules.iter().enumerate().take(self.modules.len()) {
            let fits = self.identities.items[index].len() == parsed.arena.items.len()
                && self.identities.functions[index].len() == parsed.arena.fns.len()
                && self.identities.asynchronous[index].len() == parsed.arena.exprs.len()
                && self.identities.local_statics[index].len() == parsed.arena.stmts.len();
            if !fits {
                return Err(invalid(&format!(
                    "物化身份表的 `{}` 行与会话 AST 尺寸不符",
                    parsed.path
                )));
            }
        }
        // 身份表里的定义下标必须落在前缀内。
        let definitions =
            u32::try_from(self.identities.origins().len()).expect("内建定义数量必须落在 u32");
        let buckets = self
            .identities
            .items
            .iter()
            .chain(&self.identities.functions)
            .chain(&self.identities.asynchronous)
            .chain(&self.identities.local_statics);
        if buckets
            .flatten()
            .flatten()
            .any(|definition| definition.index() >= definitions as usize)
        {
            return Err(invalid("物化身份表引用越出内建定义前缀"));
        }
        Ok(())
    }
}

/// 把物化表里的 `Span` 重绑到当前会话的源码表。
///
/// 持久形式只保留稳定字段（文件下标、字节区间、展开下标）；源码表身份是 session 局部的，
/// 跨会话沿用会让 HIR 的位置校验与诊断重锚定失效。
fn rebind_spans(
    names: &mut names::NameResolution,
    source_map: &crate::SourceMap,
) -> Result<(), Vec<Diagnostic>> {
    let rebind = |span: &crate::Span| {
        source_map
            .span(
                span.file(),
                span.start() as usize,
                span.end() as usize,
                span.expansion(),
            )
            .map_err(|_| invalid("内建块物化表的源码位置不能在当前源码表里重建"))
    };
    for definition in &mut names.definitions {
        definition.span = rebind(&definition.span)?;
    }
    for import in &mut names.imports {
        import.span = rebind(&import.span)?;
    }
    Ok(())
}

/// 安装表的内容身份：容器 schema、层 revision 与各表字节。
fn installed_tables_digest(prebuilt: &RuntimePrebuiltV1) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-builtin-prefix-v1");
    hash.update(&prebuilt.schema.to_le_bytes());
    for (layer, revision) in &prebuilt.revisions {
        hash.update(layer.as_bytes());
        hash.update(&revision.to_le_bytes());
    }
    for table in [
        &prebuilt.names[..],
        &prebuilt.identities[..],
        &prebuilt.hir[..],
        &prebuilt.record_types[..],
        &prebuilt.early_constants[..],
        &prebuilt.checked[..],
    ] {
        hash.update(&(table.len() as u64).to_le_bytes());
        hash.update(table);
    }
    for body in &prebuilt.gir_bodies {
        hash.update(&(body.len() as u64).to_le_bytes());
        hash.update(body);
    }
    *hash.finalize().as_bytes()
}

/// 解码一张物化表：先核对表内容摘要，再按当前 schema 解码。
fn decode_table<T: DeserializeOwned>(
    prebuilt: &RuntimePrebuiltV1,
    layer: &str,
    encoded: &[u8],
) -> Result<T, Vec<Diagnostic>> {
    let declared = declared_content_digest(prebuilt, layer)
        .ok_or_else(|| invalid(&format!("内建块物化表缺少 {layer} 表指纹")))?;
    if table_fingerprint(layer, &[encoded]).1 != declared {
        return Err(invalid(&format!("内建块物化表的 {layer} 表内容摘要不符")));
    }
    crate::canonical::from_slice(encoded)
        .map_err(|_| invalid(&format!("内建块的 {layer} 表不能解码")))
}

/// 校验物化表里的位置都落在内建块前缀内。
fn verify_positions(
    names: &names::NameResolution,
    modules: usize,
    sources: usize,
) -> Result<(), Vec<Diagnostic>> {
    let modules = u32::try_from(modules).expect("内建模块数量必须落在 u32");
    let sources = u32::try_from(sources).expect("内建源数量必须落在 u32");
    for definition in &names.definitions {
        let module = u32::try_from(definition.module.index()).expect("模块编号必须落在 u32");
        if module >= modules || definition.span.file().as_u32() >= sources {
            return Err(invalid("内建定义越出内建块前缀"));
        }
    }
    for import in &names.imports {
        let module = u32::try_from(import.module.index()).expect("模块编号必须落在 u32");
        let target = match import.target {
            names::ResolvedTarget::Module(target) => {
                u32::try_from(target.index()).expect("模块编号必须落在 u32") >= modules
            }
            names::ResolvedTarget::Def(target) => {
                u32::try_from(target.index()).expect("定义编号必须落在 u32")
                    >= names.definitions.len() as u32
            }
            names::ResolvedTarget::External(_) => false,
        };
        if module >= modules || target || import.span.file().as_u32() >= sources {
            return Err(invalid("内建导入越出内建块前缀"));
        }
    }
    Ok(())
}

fn invalid(message: &str) -> Vec<Diagnostic> {
    vec![Diagnostic::error(
        DiagnosticCode::BackendInvariant,
        format!("内建块物化表校验失败：{message}"),
        None,
    )]
}

#[cfg(all(test, feature = "slow-tests"))]
mod tests {
    use super::*;
    use crate::frontend::cfg::CfgContext;
    use crate::frontend::derive_builtin;
    use crate::{QueryEngine, SourceMap, TargetName};

    /// 安装到会话里的内建名称前缀必须与现算的内建派生逐字段一致。
    ///
    /// 这条断言锁住编号前缀契约：物化表的下标、稳定键、父定义与导入目标都要与同一份源在
    /// 当前编译器下派生出的结果相同，否则加载路径与源码路径会给出不同的世界。
    #[test]
    fn installed_prefix_matches_a_fresh_builtin_derivation() {
        let target = TargetName::X86_64Linux;
        let resources = crate::testing::resources();
        let derivation = derive_builtin(
            &mut SourceMap::empty(),
            &QueryEngine::new(),
            resources,
            &CfgContext::target_only(target),
        )
        .expect("内建块派生");
        let derived: names::NameResolution =
            crate::canonical::from_slice(&derivation.names).expect("派生表解码");

        let mut sources = SourceMap::new(vec![
            crate::SourceSnapshot::from_str("main.gg", "fn main() {}").unwrap(),
        ])
        .unwrap();
        let cfg = CfgContext::target_only(target);
        let output = crate::frontend::bootstrap(
            crate::frontend::SourceInput::Sources {
                source_map: &mut sources,
                entry: "main.gg",
                source_root: "",
                package_identity: "tests/prefix@1",
                require_main: true,
                cfg: &cfg,
                external_packages: &Default::default(),
            },
            &QueryEngine::new(),
            resources,
        )
        .expect("示例编译");
        let installed = &output.names;
        let count = derived.definitions.len();
        assert_eq!(
            installed.definitions.len(),
            count + 1,
            "用户定义追加在物化前缀之后"
        );
        for (index, (installed, derived)) in installed.definitions[..count]
            .iter()
            .zip(&derived.definitions)
            .enumerate()
        {
            assert_eq!(installed.id.index(), index, "定义编号占据前缀");
            assert_eq!(installed.stable_key, derived.stable_key);
            assert_eq!(installed.module.index(), derived.module.index());
            assert_eq!(
                installed.parent.map(|parent| parent.index()),
                derived.parent.map(|parent| parent.index())
            );
            assert_eq!(installed.module_binding, derived.module_binding);
            assert_eq!(installed.name, derived.name);
            assert_eq!(installed.kind, derived.kind);
            assert_eq!(installed.namespace, derived.namespace);
            assert_eq!(installed.visibility, derived.visibility);
            assert_eq!(installed.span.file().as_u32(), derived.span.file().as_u32());
            assert_eq!(installed.span.start(), derived.span.start());
            assert_eq!(installed.span.end(), derived.span.end());
        }
        assert_eq!(
            installed.imports.len(),
            derived.imports.len(),
            "内建导入整体安装"
        );
        for (installed, derived) in installed.imports.iter().zip(&derived.imports) {
            assert_eq!(installed.module.index(), derived.module.index());
            assert_eq!(installed.alias, derived.alias);
            assert_eq!(installed.namespace, derived.namespace);
            assert_eq!(installed.public, derived.public);
            assert_eq!(
                names::resolved_target_key(&installed.target),
                names::resolved_target_key(&derived.target)
            );
            assert_eq!(installed.span.start(), derived.span.start());
        }
    }
}
