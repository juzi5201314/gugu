//! `InstantiateGir`（query 13）：把 `MonoKey` 绑定到 owner body 并收集实例边。
//!
//! 实例记录即本阶段的"每实例 code fragment"骨架：替换后的签名、callee 实例键、
//! vtable/type metadata 根与外部符号；机器码由后端阶段填充。
//!
//! 边收集覆盖全部 dispatch 位点（普通调用、运算符、`for`/`try`/下标协议派发）、
//! 函数值（`Ty::Callable`）、闭包/协程捕获计划与 local static 初始化器归属。

use super::keys::{
    MonoCallAbi, MonoContext, MonoInterner, MonoKey, MonoKind, hash_domain, type_structure_size,
    unify,
};
use crate::Diagnostic;
use crate::frontend::ast;
use crate::frontend::hir;
use crate::frontend::semantics::{
    AdjustmentKind, CallableId, CapturePlan, CheckedBody, DefRef, Dispatch, Method, Reflection,
    ReflectionKind, TraitRef, Ty, TypeAdjustment, substitute,
};
use crate::query::{QueryEngine, QueryKey, QueryKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// `InstantiateGir` 结果 schema。
pub(crate) const INSTANCE_SCHEMA: u32 = 1;

/// 一条实例边：callee `MonoKey` 与重建实例所需的语义工作值。
///
/// 语义 `Ty` 是 session-local 工作值，只随 session 内 query 载荷流动，
/// 不进入任何规范编码。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CalleeSeed {
    pub key: MonoKey,
    pub callable: CallableId,
    pub arguments: Vec<Ty>,
    pub signature: Ty,
}

/// 单个 `MonoKey` 的实例化结果；callee 按 key 摘要排序去重。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InstanceRecordV1 {
    pub schema: u32,
    /// 本实例 `MonoKey` 规范字节；缓存命中时与 query key 校验一致。
    pub mono_key: Vec<u8>,
    pub definition: u32,
    pub kind: MonoKind,
    pub symbol: String,
    pub public: bool,
    pub callees: Vec<CalleeSeed>,
    /// 泛型体内经 impl 选择固定的实现稳定键（排序去重）。
    pub selected_impls: Vec<[u8; 32]>,
    /// dyn 擦除点需要的 (接口, 具体类型) vtable 根。
    pub vtable_roots: Vec<VtableRootV1>,
    /// `type_id[T]` 等显式引用、进入 metadata 根的具体类型（排序去重）。
    pub metadata_roots: Vec<Vec<u8>>,
    /// `extern "C"` 导入的外部符号名（不是 Gugu 实例）。
    pub externals: Vec<String>,
    /// 含 `TypeAsInt`/`TypeIdCount` 的 late comptime 依赖标记。
    pub uses_late_comptime: bool,
}

/// dyn 擦除点的 vtable 根。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct VtableRootV1 {
    pub interface: [u8; 32],
    pub self_type: Vec<u8>,
}

/// 驱动侧的实例工作条目：key 与替换所需的语义绑定。
#[derive(Clone)]
pub(crate) struct WalkEntry {
    pub key: MonoKey,
    pub definition: hir::DefId,
    pub kind: MonoKind,
    /// 该实例 body 事实来源；闭包/协程与无体定义为 `None`（叶实例）。
    pub body: Option<usize>,
    /// 声明泛型参数名 -> 具体类型。
    pub bindings: BTreeMap<String, Ty>,
}

/// 由 callee 种子构造下一次遍历的工作条目。
///
/// 绑定按声明 `callable_context` 的参数名与实例实参一一对应；实参数量或
/// 结构不匹配是单态化不一致，直接报错。
pub(crate) fn walk_entry(
    context: &MonoContext<'_>,
    key: MonoKey,
    callable: CallableId,
    arguments: &[Ty],
) -> Result<WalkEntry, Diagnostic> {
    let definition = context.identities.function(callable);
    let declared = context.model.callable_context(callable);
    if declared.len() != arguments.len() {
        return Err(inconsistency("实例实参数量与声明泛型不一致"));
    }
    let mut bindings = BTreeMap::new();
    for (name, argument) in declared.into_keys().zip(arguments.iter()) {
        bindings.insert(name, argument.clone());
    }
    let kind = kind_of(context, definition);
    Ok(WalkEntry {
        key,
        definition,
        kind,
        body: context.body_of[definition.index()],
        bindings,
    })
}

/// 根实例（无泛型或以声明类型直接实例化）。
pub(crate) fn root_entry(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    definition: hir::DefId,
    bindings: BTreeMap<String, Ty>,
) -> Result<WalkEntry, Diagnostic> {
    let mut type_arguments = Vec::with_capacity(bindings.len());
    for ty in bindings.values() {
        let canonical = context.encode_type(ty)?;
        interner.intern_type(canonical.clone())?;
        type_arguments.push(canonical);
    }
    Ok(WalkEntry {
        key: MonoKey {
            definition: context.definition_key(definition),
            type_arguments,
            const_arguments: Vec::new(),
            selected_impls: Vec::new(),
            call_abi: MonoCallAbi::Gugu,
            target: context.target.to_string(),
            harness_mode: context.harness,
            instrumentation_mode: 0,
        },
        definition,
        kind: kind_of(context, definition),
        body: context.body_of[definition.index()],
        bindings,
    })
}

/// `InstantiateGir` query 包装：按 `MonoKey` 规范字节缓存实例记录。
///
/// fresh 计算后回落 interner，缓存命中恢复并校验 schema；失败走统一
/// `MonoDivergence` 诊断恢复路径，不写持久对象。
pub(crate) fn instantiate(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    queries: &QueryEngine,
    entry: &WalkEntry,
) -> Result<InstanceRecordV1, Vec<Diagnostic>> {
    let key = QueryKey::new(
        QueryKind::InstantiateGir,
        INSTANCE_SCHEMA,
        entry.key.canonical_bytes(),
    );
    let mut fresh = None;
    let result = queries.compute(key, |query_context| {
        query_context.record_dependency(
            QueryKey::new(
                QueryKind::CollectMonoRoots,
                super::roots::ROOTS_SCHEMA,
                context.module.input_fingerprint.to_vec(),
            ),
            context.module.input_fingerprint,
        );
        let record = walk_instance(context, interner, entry)
            .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
        let bytes = serde_json::to_vec(&record).expect("实例记录序列化");
        fresh = Some(record);
        Ok((bytes, Vec::new()))
    });
    let result = result.map_err(|error| vec![restore_error(error)])?;
    if let Some(record) = fresh {
        return Ok(record);
    }
    let record: InstanceRecordV1 = serde_json::from_slice(result.payload())
        .map_err(|_| vec![schema_error("InstantiateGir 缓存 schema 不合法")])?;
    if record.schema != INSTANCE_SCHEMA {
        return Err(vec![schema_error("InstantiateGir schema 版本不匹配")]);
    }
    if record.mono_key != entry.key.canonical_bytes() {
        return Err(vec![schema_error("InstantiateGir 缓存与实例 key 不一致")]);
    }
    Ok(record)
}

/// query 失败恢复为诊断；`Failed` 载荷是已存储诊断 JSON，解出原始信息。
fn restore_error(error: crate::query::QueryError) -> Diagnostic {
    match error {
        crate::query::QueryError::Failed(message) => {
            let decoded: Result<Vec<StoredDiagnostic>, _> = serde_json::from_str(&message);
            if let Ok(stored) = decoded
                && let Some(first) = stored.first()
            {
                return Diagnostic::error(first.code, first.message.clone(), None);
            }
            Diagnostic::error(crate::DiagnosticCode::MonoDivergence, message, None)
        }
        other => Diagnostic::error(
            crate::DiagnosticCode::MonoDivergence,
            other.to_string(),
            None,
        ),
    }
}

fn schema_error(message: &str) -> Diagnostic {
    Diagnostic::error(crate::DiagnosticCode::MonoDivergence, message, None)
}

/// 存储诊断的 JSON 形态（与 `semantics::query::store_errors` 一致）。
#[derive(serde::Deserialize)]
struct StoredDiagnostic {
    code: crate::DiagnosticCode,
    message: String,
}

fn inconsistency(message: &str) -> Diagnostic {
    Diagnostic::error(crate::DiagnosticCode::MonoDivergence, message, None)
}

/// 收集一个实例的全部直接边。
///
/// 边来源：全部 dispatch 位点（方法/运算符/协议派发）、函数值表达式
/// （`Ty::Callable`，覆盖普通函数调用与按值传递）、闭包/协程捕获计划、
/// 类型反射与 dyn 擦除调整。
pub(crate) fn walk_instance(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
) -> Result<InstanceRecordV1, Diagnostic> {
    let Some(body_index) = entry.body else {
        return Ok(leaf_record(context, entry));
    };
    let body: &CheckedBody = &context.checked.bodies[body_index];
    let module = context.definition_module[entry.definition.index()];
    let mut edges = Edges::default();
    let mut method_callees = BTreeSet::new();
    for dispatch in &body.dispatches {
        method_callees.insert(dispatch.expression.0);
        collect_dispatch(context, interner, entry, module, dispatch, &mut edges)?;
    }
    collect_calls(
        context,
        interner,
        entry,
        module,
        body,
        &method_callees,
        &mut edges,
    )?;
    for capture in &body.captures {
        collect_capture(context, interner, entry, capture, &mut edges)?;
    }
    collect_reflections(context, entry, &body.reflections, &mut edges)?;
    for adjustment in &body.adjustments {
        collect_adjustment(context, interner, module, adjustment, entry, &mut edges)?;
    }
    let definition = &context.module.definitions[entry.definition.index()];
    Ok(InstanceRecordV1 {
        schema: INSTANCE_SCHEMA,
        mono_key: entry.key.canonical_bytes(),
        definition: entry.definition.0,
        kind: entry.kind,
        symbol: definition.name.clone(),
        public: definition.public,
        callees: edges.callees.into_values().collect(),
        selected_impls: edges.selected_impls.into_iter().collect(),
        vtable_roots: edges.vtable_roots.into_values().collect(),
        metadata_roots: edges.metadata_roots.into_iter().collect(),
        externals: edges.externals.into_iter().collect(),
        uses_late_comptime: edges.uses_late_comptime,
    })
}

/// 调用点边：遍历 body 的 AST 调用表达式，按调用点实参类型解出实例化绑定。
///
/// 声明签名的参数占位与实参实类型 unify 得到绑定；解析后的实参集进入
/// callee `MonoKey`。dispatch 记录覆盖的调用点（方法/协议派发）跳过。
fn collect_calls(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
    module: usize,
    body: &CheckedBody,
    method_callees: &BTreeSet<u32>,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let arena = &context.model.modules[module].arena;
    for (id, _) in &body.expressions {
        let ast::ExprKind::Call {
            callee,
            args: ast_args,
            ..
        } = arena.exprs[id.0 as usize].kind
        else {
            continue;
        };
        if method_callees.contains(&callee.0) {
            continue;
        }
        // callee 路径表达式自身携带 `Ty::Callable`：类型检查已在调用点完成实例化。
        let Some(Ty::Callable(callee_id, instantiated, declared_signature)) = body
            .expressions
            .iter()
            .find(|(id, _)| id == &callee)
            .map(|(_, ty)| ty)
        else {
            continue;
        };
        if context
            .model
            .foreign_definition_at(*callee_id)
            .is_some_and(|definition| definition.imported())
        {
            edges.externals.insert(import_name(context, *callee_id));
            continue;
        }
        let _ = ast_args;
        let declared = substitute(declared_signature, &entry.bindings);
        // 实例化实参取调用点 `Ty::Callable` 的实参；泛型体内以实例绑定替换。
        let mut arguments: Vec<Ty> = instantiated
            .iter()
            .map(|argument| substitute(argument, &entry.bindings))
            .collect();
        if arguments.iter().any(contains_param) {
            // 调用点携带占位参数（如参数包推断）：按声明签名与调用点实参类型统一。
            let actual: Vec<Ty> = ast_args
                .as_slice(&arena.expr_ids)
                .iter()
                .filter_map(|argument| {
                    body.expressions
                        .iter()
                        .find(|(id, _)| id == argument)
                        .map(|(_, ty)| substitute(ty, &entry.bindings))
                })
                .collect();
            let mut bindings = BTreeMap::new();
            if let Ty::Function(parameters, _) = &declared
                && parameters.len() == actual.len()
            {
                for (parameter, argument) in parameters.iter().zip(&actual) {
                    unify(parameter, argument, &mut bindings)?;
                }
            }
            arguments = callable_arguments(context, *callee_id, &bindings);
            if arguments.iter().any(contains_param) {
                // 实参不足以确定全部实例化参数的调用不在本闭合阶段展开。
                continue;
            }
        }
        let signature = context.model.normalize(&declared, &[])?;
        add_callee(context, interner, *callee_id, &arguments, &signature, edges)?;
    }
    Ok(())
}

/// 声明实参按绑定替换。
fn callable_arguments(
    context: &MonoContext<'_>,
    callee: CallableId,
    bindings: &BTreeMap<String, Ty>,
) -> Vec<Ty> {
    context
        .model
        .callable_context(callee)
        .into_values()
        .map(|ty| substitute(&ty, bindings))
        .collect()
}

/// 类型树中是否仍含泛型参数占位。
fn contains_param(ty: &Ty) -> bool {
    match ty {
        Ty::Param(_) => true,
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t)
        | Ty::MaybeUninit(t) => contains_param(t),
        Ty::Tuple(ts) => ts.iter().any(contains_param),
        Ty::Function(ts, ret) => ts.iter().any(contains_param) || contains_param(ret),
        Ty::Callable(_, args, sig) => args.iter().any(contains_param) || contains_param(sig),
        Ty::Named(_, args) | Ty::Opaque(_, args) => args.iter().any(contains_param),
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .any(|interface| interface.arguments.iter().any(contains_param)),
        Ty::Result(t, e) => contains_param(t) || contains_param(e),
        _ => false,
    }
}

/// 一个实例遍历期间的边与根累加器。
#[derive(Default)]
struct Edges {
    callees: BTreeMap<[u8; 32], CalleeSeed>,
    selected_impls: BTreeSet<[u8; 32]>,
    vtable_roots: BTreeMap<[u8; 32], VtableRootV1>,
    metadata_roots: BTreeSet<Vec<u8>>,
    externals: BTreeSet<String>,
    uses_late_comptime: bool,
}

fn leaf_record(context: &MonoContext<'_>, entry: &WalkEntry) -> InstanceRecordV1 {
    let definition = &context.module.definitions[entry.definition.index()];
    InstanceRecordV1 {
        schema: INSTANCE_SCHEMA,
        mono_key: entry.key.canonical_bytes(),
        definition: entry.definition.0,
        kind: entry.kind,
        symbol: definition.name.clone(),
        public: definition.public,
        callees: Vec::new(),
        selected_impls: Vec::new(),
        vtable_roots: Vec::new(),
        metadata_roots: Vec::new(),
        externals: Vec::new(),
        uses_late_comptime: false,
    }
}

fn kind_of(context: &MonoContext<'_>, definition: hir::DefId) -> MonoKind {
    match context.module.definitions[definition.index()].kind {
        hir::DefinitionKind::Closure => MonoKind::Closure,
        hir::DefinitionKind::Async => MonoKind::Async,
        hir::DefinitionKind::Static | hir::DefinitionKind::LocalStatic => MonoKind::StaticInit,
        hir::DefinitionKind::GlobalAsm => MonoKind::GlobalAsm,
        _ => MonoKind::Function,
    }
}

/// 单条 dispatch：动态派发登记 vtable 根；静态派发解析 callee 并实例化。
fn collect_dispatch(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
    module: usize,
    dispatch: &Dispatch,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let self_ty = substitute(&dispatch.self_ty, &entry.bindings);
    let signature = context
        .model
        .normalize(&substitute(&dispatch.signature, &entry.bindings), &[])?;
    if dispatch.dynamic {
        edges.metadata_roots.insert(context.encode_type(&self_ty)?);
        return Ok(());
    }
    let resolved = resolved_callee(context, entry, module, dispatch, &self_ty, &signature)?;
    if let Some(implementation) = resolved.implementation {
        edges
            .selected_impls
            .insert(context.item_key(implementation));
    }
    let Some(callee) = resolved.callable else {
        return Ok(());
    };
    if context
        .model
        .foreign_definition_at(callee)
        .is_some_and(|definition| definition.imported())
    {
        edges.externals.insert(import_name(context, callee));
        return Ok(());
    }
    let arguments = match resolved.arguments {
        Some(arguments) => arguments,
        None => unify_arguments(context, callee, &signature)?,
    };
    add_callee(context, interner, callee, &arguments, &signature, edges)?;
    Ok(())
}

/// 一条静态 dispatch 解析出的 callee 与实参。
struct ResolvedCallee {
    callable: Option<CallableId>,
    arguments: Option<Vec<Ty>>,
    implementation: Option<DefRef>,
}

/// 解析一条静态 dispatch 的 callee。
///
/// 检查期已固定 impl 的调用直接消费记录的 callable；泛型体内以绑定替换出的
/// 具体接收者重新执行闭世界 impl 选择（`selected_impls` 进实例记录）。
fn resolved_callee(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
    module: usize,
    dispatch: &Dispatch,
    self_ty: &Ty,
    _signature: &Ty,
) -> Result<ResolvedCallee, Diagnostic> {
    if dispatch.implementation.is_none() && dispatch.interface.is_some() {
        let interface = dispatch.interface.as_ref().expect("已判定存在接口");
        let substituted = interface.substitute(&entry.bindings);
        let member = dispatch
            .member
            .ok_or_else(|| inconsistency("trait 派发缺少成员下标"))?;
        let name = context
            .model
            .interface_member_name(interface.id, member)
            .ok_or_else(|| inconsistency("接口成员下标越界"))?;
        let selected = context
            .model
            .method(module, self_ty, Some(&substituted), &name, &[])?;
        if let Some(method) = selected {
            return Ok(ResolvedCallee {
                callable: method.callable,
                arguments: Some(method.arguments),
                implementation: method.implementation,
            });
        }
    }
    Ok(ResolvedCallee {
        callable: dispatch.callable,
        arguments: None,
        implementation: dispatch.implementation,
    })
}

/// 用声明签名作模式，从实际签名解出 callee 的泛型实参。
fn unify_arguments(
    context: &MonoContext<'_>,
    callee: CallableId,
    signature: &Ty,
) -> Result<Vec<Ty>, Diagnostic> {
    let definition = context
        .model
        .function_definition(callee)
        .ok_or_else(|| inconsistency("callee 没有函数声明"))?;
    let declared = context.model.value_type(definition)?;
    let Ty::Callable(_, arguments, declared_signature) = &declared else {
        return Err(inconsistency("callee 声明不是函数项"));
    };
    let mut bindings = BTreeMap::new();
    unify(declared_signature, signature, &mut bindings)?;
    Ok(arguments
        .iter()
        .map(|argument| substitute(argument, &bindings))
        .collect())
}

/// 闭包/协程捕获计划：以父实例绑定替换捕获签名与上下文参数。
fn collect_capture(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
    capture: &CapturePlan,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let Some(function) = capture.function else {
        return Ok(());
    };
    let signature = substitute(&capture.signature, &entry.bindings);
    let arguments = context
        .model
        .callable_context(function)
        .into_values()
        .map(|ty| substitute(&ty, &entry.bindings))
        .filter(|ty| !matches!(ty, Ty::Param(_)))
        .collect::<Vec<_>>();
    let key = callee_key(context, interner, function, &arguments, &signature)?;
    edges.callees.insert(
        key.digest(),
        CalleeSeed {
            key,
            callable: function,
            arguments,
            signature,
        },
    );
    Ok(())
}

/// 类型反射：具体类型进 metadata 根，`TypeAsInt`/`TypeIdCount` 标记 late 依赖。
fn collect_reflections(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
    reflections: &[Reflection],
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    for reflection in reflections {
        match &reflection.kind {
            ReflectionKind::Is(ty)
            | ReflectionKind::Downcast(ty)
            | ReflectionKind::DowncastCopy(ty)
            | ReflectionKind::TypeId(ty) => {
                let substituted = substitute(ty, &entry.bindings);
                edges
                    .metadata_roots
                    .insert(context.encode_type(&substituted)?);
            }
            ReflectionKind::TypeIdCount | ReflectionKind::TypeAsInt => {
                edges.uses_late_comptime = true;
            }
            ReflectionKind::TypeName => {}
        }
    }
    Ok(())
}

/// dyn 擦除调整：为具体源类型物化接口方法实例并登记 vtable 根。
fn collect_adjustment(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    module: usize,
    adjustment: &TypeAdjustment,
    entry: &WalkEntry,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    if adjustment.kind != AdjustmentKind::Erase {
        return Ok(());
    }
    let source = substitute(&adjustment.source, &entry.bindings);
    let target = substitute(&adjustment.target, &entry.bindings);
    let Ty::Dyn(interfaces) = &target else {
        return Ok(());
    };
    for interface in interfaces {
        collect_vtable(context, interner, module, &source, interface, edges)?;
    }
    Ok(())
}

/// vtable 物化：具体类型实现接口时，把每个方法的具体实例加入 callee 边。
fn collect_vtable(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    module: usize,
    source: &Ty,
    interface: &TraitRef,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let self_bytes = context.encode_type(source)?;
    let mut interface_bytes = Vec::new();
    context.encode_trait_ref_into(interface, &mut interface_bytes)?;
    let interface_key = hash_domain("gugu-mono-v1", &interface_bytes);
    edges.vtable_roots.insert(
        interface_key,
        VtableRootV1 {
            interface: interface_key,
            self_type: self_bytes.clone(),
        },
    );
    edges.metadata_roots.insert(interface_bytes);
    edges.metadata_roots.insert(self_bytes);
    for name in context.model.interface_method_members(interface.id) {
        let Some(method) = context
            .model
            .method(module, source, Some(interface), &name, &[])?
        else {
            continue;
        };
        add_method_callee(context, interner, &method, edges)?;
    }
    Ok(())
}

/// 把语义方法解析出的 callee 加入边集合。
fn add_method_callee(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    method: &Method,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    if let Some(implementation) = method.implementation {
        edges
            .selected_impls
            .insert(context.item_key(implementation));
    }
    let Some(callee) = method.callable else {
        return Ok(());
    };
    let signature = context.model.normalize(&method.signature, &[])?;
    add_callee(
        context,
        interner,
        callee,
        &method.arguments,
        &signature,
        edges,
    )
}

/// 由 (callee, 实参, 调用签名) 构造 callee 的 `MonoKey`。
///
/// 实参经规范化与规范编码进入 interner；签名参与解出绑定（见 `unify_arguments`）。
pub(crate) fn callee_key(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    callee: CallableId,
    arguments: &[Ty],
    signature: &Ty,
) -> Result<MonoKey, Diagnostic> {
    let _ = signature;
    let definition = context.identities.function(callee);
    let mut type_arguments = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let canonical = context.encode_type(argument)?;
        interner.intern_type(canonical.clone())?;
        type_arguments.push(canonical);
    }
    let key = MonoKey {
        definition: context.definition_key(definition),
        type_arguments,
        const_arguments: Vec::new(),
        selected_impls: Vec::new(),
        call_abi: MonoCallAbi::Gugu,
        target: context.target.to_string(),
        harness_mode: context.harness,
        instrumentation_mode: 0,
    };
    interner.intern_key(&key)?;
    Ok(key)
}

/// 把一个 callee 实例加入边集合（排序去重由 key 摘要承担）。
fn add_callee(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    callee: CallableId,
    arguments: &[Ty],
    signature: &Ty,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let key = callee_key(context, interner, callee, arguments, signature)?;
    edges.callees.insert(
        key.digest(),
        CalleeSeed {
            key,
            callable: callee,
            arguments: arguments.to_vec(),
            signature: signature.clone(),
        },
    );
    Ok(())
}

/// 外部导入符号名：优先 linkage 显式 `link_name`，否则声明名。
fn import_name(context: &MonoContext<'_>, callee: CallableId) -> String {
    let definition = context.identities.function(callee);
    for linkage in &context.checked.linkage {
        if linkage.definition.module == callee.module
            && context.identities.item(linkage.definition) == definition
            && let Some(name) = &linkage.import_name
        {
            return name.clone();
        }
    }
    let module = &context.model.modules[callee.module];
    match module.arena.fns.get(callee.function as usize) {
        Some(function) => function
            .name
            .map(|name| context.model.name(callee.module, name).to_owned())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// 实例输入指纹：替换后签名 + 全部边 + 元数据根（阶段 52 code fragment 输入）。
impl InstanceRecordV1 {
    pub(crate) fn fragment_input_fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.definition.to_le_bytes());
        for callee in &self.callees {
            bytes.extend_from_slice(&callee.key.digest());
        }
        for key in &self.selected_impls {
            bytes.extend_from_slice(key);
        }
        for root in &self.vtable_roots {
            bytes.extend_from_slice(&root.interface);
            bytes.extend_from_slice(&root.self_type);
        }
        for ty in &self.metadata_roots {
            bytes.extend_from_slice(ty);
        }
        bytes.push(u8::from(self.uses_late_comptime));
        hash_domain("gugu-mono-fragment-input-v1", &bytes)
    }
}

/// 沿 ancestry 的预算检查输入。
#[derive(Clone)]
pub(crate) struct AncestryLink {
    pub definition: [u8; 32],
    pub type_structure: u64,
}

/// 检查 ancestry 预算：链上不同 key 超过 256，或同一 definition 以严格增长的
/// 类型结构重复 128 次，均为无法收敛的递归单态化（`E0052`）。
pub(crate) fn check_ancestry(
    chain: &[AncestryLink],
    key: &MonoKey,
    arguments_size: u64,
) -> Result<(), Diagnostic> {
    if chain.len() >= MAX_ANCESTRY_KEYS {
        return Err(Diagnostic::error(
            crate::DiagnosticCode::MonoDivergence,
            "沿一条实例化 ancestry 出现超过 256 个不同 MonoKey，单态化无法收敛",
            None,
        ));
    }
    let mut occurrences: Vec<u64> = chain
        .iter()
        .filter(|link| link.definition == key.definition)
        .map(|link| link.type_structure)
        .collect();
    occurrences.push(arguments_size);
    let start = occurrences.len().saturating_sub(SAME_DEFINITION_LIMIT);
    let window = &occurrences[start..];
    if window.len() >= SAME_DEFINITION_LIMIT && window.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(Diagnostic::error(
            crate::DiagnosticCode::MonoDivergence,
            "同一 generic definition 以严格增长的类型结构重复超过 128 次，单态化无法收敛",
            None,
        ));
    }
    Ok(())
}

/// ancestry 上不同 MonoKey 的数量上限。
pub(crate) const MAX_ANCESTRY_KEYS: usize = 256;
/// 同一 definition 严格增长重复的次数上限。
pub(crate) const SAME_DEFINITION_LIMIT: usize = 128;

/// 一个实例泛型实参的类型结构总量（语义 `Ty` 树节点数，饱和）。
pub(crate) fn arguments_structure(arguments: &[Ty]) -> u64 {
    arguments
        .iter()
        .map(type_structure_size)
        .fold(0u64, u64::saturating_add)
}
