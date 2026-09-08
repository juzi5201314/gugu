//! 闭世界闭合 driver：根 -> 实例 -> 边迭代收敛，`MonoId` 分配与实例图 SCC。
//!
//! pending 集合按 `MonoKey` 摘要字节序 `BTreeSet` 稳定 pop；迭代式 driver 不
//! 形成嵌套 query 环。预算沿实例化 ancestry 检查：链上不同 key 超过 256，或
//! 同一 definition 以严格增长的类型结构重复 128 次，报 `E0052`；总实例达
//! `u32::MAX` 报 `E0053`。

use super::instantiate::{
    self, AncestryLink, InstanceRecordV1, WalkEntry, arguments_structure, check_ancestry,
    walk_entry,
};
use super::keys::{MonoContext, MonoInterner, MonoKind, hash_domain};
use super::roots::{self, RootCategoryV1, RootSeed};
use crate::Diagnostic;
use crate::query::QueryEngine;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// `MonoWorldV1` schema。
pub(crate) const MONO_SCHEMA: u32 = 3;

/// 闭合后的实例图：实例按 key 摘要排序，`MonoId` 为排序后下标。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MonoWorldV1 {
    pub schema: u32,
    pub input_fingerprint: [u8; 32],
    /// 根 `MonoKey` 摘要（排序）。
    pub roots: Vec<[u8; 32]>,
    pub root_categories: Vec<RootCategoryV1>,
    /// 闭合实例摘要（按 key 摘要排序，下标即 `MonoId`）。
    pub instances: Vec<InstanceSummaryV1>,
    /// 闭世界 vtable/metadata 类型根（规范字节，排序去重）。
    pub metadata_roots: Vec<Vec<u8>>,
    /// 外部导入符号（排序去重）。
    pub externals: Vec<String>,
    /// 实例图指纹（全部实例 key + 边摘要）。
    pub graph_fingerprint: [u8; 32],
    /// 公共摘要：实例 key 摘要 hex -> 对象 key（summary 投影阶段填充）。
    pub public_summaries: BTreeMap<String, [u8; 32]>,
    pub universe: crate::frontend::late::universe::TypeUniverse,
    pub late: crate::frontend::late::LateTable,
}

/// 闭合实例的规范摘要投影。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InstanceSummaryV1 {
    /// `MonoKey` 规范字节。
    pub mono_key: Vec<u8>,
    /// HIR 定义编号；generic GIR fragment 用它映射到 owner body。
    pub definition: u32,
    pub kind: MonoKind,
    pub symbol: String,
    pub public: bool,
    pub parameter_count: u32,
    pub signature_and_abi_fingerprint: [u8; 32],
    pub call_targets: Vec<(super::instantiate::CallSite, [u8; 32])>,
    /// callee `MonoKey` 摘要（排序去重）。
    pub callees: Vec<[u8; 32]>,
    pub selected_impls: Vec<[u8; 32]>,
    pub vtable_roots: Vec<super::instantiate::VtableRootV1>,
    pub metadata_roots: Vec<Vec<u8>>,
    pub uses_late_comptime: bool,
    pub fragment_input_fingerprint: [u8; 32],
    pub types: Vec<crate::frontend::late::universe::TypeRecord>,
    pub type_bindings: Vec<(u32, super::keys::StableTypeKey)>,
}

/// 收集根并闭合可达实例图。
pub(crate) fn close(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    queries: &QueryEngine,
) -> Result<MonoWorldV1, Vec<Diagnostic>> {
    let (_roots, seeds) = roots::collect(context, interner, queries)?;
    let mut driver = Driver::new(context, interner, queries);
    for seed in seeds {
        driver.enqueue_root(&seed)?;
    }
    driver.run()?;
    Ok(driver.world(input_fingerprint(context)))
}

/// 闭合输入指纹：模块指纹 + 目标 + harness。
pub(crate) fn input_fingerprint(context: &MonoContext<'_>) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&context.module.input_fingerprint);
    bytes.extend_from_slice(context.target.to_string().as_bytes());
    bytes.push(u8::from(context.harness));
    hash_domain("gugu-mono-close-input-v1", &bytes)
}

/// 空 package 的空实例图。
pub(crate) fn empty_world() -> MonoWorldV1 {
    MonoWorldV1 {
        schema: MONO_SCHEMA,
        input_fingerprint: [0; 32],
        roots: Vec::new(),
        root_categories: Vec::new(),
        instances: Vec::new(),
        metadata_roots: Vec::new(),
        externals: Vec::new(),
        graph_fingerprint: [0; 32],
        public_summaries: BTreeMap::new(),
        universe: Default::default(),
        late: Default::default(),
    }
}

struct Driver<'a, 'b> {
    context: &'a MonoContext<'b>,
    interner: &'a mut MonoInterner,
    queries: &'a QueryEngine,
    /// pending：按 key 摘要字节序稳定 pop。
    pending: BTreeSet<[u8; 32]>,
    /// 已入队 key -> 工作条目 + ancestry。
    queued: BTreeMap<[u8; 32], (WalkEntry, Vec<AncestryLink>)>,
    /// 已完成实例：按摘要排序（即最终 MonoId 顺序）。
    done: BTreeMap<[u8; 32], InstanceRecordV1>,
    root_digests: BTreeSet<[u8; 32]>,
    root_categories: BTreeMap<[u8; 32], RootCategoryV1>,
}

impl<'a, 'b> Driver<'a, 'b> {
    fn new(
        context: &'a MonoContext<'b>,
        interner: &'a mut MonoInterner,
        queries: &'a QueryEngine,
    ) -> Self {
        Self {
            context,
            interner,
            queries,
            pending: BTreeSet::new(),
            queued: BTreeMap::new(),
            done: BTreeMap::new(),
            root_digests: BTreeSet::new(),
            root_categories: BTreeMap::new(),
        }
    }

    fn enqueue_root(&mut self, seed: &RootSeed) -> Result<(), Vec<Diagnostic>> {
        let digest = seed.key.digest();
        self.root_digests.insert(digest);
        self.root_categories
            .entry(digest)
            .and_modify(|existing| {
                if seed.category < *existing {
                    *existing = seed.category;
                }
            })
            .or_insert(seed.category);
        if self.done.contains_key(&digest) || self.queued.contains_key(&digest) {
            return Ok(());
        }
        self.pending.insert(digest);
        self.queued.insert(digest, (seed.entry.clone(), Vec::new()));
        Ok(())
    }

    /// 迭代 driver：按稳定摘要顺序取出实例并登记尚未发现的 callee。
    ///
    /// 同 key 递归复用相同实例节点（`queued`/`done` 判定），只形成图边；
    /// 新 key 的 ancestry = 发现者 ancestry + 发现者链接。
    fn run(&mut self) -> Result<(), Vec<Diagnostic>> {
        while let Some(digest) = self.pending.pop_first() {
            let (entry, ancestry) = self
                .queued
                .remove(&digest)
                .ok_or_else(|| vec![missing_entry()])?;
            let record =
                instantiate::instantiate(self.context, self.interner, self.queries, &entry)?;
            let arguments: Vec<_> = entry.bindings.values().cloned().collect();
            let link = AncestryLink {
                definition: entry.key.definition,
                type_structure: arguments_structure(&arguments),
            };
            for callee in &record.callees {
                self.enqueue_callee(&digest, &ancestry, &link, callee)?;
            }
            if self.done.len() >= u32::MAX as usize {
                return Err(vec![instance_limit()]);
            }
            self.done.insert(digest, record);
        }
        Ok(())
    }

    fn enqueue_callee(
        &mut self,
        parent: &[u8; 32],
        ancestry: &[AncestryLink],
        link: &AncestryLink,
        callee: &super::instantiate::CalleeSeed,
    ) -> Result<(), Vec<Diagnostic>> {
        let digest = callee.key.digest();
        if digest == *parent || self.done.contains_key(&digest) || self.queued.contains_key(&digest)
        {
            return Ok(());
        }
        let mut chain = ancestry.to_vec();
        chain.push(link.clone());
        check_ancestry(&chain, &callee.key, arguments_structure(&callee.arguments))
            .map_err(|error| vec![error])?;
        let entry = walk_entry(
            self.context,
            callee.key.clone(),
            callee.definition,
            &callee.arguments,
        )
        .map_err(|error| vec![error])?;
        self.pending.insert(digest);
        self.queued.insert(digest, (entry, chain));
        Ok(())
    }

    fn world(self, input_fingerprint: [u8; 32]) -> MonoWorldV1 {
        let digests: Vec<[u8; 32]> = self.done.keys().copied().collect();
        let mut externals: BTreeSet<String> = BTreeSet::new();
        let instances: Vec<_> = self
            .done
            .into_values()
            .map(|record| {
                externals.extend(record.externals.iter().cloned());
                summary_of(record)
            })
            .collect();
        let mut metadata_roots: BTreeSet<Vec<u8>> = BTreeSet::new();
        for instance in &instances {
            metadata_roots.extend(instance.metadata_roots.iter().cloned());
            metadata_roots.extend(
                instance
                    .vtable_roots
                    .iter()
                    .map(|root| root.self_type.clone()),
            );
        }
        let roots: Vec<[u8; 32]> = digests
            .iter()
            .filter(|digest| self.root_digests.contains(*digest))
            .copied()
            .collect();
        let root_categories: Vec<_> = roots
            .iter()
            .map(|digest| {
                self.root_categories
                    .get(digest)
                    .copied()
                    .expect("每个 root digest 都已登记类别")
            })
            .collect();
        let mut hash = blake3::Hasher::new_derive_key("gugu-mono-graph-v2");
        serde_json::to_writer(
            &mut hash,
            &(
                &roots,
                &root_categories,
                &instances,
                &metadata_roots,
                &externals,
            ),
        )
        .expect("闭世界图仅包含可规范序列化的稳定身份与实例事实");
        MonoWorldV1 {
            schema: MONO_SCHEMA,
            input_fingerprint,
            roots,
            root_categories,
            instances,
            metadata_roots: metadata_roots.into_iter().collect(),
            externals: externals.into_iter().collect(),
            graph_fingerprint: *hash.finalize().as_bytes(),
            public_summaries: BTreeMap::new(),
            universe: Default::default(),
            late: Default::default(),
        }
    }
}

fn summary_of(record: InstanceRecordV1) -> InstanceSummaryV1 {
    let fragment_input_fingerprint = record.fragment_input_fingerprint();
    let mut callees: Vec<[u8; 32]> = record
        .callees
        .iter()
        .map(|seed| seed.key.digest())
        .collect();
    callees.sort_unstable();
    callees.dedup();
    let mut vtable_roots = record.vtable_roots;
    vtable_roots.sort();
    vtable_roots.dedup();
    let mut metadata_roots = record.metadata_roots;
    metadata_roots.sort();
    metadata_roots.dedup();
    InstanceSummaryV1 {
        mono_key: record.mono_key,
        definition: record.definition,
        kind: record.kind,
        symbol: record.symbol,
        public: record.public,
        parameter_count: record.parameter_count,
        signature_and_abi_fingerprint: record.signature_and_abi_fingerprint,
        call_targets: record.call_targets,
        callees,
        selected_impls: record.selected_impls,
        vtable_roots,
        metadata_roots,
        uses_late_comptime: record.uses_late_comptime,
        fragment_input_fingerprint,
        types: record.types,
        type_bindings: record.type_bindings,
    }
}

fn missing_entry() -> Diagnostic {
    Diagnostic::error(
        crate::DiagnosticCode::MonoDivergence,
        "闭合驱动缺少实例工作条目",
        None,
    )
}

fn instance_limit() -> Diagnostic {
    Diagnostic::error(
        crate::DiagnosticCode::MonoInstanceLimit,
        "单态化实例总数超过 u32 上界",
        None,
    )
}
