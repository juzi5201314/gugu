//! generic GIR：从冻结 HIR owner 构造显式 CFG，经 query 缓存后进入下游。
pub(crate) mod body;
mod build;
mod dump;
mod query;
#[cfg(test)]
mod tests;
mod verify;

pub(crate) use body::{BodyKind, GirBody};
pub(crate) use dump::dump_world;
pub(crate) use query::build_world;
pub(crate) use verify::verify;

use crate::frontend::hir::{self, TypeId};
use crate::frontend::mono::MonoWorldV1;
use crate::{Diagnostic, DiagnosticCode};
use serde::{Deserialize, Serialize};

pub(crate) const WORLD_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GirFragment {
    pub(crate) digest: [u8; 32],
    pub(crate) body: u32,
    pub(crate) body_fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GirWorldV1 {
    pub(crate) schema: u32,
    pub(crate) hir_fingerprint: [u8; 32],
    pub(crate) bodies: Vec<GirBody>,
    pub(crate) fragments: Vec<GirFragment>,
    pub(crate) fingerprint: [u8; 32],
}

pub(crate) fn empty_world() -> GirWorldV1 {
    GirWorldV1 {
        schema: WORLD_SCHEMA,
        hir_fingerprint: [0; 32],
        bodies: Vec::new(),
        fragments: Vec::new(),
        fingerprint: world_fingerprint(&[], &[], [0; 32]),
    }
}

pub(crate) fn world_fingerprint(
    bodies: &[GirBody],
    fragments: &[GirFragment],
    hir_fingerprint: [u8; 32],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-gir-world-v1");
    hash.update(&hir_fingerprint);
    hash.update(&(bodies.len() as u64).to_le_bytes());
    for body in bodies {
        hash.update(&body.fingerprint());
    }
    let fragment_bytes = serde_json::to_vec(fragments).expect("GIR fragment 序列化");
    hash.update(&fragment_bytes);
    *hash.finalize().as_bytes()
}

pub(crate) fn gir_error(message: &str, location: Option<&hir::Location>) -> Diagnostic {
    let _ = location;
    Diagnostic::error(DiagnosticCode::GirInvariant, message, None)
}

pub(crate) fn primitive_types(module: &hir::Module) -> Result<Primitives, Diagnostic> {
    let unit = find_type(module, &hir::Type::Unit, "Unit")?;
    let never = find_type(module, &hir::Type::Never, "Never")?;
    let bool_ty = find_type(module, &hir::Type::Bool, "Bool")?;
    let ptr_unit = find_type(module, &hir::Type::Ptr(unit), "Ptr(Unit)")?;
    Ok(Primitives {
        unit,
        never,
        bool_ty,
        ptr_unit,
    })
}

fn find_type(module: &hir::Module, expected: &hir::Type, name: &str) -> Result<TypeId, Diagnostic> {
    module
        .types
        .iter()
        .position(|ty| ty == expected)
        .map(|index| TypeId(index as u32))
        .ok_or_else(|| gir_error(&format!("HIR 类型表缺少 {name}"), None))
}

#[derive(Clone, Copy)]
pub(crate) struct Primitives {
    pub(crate) unit: TypeId,
    pub(crate) never: TypeId,
    pub(crate) bool_ty: TypeId,
    pub(crate) ptr_unit: TypeId,
}

pub(crate) fn body_kind(kind: hir::DefinitionKind) -> BodyKind {
    match kind {
        hir::DefinitionKind::Closure => BodyKind::Closure,
        hir::DefinitionKind::Async => BodyKind::Async,
        hir::DefinitionKind::Constant
        | hir::DefinitionKind::Static
        | hir::DefinitionKind::LocalStatic => BodyKind::StaticInit,
        hir::DefinitionKind::GlobalAsm => BodyKind::GlobalAsm,
        _ => BodyKind::Function,
    }
}

pub(crate) fn fragments_of(mono: &MonoWorldV1, bodies: &[GirBody]) -> Vec<GirFragment> {
    let mut fragments = Vec::with_capacity(mono.instances.len());
    for instance in &mono.instances {
        let definition = hir::DefId(instance.definition);
        let Some(index) = bodies.iter().position(|body| body.owner == definition) else {
            continue;
        };
        fragments.push(GirFragment {
            digest: crate::frontend::mono::digest_of(&instance.mono_key),
            body: index as u32,
            body_fingerprint: bodies[index].fingerprint(),
        });
    }
    fragments
}
