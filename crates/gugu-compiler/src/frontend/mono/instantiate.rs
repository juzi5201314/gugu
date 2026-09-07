//! `InstantiateGir`：绑定具体实例，按 HIR owner 收集调用、函数值、初始化器与 metadata 边。

use super::keys::{
    MonoContext, MonoInterner, MonoKey, MonoKind, hash_domain, type_structure_size, unify,
};
use crate::frontend::semantics::{TraitRef, Ty, substitute};
use crate::frontend::{ast, hir};
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::{Diagnostic, DiagnosticCode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const INSTANCE_SCHEMA: u32 = 2;

/// 调用位点使用 owner 内的确定性编号，不把定义级 callee 当作实例身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum CallSite {
    Expression(u32),
    Dispatch(u32),
    Initializer(u32),
}

/// Ty 与 DefId 只存在于当前输入快照的 query 工作载荷，不参与稳定编码。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CalleeSeed {
    pub key: MonoKey,
    pub definition: hir::DefId,
    pub arguments: Vec<Ty>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InstanceRecordV1 {
    pub schema: u32,
    pub mono_key: Vec<u8>,
    pub definition: u32,
    pub kind: MonoKind,
    pub symbol: String,
    pub public: bool,
    pub parameter_count: u32,
    pub signature_and_abi_fingerprint: [u8; 32],
    pub body_fingerprint: [u8; 32],
    pub callees: Vec<CalleeSeed>,
    pub call_targets: Vec<(CallSite, [u8; 32])>,
    pub selected_impls: Vec<[u8; 32]>,
    pub vtable_roots: Vec<VtableRootV1>,
    pub metadata_roots: Vec<Vec<u8>>,
    pub externals: Vec<String>,
    pub uses_late_comptime: bool,
}

/// 同一接口的每个具体接收者都需要独立 vtable。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct VtableRootV1 {
    pub interface: [u8; 32],
    pub self_type: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct WalkEntry {
    pub key: MonoKey,
    pub definition: hir::DefId,
    pub kind: MonoKind,
    pub body: Option<usize>,
    pub bindings: BTreeMap<String, Ty>,
}

pub(crate) fn walk_entry(
    context: &MonoContext<'_>,
    key: MonoKey,
    definition: hir::DefId,
    arguments: &[Ty],
) -> Result<WalkEntry, Diagnostic> {
    let declared = context.parameter_context(definition);
    if declared.len() != arguments.len() {
        return Err(inconsistency("实例实参数量与声明泛型不一致"));
    }
    Ok(WalkEntry {
        key,
        definition,
        kind: kind_of(context, definition),
        body: context.owner_of[definition.index()],
        bindings: declared
            .into_keys()
            .zip(arguments.iter().cloned())
            .collect(),
    })
}

pub(crate) fn root_entry(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    definition: hir::DefId,
    bindings: BTreeMap<String, Ty>,
) -> Result<WalkEntry, Diagnostic> {
    let arguments: Vec<_> = context
        .parameter_context(definition)
        .into_values()
        .map(|ty| substitute(&ty, &bindings))
        .collect();
    let key = instance_key(context, interner, definition, &arguments, Vec::new())?;
    walk_entry(context, key, definition, &arguments)
}

/// session cell 不会重新验证旧依赖，故输入快照与 MonoKey 共同限定一次实例化计算。
pub(crate) fn instantiate(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    queries: &QueryEngine,
    entry: &WalkEntry,
) -> Result<InstanceRecordV1, Vec<Diagnostic>> {
    let mut input = context.module.input_fingerprint.to_vec();
    input.extend_from_slice(&entry.key.canonical_bytes());
    let key = QueryKey::new(QueryKind::InstantiateGir, INSTANCE_SCHEMA, input);
    let mut fresh = None;
    let result = queries
        .compute(key, |query| {
            for source in context.sources.snapshots() {
                query.record_dependency(
                    QueryKey::new(QueryKind::SourceSnapshot, 1, source.logical_path()),
                    source.content_hash(),
                );
            }
            let record = walk_instance(context, interner, entry)
                .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
            let bytes = serde_json::to_vec(&record).expect("实例记录可序列化");
            fresh = Some(record);
            Ok((bytes, Vec::new()))
        })
        .map_err(|error| {
            crate::frontend::semantics::query::restore_errors(error, context.sources)
        })?;
    if let Some(record) = fresh {
        return Ok(record);
    }
    let record: InstanceRecordV1 = serde_json::from_slice(result.payload())
        .map_err(|_| vec![inconsistency("InstantiateGir 缓存 schema 不合法")])?;
    if record.schema != INSTANCE_SCHEMA
        || record.mono_key != entry.key.canonical_bytes()
        || record.definition != entry.definition.0
    {
        return Err(vec![inconsistency("InstantiateGir 缓存与实例身份不一致")]);
    }
    for seed in &record.callees {
        let rebuilt = instance_key(
            context,
            interner,
            seed.definition,
            &seed.arguments,
            seed.key.const_arguments.clone(),
        )
        .map_err(|error| vec![error])?;
        if rebuilt != seed.key {
            return Err(vec![inconsistency("缓存 callee 的具体类型与实例键不一致")]);
        }
    }
    for ty in &record.metadata_roots {
        interner
            .intern_type(ty.clone())
            .map_err(|error| vec![error])?;
    }
    Ok(record)
}

#[derive(Default)]
struct Edges {
    callees: BTreeMap<[u8; 32], CalleeSeed>,
    call_targets: BTreeMap<CallSite, [u8; 32]>,
    selected_impls: BTreeSet<[u8; 32]>,
    vtable_roots: BTreeSet<VtableRootV1>,
    metadata_roots: BTreeSet<Vec<u8>>,
    externals: BTreeSet<String>,
    uses_late_comptime: bool,
}

pub(crate) fn walk_instance(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
) -> Result<InstanceRecordV1, Diagnostic> {
    let mut edges = Edges::default();
    let mut parameter_count = 0;
    if let Some(body) = entry.body {
        let owner = &context.module.owners[body];
        parameter_count = u32::try_from(owner.parameters.len()).expect("HIR 参数数量不超过 u32");
        collect_owner(context, interner, entry, owner, &mut edges)?;
    }
    let definition = &context.module.definitions[entry.definition.index()];
    Ok(InstanceRecordV1 {
        schema: INSTANCE_SCHEMA,
        mono_key: entry.key.canonical_bytes(),
        definition: entry.definition.0,
        kind: entry.kind,
        symbol: definition.name.clone(),
        public: definition.public,
        parameter_count,
        signature_and_abi_fingerprint: hash_domain(
            "gugu-mono-signature-v1",
            &context.signature_bytes(entry.definition, &entry.bindings)?,
        ),
        body_fingerprint: context.body_fingerprint(entry.definition),
        callees: edges.callees.into_values().collect(),
        call_targets: edges.call_targets.into_iter().collect(),
        selected_impls: edges.selected_impls.into_iter().collect(),
        vtable_roots: edges.vtable_roots.into_iter().collect(),
        metadata_roots: edges.metadata_roots.into_iter().collect(),
        externals: edges.externals.into_iter().collect(),
        uses_late_comptime: edges.uses_late_comptime,
    })
}

fn collect_owner(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
    owner: &hir::Owner,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let module = context.definition_module[entry.definition.index()];
    let dispatches = owner
        .dispatches
        .iter()
        .map(|dispatch| resolve_dispatch(context, entry, module, dispatch, edges))
        .collect::<Result<Vec<_>, _>>()?;
    for (index, resolved) in dispatches.iter().enumerate() {
        if let Some(resolved) = resolved
            && !has_comptime_parameters(context, resolved.definition)
        {
            let site = CallSite::Dispatch(u32::try_from(index).expect("dispatch 编号不超过 u32"));
            add_callee(
                context,
                interner,
                resolved.definition,
                &resolved.arguments,
                Vec::new(),
                Some(site),
                edges,
            )?;
        }
    }
    let mut direct_values = BTreeSet::new();
    for (index, expression) in owner.expressions.iter().enumerate() {
        let id = hir::ExprId(u32::try_from(index).expect("HIR 表达式编号不超过 u32"));
        if let hir::ExprKind::Call {
            target,
            receiver,
            arguments,
        }
        | hir::ExprKind::SpawnCall {
            target,
            receiver,
            arguments,
        } = &expression.kind
        {
            collect_call(
                context,
                interner,
                entry,
                owner,
                id,
                target,
                *receiver,
                arguments,
                &dispatches,
                &mut direct_values,
                edges,
            )?;
        }
    }
    for index in 0..owner.expressions.len() {
        let id = hir::ExprId(u32::try_from(index).expect("HIR 表达式编号不超过 u32"));
        if !direct_values.contains(&id) {
            let ty = callable_type(context, entry, owner, id)?;
            if let Ty::Callable(callable, arguments, _) = ty {
                let definition = context.identities.function(callable);
                if has_comptime_parameters(context, definition) {
                    return Err(inconsistency("函数值缺少 comptime 实参，不能形成具体实例"));
                }
                let constants = if kind_of(context, definition) == MonoKind::Closure {
                    entry.key.const_arguments.clone()
                } else {
                    Vec::new()
                };
                add_callee(
                    context, interner, definition, &arguments, constants, None, edges,
                )?;
            }
        }
        collect_expression(context, interner, entry, owner, id, edges)?;
    }
    for (index, statement) in owner.statements.iter().enumerate() {
        if let hir::StatementKind::Static { definition, .. } = statement.kind {
            let arguments = nested_arguments(context, definition, &entry.bindings);
            add_callee(
                context,
                interner,
                definition,
                &arguments,
                entry.key.const_arguments.clone(),
                Some(CallSite::Initializer(
                    u32::try_from(index).expect("StmtId 不超过 u32"),
                )),
                edges,
            )?;
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "调用位点同时绑定 HIR 操作数、已选择派发与闭合工作集"
)]
fn collect_call(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
    owner: &hir::Owner,
    id: hir::ExprId,
    target: &hir::CallTarget,
    receiver: Option<hir::ExprId>,
    arguments: &std::ops::Range<u32>,
    dispatches: &[Option<ResolvedCallee>],
    direct_values: &mut BTreeSet<hir::ExprId>,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let value;
    let resolved = match target {
        hir::CallTarget::Dispatch(index) => {
            dispatches[usize::try_from(*index).expect("dispatch 编号适配宿主")].as_ref()
        }
        hir::CallTarget::Value(callee) => {
            direct_values.insert(*callee);
            value = match callable_type(context, entry, owner, *callee)? {
                Ty::Callable(callable, arguments, signature) => Some(ResolvedCallee {
                    definition: context.identities.function(callable),
                    arguments,
                    signature: *signature,
                }),
                _ => None,
            };
            value.as_ref()
        }
        hir::CallTarget::Builtin(_) | hir::CallTarget::Constructor { .. } => None,
    };
    let Some(resolved) = resolved else {
        return Ok(());
    };
    let values = &owner.expression_ids[usize::try_from(arguments.start).expect("HIR range 适配宿主")
        ..usize::try_from(arguments.end).expect("HIR range 适配宿主")];
    let constants = constant_arguments(context, entry, owner, resolved, receiver, values)?;
    add_callee(
        context,
        interner,
        resolved.definition,
        &resolved.arguments,
        constants,
        Some(CallSite::Expression(id.0)),
        edges,
    )?;
    Ok(())
}

fn callable_type(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
    owner: &hir::Owner,
    id: hir::ExprId,
) -> Result<Ty, Diagnostic> {
    let expression = &owner.expressions[id.index()];
    let mut ty = owner.expression_inputs[id.index()];
    for adjustment in &owner.adjustments[usize::try_from(expression.adjustments.start)
        .expect("HIR range 适配宿主")
        ..usize::try_from(expression.adjustments.end).expect("HIR range 适配宿主")]
    {
        if let hir::Adjustment::Instantiate(instantiated) = adjustment {
            ty = *instantiated;
        }
    }
    context.type_at(ty, &entry.bindings)
}

struct ResolvedCallee {
    definition: hir::DefId,
    arguments: Vec<Ty>,
    signature: Ty,
}

fn resolve_dispatch(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
    module: usize,
    dispatch: &hir::Dispatch,
    edges: &mut Edges,
) -> Result<Option<ResolvedCallee>, Diagnostic> {
    let self_ty = context.type_at(dispatch.self_ty, &entry.bindings)?;
    let self_ty = context
        .model
        .hidden_type(&self_ty, &context.checked.hidden_types)?;
    let signature = context.type_at(dispatch.signature, &entry.bindings)?;
    if dispatch.dynamic {
        edges.metadata_roots.insert(context.encode_type(&self_ty)?);
        return Ok(None);
    }
    if let Some(interface) = &dispatch.interface {
        let interface = context
            .semantic_trait(interface)?
            .substitute(&entry.bindings);
        let member = dispatch
            .member
            .ok_or_else(|| inconsistency("接口派发缺少成员"))?;
        let name = context
            .model
            .interface_member_name(interface.id, member)
            .ok_or_else(|| inconsistency("接口派发成员不存在"))?;
        let method = context
            .model
            .method(module, &self_ty, Some(&interface), &name, &[])?
            .ok_or_else(|| inconsistency("具体接收者没有已检查接口的实现"))?;
        if let Some(implementation) = method.implementation {
            edges
                .selected_impls
                .insert(context.item_key(implementation));
        }
        let Some(callable) = method.callable else {
            return Ok(None);
        };
        // 接收者与 impl 实参已由具体类型选择。只对方法自身的泛型做签名绑定；
        // 不把调用者同名的类型参数误当作被调用者的声明参数。
        let arguments = if method.arguments.iter().any(contains_param) {
            let mut bindings = BTreeMap::new();
            unify(&method.signature, &signature, &mut bindings)?;
            method
                .arguments
                .iter()
                .map(|ty| context.model.normalize(&substitute(ty, &bindings), &[]))
                .collect::<Result<_, _>>()?
        } else {
            method.arguments
        };
        return Ok(Some(ResolvedCallee {
            definition: context.identities.function(callable),
            arguments,
            signature,
        }));
    }
    let Some(definition) = dispatch.function else {
        return Ok(None);
    };
    if let Some(implementation) = dispatch.implementation {
        edges
            .selected_impls
            .insert(context.definition_key(implementation));
    }
    let declared = context.module.definitions[definition.index()]
        .signature
        .ok_or_else(|| inconsistency("派发目标没有声明签名"))?;
    let declared = context.semantic_type(declared)?;
    let Ty::Callable(_, arguments, declared_signature) = declared else {
        return Err(inconsistency("派发目标不是函数项"));
    };
    let mut bindings = BTreeMap::new();
    unify(declared_signature, &signature, &mut bindings)?;
    Ok(Some(ResolvedCallee {
        definition,
        arguments: arguments
            .iter()
            .map(|ty| context.model.normalize(&substitute(ty, &bindings), &[]))
            .collect::<Result<_, _>>()?,
        signature,
    }))
}

fn collect_expression(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    entry: &WalkEntry,
    owner: &hir::Owner,
    id: hir::ExprId,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let expression = &owner.expressions[id.index()];
    match &expression.kind {
        hir::ExprKind::Spawn { definition } => {
            let arguments = nested_arguments(context, *definition, &entry.bindings);
            add_callee(
                context,
                interner,
                *definition,
                &arguments,
                entry.key.const_arguments.clone(),
                None,
                edges,
            )?;
        }
        hir::ExprKind::Resolved(hir::Res::Def(definition))
        | hir::ExprKind::Resolved(hir::Res::Associated { definition, .. })
            if matches!(
                context.module.definitions[definition.index()].kind,
                hir::DefinitionKind::Constant | hir::DefinitionKind::Static
            ) =>
        {
            add_callee(context, interner, *definition, &[], Vec::new(), None, edges)?;
        }
        hir::ExprKind::Intrinsic {
            operation, types, ..
        } => {
            if matches!(
                operation,
                hir::Builtin::TypeIdCount | hir::Builtin::TypeAsInt
            ) {
                edges.uses_late_comptime = true;
            }
            if matches!(
                operation,
                hir::Builtin::TypeId
                    | hir::Builtin::Is
                    | hir::Builtin::Downcast
                    | hir::Builtin::DowncastCopy
            ) {
                for ty in types {
                    let bytes = context.encode_type(&context.type_at(*ty, &entry.bindings)?)?;
                    interner.intern_type(bytes.clone())?;
                    edges.metadata_roots.insert(bytes);
                }
            }
        }
        _ => {}
    }
    let range = usize::try_from(expression.adjustments.start).expect("HIR range 适配宿主")
        ..usize::try_from(expression.adjustments.end).expect("HIR range 适配宿主");
    for adjustment in &owner.adjustments[range] {
        let hir::Adjustment::Erase(target) = adjustment else {
            continue;
        };
        let Ty::Dyn(interfaces) = context.type_at(*target, &entry.bindings)? else {
            continue;
        };
        let source = callable_type(context, entry, owner, id)?;
        for interface in interfaces {
            collect_vtable(
                context,
                interner,
                context.definition_module[entry.definition.index()],
                &source,
                &interface,
                edges,
            )?;
        }
    }
    Ok(())
}

fn collect_vtable(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    module: usize,
    source: &Ty,
    interface: &TraitRef,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    let self_type = context.encode_type(source)?;
    interner.intern_type(self_type.clone())?;
    let mut interface_bytes = Vec::new();
    context.encode_trait_ref_into(interface, &mut interface_bytes)?;
    let interface_key = hash_domain("gugu-mono-v1", &interface_bytes);
    edges.vtable_roots.insert(VtableRootV1 {
        interface: interface_key,
        self_type: self_type.clone(),
    });
    edges.metadata_roots.insert(self_type);
    let dyn_type = context.encode_type(&Ty::Dyn(vec![interface.clone()]))?;
    interner.intern_type(dyn_type.clone())?;
    edges.metadata_roots.insert(dyn_type);
    for name in context.model.interface_method_members(interface.id) {
        let method = context
            .model
            .method(module, source, Some(interface), &name, &[])?
            .ok_or_else(|| inconsistency("vtable 槽没有具体实现"))?;
        if let Some(implementation) = method.implementation {
            edges
                .selected_impls
                .insert(context.item_key(implementation));
        }
        if let Some(callable) = method.callable {
            add_callee(
                context,
                interner,
                context.identities.function(callable),
                &method.arguments,
                Vec::new(),
                None,
                edges,
            )?;
        }
    }
    Ok(())
}

fn nested_arguments(
    context: &MonoContext<'_>,
    definition: hir::DefId,
    bindings: &BTreeMap<String, Ty>,
) -> Vec<Ty> {
    context
        .parameter_context(definition)
        .into_values()
        .map(|ty| substitute(&ty, bindings))
        .collect()
}

fn instance_key(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    definition: hir::DefId,
    arguments: &[Ty],
    const_arguments: Vec<Vec<u8>>,
) -> Result<MonoKey, Diagnostic> {
    let declared = context.parameter_context(definition);
    if declared.len() != arguments.len() {
        return Err(inconsistency("实例实参数量与声明泛型不一致"));
    }
    let mut type_arguments = Vec::with_capacity(arguments.len());
    let mut bindings = BTreeMap::new();
    for (name, argument) in declared.into_keys().zip(arguments) {
        let argument = context.model.normalize(argument, &[])?;
        type_arguments.push(interner.intern_type(context.encode_type(&argument)?)?);
        bindings.insert(name, argument);
    }
    let key = MonoKey {
        definition: context.definition_key(definition),
        type_arguments,
        const_arguments,
        selected_impls: context.selected_impls(definition, &bindings)?,
        call_abi: context.call_abi(definition),
        target: context.target.to_string(),
        harness_mode: context.harness,
        instrumentation_mode: 0,
    };
    interner.intern_key(&key)?;
    Ok(key)
}

fn add_callee(
    context: &MonoContext<'_>,
    interner: &mut MonoInterner,
    definition: hir::DefId,
    arguments: &[Ty],
    constants: Vec<Vec<u8>>,
    site: Option<CallSite>,
    edges: &mut Edges,
) -> Result<(), Diagnostic> {
    if let Some(callable) = context.callable_of[definition.index()]
        && context
            .model
            .foreign_definition_at(callable)
            .is_some_and(|declaration| declaration.imported())
    {
        let name = context
            .module
            .linkage
            .iter()
            .find(|linkage| linkage.definition == definition)
            .and_then(|linkage| linkage.import_name.clone())
            .unwrap_or_else(|| context.module.definitions[definition.index()].name.clone());
        edges.externals.insert(name);
        return Ok(());
    }
    let arguments = arguments
        .iter()
        .map(|ty| context.model.normalize(ty, &[]))
        .collect::<Result<Vec<_>, _>>()?;
    let key = instance_key(context, interner, definition, &arguments, constants)?;
    let digest = key.digest();
    if let Some(site) = site {
        edges.call_targets.insert(site, digest);
    }
    edges.callees.entry(digest).or_insert(CalleeSeed {
        key,
        definition,
        arguments,
    });
    Ok(())
}

fn contains_param(ty: &Ty) -> bool {
    match ty {
        Ty::Param(_) | Ty::Var(_) => true,
        Ty::Ref(inner)
        | Ty::Ptr(inner)
        | Ty::Slice(inner)
        | Ty::Array(inner, _)
        | Ty::Option(inner)
        | Ty::Chan(inner)
        | Ty::Join(inner)
        | Ty::MaybeUninit(inner) => contains_param(inner),
        Ty::Tuple(types) => types.iter().any(contains_param),
        Ty::Function(types, result) => types.iter().any(contains_param) || contains_param(result),
        Ty::Callable(_, arguments, signature) => {
            arguments.iter().any(contains_param) || contains_param(signature)
        }
        Ty::Named(_, arguments) | Ty::Opaque(_, arguments) => arguments.iter().any(contains_param),
        Ty::Projection(self_ty, interface, _) => {
            contains_param(self_ty) || interface.arguments.iter().any(contains_param)
        }
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .any(|interface| interface.arguments.iter().any(contains_param)),
        Ty::Result(value, error) => contains_param(value) || contains_param(error),
        _ => false,
    }
}

fn has_comptime_parameters(context: &MonoContext<'_>, definition: hir::DefId) -> bool {
    let Some(callable) = context.callable_of[definition.index()] else {
        return false;
    };
    let parsed = &context.model.modules[callable.module];
    let function = &parsed.arena.fns[usize::try_from(callable.function).expect("FnId 适配宿主")];
    function
        .params
        .as_slice(&parsed.arena.params)
        .iter()
        .enumerate()
        .any(|(index, parameter)| {
            parameter.comptime
                && parsed.configured.param_active(
                    usize::try_from(function.params.start).expect("参数 range 适配宿主") + index,
                )
        })
}

fn constant_arguments(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
    owner: &hir::Owner,
    callee: &ResolvedCallee,
    receiver: Option<hir::ExprId>,
    arguments: &[hir::ExprId],
) -> Result<Vec<Vec<u8>>, Diagnostic> {
    if !has_comptime_parameters(context, callee.definition) {
        return Ok(
            if kind_of(context, callee.definition) == MonoKind::Closure {
                entry.key.const_arguments.clone()
            } else {
                Vec::new()
            },
        );
    }
    let Some(callable) = context.callable_of[callee.definition.index()] else {
        return Ok(Vec::new());
    };
    let parsed = &context.model.modules[callable.module];
    let function = &parsed.arena.fns[usize::try_from(callable.function).expect("FnId 适配宿主")];
    let parameters = function.params.as_slice(&parsed.arena.params);
    let supplied: Vec<_> = receiver
        .into_iter()
        .chain(arguments.iter().copied())
        .collect();
    let signature = callee
        .signature
        .signature()
        .ok_or_else(|| inconsistency("callee 签名不是函数"))?
        .0;
    let module = context.definition_module[entry.definition.index()];
    let mut constants = Vec::new();
    for (position, (_, parameter)) in parameters
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            parsed.configured.param_active(
                usize::try_from(function.params.start).expect("参数 range 适配宿主") + index,
            )
        })
        .enumerate()
    {
        if !parameter.comptime {
            continue;
        }
        let argument = supplied
            .get(position)
            .ok_or_else(|| inconsistency("缺少已经检查的 comptime 实参"))?;
        let location = &owner.expressions[argument.index()].location;
        let arena = &context.model.modules[module].arena;
        let index = arena
            .exprs
            .iter()
            .position(|expression| {
                expression.span.start() == location.start
                    && expression.span.end() == location.end
                    && expression.span.expansion().as_u32() == location.expansion
            })
            .ok_or_else(|| inconsistency("comptime 实参缺少已检查的源码表达式"))?;
        let ty = signature
            .get(position)
            .ok_or_else(|| inconsistency("comptime 实参缺少签名类型"))?;
        let value = context.model.eval_early_const(
            module,
            ast::ExprId(u32::try_from(index).expect("AST 编号不超过 u32")),
            ty,
        )?;
        constants.push(super::types::encode_constant(&value)?);
    }
    Ok(constants)
}

fn kind_of(context: &MonoContext<'_>, definition: hir::DefId) -> MonoKind {
    match context.module.definitions[definition.index()].kind {
        hir::DefinitionKind::Closure => MonoKind::Closure,
        hir::DefinitionKind::Async => MonoKind::Async,
        hir::DefinitionKind::Constant
        | hir::DefinitionKind::Static
        | hir::DefinitionKind::LocalStatic => MonoKind::StaticInit,
        hir::DefinitionKind::GlobalAsm => MonoKind::GlobalAsm,
        _ => MonoKind::Function,
    }
}

impl InstanceRecordV1 {
    pub(crate) fn fragment_input_fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.mono_key);
        bytes.extend_from_slice(&self.signature_and_abi_fingerprint);
        bytes.extend_from_slice(&self.body_fingerprint);
        bytes.extend_from_slice(
            &u64::try_from(self.callees.len())
                .expect("callee 数量适配 GBC1")
                .to_le_bytes(),
        );
        for callee in &self.callees {
            bytes.extend_from_slice(&callee.key.digest());
        }
        let dependencies = serde_json::to_vec(&(
            &self.call_targets,
            &self.selected_impls,
            &self.vtable_roots,
            &self.metadata_roots,
            &self.externals,
            self.uses_late_comptime,
        ))
        .expect("稳定实例依赖可序列化");
        bytes.extend_from_slice(&dependencies);
        hash_domain("gugu-mono-fragment-input-v2", &bytes)
    }
}

fn inconsistency(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::MonoDivergence, message, None)
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
