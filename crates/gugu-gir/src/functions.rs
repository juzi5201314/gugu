//! 内建实例的物化函数表：可安装集的判定、表项内容校验与构建期录制。
//!
//! 函数表在构建期由 canonical 程序的编译录制，每项按实例 MonoKey digest 存该实例的分析
//! 分片（函数摘要与本 owner 的证明）。
//!
//! 用户编译在单态化闭合之后求可安装集：实例 digest 在表内、它的 owner 在本 world 只有这一个
//! 实例，并且全部可执行 callee 同样可安装（按 callee 闭合取最大不动点）。在这个集合上安装的
//! 结果与现算一致：MonoKey 决定调用目标，集合内的 body 只调用集合内的 body；单实例 owner 的
//! 证明不与其它实例合并；内建 owner 与表达式编号处于编号前缀，证明键在两个 world 里同义。
//! 不在集合内的实例照常现算，这是安装边界本身而不是回退；表项内容校验失败才是 compiler
//! invariant。

use crate::analysis::{self, AnalysisWorldV1};
use crate::hir::Module;
use crate::mono::{MonoWorldV1, digest_hex, digest_of};
use crate::runtime::RuntimeFunctionArtifactV1;
use crate::{Diagnostic, DiagnosticCode};

/// 函数表项层字节的内容摘要域。
const FUNCTION_DOMAIN: &str = "gugu-builtin-function-v1";

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
            if fingerprint(artifact) != artifact.fingerprint {
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

/// 构建期录制：从 canonical 程序的已完成 world 取内建实例的分析分片，按 key 升序。
///
/// `builtin` 是内建编号前缀的定义数。入选条件与安装端相同（单实例 owner、callee 闭合），另
/// 排除观察到预算耗尽的 owner：安装端组装的 SCC 预算位恒为假。
pub fn record(
    module: &Module,
    mono: &MonoWorldV1,
    world: &AnalysisWorldV1,
    builtin: usize,
) -> Result<Vec<RuntimeFunctionArtifactV1>, Diagnostic> {
    let selected = select(module, mono, |owner, _| {
        let index = u32::try_from(owner).expect("owner 下标适配 u32");
        (module.owners[owner].definition.index() < builtin && !world.owner_exhausted(index))
            .then_some(index)
    })?;
    selected
        .into_iter()
        .map(|(index, owner)| {
            let mono_key = &mono.instances[index].mono_key;
            let analysis = analysis::encode_shard(world, owner, mono_key)
                .ok_or_else(|| invariant("录制的内建实例不在分析 world 中".to_owned()))?;
            let mut artifact = RuntimeFunctionArtifactV1 {
                stable_mono_key: digest_hex(&digest_of(mono_key)),
                fingerprint: [0; 32],
                analysis,
            };
            artifact.fingerprint = fingerprint(&artifact);
            Ok(artifact)
        })
        .collect()
}

/// 函数表各层的 revision，按层名排序；安装端要求与物化表声明完全一致。
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

/// 函数表项层字节的内容摘要；物化端写入、安装端重算比对。
pub fn fingerprint(artifact: &RuntimeFunctionArtifactV1) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key(FUNCTION_DOMAIN);
    hash.update(artifact.stable_mono_key.as_bytes());
    hash.update(&(artifact.analysis.len() as u64).to_le_bytes());
    hash.update(&artifact.analysis);
    *hash.finalize().as_bytes()
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
