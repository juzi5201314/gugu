//! 加载期的内建块前缀：解码物化表、重算指纹、校验编号前缀契约。
//!
//! 内建块在所有稠密编号空间里占据前缀，顺序由 artifact 的 source manifest 固定，因此物化
//! 表的下标可以直接沿用，跨块引用只走稳定键。序列化里的指纹只作断言：加载端重算表内容摘要
//! 并比对，再跑结构校验；缺少物化表、revision 不符或校验失败一律按 compiler invariant 失败，
//! 内建块没有源码回退路径。
//!
//! 语义层各表由 [`SemaPrefix`] 解码与持有；这里在其上追加 GIR 层的 body、契约记录类型与
//! 内建实例的函数表。

use gugu_compiler_core::{RuntimeFunctionArtifactV1, RuntimePrebuiltV1};

use crate::Diagnostic;
use crate::prebuild::layer_revisions;
use crate::sema_prefix::{
    SemaPrefix, declared_content_digest, decode_table, invalid, table_fingerprint,
};

/// 契约记录类型的层名：七个 runtime layout verifier 的校验对象。
const RECORD_TYPES_TABLE: &str = "record.types";

/// GIR body 表的层名：内建 owner 的 generic GIR，与 owner 顺序一一对应。
const GIR_BODIES_TABLE: &str = "gir.bodies";

/// 已校验的内建块前缀：当前会话可直接安装的派生表。
pub struct BuiltinPrefix<'a> {
    sema: SemaPrefix,
    /// 内建契约记录的类型目录：layout verifier 在加载期逐字段校验它。
    record_types: Vec<crate::gir::concrete::RecordTypes>,
    /// 内建 owner 的 generic GIR body：按 owner 顺序排列，装载时逐条校验 owner 与稳定键。
    gir_bodies: Vec<crate::gir::GirBody>,
    /// 内建实例的函数表：按 key 升序，表项内容在求可安装集时逐项校验。
    functions: &'a [RuntimeFunctionArtifactV1],
}

impl<'a> BuiltinPrefix<'a> {
    /// 解码并校验内建块前缀。
    ///
    /// `sources` 是本次会话安装的内建源单元数（文件 ID 前缀长度），语义层据此校验表内位置。
    pub fn decode(
        prebuilt: Option<&'a RuntimePrebuiltV1>,
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
        if !prebuilt.functions.is_empty()
            && crate::functions::declared_revisions(&prebuilt.function_revisions)
                != crate::functions::revisions()
        {
            return Err(invalid("内建块函数表的分析层 revision 与当前编译器不符"));
        }
        let sema = SemaPrefix::decode(prebuilt, sources, source_map)?;
        let record_types: Vec<crate::gir::concrete::RecordTypes> =
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
        Ok(Self {
            sema,
            record_types,
            gir_bodies,
            functions: &prebuilt.functions,
        })
    }

    /// 语义层前缀：名称、身份、HIR 片段、早期常量与类型检查表。
    pub fn sema(&self) -> &SemaPrefix {
        &self.sema
    }

    /// 内建 owner 的 generic GIR body；下标与 HIR `owners` 一一对应。
    pub fn gir_body(&self, index: usize) -> Option<&crate::gir::GirBody> {
        self.gir_bodies.get(index)
    }

    /// 内建契约记录的类型目录；安装后仍需过七个 layout verifier。
    pub fn record_types(&self) -> &[crate::gir::concrete::RecordTypes] {
        &self.record_types
    }

    /// 内建实例的函数表；为空表示物化表未录制函数层。
    pub fn functions(&self) -> &'a [RuntimeFunctionArtifactV1] {
        self.functions
    }
}

#[cfg(all(test, feature = "slow-tests"))]
mod tests {
    use crate::cfg::CfgContext;
    use crate::derive_builtin;
    use crate::names;
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
        let output = crate::bootstrap(
            crate::SourceInput::Sources {
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
