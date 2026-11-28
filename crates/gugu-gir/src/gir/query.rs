//! `BuildGenericGir`：每个冻结 HIR owner 一个 generic body。
use super::build;
use super::placement::PlacementWorldV1;
use super::{
    CachedBodyFingerprints, GirBody, GirWorldV1, WORLD_SCHEMA, fragments_of_cached,
    world_fingerprint_cached,
};
use crate::hir::{self, Validated};
use crate::mono::MonoWorldV1;
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::semantics::query::{restore_errors, store_errors};
use crate::{Diagnostic, SourceMap};
use std::sync::Arc;

pub const BUILD_SCHEMA: u32 = 23;

pub fn build_world(
    hir: &Validated,
    queries: &QueryEngine,
    sources: &SourceMap,
    installed: Option<&crate::prefix::BuiltinPrefix>,
) -> Result<GirWorldV1, Vec<Diagnostic>> {
    let module = hir.module();
    let mut bodies = installed
        .map(|prefix| prefix.gir_bodies().clone())
        .unwrap_or_default();
    let prefix_fingerprints = installed
        .map(|prefix| Arc::clone(prefix.gir_body_fingerprints()))
        .unwrap_or_else(|| Arc::from([]));
    if bodies.len() > module.owners.len() {
        return Err(vec![super::gir_error(
            "物化 GIR 前缀超过当前 owner 表",
            None,
        )]);
    }
    let mut suffix_fingerprints =
        Vec::with_capacity(module.owners.len().saturating_sub(bodies.len()));
    for (index, owner) in module.owners.iter().enumerate() {
        if let Some(body) = installed.and_then(|prefix| prefix.gir_body(index)) {
            // 物化 body 已在 `BuiltinPrefix::decode` 阶段按内建 HIR 完整通过 `gir::verify`，
            // 这里只核对当前会话的 owner/稳定键/表达式表长度，避免每次编译 O(N) 重复扫描。
            let definition = &module.definitions[owner.definition.index()];
            if body.owner != owner.definition
                || body.owner_key != definition.key
                || body.expression_locals.len() != owner.expressions.len()
            {
                return Err(vec![super::gir_error("物化 GIR 与当前 owner 不一致", None)]);
            }
            debug_assert!(std::ptr::eq(&bodies[index], body));
            continue;
        }
        debug_assert_eq!(bodies.len(), index, "用户 owner 紧随共享内建前缀");
        let body = compute_body(hir, index, owner, queries, sources)?;
        suffix_fingerprints.push(body.fingerprint());
        bodies.push(body);
    }
    let body_fingerprints =
        CachedBodyFingerprints::from_parts(prefix_fingerprints, suffix_fingerprints);
    let placement = PlacementWorldV1::empty();
    let fingerprint = world_fingerprint_cached(
        &bodies,
        &body_fingerprints,
        &[],
        hir.fingerprint(),
        &placement,
        &[],
    );
    Ok(GirWorldV1 {
        schema: WORLD_SCHEMA,
        hir_fingerprint: hir.fingerprint(),
        bodies,
        fragments: Vec::new(),
        placement,
        concrete: Vec::new(),
        fingerprint,
        body_fingerprints,
    })
}

pub fn attach_fragments(world: GirWorldV1, mono: &MonoWorldV1) -> GirWorldV1 {
    let fragments = fragments_of_cached(mono, &world.bodies, &world.body_fingerprints);
    let fingerprint = world_fingerprint_cached(
        &world.bodies,
        &world.body_fingerprints,
        &fragments,
        world.hir_fingerprint,
        &world.placement,
        &world.concrete,
    );
    GirWorldV1 {
        fragments,
        fingerprint,
        ..world
    }
}

fn compute_body(
    hir: &Validated,
    owner_index: usize,
    owner: &hir::Owner,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<GirBody, Vec<Diagnostic>> {
    let module = hir.module();
    let mut hash = blake3::Hasher::new_derive_key("gugu-build-generic-gir-v2");
    hash.update(&crate::semantics::owner_dependency_fingerprint(
        module, owner,
    ));
    hash.update(&(owner_index as u64).to_le_bytes());
    let key = QueryKey::new(
        QueryKind::BuildGenericGir,
        BUILD_SCHEMA,
        *hash.finalize().as_bytes(),
    );
    let mut fresh = None;
    let result = queries
        .compute(key, |context| {
            context.record_dependency(
                QueryKey::new(
                    QueryKind::LowerHir,
                    crate::semantics::LOWER_SCHEMA,
                    module.input_fingerprint,
                ),
                hir.fingerprint(),
            );
            match build::lower(module, owner) {
                Ok(body) => {
                    let payload = crate::canonical::to_vec(&body).expect("GIR schema 编码");
                    fresh = Some(body);
                    Ok((payload, Vec::new()))
                }
                Err(error) => Err(store_errors(&[error])),
            }
        })
        .map_err(|error| restore_errors(error, sources))?;
    if let Some(body) = fresh {
        return Ok(body);
    }
    let body: GirBody = crate::canonical::from_slice(result.payload())
        .map_err(|_| vec![super::gir_error("GIR query 缓存 schema 不合法", None)])?;
    let definition = &module.definitions[owner.definition.index()];
    if body.owner != owner.definition || body.owner_key != definition.key {
        return Err(vec![super::gir_error("缓存 GIR 与当前 owner 不一致", None)]);
    }
    super::verify(module, &body).map_err(|error| vec![error])?;
    Ok(body)
}
