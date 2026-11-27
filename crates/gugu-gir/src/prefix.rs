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
use std::sync::{Arc, Mutex, OnceLock};

use crate::Diagnostic;
use crate::prebuild::layer_revisions;
use crate::sema_prefix::{
    SemaPrefix, declared_content_digest, decode_table, invalid, table_fingerprint,
};

/// 契约记录类型的层名：七个 runtime layout verifier 的校验对象。
const RECORD_TYPES_TABLE: &str = "record.types";

/// GIR body 表的层名：内建 owner 的 generic GIR，与 owner 顺序一一对应。
const GIR_BODIES_TABLE: &str = "gir.bodies";

/// 最多保留八份内层缓冲身份；小表线性点查避免为少量目标构造树节点。
const PREFIX_CACHE_CAPACITY: usize = 8;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PrefixCacheKey {
    target: String,
    content_identity: [u8; 32],
}

struct DecodedBuiltinPrefix {
    sema: Arc<crate::sema_prefix::DecodedSemaPrefix>,
    record_types: Vec<crate::gir::concrete::RecordTypes>,
    gir_bodies: gugu_base::SharedTable<crate::gir::GirBody>,
}

type PrefixCell = OnceLock<Result<Arc<DecodedBuiltinPrefix>, Vec<Diagnostic>>>;

struct CacheEntry {
    key: PrefixCacheKey,
    // 持有 Bytes 的所有权使指针身份在登记期间不被回收复用；元数据仍逐字段比较。
    encoded: RuntimePrebuiltV1,
    cell: Arc<PrefixCell>,
}

fn prefix_cache() -> &'static Mutex<Vec<CacheEntry>> {
    static CACHE: OnceLock<Mutex<Vec<CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn same_allocation(left: &RuntimePrebuiltV1, right: &RuntimePrebuiltV1) -> bool {
    let same_bytes = |left: &bytes::Bytes, right: &bytes::Bytes| {
        left.len() == right.len() && left.as_ptr() == right.as_ptr()
    };
    left.schema == right.schema
        && left.revisions == right.revisions
        && left.module_order == right.module_order
        && left.fingerprints == right.fingerprints
        && left.blocks == right.blocks
        && same_bytes(&left.names, &right.names)
        && same_bytes(&left.identities, &right.identities)
        && same_bytes(&left.hir, &right.hir)
        && same_bytes(&left.record_types, &right.record_types)
        && same_bytes(&left.early_constants, &right.early_constants)
        && same_bytes(&left.checked, &right.checked)
        && left.gir_bodies.len() == right.gir_bodies.len()
        && left
            .gir_bodies
            .iter()
            .zip(&right.gir_bodies)
            .all(|(left, right)| same_bytes(left, right))
}

fn cached_cell(
    registry: &Mutex<Vec<CacheEntry>>,
    prebuilt: &RuntimePrebuiltV1,
    target: &str,
    digest: impl FnOnce(&RuntimePrebuiltV1) -> [u8; 32],
) -> (Arc<PrefixCell>, [u8; 32]) {
    {
        let cache = registry.lock().expect("内建前缀缓存锁中毒");
        if let Some(entry) = cache
            .iter()
            .find(|entry| entry.key.target == target && same_allocation(&entry.encoded, prebuilt))
        {
            return (Arc::clone(&entry.cell), entry.key.content_identity);
        }
    }
    // 只有未登记的不可变缓冲才扫描字节；不同 artifact 的摘要与解码不持有 registry 锁。
    let content_identity = digest(prebuilt);
    let key = PrefixCacheKey {
        target: target.to_owned(),
        content_identity,
    };
    let mut cache = registry.lock().expect("内建前缀缓存锁中毒");
    if let Some(entry) = cache
        .iter()
        .find(|entry| entry.key.target == target && same_allocation(&entry.encoded, prebuilt))
    {
        return (Arc::clone(&entry.cell), entry.key.content_identity);
    }
    let cell = cache
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| Arc::clone(&entry.cell))
        .unwrap_or_else(|| Arc::new(OnceLock::new()));
    debug_assert!(cache.len() <= PREFIX_CACHE_CAPACITY);
    if cache.len() == PREFIX_CACHE_CAPACITY {
        cache.remove(0);
    }
    cache.push(CacheEntry {
        key,
        encoded: prebuilt.clone(),
        cell: Arc::clone(&cell),
    });
    (cell, content_identity)
}

fn load_base(
    registry: &Mutex<Vec<CacheEntry>>,
    prebuilt: &RuntimePrebuiltV1,
    target: &str,
) -> Result<Arc<DecodedBuiltinPrefix>, Vec<Diagnostic>> {
    prebuilt
        .validate()
        .map_err(|error| invalid(&format!("内建块物化表结构非法：{error}")))?;
    if prebuilt.revisions != layer_revisions() {
        return Err(invalid("内建块物化表的派生层 revision 与当前编译器不符"));
    }
    let (cell, content_identity) = cached_cell(
        registry,
        prebuilt,
        target,
        crate::sema_prefix::installed_tables_digest,
    );
    cell.get_or_init(|| decode_base(prebuilt, content_identity).map(Arc::new))
        .clone()
}

/// 已校验的内建块前缀：当前会话可直接安装的派生表。
pub struct BuiltinPrefix<'a> {
    data: Arc<DecodedBuiltinPrefix>,
    sema: SemaPrefix,
    /// 内建实例的函数表：按 key 升序，表项内容在求可安装集时逐项校验。
    functions: &'a [RuntimeFunctionArtifactV1],
}

impl<'a> BuiltinPrefix<'a> {
    /// 解码并校验内建块前缀。
    ///
    /// `sources` 是本次会话安装的内建源单元数（文件 ID 前缀长度），语义层据此校验表内位置。
    pub fn decode(
        prebuilt: Option<&'a RuntimePrebuiltV1>,
        target: &str,
        sources: usize,
        source_map: &crate::SourceMap,
    ) -> Result<Self, Vec<Diagnostic>> {
        let Some(prebuilt) = prebuilt else {
            return Err(invalid("runtime artifact 缺少内建块物化表"));
        };
        if !prebuilt.functions.is_empty()
            && crate::functions::declared_revisions(&prebuilt.function_revisions)
                != crate::functions::revisions()
        {
            return Err(invalid("内建块函数表的分析层 revision 与当前编译器不符"));
        }
        let data = load_base(prefix_cache(), prebuilt, target)?;
        let sema = SemaPrefix::install(Arc::clone(&data.sema), sources, source_map)?;
        Ok(Self {
            data,
            sema,
            functions: &prebuilt.functions,
        })
    }

    /// 语义层前缀：名称、身份、HIR 片段、早期常量与类型检查表。
    pub fn sema(&self) -> &SemaPrefix {
        &self.sema
    }

    /// 内建 owner 的 generic GIR body；下标与 HIR `owners` 一一对应。
    pub fn gir_body(&self, index: usize) -> Option<&crate::gir::GirBody> {
        self.data.gir_bodies.get(index)
    }

    /// 已共享的内建 GIR body 前缀；用户 owner 追加在当前 world 的 suffix。
    pub fn gir_bodies(&self) -> &gugu_base::SharedTable<crate::gir::GirBody> {
        &self.data.gir_bodies
    }

    /// 内建契约记录的类型目录；安装后仍需过七个 layout verifier。
    pub fn record_types(&self) -> &[crate::gir::concrete::RecordTypes] {
        &self.data.record_types
    }

    /// 内建实例的函数表；为空表示物化表未录制函数层。
    pub fn functions(&self) -> &'a [RuntimeFunctionArtifactV1] {
        self.functions
    }
}

fn decode_base(
    prebuilt: &RuntimePrebuiltV1,
    content_identity: [u8; 32],
) -> Result<DecodedBuiltinPrefix, Vec<Diagnostic>> {
    let sema = crate::sema_prefix::DecodedSemaPrefix::decode(prebuilt, content_identity)?;
    let record_types = decode_table(prebuilt, RECORD_TYPES_TABLE, &prebuilt.record_types)?;
    // GIR body 逐条编码，表摘要按「空表头 + 全部 body」一组计算，与构建期一致。
    let mut slices: Vec<&[u8]> = prebuilt.gir_bodies.iter().map(AsRef::as_ref).collect();
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
    let mut gir_bodies = gugu_base::SharedTable::from(gir_bodies);
    gir_bodies.share();
    Ok(DecodedBuiltinPrefix {
        sema,
        record_types,
        gir_bodies,
    })
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
                embed: None,
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
        for (index, (installed, derived)) in installed
            .definitions
            .iter()
            .take(count)
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
            let span = output.names.definition_span(index);
            assert_eq!(span.file().as_u32(), derived.span.file().as_u32());
            assert_eq!(span.start(), derived.span.start());
            assert_eq!(span.end(), derived.span.end());
        }
        assert_eq!(
            installed.imports.len(),
            derived.imports.len(),
            "内建导入整体安装"
        );
        for (index, (installed, derived)) in
            installed.imports.iter().zip(&derived.imports).enumerate()
        {
            assert_eq!(installed.module.index(), derived.module.index());
            assert_eq!(installed.alias, derived.alias);
            assert_eq!(installed.namespace, derived.namespace);
            assert_eq!(installed.public, derived.public);
            assert_eq!(
                names::resolved_target_key(&installed.target),
                names::resolved_target_key(&derived.target)
            );
            assert_eq!(
                output.names.import_span(index).start(),
                derived.span.start()
            );
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::{
        BuiltinPrefix, CacheEntry, PREFIX_CACHE_CAPACITY, cached_cell, load_base, prefix_cache,
    };
    use crate::{SourceMap, SourceSnapshot, TargetName};
    use std::sync::{Arc, Mutex};

    fn source_map(resources: crate::runtime::RuntimeResources<'_>, main: &str) -> SourceMap {
        let mut builtins = resources
            .source_blobs()
            .iter()
            .map(|source| {
                SourceSnapshot::from_str(&source.logical_path, &source.source)
                    .expect("artifact 源码快照有效")
            })
            .collect::<Vec<_>>();
        builtins.sort_by_key(|source| crate::is_probe_builtin_source(source.logical_path()));
        let mut source_map = SourceMap::new(vec![
            SourceSnapshot::from_str("main.gg", main).expect("用户源码快照有效"),
        ])
        .expect("用户源码表有效");
        source_map
            .prepend_builtin_prefix(builtins)
            .expect("artifact 与用户源码路径不冲突");
        source_map
    }

    #[test]
    fn decoded_base_is_shared_and_spans_are_rebound_per_session() {
        let resources = crate::testing::resources_for(TargetName::X86_64Linux);
        let materialized_sources = resources
            .source_blobs()
            .iter()
            .filter(|source| !crate::is_probe_builtin_source(&source.logical_path))
            .count();
        let first_map = source_map(resources, "fn main() {}\n");
        let first = BuiltinPrefix::decode(
            resources.prebuilt(),
            resources.target_name(),
            materialized_sources,
            &first_map,
        )
        .expect("首次解码内建基底");
        let first_table = first_map.table();
        let first_names = first.sema.names();
        let first_definition_span = first_names.definition_span(0).clone();
        let first_import_span = first_names.import_span(0).clone();

        let second_map = source_map(resources, "fn main() { let value = 1 }\n");
        let second = BuiltinPrefix::decode(
            resources.prebuilt(),
            resources.target_name(),
            materialized_sources,
            &second_map,
        )
        .expect("复用内建基底并安装到新会话");
        let second_names = second.sema.names();

        assert!(std::sync::Arc::ptr_eq(&first.data, &second.data));
        assert!(std::ptr::eq(
            &first_names.definitions[0],
            &second_names.definitions[0]
        ));
        assert!(std::ptr::eq(
            &first_names.imports[0],
            &second_names.imports[0]
        ));
        assert_ne!(first_table, second_map.table());
        assert_eq!(first_definition_span.table(), first_table);
        assert_eq!(first_import_span.table(), first_table);
        assert_eq!(first_names.definition_span(0), &first_definition_span);
        assert_eq!(first_names.import_span(0), &first_import_span);
        assert_eq!(second_names.definition_span(0).table(), second_map.table());
        assert_eq!(second_names.import_span(0).table(), second_map.table());
        assert_eq!(
            crate::canonical::to_vec(first_names).expect("首次会话名称表可编码"),
            crate::canonical::to_vec(second_names).expect("后续会话名称表可编码")
        );
    }

    #[test]
    fn warm_allocation_hit_skips_digest_and_equal_bytes_share_decoded_cell() {
        let resources = crate::testing::resources_for(TargetName::X86_64Linux);
        let prebuilt = resources.prebuilt().expect("物化表");
        let registry = Mutex::new(Vec::<CacheEntry>::new());
        let (_, identity) = cached_cell(
            &registry,
            prebuilt,
            resources.target_name(),
            crate::sema_prefix::installed_tables_digest,
        );
        let (same, same_identity) =
            cached_cell(&registry, prebuilt, resources.target_name(), |_| {
                panic!("同一 Bytes allocation 命中不能重算摘要")
            });
        assert_eq!(identity, same_identity);

        let mut equal_content = prebuilt.clone();
        equal_content.names = bytes::Bytes::copy_from_slice(&prebuilt.names);
        assert_ne!(equal_content.names.as_ptr(), prebuilt.names.as_ptr());
        let (equal, _) = cached_cell(
            &registry,
            &equal_content,
            resources.target_name(),
            crate::sema_prefix::installed_tables_digest,
        );
        assert!(Arc::ptr_eq(&same, &equal));
    }

    #[test]
    fn cache_cells_are_isolated_by_target() {
        let resources = crate::testing::resources_for(TargetName::X86_64Linux);
        let prebuilt = resources.prebuilt().expect("物化表");
        let registry = Mutex::new(Vec::<CacheEntry>::new());
        let (linux, _) = cached_cell(&registry, prebuilt, "x86_64-linux", |_| [1; 32]);
        let (windows, _) = cached_cell(&registry, prebuilt, "x86_64-windows", |_| [1; 32]);
        assert!(!Arc::ptr_eq(&linux, &windows));
    }

    #[test]
    fn warm_cache_rejects_changed_blocks_fingerprints_revisions_and_bytes() {
        let resources = crate::testing::resources_for(TargetName::X86_64Linux);
        let original = resources.prebuilt().expect("物化表");
        let target = resources.target_name();
        let registry = Mutex::new(Vec::<CacheEntry>::new());
        load_base(&registry, original, target).expect("填充合法基底缓存");

        let mut changed_blocks = original.clone();
        let last = changed_blocks.blocks.last_mut().expect("至少有一个模块块");
        last.definitions.len += 1;
        assert!(changed_blocks.validate().is_ok(), "末块范围仍结构连续");
        assert!(load_base(&registry, &changed_blocks, target).is_err());

        let mut changed_fingerprint = original.clone();
        changed_fingerprint.fingerprints[0].1[0] ^= 1;
        assert!(load_base(&registry, &changed_fingerprint, target).is_err());

        let mut changed_revision = original.clone();
        changed_revision.revisions[0].1 = changed_revision.revisions[0].1.wrapping_add(1);
        assert!(load_base(&registry, &changed_revision, target).is_err());

        let mut changed_bytes = original.clone();
        changed_bytes.names = bytes::Bytes::from_static(b"damaged names table");
        assert!(load_base(&registry, &changed_bytes, target).is_err());
    }

    #[test]
    fn registry_stays_bounded() {
        let resources = crate::testing::resources_for(TargetName::X86_64Linux);
        let prebuilt = resources.prebuilt().expect("物化表");
        let registry = Mutex::new(Vec::<CacheEntry>::new());
        for index in 0..=PREFIX_CACHE_CAPACITY {
            let target = format!("test-target-{index}");
            let (_, identity) = cached_cell(&registry, prebuilt, &target, |_| [index as u8; 32]);
            assert_eq!(identity, [index as u8; 32]);
        }
        assert_eq!(registry.lock().unwrap().len(), PREFIX_CACHE_CAPACITY);
    }

    #[test]
    fn decoded_cell_caches_failure_for_its_exact_content_identity() {
        let resources = crate::testing::resources_for(TargetName::X86_64Linux);
        let mut damaged = resources.prebuilt().expect("物化表").clone();
        damaged.names = bytes::Bytes::from_static(b"damaged names table");
        let registry = prefix_cache();
        assert!(load_base(registry, &damaged, resources.target_name()).is_err());
        assert!(load_base(registry, &damaged, resources.target_name()).is_err());
    }
}
