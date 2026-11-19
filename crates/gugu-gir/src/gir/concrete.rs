//! 单态化 GIR：操作树不分叉，类型与调用绑定只消费既有实例化结果。
mod calls;
mod layout;
mod materialize;

use super::body::{ConstValue, GirBody};
use super::passing::PassingClass;
use crate::hir;
use crate::mono::{MonoWorldV1, collect::InstanceSummaryV1, keys::MonoContext};
use crate::types::Layout;
use crate::{Diagnostic, DiagnosticCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: u32 = 4;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub ty: u32,
    pub offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TypeKind {
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
    Formatter,
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
    Panic,
    MaybeUninit(u32),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TypeLayout {
    pub key: [u8; 32],
    pub layout: Option<Layout>,
    pub kind: TypeKind,
    pub passing: PassingClass,
    pub transparent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Protocol {
    Eq,
    Cmp,
    Clone,
    TryBranch,
    TryFromValue,
    TryFromError,
    IntoIter,
    IterNext,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConcreteCall {
    pub site: crate::mono::instantiate::CallSite,
    pub target: Option<[u8; 32]>,
    pub protocol: Option<Protocol>,
    pub parameters: Vec<u32>,
    pub result: u32,
}
/// 契约签名只形成类型布局，不形成可执行 GIR 体。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecordTypes {
    pub definition: hir::DefId,
    pub parameters: Vec<hir::TypeId>,
    pub types: Vec<TypeLayout>,
}

pub fn record_types(
    context: &MonoContext<'_>,
    definition: hir::DefId,
) -> Result<RecordTypes, Diagnostic> {
    let parameters = crate::contract::contract_view::source_parameters(context.module, definition)
        .map_err(|error| invalid(error.message()))?;
    let substitutions = BTreeMap::new();
    let mut layouts = layout::Builder::new(context, &substitutions);
    let parameters = parameters
        .iter()
        .map(|parameter| layouts.source(*parameter))
        .collect::<Result<_, _>>()?;
    Ok(RecordTypes {
        definition,
        parameters,
        types: layouts.finish()?,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConcreteBody {
    pub schema: u32,
    pub instance: [u8; 32],
    pub generic_body: u32,
    pub body: GirBody,
    pub types: Vec<TypeLayout>,
    pub calls: Vec<ConcreteCall>,
    pub fingerprint: [u8; 32],
    #[serde(skip)]
    pub fingerprint_valid: bool,
}

impl ConcreteBody {
    pub fn fingerprint(&self) -> [u8; 32] {
        if self.fingerprint_valid {
            return self.fingerprint;
        }
        crate::hash::hash_domain(
            "gugu-concrete-gir-v1",
            &crate::canonical::to_vec(&(
                self.schema,
                self.instance,
                self.generic_body,
                &self.body,
                &self.types,
                &self.calls,
            ))
            .expect("具体 GIR 编码"),
        )
    }

    pub fn verify(&self) -> Result<(), Diagnostic> {
        if !self.fingerprint_valid && self.fingerprint != self.fingerprint() {
            return Err(invalid("具体 GIR 的 schema、实例替换或指纹不合法"));
        }
        self.verify_structure()
    }

    /// 新鲜构造或 pass 修改后的对象仍完整校验结构，只计算一次规范指纹。
    pub fn refresh_fingerprint(&mut self) -> Result<(), Diagnostic> {
        self.fingerprint_valid = false;
        self.fingerprint = self.fingerprint();
        self.fingerprint_valid = true;
        self.verify_structure()
    }

    fn verify_structure(&self) -> Result<(), Diagnostic> {
        if self.schema != SCHEMA || self.body.generic_params != 0 {
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

pub fn build(
    context: &MonoContext<'_>,
    world: &super::GirWorldV1,
    mono: &MonoWorldV1,
) -> Result<Vec<ConcreteBody>, Vec<Diagnostic>> {
    let mut bodies = Vec::with_capacity(world.fragments.len());
    // 闭合实例按摘要排序，位置即 MonoId；只计算一次摘要，避免逐 fragment 重复哈希。
    let digests: Vec<_> = mono
        .instances
        .iter()
        .map(|instance| crate::mono::digest_of(&instance.mono_key))
        .collect();
    debug_assert!(digests.windows(2).all(|pair| pair[0] < pair[1]));
    for fragment in &world.fragments {
        let instance_index = digests
            .binary_search(&fragment.digest)
            .map_err(|_| vec![invalid("GIR fragment 不在闭合实例图中")])?;
        let instance = &mono.instances[instance_index];
        let body = build_body(context, world, mono, &digests, instance, fragment.body)
            .map_err(|error| vec![error])?;
        bodies.push(body);
    }
    Ok(bodies)
}

fn build_body(
    context: &MonoContext<'_>,
    world: &super::GirWorldV1,
    mono: &MonoWorldV1,
    digests: &[[u8; 32]],
    instance: &InstanceSummaryV1,
    body_index: u32,
) -> Result<ConcreteBody, Diagnostic> {
    if let Some(expanded) = crate::mono::packs::expand(
        context,
        hir::DefId(instance.definition),
        &instance.substitutions,
    )? {
        let nested = MonoContext::new(
            context.model,
            context.checked,
            context.identities,
            &expanded.module,
            context.sources,
            context.target,
            context.harness,
        );
        let owner = &expanded.module.owners
            [nested.owner_of[instance.definition as usize].expect("pack owner")];
        let mut body = super::build::lower(&expanded.module, owner)?;
        for statement in &mut body.statements {
            if let super::body::StatementKind::Assign(
                _,
                super::body::Rvalue::CheckedOp { check, .. },
            ) = &mut statement.kind
            {
                *check = expanded.checks[*check as usize];
            }
        }
        body.expression_locals.truncate(expanded.expressions);
        for local in &mut body.locals {
            if local
                .hir_local
                .is_some_and(|local| local.index() >= expanded.locals)
            {
                local.hir_local = None;
            }
        }
        for scope in &mut body.source_scopes {
            scope.hir_scope = expanded.scopes[scope.hir_scope.index()];
        }
        for record in &mut body.exit_records {
            record.plan = expanded.plans[record.plan as usize];
        }
        for region in &mut body.cleanup_regions {
            match &mut region.action {
                hir::CleanupAction::Action { cleanup, .. } => {
                    *cleanup = expanded.cleanup[*cleanup as usize]
                }
                hir::CleanupAction::DrainChain { until } => {
                    if let Some(cleanup) = until {
                        *cleanup = expanded.cleanup[*cleanup as usize]
                    }
                }
            }
        }
        return finish_body(&nested, mono, digests, instance, body_index, body);
    }
    finish_body(
        context,
        mono,
        digests,
        instance,
        body_index,
        world.bodies[index(body_index)].clone(),
    )
}

fn finish_body(
    context: &MonoContext<'_>,
    mono: &MonoWorldV1,
    digests: &[[u8; 32]],
    instance: &InstanceSummaryV1,
    body_index: u32,
    mut body: GirBody,
) -> Result<ConcreteBody, Diagnostic> {
    let mut layouts = layout::Builder::new(context, &instance.substitutions);
    let calls = calls::bind(&body, mono, digests, instance, &mut layouts)?;
    materialize::constants(context, mono, instance, &mut body)?;
    materialize::types(&mut body, &calls, &mut layouts)?;
    materialize::late(mono, instance, &mut body)?;
    body.generic_params = 0;
    let types = layouts.finish()?;
    let mut concrete = ConcreteBody {
        schema: SCHEMA,
        instance: crate::mono::digest_of(&instance.mono_key),
        generic_body: body_index,
        body,
        types,
        calls,
        fingerprint: [0; 32],
        fingerprint_valid: false,
    };
    concrete.refresh_fingerprint()?;
    Ok(concrete)
}

pub fn index(value: u32) -> usize {
    usize::try_from(value).expect("稠密 u32 编号适配宿主")
}

pub fn id(value: usize) -> u32 {
    u32::try_from(value).expect("arena 数量不超过 u32")
}

pub fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::LirInvariant, message, None)
}
