//! 加载期的内建块前缀：解码物化表、重算指纹、校验编号前缀契约。
//!
//! 内建块在所有稠密编号空间里占据前缀，顺序由 artifact 的 source manifest 固定，因此物化
//! 表的下标可以直接沿用，跨块引用只走稳定键。序列化里的指纹只作断言：加载端重算表内容摘要
//! 并比对，再跑结构校验；缺少物化表、revision 不符或校验失败一律按 compiler invariant 失败，
//! 内建块没有源码回退路径。

use gugu_compiler_core::RuntimePrebuiltV1;

use crate::frontend::ParsedModule;
use crate::frontend::names;
use crate::frontend::prebuild::{declared_content_digest, layer_revisions, table_fingerprint};
use crate::{Diagnostic, DiagnosticCode};

/// names 表的层名：域摘要由层名派生，因此查指纹不依赖表的声明顺序。
const NAMES_TABLE: &str = "names";

/// 已校验的内建块前缀：当前会话可直接安装的派生表。
pub(crate) struct BuiltinPrefix {
    names: names::NameResolution,
    modules: Vec<String>,
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
        Ok(Self {
            names,
            modules: prebuilt.module_order.clone(),
        })
    }

    /// 前缀定义数量：用户定义的编号从它开始。
    pub(crate) fn definition_count(&self) -> u32 {
        u32::try_from(self.names.definitions.len()).expect("内建定义数量必须落在 u32")
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
