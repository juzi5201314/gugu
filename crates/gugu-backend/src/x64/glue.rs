//! runtime glue 绑定：把 glue 名解析为镜像内的唯一定义。
//!
//! 绑定表在 `BuildIr` 内、LIR 冻结之后解析：`Item` 条目在内建源里找到唯一的非泛型 `fn`
//! 定义与唯一实例；`Fragment` 条目由登记机器片段提供（放置与符号登记在镜像写出阶段）。
//! 被 LIR 引用的 glue 缺失定义、定义重复、实例不唯一或调用点 ABI 形状不一致都在写出镜像
//! 前失败（`E0062`），不再静默兜底到 stub；未被引用的条目留给后续阶段补齐定义。

use crate::hash::hash_domain;
use crate::hir;
use crate::lir;
use crate::lir::body::{CallTarget, Op, Terminator, Value, ValueType};
use crate::runtime::{GLUE_DEFS, GlueDef, GlueDefKind, RuntimeResources};
use crate::x64::{abi, mangle};
use crate::{Diagnostic, diagnostics::DiagnosticCode};
use std::collections::BTreeMap;
use std::ops::Range;

/// op 形式隐含调用的 glue 目录：这些 op 没有 `Call` 节点，lowering 时才发出对 runtime 的
/// 调用。引用判定、glue 闭合检查与 lowering 必须共用这一份目录，否则某个 op 的入口缺失会
/// 绕过 E0062 直达镜像。返回 `None` 表示该 op 不隐含 runtime 调用（`Call`/`ForeignCall` 走
/// 各自的调用点判定；`PlatformCall`/`RuntimeMachineCall` 的 glue 名来自 op 自身）。
pub fn implied_glue(op: &Op) -> Option<&'static str> {
    Some(match op {
        Op::RegionPublish { .. } => "gc_region_publish",
        Op::RegionReset { .. } => "gc_region_reset",
        Op::PromoteManaged { .. } => "gc_region_promote",
        Op::RegionTransfer { .. } => "gc_region_transfer",
        Op::MarkTicketBatch => "gc_mark_ticket_batch",
        Op::EdgeDeltaBatch => "gc_edge_delta_batch",
        Op::ResolveSharedHandle => "gc_resolve_shared_handle",
        Op::SharedAccessBegin { .. } => "gc_shared_access_begin",
        Op::SharedAccessEnd { .. } => "gc_shared_access_end",
        Op::SharedFieldBarrier { .. } | Op::SharedFieldBarrierReserved { .. } => {
            "gc_shared_field_barrier"
        }
        Op::ForwardSharedHandle => "gc_forward_shared_handle",
        Op::GcWriteBarrierReserved { .. } => "gc_write_barrier_reserved",
        Op::GcWriteBarrier { .. } => "gc_write_barrier",
        Op::GcAlloc { placement, .. } => match placement {
            crate::gir::placement::PlacementKind::SharedHeap => "gc_shared_alloc",
            crate::gir::placement::PlacementKind::TurnRegion => "gc_region_slow",
            _ => "gc_alloc_slow",
        },
        Op::RegionAlloc { .. } => "gc_region_slow",
        // 机器原语同样是对 runtime 的直接调用：它们的入口名由 op 自身决定。
        Op::RuntimeMachineCall(machine) => machine.name(),
        _ => return None,
    })
}

/// 分配 ABI 的传出车道数：`rax/rbx/rcx/rdx` 四参、结果在 `rax`。
///
/// 与 `backend/x64/lower/alloc.rs::slow_alloc` 的寄存器落位是同一契约，由
/// `allocation_slow_abi_moves_bytes_before_immediates` 在 machine 级钉住。
pub const ALLOCATION_ABI_LANES: usize = 4;

/// region 生命周期入口的车道数：`publish` 收计划编号与 export 声明两参，其余三个只收计划编号；
/// 结果都为空。与 `backend/x64/lower/alloc.rs::region_end` 的寄存器落位是同一契约。
pub const REGION_END_LANES_PUBLISH: usize = 2;
pub const REGION_END_LANES_SIMPLE: usize = 1;

/// 该 op 的隐含调用由 lowering 合成传出参数时所应满足的 ABI 形状：`(参数车道数, 结果车道数)`。
///
/// 分配族（`GcAlloc`/`RegionAlloc`）由 `slow_alloc` 合成四整数参 + 单整数结果：bytes 取自
/// 操作数，type_id/align/flags 是立即数。region 生命周期入口（`RegionPublish` 的 plan 与
/// export、其余三个的 plan）由 `region_end` 合成立即数实参、结果为空。这些 op 的操作数车道数
/// 与 ABI 车道数无关，逐条车道比对不成立，只能校验形状。
///
/// 返回 `None` 的 op 走 `runtime_named`：操作数即 ABI 车道，逐条比对成立。
pub fn implied_glue_synthesized_shape(op: &Op) -> Option<(usize, usize)> {
    match op {
        Op::GcAlloc { .. } | Op::RegionAlloc { .. } => Some((ALLOCATION_ABI_LANES, 1)),
        Op::RegionPublish { .. } => Some((REGION_END_LANES_PUBLISH, 0)),
        Op::RegionReset { .. } | Op::PromoteManaged { .. } | Op::RegionTransfer { .. } => {
            Some((REGION_END_LANES_SIMPLE, 0))
        }
        _ => None,
    }
}

/// glue 的最终定义目标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingTarget {
    /// Gugu 源项定义：绑定实例的函数体符号改为 `mangle_runtime(glue)`。
    Instance([u8; 32]),
    /// 镜像 writer 放置的登记机器片段。
    Fragment,
}

/// 已验证的 runtime glue 绑定表；按 glue 名稳定排序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeGlueBindings {
    map: BTreeMap<&'static str, BindingTarget>,
    /// 构造时生成一次；热路径点查与 writer 的有序枚举共用同一集合。
    symbols: std::collections::BTreeSet<String>,
    runtime_owners: std::collections::BTreeSet<[u8; 32]>,
    fingerprint: [u8; 32],
}

impl RuntimeGlueBindings {
    /// 解析登记表：被引用的 `Item` glue 必须有唯一的非泛型定义与唯一实例。
    pub fn resolve(
        resources: &RuntimeResources<'_>,
        hir: &hir::Validated,
        lir: &lir::Validated,
    ) -> Result<Self, Diagnostic> {
        let mut bindings = Self::resolve_defs(resources.glue_defs(), hir, lir)?;
        let paths: std::collections::BTreeSet<_> = resources.runtime_paths().collect();
        let hashes: BTreeMap<_, _> = resources
            .source_blobs()
            .iter()
            .filter(|source| paths.contains(source.logical_path.as_str()))
            .map(|source| {
                (
                    source.logical_path.as_str(),
                    *blake3::hash(source.source.as_bytes()).as_bytes(),
                )
            })
            .collect();
        let module = hir.module();
        bindings.runtime_owners = module
            .definitions
            .iter()
            .filter_map(|definition| {
                let source = &module.sources[definition.location.as_ref()?.source as usize];
                (hashes.get(source.path.as_str()) == Some(&source.hash)).then_some(definition.key)
            })
            .collect();
        Ok(bindings.sealed())
    }

    /// 按给定定义表解析；确定性测试用合成表覆盖缺失、重复与签名不一致。
    pub fn resolve_defs(
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
            symbols: map
                .keys()
                .map(|glue| mangle::mangle_runtime(glue))
                .collect(),
            map,
            runtime_owners: Default::default(),
            fingerprint: [0; 32],
        };
        let bindings = bindings.sealed();
        bindings.verify_abi(lir)?;
        bindings.verify_closure(lir)?;
        Ok(bindings)
    }

    /// 尚未提供 Gugu 执行体的 GC 算法入口；由计划步骤 6–8 交付。
    ///
    /// 计划第 3.6 条只允许「相邻条目尚未实现的入口」走编译期拒绝；这些条目属于本切片后续
    /// 步骤，因此闭合检查对它们放行，其余任何被可达代码引用却无定义的入口都必须阻断。清单
    /// 清空即代表本切片的 GC 执行体全部就位。
    const PENDING_GLUE_IMPL: &'static [&'static str] = &[
        "type_is",
        "downcast",
        "downcast_copy",
        "gc_mark_ticket_batch",
        "gc_edge_delta_batch",
        "gc_forward_shared_handle",
        // 平台范围原语只实现了 reserve/commit/decommit/release 四个（`fragments.rs` 的
        // `PLATFORM_OPS_WITH_CODE`）：guard 页、显式清零、字上等待/唤醒与 entropy 属步骤 6 的
        // arena 与步骤 8 的并发协议，两个 hint 是可选能力。缺口在此登记，绝不静默落桩。
        "platform_protect_guard",
        "platform_unprotect",
        "platform_wait",
        "platform_wake",
        "platform_entropy",
        "platform_zero",
        "platform_set_dump_policy",
        "platform_low_memory_hint",
        "platform_huge_page_hint",
        // `wide_div` 的 signed/remainder 语义烘在 `RuntimeCall` 标志位里，不在调用车道上；
        // 单一 glue 名无法区分四种语义，由后续实现阶段拆名或改为参数化调用。
        "wide_div",
        // `blocking_io` 是遗留 trait 接缝（gir-lir.md：文件 I/O 已迁到 `fs_start`/`fs_poll`），
        // 无源码路径发射它；登记以保持目录完整。
        "blocking_io",
    ];

    /// 闭合检查：可达代码引用到的每个 glue 入口都必须有真实定义。
    ///
    /// 缺口在此以 `E0062` 阻断并让镜像动作失败，而不是留到 ELF/PE 把符号兜底成
    /// `__gugu_stub_exit`；未被可达代码引用的入口不强制实例化（计划第 3.6 条）。
    fn verify_closure(&self, lir: &lir::Validated) -> Result<(), Diagnostic> {
        let defined =
            |glue: &str| self.map.contains_key(glue) || Self::PENDING_GLUE_IMPL.contains(&glue);
        for body in lir.bodies() {
            for instruction in &body.instructions {
                if let Some(glue) = implied_glue(&instruction.op)
                    && !defined(glue)
                {
                    return Err(binding_error(format!(
                        "runtime glue `{glue}` 被 `{}` 引用但没有任何定义",
                        body.name
                    )));
                }
            }
            for block in &body.blocks {
                let Terminator::Invoke { call, .. } = &block.terminator else {
                    continue;
                };
                let CallTarget::Runtime(runtime) = &call.target else {
                    continue;
                };
                let glue = mangle::runtime_call_name(*runtime);
                if !defined(glue) {
                    return Err(binding_error(format!(
                        "runtime glue `{glue}` 被 `{}` 的 invoke 引用但没有任何定义",
                        body.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// 绑定表指纹；glue 名与目标实例的规范编码。
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 绑定表的规范编码；action key 与 dump 消费同一份字节。
    pub fn summary(&self) -> Vec<u8> {
        self.canonical()
    }

    /// 已绑定的 glue 数量。
    pub fn bound(&self) -> u32 {
        u32::try_from(self.map.len()).expect("glue 数量适配 u32")
    }

    /// 绑定实例的 glue 名；该实例没有被绑定返回 `None`。
    pub fn glue_of_instance(&self, instance: &[u8; 32]) -> Option<&'static str> {
        self.map.iter().find_map(|(glue, target)| match target {
            BindingTarget::Instance(bound) if bound == instance => Some(*glue),
            _ => None,
        })
    }

    /// 该 mangled 符号是否属于已绑定 glue。
    pub fn is_bound_symbol(&self, symbol: &str) -> bool {
        self.symbols.contains(symbol)
    }

    /// 已绑定 glue 的 mangled 符号名集合；镜像 writer 用它判定「绑定但缺失定义」。
    pub fn bound_symbols(&self) -> std::collections::BTreeSet<String> {
        self.symbols.clone()
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
            if matches!(*glue, "foreign_dispatch" | "foreign_dirty_dispatch")
                && (body.signature.parameters
                    != vec![ValueType::scalar(crate::lir::body::Type::I64); 4]
                    || !body.signature.results.is_empty())
            {
                return Err(binding_error(
                    "foreign_dispatch 必须是四个机器字参数且无返回车道",
                ));
            }
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
                    if !runtime_abi_types_match(parameters, &body.signature.parameters)
                        || !runtime_abi_types_match(results, &body.signature.results)
                    {
                        return Err(binding_error(format!(
                            "runtime glue `{glue}` 在 `{}` 的调用点 ABI 形状与绑定函数不一致：call={parameters:?}/{results:?} bound={:?}/{:?}",
                            site.name, body.signature.parameters, body.signature.results
                        )));
                    }
                }
                // op 形式的隐含调用没有 `Call` 节点：lowering 在 `runtime_named` 里按
                // `INTERNAL_INTEGER` 顺序落位，因此车道数必须与签名逐一对应，且两侧都不得出现
                // 浮点车道（整数寄存器车道装不下 SSE 值）。车道漂移在写出镜像前失败。
                for (index, instruction) in site.instructions.iter().enumerate() {
                    if implied_glue(&instruction.op) != Some(*glue) {
                        continue;
                    }
                    let arguments = site
                        .args(&instruction.arguments)
                        .iter()
                        .map(|id| &site.values[id.index()]);
                    let results = lanes(&site.values, &instruction.results);
                    // 分配族与 region 生命周期入口的传出参数由 lowering 合成（分配族：bytes 取自
                    // 操作数，type/align/flags 是立即数；region：plan/export 是立即数），操作数车道
                    // 不等于 ABI 车道；这类 op 只校验 ABI 形状本身。
                    let matches = if let Some((params, results)) =
                        implied_glue_synthesized_shape(&instruction.op)
                    {
                        body.signature.parameters.len() == params
                            && body
                                .signature
                                .parameters
                                .iter()
                                .all(|ty| !abi::is_float(*ty))
                            && body.signature.results.len() == results
                            && body.signature.results.iter().all(|ty| !abi::is_float(*ty))
                    } else {
                        lanes_are_integer(arguments, &body.signature.parameters)
                            && lanes_are_integer(results.iter(), &body.signature.results)
                    };
                    if !matches {
                        return Err(binding_error(format!(
                            "runtime glue `{glue}` 在 `{}` 第 {index} 条指令的隐含调用车道形状与绑定函数不一致",
                            site.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// dump 用的逐条清单：`runtime-glue <名> -> instance <hex>|fragment`，按 glue 名排序。
    pub fn dump_lines(&self) -> Vec<String> {
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

    pub(crate) fn defers_automatic_poll(&self, owner: &[u8; 32]) -> bool {
        self.runtime_owners.contains(owner)
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
        for owner in &self.runtime_owners {
            bytes.extend_from_slice(b"runtime-owner=");
            bytes.extend_from_slice(owner);
        }
        bytes
    }
}

/// runtime glue 的机器 ABI 只携带指针宽度；Stack/GcHeap/Metadata/Raw provenance 在边界处
/// 不改变寄存器或栈车道，不能把来源标签误当成不同的物理签名。
pub(crate) fn runtime_abi_types_match(actual: &[ValueType], expected: &[ValueType]) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual == expected
                || (actual.ty == expected.ty && actual.ty == crate::lir::body::Type::Ptr)
        })
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
    Ok(instance)
}

/// 最终 LIR 中是否存在引用该 glue 的调用点或隐含调用 op。
fn referenced(lir: &lir::Validated, glue: &str) -> bool {
    if matches!(glue, "foreign_dispatch" | "foreign_dirty_dispatch") {
        return lir.bodies().iter().any(|body| {
            body.instructions.iter().any(|instruction| matches!(&instruction.op, Op::Call(call) | Op::ForeignCall(call) if super::lower::foreign::dispatcher(call.kind) == Some(glue)))
                || body.blocks.iter().any(|block| matches!(&block.terminator, Terminator::Invoke { call, .. } if super::lower::foreign::dispatcher(call.kind) == Some(glue)))
        });
    }
    lir.bodies().iter().any(|body| {
        body.instructions.iter().any(|instruction| {
            implied_glue(&instruction.op) == Some(glue)
                || matches!(
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

/// 指令实参/结果的值区间切片；LIR 结构校验已保证区间落在值表之内，越界按空车道处理以免
/// 把结构错误伪装成车道匹配。
fn lanes<'a>(operands: &'a [Value], range: &Range<u32>) -> &'a [Value] {
    let start = usize::try_from(range.start).unwrap_or(usize::MAX);
    let end = usize::try_from(range.end).unwrap_or(usize::MAX);
    operands.get(start..end).unwrap_or(&[])
}

/// op 形式的隐含调用只走整数车道：车道数与签名一致，且两侧都不得出现浮点车道。
fn lanes_are_integer<'a>(
    mut values: impl ExactSizeIterator<Item = &'a Value>,
    expected: &[ValueType],
) -> bool {
    values.len() == expected.len()
        && values.all(|value| !abi::is_float(value.kind))
        && expected.iter().all(|ty| !abi::is_float(*ty))
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

/// 判断 runtime glue 是否已经登记定义，或是否被显式列为当前迁移切片的缺口。
///
/// lowering 只在真正发出 runtime 符号调用时调用本查询；由 lowering 内联的 runtime 内建不会
/// 经过这里，因此不会因为名字存在于 `RuntimeCall` 目录而误报。
pub fn glue_is_declared(name: &str) -> bool {
    GLUE_DEFS.iter().any(|definition| definition.glue == name)
        || RuntimeGlueBindings::PENDING_GLUE_IMPL.contains(&name)
}

/// 闭世界 glue 目录里的全部入口名：真实定义与显式缺口记录的并集。
///
/// 诊断按名字反查符号哈希时必须遍历同一份闭世界目录，否则「引用但未实现」的入口会退化成
/// 一堆无法辨认的哈希。
pub fn declared_glue_names() -> impl Iterator<Item = &'static str> {
    GLUE_DEFS
        .iter()
        .map(|definition| definition.glue)
        .chain(RuntimeGlueBindings::PENDING_GLUE_IMPL.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lir::body::{Definition, Origin, Type, id};

    fn value(ty: Type) -> Value {
        Value {
            definition: Definition::Parameter {
                block: crate::lir::body::BlockId(id(0)),
                index: 0,
            },
            kind: ValueType::scalar(ty),
            origin: Origin::None,
            uses: 0..0,
        }
    }

    /// op 形式的隐含调用只走整数车道：车道数必须与签名一致，任一侧出现浮点车道都拒绝
    /// （`runtime_named` 按 `INTERNAL_INTEGER` 落位，整数寄存器装不下 SSE 值）。
    #[test]
    fn implied_lanes_require_integer_lanes_of_matching_arity() {
        let ints = [value(Type::I64), value(Type::Ptr)];
        let floats = [value(Type::F64)];
        let expected = [ValueType::scalar(Type::I64), ValueType::scalar(Type::Ptr)];
        assert!(
            lanes_are_integer(ints.iter(), &expected),
            "同宽整数车道必须通过"
        );
        assert!(
            !lanes_are_integer(ints[..1].iter(), &expected),
            "车道数不足必须拒绝"
        );
        assert!(
            !lanes_are_integer(floats.iter(), &[ValueType::scalar(Type::I64)]),
            "实参浮点车道必须拒绝"
        );
        assert!(
            !lanes_are_integer(ints[..1].iter(), &[ValueType::scalar(Type::F64)]),
            "签名浮点车道必须拒绝"
        );
    }

    /// 值区间越界按空车道处理，不把结构错误伪装成车道匹配。
    #[test]
    fn lane_slices_stop_at_the_value_table_end() {
        let values = [value(Type::I64)];
        assert_eq!(lanes(&values, &(0..1)).len(), 1);
        assert_eq!(lanes(&values, &(0..4)).len(), 0, "越界必须退化为空车道");
        assert_eq!(lanes(&values, &(2..3)).len(), 0);
    }
}
