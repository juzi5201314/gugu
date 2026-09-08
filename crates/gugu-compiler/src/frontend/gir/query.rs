//! `BuildGenericGir`：每个冻结 HIR owner 一个 generic body。
use super::build;
use super::{GirBody, GirWorldV1, WORLD_SCHEMA, fragments_of, world_fingerprint};
use crate::frontend::hir::{self, Validated};
use crate::frontend::mono::MonoWorldV1;
use crate::frontend::semantics::query::{restore_errors, store_errors};
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::{Diagnostic, SourceMap};

pub(crate) fn build_world(
    hir: &Validated,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<GirWorldV1, Vec<Diagnostic>> {
    let module = hir.module();
    let mut bodies = Vec::with_capacity(module.owners.len());
    for (index, owner) in module.owners.iter().enumerate() {
        bodies.push(compute_body(hir, index, owner, queries, sources)?);
    }
    let fingerprint = world_fingerprint(&bodies, &[], hir.fingerprint());
    Ok(GirWorldV1 {
        schema: WORLD_SCHEMA,
        hir_fingerprint: hir.fingerprint(),
        bodies,
        fragments: Vec::new(),
        fingerprint,
    })
}

pub(crate) fn attach_fragments(world: GirWorldV1, mono: &MonoWorldV1) -> GirWorldV1 {
    let fragments = fragments_of(mono, &world.bodies);
    let fingerprint = world_fingerprint(&world.bodies, &fragments, world.hir_fingerprint);
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
    let definition = &module.definitions[owner.definition.index()];
    let mut hash = blake3::Hasher::new_derive_key("gugu-build-generic-gir-v1");
    hash.update(&hir.fingerprint());
    hash.update(&definition.key);
    hash.update(&(owner_index as u64).to_le_bytes());
    let key = QueryKey::new(QueryKind::BuildGenericGir, 1, *hash.finalize().as_bytes());
    let mut fresh = None;
    let result = queries
        .compute(key, |context| {
            context.record_dependency(
                QueryKey::new(QueryKind::LowerHir, 5, module.input_fingerprint),
                hir.fingerprint(),
            );
            match build::lower(module, owner) {
                Ok(body) => {
                    let payload = serde_json::to_vec(&body).expect("GIR schema 序列化");
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
    let body: GirBody = serde_json::from_slice(result.payload())
        .map_err(|_| vec![super::gir_error("GIR query 缓存 schema 不合法", None)])?;
    super::verify(module, &body).map_err(|error| vec![error])?;
    Ok(body)
}
