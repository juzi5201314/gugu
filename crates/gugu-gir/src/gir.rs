//! generic GIR：从冻结 HIR owner 构造显式 CFG，经 query 缓存后进入下游。
pub mod body;
mod build;
pub mod concrete;
mod dump;
mod liveness;
pub mod pass;
pub mod passing;
pub mod placement;
mod query;
#[cfg(all(test, feature = "slow-tests"))]
pub mod tests;
mod verify;

pub use body::{BodyKind, GirBody};
pub use dump::dump_world;
pub use passing::large_copy_lints;
pub use placement::run as place_world;
pub use query::{BUILD_SCHEMA, attach_fragments, build_world};
pub use verify::verify;

use crate::hir::{self, TypeId};
use crate::mono::MonoWorldV1;
use crate::{Diagnostic, DiagnosticCode};
use serde::{Deserialize, Serialize};

pub const WORLD_SCHEMA: u32 = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GirFragment {
    pub digest: [u8; 32],
    pub body: u32,
    pub body_fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GirWorldV1 {
    pub schema: u32,
    pub hir_fingerprint: [u8; 32],
    pub bodies: Vec<GirBody>,
    pub fragments: Vec<GirFragment>,
    pub placement: placement::PlacementWorldV1,
    pub concrete: Vec<concrete::ConcreteBody>,
    pub fingerprint: [u8; 32],
}

impl GirWorldV1 {
    /// 返回资源 lease 动作数量：acquire、release、transfer、finalize。
    pub fn resource_action_counts(&self) -> (u32, u32, u32, u32) {
        let mut counts = (0_u32, 0_u32, 0_u32, 0_u32);
        for body in &self.bodies {
            for statement in &body.statements {
                if let body::StatementKind::ResourceAction { action, .. } = &statement.kind {
                    match action {
                        body::ResourceActionKind::AcquireLease => counts.0 += 1,
                        body::ResourceActionKind::ReleaseLease => counts.1 += 1,
                        body::ResourceActionKind::Transfer => counts.2 += 1,
                        body::ResourceActionKind::Finalize => counts.3 += 1,
                    }
                }
            }
        }
        counts
    }
}

pub fn empty_world() -> GirWorldV1 {
    let placement = placement::PlacementWorldV1::empty();
    GirWorldV1 {
        schema: WORLD_SCHEMA,
        hir_fingerprint: [0; 32],
        bodies: Vec::new(),
        fragments: Vec::new(),
        fingerprint: world_fingerprint(&[], &[], [0; 32], &placement, &[]),
        placement,
        concrete: Vec::new(),
    }
}

pub fn world_fingerprint(
    bodies: &[GirBody],
    fragments: &[GirFragment],
    hir_fingerprint: [u8; 32],
    placement: &placement::PlacementWorldV1,
    concrete: &[concrete::ConcreteBody],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-gir-world-v3");
    hash.update(&hir_fingerprint);
    hash.update(&(bodies.len() as u64).to_le_bytes());
    for body in bodies {
        hash.update(&body.fingerprint());
    }
    let fragment_bytes = crate::canonical::to_vec(fragments).expect("GIR fragment 编码");
    hash.update(&fragment_bytes);
    hash.update(&placement.fingerprint);
    for body in concrete {
        hash.update(&body.fingerprint);
    }
    *hash.finalize().as_bytes()
}

pub fn gir_error(message: &str, location: Option<&hir::Location>) -> Diagnostic {
    let _ = location;
    Diagnostic::error(DiagnosticCode::GirInvariant, message, None)
}

pub fn primitive_types(module: &hir::Module) -> Result<Primitives, Diagnostic> {
    let unit = find_type(module, &hir::Type::Unit, "Unit")?;
    let never = find_type(module, &hir::Type::Never, "Never")?;
    let bool_ty = find_type(module, &hir::Type::Bool, "Bool")?;
    let string = find_type(module, &hir::Type::String, "String")?;
    let ptr_unit = find_type(module, &hir::Type::Ptr(unit), "Ptr(Unit)")?;
    Ok(Primitives {
        unit,
        never,
        bool_ty,
        string,
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
pub struct Primitives {
    pub unit: TypeId,
    pub never: TypeId,
    pub bool_ty: TypeId,
    pub string: TypeId,
    pub ptr_unit: TypeId,
}

pub fn body_kind(kind: hir::DefinitionKind) -> BodyKind {
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

pub fn fragments_of(mono: &MonoWorldV1, bodies: &[GirBody]) -> Vec<GirFragment> {
    // DefId 是 HIR 稠密 arena 下标；这里只索引具有 body 的前缀范围。
    let extent = bodies
        .iter()
        .map(|body| body.owner.index() + 1)
        .max()
        .unwrap_or(0);
    let mut owners = vec![None; extent];
    let mut fingerprints = vec![None; bodies.len()];
    for (index, body) in bodies.iter().enumerate() {
        debug_assert!(body.owner.index() < owners.len());
        owners[body.owner.index()].get_or_insert(index);
    }
    let mut fragments = Vec::with_capacity(mono.instances.len());
    for instance in &mono.instances {
        let definition = hir::DefId(instance.definition);
        let Some(&Some(index)) = owners.get(definition.index()) else {
            continue;
        };
        fragments.push(GirFragment {
            digest: crate::mono::digest_of(&instance.mono_key),
            body: u32::try_from(index).expect("body arena 使用 u32 编号"),
            body_fingerprint: *fingerprints[index]
                .get_or_insert_with(|| bodies[index].fingerprint()),
        });
    }
    fragments
}
