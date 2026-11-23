//! Source-backed runtime identities from the existing canonical glue resolver.
use std::collections::BTreeMap;

use super::{Error, abi};
use crate::{
    hir,
    lir::{
        self,
        body::{Body, Call, CallTarget, Op, Signature, ValueType},
    },
    runtime::RuntimeResources,
    x64::{glue::RuntimeGlueBindings, mangle},
};

pub struct Catalog<'a> {
    bodies: &'a [Body],
    functions: BTreeMap<&'static str, &'a Body>,
}

impl<'a> Catalog<'a> {
    pub fn resolve(
        resources: &RuntimeResources<'_>,
        hir: &hir::Validated,
        lir: &'a lir::Validated,
    ) -> Result<Self, Error> {
        let bindings = RuntimeGlueBindings::resolve(resources, hir, lir)
            .map_err(|error| super::error(error.message()))?;
        Self::from_resolved(&bindings, lir)
    }

    pub fn from_resolved(
        bindings: &RuntimeGlueBindings,
        lir: &'a lir::Validated,
    ) -> Result<Self, Error> {
        let mut functions = BTreeMap::new();
        for body in lir.bodies() {
            if let Some(name) = bindings.glue_of_instance(&body.instance)
                && functions.insert(name, body).is_some()
            {
                return Err(super::error("duplicate A64 runtime source definition"));
            }
        }
        Ok(Self {
            bodies: lir.bodies(),
            functions,
        })
    }

    pub(crate) fn validate_bodies(&self, bodies: &[Body]) -> Result<(), Error> {
        let instances = bodies
            .iter()
            .map(|body| body.instance)
            .collect::<std::collections::BTreeSet<_>>();
        for body in bodies {
            if self
                .bodies
                .iter()
                .find(|frozen| frozen.instance == body.instance)
                != Some(body)
            {
                return Err(super::error("A64 runtime catalog and frozen body differ"));
            }
            for instruction in &body.instructions {
                let key = match &instruction.op {
                    Op::Call(call) => match call.target {
                        CallTarget::Instance(key) => Some(key),
                        CallTarget::Runtime(runtime) => {
                            Some(self.function(mangle::runtime_call_name(runtime))?.instance)
                        }
                        _ => None,
                    },
                    op if machine_binding(op).is_some() => None,
                    op => operation_name(op)
                        .map(|name| self.function(name).map(|body| body.instance))
                        .transpose()?,
                };
                if key.is_some_and(|key| !instances.contains(&key)) {
                    return Err(super::error(
                        "A64 bound call requires its compiled source body, not an explicit code definition",
                    ));
                }
            }
        }
        Ok(())
    }

    /// glue → 绑定的源 body；世界用它为每个 Item glue 发射 `__gugu_runtime_*` 别名符号。
    pub fn bindings(&self) -> impl Iterator<Item = (&'static str, &'a Body)> {
        self.functions.iter().map(|(name, body)| (*name, *body))
    }

    pub fn function(&self, name: &str) -> Result<&'a Body, Error> {
        self.functions.get(name).copied().ok_or_else(|| {
            super::error(&format!("A64 runtime `{name}` requires a canonical source definition; machine bindings are not registered"))
        })
    }

    pub(crate) fn checked_target(
        &self,
        name: &str,
        parameters: &[ValueType],
        results: &[ValueType],
    ) -> Result<(&'a Body, String), Error> {
        self.target(name, parameters, results, false)
    }

    pub(crate) fn operation_target(
        &self,
        name: &str,
        parameters: &[ValueType],
        results: &[ValueType],
    ) -> Result<(&'a Body, String), Error> {
        self.target(name, parameters, results, true)
    }

    fn target(
        &self,
        name: &str,
        parameters: &[ValueType],
        results: &[ValueType],
        integer_lanes: bool,
    ) -> Result<(&'a Body, String), Error> {
        let body = self.function(name)?;
        let Signature { sret, by_value, .. } = &body.signature;
        let compatible = |actual: &[ValueType], expected: &[ValueType]| {
            if !integer_lanes {
                return crate::x64::glue::runtime_abi_types_match(actual, expected);
            }
            actual.len() == expected.len()
                && actual.iter().zip(expected).all(|(a, b)| {
                    abi::class(a.ty) == abi::GPR
                        && abi::class(b.ty) == abi::GPR
                        && a.ty.bytes().is_some()
                        && a.ty.bytes() == b.ty.bytes()
                })
        };
        if sret.is_some()
            || !by_value.is_empty()
            || !compatible(parameters, &body.signature.parameters)
            || !compatible(results, &body.signature.results)
        {
            return Err(super::error(&format!(
                "A64 runtime `{name}` call ABI differs from its source definition"
            )));
        }
        abi::arguments(parameters.iter().map(|kind| kind.ty))?;
        abi::results(results.iter().map(|kind| kind.ty))?;
        // 运行时调用统一走 `__gugu_runtime_*` 命名空间；Item glue 的别名由世界符号表发射。
        Ok((body, mangle::mangle_runtime(name)))
    }

    pub(crate) fn call_target(&self, call: &Call) -> Result<String, Error> {
        let CallTarget::Runtime(runtime) = call.target else {
            return Err(super::error("expected A64 runtime call"));
        };
        if (
            call.may_unwind,
            call.may_suspend,
            call.may_allocate,
            call.captures_arguments,
        ) != runtime.effects()
        {
            return Err(super::error(
                "A64 runtime call effects differ from the canonical operation",
            ));
        }
        let (body, target) = self.checked_target(
            mangle::runtime_call_name(runtime),
            &call.parameters,
            &call.results,
        )?;
        if call.poll_free_leaf && !body.poll_interface.leaf {
            return Err(super::error(
                "A64 runtime leaf call lacks a source leaf proof",
            ));
        }
        Ok(target)
    }

    #[cfg(test)]
    pub(crate) fn model(bodies: &'a [Body], entries: &[(&'static str, [u8; 32])]) -> Self {
        Self {
            bodies,
            functions: entries
                .iter()
                .map(|(name, key)| {
                    (
                        *name,
                        bodies.iter().find(|body| body.instance == *key).unwrap(),
                    )
                })
                .collect(),
        }
    }
}

/// 机器片段绑定的 op：`RuntimeMachineCall` 用 `machine.name()`，`PlatformCall` 用 `platform_*`。
/// 它们绕过 catalog 直接调 `mangle_runtime` 符号；片段定义或源绑定别名在世界中给出。
pub(crate) fn machine_binding(op: &Op) -> Option<String> {
    match op {
        Op::RuntimeMachineCall(machine) => Some(machine.name().into()),
        Op::PlatformCall(platform) => Some(format!("platform_{}", platform.name())),
        _ => None,
    }
}

/// lowering 后是否变成一次真实调用：源绑定 glue、机器/平台片段或分配慢路。
/// 帧布局与 safepoint 审计必须把这些隐式调用点计入。
pub(crate) fn implicit_call(op: &Op) -> bool {
    machine_binding(op).is_some()
        || matches!(op, Op::GcAlloc { .. } | Op::RegionAlloc { .. })
        || operation_name(op).is_some()
}

pub(crate) fn operation_name(op: &Op) -> Option<&'static str> {
    if matches!(op, Op::GcAlloc { .. } | Op::RegionAlloc { .. }) {
        return None;
    }
    crate::x64::glue::implied_glue(op)
}

pub(crate) fn proof_marker(op: &Op) -> bool {
    matches!(
        op,
        Op::BarrierReserve(_)
            | Op::ScopedViewBegin { .. }
            | Op::ScopedViewEnd { .. }
            | Op::NoSafepointBegin(_)
            | Op::NoSafepointEnd(_)
    )
}
