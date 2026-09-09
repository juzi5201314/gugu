//! LIR 是 backend 的唯一低层输入；构造与缓存恢复共享结构 verifier。
mod body;
mod build;
mod dump;
mod effects;
mod pass;
mod uses;
mod verify;

use crate::{
    Diagnostic, DiagnosticCode,
    frontend::{gir, hir, mono},
    query::{QueryEngine, QueryKey, QueryKind},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) const SCHEMA: u32 = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct World {
    schema: u32,
    target: String,
    input_fingerprint: [u8; 32],
    bodies: Vec<body::Body>,
    fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Validated {
    world: Arc<World>,
}

impl Validated {
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.world.fingerprint
    }
    pub(crate) fn bodies(&self) -> usize {
        self.world.bodies.len()
    }
    pub(crate) fn blocks(&self) -> usize {
        self.world.bodies.iter().map(|body| body.blocks.len()).sum()
    }
    pub(crate) fn instructions(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| body.instructions.len())
            .sum()
    }
    pub(crate) fn memory_operations(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| {
                body.instructions
                    .iter()
                    .filter(|instruction| instruction.memory.is_some())
                    .count()
                    + body
                        .blocks
                        .iter()
                        .filter(|block| matches!(block.terminator, body::Terminator::Invoke { .. }))
                        .count()
            })
            .sum()
    }
    pub(crate) fn safepoints(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| body.safepoints.len())
            .sum()
    }
    /// 世界内 `SafepointPoll` 数量。
    pub(crate) fn poll_count(&self) -> usize {
        self.world
            .bodies
            .iter()
            .flat_map(|body| &body.instructions)
            .filter(|instruction| matches!(instruction.op, body::Op::SafepointPoll { .. }))
            .count()
    }
    /// 被分类为 poll-free 叶的 managed 直接调用目标数量。
    pub(crate) fn poll_free_leaf_count(&self) -> usize {
        let mut keys = std::collections::BTreeSet::new();
        for body in &self.world.bodies {
            for instruction in &body.instructions {
                if let body::Op::Call(call) | body::Op::ForeignCall(call) = &instruction.op
                    && call.poll_free_leaf
                    && let body::CallTarget::Instance(key) = call.target
                {
                    keys.insert(key);
                }
            }
            for block in &body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                    && call.poll_free_leaf
                    && let body::CallTarget::Instance(key) = call.target
                {
                    keys.insert(key);
                }
            }
        }
        keys.len()
    }
    /// poll 摘要按 body 顺序的域哈希。
    pub(crate) fn poll_summary_fingerprint(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_derive_key("gugu-lir-poll-summary-v1");
        for body in &self.world.bodies {
            hash.update(&serde_json::to_vec(&body.poll_summary).expect("poll 摘要可序列化"));
        }
        *hash.finalize().as_bytes()
    }
    pub(crate) fn dump(&self) -> String {
        dump::world(&self.world)
    }
    /// 固定管线 revision。
    pub(crate) fn optimization_revision(&self) -> u32 {
        pass::policy::PASS_PIPELINE_REVISION
    }
    /// poll 预算。
    pub(crate) fn poll_budget(&self) -> u32 {
        pass::policy::POLL_BUDGET
    }
}

/// 进入 action key 的优化策略规范字节。
pub(crate) fn optimization_policy_bytes() -> Vec<u8> {
    pass::policy::OptimizationPolicyV1::default().canonical_bytes()
}

pub(crate) fn build(
    hir: &hir::Validated,
    gir: &gir::GirWorldV1,
    mono: &mono::MonoWorldV1,
    target: crate::TargetName,
    queries: &QueryEngine,
    sources: &crate::SourceMap,
) -> Result<Validated, Vec<Diagnostic>> {
    let profile = target.descriptor().cost_profile;
    let target = target.to_string();
    let mut hash = blake3::Hasher::new_derive_key("gugu-lir-input-v1");
    hash.update(&hir.fingerprint());
    hash.update(&gir.fingerprint);
    hash.update(&mono.graph_fingerprint);
    hash.update(&mono.universe.fingerprint);
    hash.update(&mono.late.fingerprint);
    hash.update(target.as_bytes());
    let fingerprint = *hash.finalize().as_bytes();
    let mut bodies = Vec::with_capacity(gir.concrete.len());
    for concrete in &gir.concrete {
        let mut bytes = fingerprint.to_vec();
        bytes.extend_from_slice(&concrete.instance);
        bytes.extend_from_slice(&concrete.fingerprint);
        let key = QueryKey::new(QueryKind::BuildLir, SCHEMA, &bytes);
        let computed = queries.compute(key, |context| {
            for source in sources.snapshots() {
                context.record_dependency(
                    QueryKey::new(QueryKind::SourceSnapshot, 1, source.logical_path()),
                    source.content_hash(),
                );
            }
            context.record_dependency(
                QueryKey::new(QueryKind::BuildGenericGir, 3, &concrete.body.owner_key),
                concrete.fingerprint,
            );
            context.record_dependency(
                QueryKey::new(QueryKind::FreezeTypeUniverse, 1, mono.graph_fingerprint),
                mono.universe.fingerprint,
            );
            context.record_dependency(
                QueryKey::new(QueryKind::EvaluateLateComptime, 1, mono.graph_fingerprint),
                mono.late.fingerprint,
            );
            let body = build::lower(concrete, hir.module(), gir, mono, &target, fingerprint)
                .and_then(|body| {
                    verify::verify_structure(&body, hir.module())?;
                    Ok(body)
                })
                .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
            Ok((
                serde_json::to_vec(&body).expect("LIR body 可序列化"),
                Vec::new(),
            ))
        });
        let computed = computed
            .map_err(|error| crate::frontend::semantics::query::restore_errors(error, sources))?;
        let body: body::Body = serde_json::from_slice(computed.payload())
            .map_err(|_| vec![invalid("缓存 LIR 不是合法 schema")])?;
        if body.input_fingerprint != fingerprint
            || body.instance != concrete.instance
            || body.target != target
        {
            return Err(vec![invalid("缓存 LIR 没有绑定当前输入或目标")]);
        }
        verify::verify_structure(&body, hir.module()).map_err(|error| vec![error])?;
        bodies.push(body);
    }
    pass::optimize_world(&mut bodies, hir.module(), &profile)?;
    for body in &bodies {
        verify::verify(body, hir.module()).map_err(|error| vec![error])?;
    }
    let mut world = World {
        schema: SCHEMA,
        target,
        input_fingerprint: fingerprint,
        bodies,
        fingerprint: [0; 32],
    };
    world.fingerprint = world_fingerprint(&world);
    verify_world(&world, mono).map_err(|error| vec![error])?;
    Ok(Validated {
        world: Arc::new(world),
    })
}

fn world_fingerprint(world: &World) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-lir-world-v2");
    hash.update(&world.schema.to_le_bytes());
    hash.update(world.target.as_bytes());
    hash.update(&world.input_fingerprint);
    for body in &world.bodies {
        hash.update(&body.fingerprint());
    }
    *hash.finalize().as_bytes()
}

fn verify_world(world: &World, mono: &mono::MonoWorldV1) -> Result<(), Diagnostic> {
    if world.schema != SCHEMA
        || world.fingerprint != world_fingerprint(world)
        || !world
            .bodies
            .windows(2)
            .all(|pair| pair[0].instance < pair[1].instance)
    {
        return Err(invalid("LIR world 身份、schema 或规范实例顺序不匹配"));
    }
    for body in &world.bodies {
        for instruction in &body.instructions {
            match &instruction.op {
                body::Op::SymbolAddr(body::Symbol::Instance(key)) => instance_exists(mono, key)?,
                body::Op::SymbolAddr(body::Symbol::TypeId(key)) => {
                    mono.universe.record(key)?;
                }
                body::Op::Call(call) | body::Op::ForeignCall(call) => {
                    verify_call(world, mono, call)?
                }
                _ => {}
            }
        }
        for block in &body.blocks {
            if let body::Terminator::Invoke { call, .. } | body::Terminator::TailCall { call, .. } =
                &block.terminator
            {
                verify_call(world, mono, call)?;
            }
        }
    }
    Ok(())
}

fn instance_exists(mono: &mono::MonoWorldV1, key: &[u8; 32]) -> Result<(), Diagnostic> {
    if mono
        .instances
        .iter()
        .any(|instance| &mono::digest_of(&instance.mono_key) == key)
    {
        Ok(())
    } else {
        Err(invalid("LIR 引用了实例闭包之外的代码"))
    }
}

fn verify_call(
    world: &World,
    mono: &mono::MonoWorldV1,
    call: &body::Call,
) -> Result<(), Diagnostic> {
    let body::CallTarget::Instance(key) = call.target else {
        return Ok(());
    };
    instance_exists(mono, &key)?;
    let target = world
        .bodies
        .binary_search_by_key(&key, |body| body.instance)
        .ok()
        .map(|index| &world.bodies[index])
        .ok_or_else(|| invalid("静态调用目标没有具体 LIR body"))?;
    if target.signature.parameters.len() != call.parameters.len()
        || target.signature.results.len() != call.results.len()
        || target.signature.by_value != call.by_value
        || target.signature.sret.map(|(bytes, _, key)| (0, bytes, key)) != call.sret
        || target
            .signature
            .parameters
            .iter()
            .zip(&call.parameters)
            .enumerate()
            .any(|(index, (expected, actual))| {
                let address = target.signature.by_value.iter().any(|(parameter, _, _)| {
                    usize::try_from(*parameter).expect("参数编号") == index
                }) || target.signature.sret.is_some() && index == 0;
                if address {
                    actual.ty != body::Type::Ptr
                        || matches!(
                            actual.provenance,
                            Some(body::Provenance::Code | body::Provenance::Metadata)
                        )
                } else {
                    !verify::compatible(*expected, *actual)
                }
            })
        || target
            .signature
            .results
            .iter()
            .zip(&call.results)
            .any(|(expected, actual)| !verify::compatible(*expected, *actual))
    {
        return Err(invalid(&format!(
            "LIR 静态调用与目标机器签名不一致：{}，期望 {:?}，实际 {:?}",
            target.name, target.signature, call
        )));
    }
    Ok(())
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::LirInvariant, message, None)
}

#[cfg(test)]
mod tests;
