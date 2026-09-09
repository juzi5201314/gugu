//! 单态化 GIR：操作树不分叉，类型与调用绑定只消费既有实例化结果。
mod calls;
mod layout;
mod materialize;

use super::body::{ConstValue, GirBody};
use super::passing::PassingClass;
use crate::frontend::hir;
use crate::frontend::mono::{MonoWorldV1, collect::InstanceSummaryV1, keys::MonoContext};
use crate::frontend::types::Layout;
use crate::{Diagnostic, DiagnosticCode};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Field {
    pub(crate) ty: u32,
    pub(crate) offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum TypeKind {
    Never,
    Unit,
    Bool,
    Char,
    Int {
        signed: bool,
        bits: u16,
    },
    Float(u16),
    TypeId,
    String,
    Reference(u32),
    Pointer(u32),
    Slice(u32),
    Array {
        element: u32,
        count: u64,
    },
    Aggregate {
        tag_bytes: u8,
        variants: Vec<Vec<Field>>,
    },
    Function {
        parameters: Vec<u32>,
        result: u32,
    },
    FunctionItem {
        definition: u32,
        capturing: bool,
        arguments: Vec<[u8; 32]>,
    },
    Dynamic,
    Channel(u32),
    Join(u32),
    MaybeUninit(u32),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TypeLayout {
    pub(crate) key: [u8; 32],
    pub(crate) layout: Option<Layout>,
    pub(crate) kind: TypeKind,
    pub(crate) passing: PassingClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Protocol {
    Clone,
    TryBranch,
    TryFromValue,
    TryFromError,
    IntoIter,
    IterNext,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConcreteCall {
    pub(crate) site: crate::frontend::mono::instantiate::CallSite,
    pub(crate) target: Option<[u8; 32]>,
    pub(crate) protocol: Option<Protocol>,
    pub(crate) parameters: Vec<u32>,
    pub(crate) result: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConcreteBody {
    pub(crate) schema: u32,
    pub(crate) instance: [u8; 32],
    pub(crate) generic_body: u32,
    pub(crate) body: GirBody,
    pub(crate) types: Vec<TypeLayout>,
    pub(crate) calls: Vec<ConcreteCall>,
    pub(crate) fingerprint: [u8; 32],
}

impl ConcreteBody {
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        crate::frontend::mono::keys::hash_domain(
            "gugu-concrete-gir-v1",
            &serde_json::to_vec(&(
                self.schema,
                self.instance,
                self.generic_body,
                &self.body,
                &self.types,
                &self.calls,
            ))
            .expect("具体 GIR 可序列化"),
        )
    }

    pub(crate) fn verify(&self) -> Result<(), Diagnostic> {
        if self.schema != SCHEMA
            || self.body.generic_params != 0
            || self.fingerprint != self.fingerprint()
        {
            return Err(invalid("具体 GIR 的 schema、实例替换或指纹不合法"));
        }
        for local in &self.body.locals {
            if local.ty.index() >= self.types.len() || self.types[local.ty.index()].layout.is_none()
            {
                return Err(invalid("具体 local 缺少布局"));
            }
        }
        for ty in &self.types {
            if ty.layout.is_some_and(|layout| {
                !layout.align.is_power_of_two() || layout.size % layout.align != 0
            }) {
                return Err(invalid("具体类型布局不合法"));
            }
            if let TypeKind::Aggregate { variants, .. } = &ty.kind {
                for field in variants.iter().flatten() {
                    let field_ty = self
                        .types
                        .get(index(field.ty))
                        .ok_or_else(|| invalid("具体字段类型越界"))?;
                    if let (Some(outer), Some(inner)) = (ty.layout, field_ty.layout)
                        && field
                            .offset
                            .checked_add(inner.size)
                            .is_none_or(|end| end > outer.size)
                    {
                        return Err(invalid("具体字段越过聚合布局"));
                    }
                }
            }
        }
        if self
            .body
            .constants
            .iter()
            .any(|constant| unresolved(&constant.value))
        {
            return Err(invalid("具体 GIR 仍有未物化常量"));
        }
        Ok(())
    }
}

fn unresolved(value: &ConstValue) -> bool {
    match value {
        ConstValue::Definition(_) => true,
        ConstValue::Aggregate(values) => values.iter().any(unresolved),
        _ => false,
    }
}

pub(crate) fn build(
    context: &MonoContext<'_>,
    world: &super::GirWorldV1,
    mono: &MonoWorldV1,
) -> Result<Vec<ConcreteBody>, Vec<Diagnostic>> {
    let mut bodies = Vec::with_capacity(world.fragments.len());
    for fragment in &world.fragments {
        let instance = mono
            .instances
            .iter()
            .find(|instance| {
                crate::frontend::mono::digest_of(&instance.mono_key) == fragment.digest
            })
            .ok_or_else(|| vec![invalid("GIR fragment 不在闭合实例图中")])?;
        let body = build_body(context, world, mono, instance, fragment.body)
            .map_err(|error| vec![error])?;
        bodies.push(body);
    }
    Ok(bodies)
}

fn build_body(
    context: &MonoContext<'_>,
    world: &super::GirWorldV1,
    mono: &MonoWorldV1,
    instance: &InstanceSummaryV1,
    body_index: u32,
) -> Result<ConcreteBody, Diagnostic> {
    let mut body = world.bodies[index(body_index)].clone();
    let mut layouts = layout::Builder::new(context, instance);
    let calls = calls::bind(&body, mono, &mut layouts)?;
    materialize::constants(context, mono, instance, &mut body)?;
    materialize::types(&mut body, &calls, &mut layouts)?;
    materialize::late(mono, instance, &mut body)?;
    body.generic_params = 0;
    let types = layouts.finish()?;
    let mut concrete = ConcreteBody {
        schema: SCHEMA,
        instance: crate::frontend::mono::digest_of(&instance.mono_key),
        generic_body: body_index,
        body,
        types,
        calls,
        fingerprint: [0; 32],
    };
    concrete.fingerprint = concrete.fingerprint();
    concrete.verify()?;
    Ok(concrete)
}

pub(crate) fn index(value: u32) -> usize {
    usize::try_from(value).expect("稠密 u32 编号适配宿主")
}

pub(crate) fn id(value: usize) -> u32 {
    u32::try_from(value).expect("arena 数量不超过 u32")
}

pub(crate) fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::LirInvariant, message, None)
}
