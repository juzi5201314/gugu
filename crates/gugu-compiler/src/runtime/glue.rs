//! runtime glue 绑定：把 glue 名解析为镜像内的唯一定义。
//!
//! 绑定表在 `BuildIr` 内、LIR 冻结之后解析：`Item` 条目在内建源里找到唯一的非泛型 `fn`
//! 定义与唯一实例；`Fragment` 条目由登记机器片段提供（放置与符号登记在镜像写出阶段）。
//! 被 LIR 引用的 glue 缺失定义、定义重复、实例不唯一或调用点 ABI 形状不一致都在写出镜像
//! 前失败（`E0062`），不再静默兜底到 stub；未被引用的条目留给后续阶段补齐定义。

use super::{GlueDef, GlueDefKind, RuntimeResources};
use crate::backend::x64::mangle;
use crate::frontend::hir;
use crate::frontend::mono::keys::hash_domain;
use crate::lir;
use crate::lir::body::{CallTarget, Op, Terminator};
use crate::{Diagnostic, DiagnosticCode};
use std::collections::BTreeMap;

/// glue 的最终定义目标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BindingTarget {
    /// Gugu 源项定义：绑定实例的函数体符号改为 `mangle_runtime(glue)`。
    Instance([u8; 32]),
    /// 镜像 writer 放置的登记机器片段。
    Fragment,
}

/// 已验证的 runtime glue 绑定表；按 glue 名稳定排序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeGlueBindings {
    map: BTreeMap<&'static str, BindingTarget>,
    fingerprint: [u8; 32],
}

impl RuntimeGlueBindings {
    /// 解析登记表：被引用的 `Item` glue 必须有唯一的非泛型定义与唯一实例。
    pub(crate) fn resolve(
        resources: &RuntimeResources,
        hir: &hir::Validated,
        lir: &lir::Validated,
    ) -> Result<Self, Diagnostic> {
        Self::resolve_defs(resources.glue_defs(), hir, lir)
    }

    /// 按给定定义表解析；确定性测试用合成表覆盖缺失、重复与签名不一致。
    pub(crate) fn resolve_defs(
        defs: &[GlueDef],
        hir: &hir::Validated,
        lir: &lir::Validated,
    ) -> Result<Self, Diagnostic> {
        let module = hir.module();
        let mut map = BTreeMap::new();
        for def in defs {
            let target = match &def.def {
                GlueDefKind::Fragment => BindingTarget::Fragment,
                GlueDefKind::Item { source, item } => {
                    match resolve_item(module, lir, def.glue, source, item)? {
                        Some(instance) => BindingTarget::Instance(instance),
                        None if referenced(lir, def.glue) => {
                            return Err(binding_error(format!(
                                "runtime glue `{}` 引用的源项 `{item}` 不在 {source} 中",
                                def.glue
                            )));
                        }
                        None => continue,
                    }
                }
            };
            if map.insert(def.glue, target).is_some() {
                return Err(binding_error(format!(
                    "runtime glue `{}` 重复登记",
                    def.glue
                )));
            }
        }
        let bindings = Self {
            map,
            fingerprint: [0; 32],
        };
        let bindings = bindings.sealed();
        bindings.verify_abi(lir)?;
        Ok(bindings)
    }

    /// 绑定表指纹；glue 名与目标实例的规范编码。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 绑定表的规范编码；action key 与 dump 消费同一份字节。
    pub(crate) fn summary(&self) -> Vec<u8> {
        self.canonical()
    }

    /// 已绑定的 glue 数量。
    pub(crate) fn bound(&self) -> u32 {
        u32::try_from(self.map.len()).expect("glue 数量适配 u32")
    }

    /// 绑定实例的 glue 名；该实例没有被绑定返回 `None`。
    pub(crate) fn glue_of_instance(&self, instance: &[u8; 32]) -> Option<&'static str> {
        self.map.iter().find_map(|(glue, target)| match target {
            BindingTarget::Instance(bound) if bound == instance => Some(*glue),
            _ => None,
        })
    }

    /// 该 mangled 符号是否属于已绑定 glue。
    pub(crate) fn is_bound_symbol(&self, symbol: &str) -> bool {
        self.map
            .keys()
            .any(|glue| mangle::mangle_runtime(glue) == symbol)
    }

    /// 已绑定 glue 的 mangled 符号名集合；镜像 writer 用它判定「绑定但缺失定义」。
    pub(crate) fn bound_symbols(&self) -> std::collections::BTreeSet<String> {
        self.map
            .keys()
            .map(|glue| mangle::mangle_runtime(glue))
            .collect()
    }

    /// 每个 Item 绑定的调用点 ABI 形状必须与绑定函数自身的签名一致。
    fn verify_abi(&self, lir: &lir::Validated) -> Result<(), Diagnostic> {
        let bodies = lir.bodies();
        for (glue, target) in &self.map {
            let BindingTarget::Instance(instance) = target else {
                continue;
            };
            let Some(body) = bodies.iter().find(|body| body.instance == *instance) else {
                return Err(binding_error(format!(
                    "runtime glue `{glue}` 的绑定实例不在 LIR 世界中"
                )));
            };
            for site in bodies {
                for (parameters, results) in site
                    .instructions
                    .iter()
                    .filter_map(|instruction| match &instruction.op {
                        Op::Call(call) | Op::ForeignCall(call) => match &call.target {
                            CallTarget::Runtime(runtime)
                                if mangle::runtime_call_name(*runtime) == *glue =>
                            {
                                Some((&call.parameters, &call.results))
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .chain(std::iter::once(&site.blocks).flat_map(|blocks| {
                        blocks.iter().filter_map(|block| match &block.terminator {
                            Terminator::Invoke { call, .. }
                                if matches!(
                                    &call.target,
                                    CallTarget::Runtime(runtime)
                                        if mangle::runtime_call_name(*runtime) == *glue
                                ) =>
                            {
                                Some((&call.parameters, &call.results))
                            }
                            _ => None,
                        })
                    }))
                {
                    if parameters != &body.signature.parameters
                        || results != &body.signature.results
                    {
                        return Err(binding_error(format!(
                            "runtime glue `{glue}` 在 `{}` 的调用点 ABI 形状与绑定函数不一致",
                            site.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// dump 用的逐条清单：`runtime-glue <名> -> instance <hex>|fragment`，按 glue 名排序。
    pub(crate) fn dump_lines(&self) -> Vec<String> {
        self.map
            .iter()
            .map(|(glue, target)| match target {
                BindingTarget::Instance(instance) => {
                    format!("runtime-glue {glue} -> instance {}", hex(instance))
                }
                BindingTarget::Fragment => format!("runtime-glue {glue} -> fragment"),
            })
            .collect()
    }

    fn sealed(mut self) -> Self {
        let canonical = self.canonical();
        self.fingerprint = hash_domain("gugu-runtime-glue-bindings-v1", &canonical);
        self
    }

    fn canonical(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.map.len() * 96);
        for (glue, target) in &self.map {
            bytes.extend_from_slice(glue.as_bytes());
            bytes.push(b'=');
            match target {
                BindingTarget::Instance(instance) => {
                    bytes.extend_from_slice(hex(instance).as_bytes())
                }
                BindingTarget::Fragment => bytes.extend_from_slice(b"fragment"),
            }
            bytes.push(b'\n');
        }
        bytes
    }
}

/// 解析内建源里唯一的非泛型 `fn` 定义与唯一实例；定义缺席返回 `None`。
fn resolve_item(
    module: &hir::Module,
    lir: &lir::Validated,
    glue: &str,
    source: &str,
    item: &str,
) -> Result<Option<[u8; 32]>, Diagnostic> {
    let mut definition = None;
    for candidate in &module.definitions {
        if candidate.name != item || candidate.kind != hir::DefinitionKind::Function {
            continue;
        }
        let in_source = candidate.location.as_ref().is_some_and(|location| {
            module.sources[usize::try_from(location.source).expect("source 下标")].path == source
        });
        if !in_source {
            continue;
        }
        if definition.replace(candidate).is_some() {
            return Err(binding_error(format!(
                "runtime glue `{glue}` 在 {source} 中重复定义 `{item}`"
            )));
        }
    }
    let Some(definition) = definition else {
        return Ok(None);
    };
    if !definition.parameters.is_empty() || !definition.obligations.is_empty() {
        return Err(binding_error(format!(
            "runtime glue `{glue}` 的绑定函数 `{item}` 必须是泛型无关的"
        )));
    }
    let mut instance = None;
    for body in lir.bodies() {
        if body.owner != definition.key {
            continue;
        }
        if instance.replace(body.instance).is_some() {
            return Err(binding_error(format!(
                "runtime glue `{glue}` 的绑定函数 `{item}` 有多个实例"
            )));
        }
    }
    match instance {
        Some(instance) => Ok(Some(instance)),
        None => Err(binding_error(format!(
            "runtime glue `{glue}` 的绑定函数 `{item}` 在镜像中没有实例"
        ))),
    }
}

/// 最终 LIR 中是否存在引用该 glue 的调用点。
fn referenced(lir: &lir::Validated, glue: &str) -> bool {
    lir.bodies().iter().any(|body| {
        body.instructions.iter().any(|instruction| {
            matches!(
                &instruction.op,
                Op::Call(call) | Op::ForeignCall(call)
                    if matches!(
                        &call.target,
                        CallTarget::Runtime(runtime)
                            if mangle::runtime_call_name(*runtime) == glue
                    )
            )
        }) || body.blocks.iter().any(|block| {
            matches!(
                &block.terminator,
                Terminator::Invoke { call, .. }
                    if matches!(
                        &call.target,
                        CallTarget::Runtime(runtime)
                            if mangle::runtime_call_name(*runtime) == glue
                    )
            )
        })
    })
}

fn binding_error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::RuntimeGlueBinding, message, None)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}
