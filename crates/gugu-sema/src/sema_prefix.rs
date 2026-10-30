//! 内建块前缀的语义层部分：名称、身份、HIR 片段、早期常量与类型检查表。
//!
//! 语义层只依赖这一部分；GIR 层的 body 与契约记录类型由上层 `prefix::BuiltinPrefix` 携带。
//! 物化表的结构校验与层 revision 比对由上层统一完成后才进入这里，因此本模块只解码并校验
//! 语义层各表。

use gugu_compiler_core::RuntimePrebuiltV1;
use serde::de::DeserializeOwned;

use crate::ParsedModule;
use crate::names;
use crate::semantics::Identities;
use crate::{Diagnostic, DiagnosticCode};

/// names 表的层名：域摘要由层名派生，因此查指纹不依赖表的声明顺序。
const NAMES_TABLE: &str = "names";

/// 身份表的层名：`DefRef` 与 `HIR DefId` 的双向映射。
const IDENTITIES_TABLE: &str = "identities";

/// HIR 片段的层名：内建模块的全部 HIR 表。
const HIR_TABLE: &str = "hir.module";

/// 早期常量表的层名：内建顶层 const/static 的 comptime 求值结果。
const EARLY_TABLE: &str = "early.constants";

/// 类型检查表的层名：内建每个 body 的推断结果与义务证据。
const CHECKED_TABLE: &str = "checked.semantics";

/// 已校验的内建块语义层前缀：当前会话可直接安装的语义派生表。
pub struct SemaPrefix {
    names: names::NameResolution,
    identities: Identities,
    /// 内建模块的 HIR 片段：定义、类型与各侧表整段接管，用户侧只追加。已通过 verifier。
    hir: std::sync::Arc<crate::hir::Validated>,
    /// 内建顶层 const/static 的 comptime 求值结果；用户定义照旧现算后追加。
    early_constants: crate::semantics::comptime::EarlyConstTable,
    /// 内建每个 body 的类型检查结果；用户 body 现算后按定义序追加。
    checked: crate::semantics::CheckedSemantics,
    modules: Vec<String>,
    identity: [u8; 32],
}

impl SemaPrefix {
    /// 解码并校验语义层各表。
    ///
    /// 前置条件：调用方已完成物化表结构校验与层 revision 比对。`sources` 是本次会话安装的
    /// 内建源单元数（文件 ID 前缀长度）：物化表里的位置必须落在该前缀内，否则内建定义会引用
    /// 用户源码。表里只保存稳定字段，`Span` 携带的源码表身份是构建期会话的局部编号，因此在
    /// 加载时按当前源码表重建。
    pub fn decode(
        prebuilt: &RuntimePrebuiltV1,
        sources: usize,
        source_map: &crate::SourceMap,
    ) -> Result<Self, Vec<Diagnostic>> {
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
        let (hir, _) =
            crate::hir::Validated::freeze(decode_table(prebuilt, HIR_TABLE, &prebuilt.hir)?)
                .map_err(|error| invalid(&format!("内建块 HIR 片段结构非法：{error:?}")))?;
        if hir.module().definitions.len() != identities.origins().len()
            || hir.module().opaques.len() != identities.opaques.len()
            || hir.module().interfaces.len() != identities.interfaces.len()
        {
            return Err(invalid("内建块物化表的 HIR 与身份表不同源"));
        }
        verify_blocks(prebuilt, &hir)?;
        let early_constants: crate::semantics::comptime::EarlyConstTable =
            decode_table(prebuilt, EARLY_TABLE, &prebuilt.early_constants)?;
        let checked: crate::semantics::CheckedSemantics =
            decode_table(prebuilt, CHECKED_TABLE, &prebuilt.checked)?;
        Ok(Self {
            names,
            identities,
            hir: std::sync::Arc::new(hir),
            early_constants,
            checked,
            modules: prebuilt.module_order.clone(),
            identity: installed_tables_digest(prebuilt),
        })
    }

    /// 内建每个 body 的类型检查结果。
    pub fn checked(&self) -> &crate::semantics::CheckedSemantics {
        &self.checked
    }

    /// 内建顶层 const/static 的早期常量表。
    pub fn early_constants(&self) -> &crate::semantics::comptime::EarlyConstTable {
        &self.early_constants
    }

    /// 内建模块的 HIR 片段：已通过 verifier，两条加载路径必须给出同一份。
    pub fn hir(&self) -> &crate::hir::Validated {
        &self.hir
    }

    /// 安装表的内容身份：参与 query 键，缓存不能跨 artifact 复用。
    ///
    /// 世界身份（如 HIR `input_fingerprint`）只由源码与依赖决定，不带它；这里只保证会话内的
    /// 记忆化不会把另一份物化表的派生结果当成自己的。
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }

    /// 内建身份表：`DefRef` 与 `HIR DefId` 的映射，以及各 owner 的身份桶。
    pub fn identities(&self) -> &Identities {
        &self.identities
    }

    /// 名称解析前缀的定义数量：用户定义的 `names::DefId` 从它开始。
    pub fn definition_count(&self) -> u32 {
        u32::try_from(self.names.definitions.len()).expect("内建定义数量必须落在 u32")
    }

    /// 身份前缀的 `HIR DefId` 数量：用户定义的 HIR 编号从它开始。
    pub fn hir_definition_count(&self) -> u32 {
        u32::try_from(self.identities.origins().len()).expect("内建身份数量必须落在 u32")
    }

    /// 该模块路径是否由物化表覆盖；未覆盖的内建模块（探针源）追加在物化块之后派生。
    pub fn covers_module(&self, path: &str) -> bool {
        self.modules
            .binary_search_by(|candidate| candidate.as_str().cmp(path))
            .is_ok()
    }

    /// 内建名称解析片段。
    pub fn names(&self) -> &names::NameResolution {
        &self.names
    }

    /// 校验物化模块恰好占据模块表前缀。
    ///
    /// 前缀位置是物化表下标的全部依据：物化模块一旦被未覆盖的模块插到中间，表里的模块下标
    /// 就会指向别的模块。
    pub fn verify_installed(&self, modules: &[ParsedModule]) -> Result<(), Vec<Diagnostic>> {
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

/// 模块块必须逐段对应定义表与 owner 表：区间首尾相接、长度合计等于表长，且每个定义确实
/// 归属它所在块声明的模块编号。
fn verify_blocks(
    prebuilt: &RuntimePrebuiltV1,
    hir: &crate::hir::Validated,
) -> Result<(), Vec<Diagnostic>> {
    let mut definitions = 0_usize;
    let mut owners = 0_usize;
    for (index, block) in prebuilt.blocks.iter().enumerate() {
        definitions += block.definitions.len as usize;
        owners += block.owners.len as usize;
        for definition in &hir.module().definitions[block.definitions.start as usize
            ..(block.definitions.start + block.definitions.len) as usize]
        {
            if definition.module as usize != index {
                return Err(invalid("内建块物化表的模块块与定义归属不一致"));
            }
        }
    }
    if definitions != hir.module().definitions.len() || owners != hir.module().owners.len() {
        return Err(invalid("内建块物化表的模块块没有覆盖整张表"));
    }
    Ok(())
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
///
/// 覆盖全部安装表（含 GIR 层两张）：语义层 query 的记忆化同样不能跨 artifact 复用。
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
pub fn decode_table<T: DeserializeOwned>(
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

/// 物化表校验失败：compiler invariant，不是用户诊断。
pub fn invalid(message: &str) -> Vec<Diagnostic> {
    vec![Diagnostic::error(
        DiagnosticCode::BackendInvariant,
        format!("内建块物化表校验失败：{message}"),
        None,
    )]
}

/// 表指纹：(标识域摘要, 内容摘要)。构建期物化与加载期安装共用同一口径。
pub fn table_fingerprint(layer: &str, content: &[&[u8]]) -> ([u8; 32], [u8; 32]) {
    (
        keyed_digest("gugu-prebuilt-table-domain-v1", &[layer.as_bytes()]),
        keyed_digest("gugu-prebuilt-table-content-v1", content),
    )
}

/// 物化表里为某一层声明的内容摘要；层名决定域摘要，因此查表不依赖表顺序。
pub fn declared_content_digest(prebuilt: &RuntimePrebuiltV1, layer: &str) -> Option<[u8; 32]> {
    let domain = keyed_digest("gugu-prebuilt-table-domain-v1", &[layer.as_bytes()]);
    prebuilt
        .fingerprints
        .iter()
        .find(|(candidate, _)| *candidate == domain)
        .map(|(_, content)| *content)
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
