//! 内建实例的物化函数表：可安装集的判定、表项内容校验与构建期录制。
//!
//! 函数表在构建期由 canonical 程序的编译录制，每项按实例 MonoKey digest 存该实例各层的冻结
//! 结果：分析分片（函数摘要与本 owner 的证明）与下层（LIR）字节。
//!
//! 用户编译在单态化闭合之后求可安装集：实例 digest 在表内、它的 owner 在本 world 只有这一个
//! 实例，并且全部可执行 callee 同样可安装（按 callee 闭合取最大不动点）。在这个集合上安装的
//! 结果与现算一致：MonoKey 决定调用目标，集合内的 body 只调用集合内的 body；单实例 owner 的
//! 证明不与其它实例合并；内建 owner 与表达式编号处于编号前缀，证明键在两个 world 里同义；
//! 录制排除了读取类型宇宙稠密编号的实例，用户类型改变编号也不影响已安装的字节。不在集合内
//! 的实例照常现算，这是安装边界本身而不是回退；表项内容校验失败才是 compiler invariant。

use crate::analysis::{self, AnalysisWorldV1};
use crate::gir::body::{ConstValue, IntrinsicOp, Rvalue, StatementKind};
use crate::gir::{GirWorldV1, concrete::ConcreteBody};
use crate::hir::Module;
use crate::mono::{MonoWorldV1, digest_hex, digest_of};
use crate::runtime::RuntimeFunctionArtifactV1;
use crate::{Diagnostic, DiagnosticCode};

/// 本层在 `function_revisions` 里的层名前缀。
const LAYER: &str = "analysis.";

/// 本 world 的可安装集：`(实例 digest, 表项下标)`，按 digest 严格升序。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstalledFunctions {
    members: Vec<([u8; 32], u32)>,
}

impl InstalledFunctions {
    /// 闭合实例图上的可安装集，并核对每个入选表项的内容摘要。
    pub fn resolve(
        module: &Module,
        mono: &MonoWorldV1,
        entries: &[RuntimeFunctionArtifactV1],
    ) -> Result<Self, Diagnostic> {
        if entries.is_empty() {
            return Ok(Self::default());
        }
        let selected = select(module, mono, |_, digest| {
            let key = digest_hex(digest);
            entries
                .binary_search_by(|candidate| candidate.stable_mono_key.as_str().cmp(&key))
                .ok()
                .map(|position| u32::try_from(position).expect("函数表下标适配 u32"))
        })?;
        let mut members = Vec::with_capacity(selected.len());
        for (index, position) in selected {
            let artifact = &entries[position as usize];
            if artifact.content_fingerprint() != artifact.fingerprint {
                return Err(invariant(format!(
                    "内建函数表项 `{}` 的内容摘要不符",
                    artifact.stable_mono_key
                )));
            }
            members.push((digest_of(&mono.instances[index].mono_key), position));
        }
        Ok(Self { members })
    }

    /// 入选实例的表项；不在可安装集内返回 `None`。
    pub fn entry<'a>(
        &self,
        entries: &'a [RuntimeFunctionArtifactV1],
        digest: &[u8; 32],
    ) -> Option<&'a RuntimeFunctionArtifactV1> {
        self.members
            .binary_search_by(|(member, _)| member.cmp(digest))
            .ok()
            .map(|index| &entries[self.members[index].1 as usize])
    }

    /// 入选实例数。
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// 可安装集是否为空。
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

/// 构建期录制：从 canonical 程序的已完成 world 取内建实例的各层结果，按 key 升序。
///
/// `builtin` 是内建编号前缀的定义数；`lower` 按实例 digest 给出下层（LIR）的录制字节。入选
/// 条件与安装端相同（单实例 owner、callee 闭合），另排除观察到预算耗尽的 owner（安装端组装的
/// SCC 预算位恒为假）、没有具体 body 的实例，以及读取类型宇宙稠密编号的实例
/// （[`reads_type_numbering`]）。
pub fn record(
    module: &Module,
    mono: &MonoWorldV1,
    gir: &GirWorldV1,
    world: &AnalysisWorldV1,
    builtin: usize,
    lower: impl Fn(&[u8; 32]) -> Option<Vec<u8>>,
) -> Result<Vec<RuntimeFunctionArtifactV1>, Diagnostic> {
    let mut concrete: Vec<([u8; 32], &ConcreteBody)> = gir
        .concrete
        .iter()
        .map(|body| (body.instance, body))
        .collect();
    concrete.sort_unstable_by_key(|(instance, _)| *instance);
    let mut late: Vec<[u8; 32]> = mono
        .late
        .results
        .iter()
        .map(|result| result.key.instance)
        .collect();
    late.sort_unstable();
    let selected = select(module, mono, |owner, digest| {
        let index = u32::try_from(owner).expect("owner 下标适配 u32");
        let body = concrete
            .binary_search_by_key(digest, |(instance, _)| *instance)
            .ok()
            .map(|position| concrete[position].1)?;
        (module.owners[owner].definition.index() < builtin
            && !world.owner_exhausted(index)
            && !reads_type_numbering(body, &late))
        .then_some(index)
    })?;
    selected
        .into_iter()
        .map(|(index, owner)| {
            let mono_key = &mono.instances[index].mono_key;
            let digest = digest_of(mono_key);
            let analysis = analysis::encode_shard(world, owner, mono_key)
                .ok_or_else(|| invariant("录制的内建实例不在分析 world 中".to_owned()))?;
            let lir = lower(&digest)
                .ok_or_else(|| invariant("录制的内建实例没有 LIR body".to_owned()))?;
            let mut artifact = RuntimeFunctionArtifactV1 {
                stable_mono_key: digest_hex(&digest),
                fingerprint: [0; 32],
                analysis,
                lir,
            };
            artifact.fingerprint = artifact.content_fingerprint();
            Ok(artifact)
        })
        .collect()
}

/// 具体 body 是否读取类型宇宙的稠密编号：类型常量与带类型实参的 `TypeId` 降为编号整数，
/// late 值（如 `TypeIdCount`）可以是编号的函数。编号随用户类型变化，这类 body 不能冻结。
///
/// `late` 是有 late 结果的实例 digest，升序。
fn reads_type_numbering(body: &ConcreteBody, late: &[[u8; 32]]) -> bool {
    fn mentions_type(value: &ConstValue) -> bool {
        match value {
            ConstValue::Type(_) => true,
            ConstValue::Aggregate(values) => values.iter().any(mentions_type),
            _ => false,
        }
    }
    late.binary_search(&body.instance).is_ok()
        || body
            .body
            .constants
            .iter()
            .any(|constant| mentions_type(&constant.value))
        || body.body.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, Rvalue::Intrinsic { op: IntrinsicOp::TypeId, types, .. })
                    if !types.is_empty()
            )
        })
}

/// 本层的 revision，按层名排序；安装端要求与物化表里 `analysis.` 前缀的条目完全一致。
pub fn revisions() -> Vec<(String, u32)> {
    vec![
        (
            "analysis.semantics".to_owned(),
            analysis::ANALYSIS_SEMANTICS_REVISION,
        ),
        ("analysis.shard".to_owned(), analysis::SHARD_REVISION),
        ("analysis.world".to_owned(), analysis::WORLD_SCHEMA_VERSION),
    ]
}

/// 物化表里属于本层的 revision 条目。
pub fn declared_revisions(declared: &[(String, u32)]) -> Vec<(String, u32)> {
    declared
        .iter()
        .filter(|(layer, _)| layer.starts_with(LAYER))
        .cloned()
        .collect()
}

/// 入选实例 `(实例下标, accept 的取值)`，按实例下标（即 digest）升序。
///
/// 无 HIR owner 的实例（常量）没有可执行 body，不入选，调用它们也不影响 caller；多实例
/// owner 的证明随其它实例变化，不入选；`accept` 按 `(owner 下标, digest)` 给出其余条件，
/// 最后按 callee 闭合。
fn select<T: Copy>(
    module: &Module,
    mono: &MonoWorldV1,
    accept: impl Fn(usize, &[u8; 32]) -> Option<T>,
) -> Result<Vec<(usize, T)>, Diagnostic> {
    let digests: Vec<[u8; 32]> = mono
        .instances
        .iter()
        .map(|instance| digest_of(&instance.mono_key))
        .collect();
    debug_assert!(digests.windows(2).all(|pair| pair[0] < pair[1]));
    let mut owner_by_key: Vec<([u8; 32], usize)> = module
        .owners
        .iter()
        .enumerate()
        .map(|(index, owner)| (module.definitions[owner.definition.index()].key, index))
        .collect();
    owner_by_key.sort_unstable();
    let owners = mono
        .instances
        .iter()
        .map(|instance| {
            let key: [u8; 32] = instance
                .mono_key
                .get(..32)
                .and_then(|key| key.try_into().ok())
                .ok_or_else(|| invariant("实例键缺少定义身份".to_owned()))?;
            Ok(owner_by_key
                .binary_search_by(|(candidate, _)| candidate.cmp(&key))
                .ok()
                .map(|position| owner_by_key[position].1))
        })
        .collect::<Result<Vec<Option<usize>>, Diagnostic>>()?;
    let shared = shared_owners(&owners, module.owners.len());
    let mut entry: Vec<Option<T>> = owners
        .iter()
        .zip(&digests)
        .map(|(owner, digest)| match *owner {
            Some(owner) if !shared[owner] => accept(owner, digest),
            _ => None,
        })
        .collect();
    let bodyless: Vec<bool> = owners.iter().map(Option::is_none).collect();
    let callees: Vec<&[[u8; 32]]> = mono
        .instances
        .iter()
        .map(|instance| instance.callees.as_slice())
        .collect();
    close_over_callees(&digests, &bodyless, &callees, &mut entry);
    Ok(entry
        .into_iter()
        .enumerate()
        .filter_map(|(index, slot)| slot.map(|value| (index, value)))
        .collect())
}

/// 在本 world 拥有多个实例的 owner：按 owner 下标的稠密位表。
fn shared_owners(owners: &[Option<usize>], count: usize) -> Vec<bool> {
    let mut seen = vec![false; count];
    let mut shared = vec![false; count];
    for &owner in owners.iter().flatten() {
        shared[owner] |= seen[owner];
        seen[owner] = true;
    }
    shared
}

/// 按 callee 闭合求最大不动点：不可安装的可执行 callee 逐层撤销其 caller。
///
/// `digests` 严格升序；`bodyless` 标记没有可执行 body 的实例，调用它们不影响安装。闭合实例图
/// 之外的 callee 无从核对，caller 直接撤销。
fn close_over_callees<T>(
    digests: &[[u8; 32]],
    bodyless: &[bool],
    callees: &[&[[u8; 32]]],
    entry: &mut [Option<T>],
) {
    let mut callers = vec![Vec::new(); digests.len()];
    for (index, edges) in callees.iter().enumerate() {
        for callee in *edges {
            match digests.binary_search(callee) {
                Ok(target) if bodyless[target] => {}
                Ok(target) => callers[target].push(index),
                Err(_) => entry[index] = None,
            }
        }
    }
    let mut work: Vec<usize> = (0..digests.len())
        .filter(|&index| entry[index].is_none() && !bodyless[index])
        .collect();
    while let Some(removed) = work.pop() {
        for &caller in &callers[removed] {
            if entry[caller].take().is_some() {
                work.push(caller);
            }
        }
    }
}

fn invariant(message: String) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::BackendInvariant, message, None)
}

#[cfg(test)]
mod tests;
