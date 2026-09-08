use super::{
    LateKey, LateResult, LateTable,
    eval::Evaluator,
    graph::Program,
    universe::{TypeUniverse, VtableReference, invalid},
};
use crate::frontend::semantics::{
    comptime::registry,
    query::{restore_errors, store_errors},
};
use crate::frontend::{
    hir,
    mono::{MonoWorldV1, digest_of, hash_domain},
};
use crate::query::{QueryKey, QueryKind};
use crate::{Diagnostic, QueryEngine, SourceMap};
use std::collections::BTreeMap;

pub(super) fn run(
    module: &hir::Module,
    world: &mut MonoWorldV1,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<(), Vec<Diagnostic>> {
    let freeze_key = QueryKey::new(QueryKind::FreezeTypeUniverse, 1, world.graph_fingerprint);
    let frozen = queries
        .compute(freeze_key.clone(), |query| {
            query.record_dependency(
                QueryKey::new(
                    QueryKind::CollectMonoRoots,
                    crate::frontend::mono::roots::ROOTS_SCHEMA,
                    world.input_fingerprint,
                ),
                world.graph_fingerprint,
            );
            let universe = freeze(world).map_err(|error| store_errors(&[error]))?;
            Ok((
                serde_json::to_vec(&universe).expect("类型表可序列化"),
                Vec::new(),
            ))
        })
        .map_err(|error| restore_errors(error, sources))?;
    world.universe = serde_json::from_slice(frozen.payload())
        .map_err(|_| vec![invalid("类型冻结缓存 schema 不合法")])?;
    world.universe.verify().map_err(|e| vec![e])?;
    let program = Program::new(module, world);
    let mut requests = BTreeMap::new();
    for (instance, summary) in world.instances.iter().enumerate() {
        let Ok(owner) = program.owner(instance) else {
            continue;
        };
        let definition = &module.definitions[owner.definition.index()];
        for (index, expression) in owner.expressions.iter().enumerate() {
            let id = hir::ExprId(index as u32);
            let mandatory = matches!(
                expression.kind,
                hir::ExprKind::Intrinsic {
                    operation: hir::Builtin::TypeId | hir::Builtin::TypeIdCount,
                    ..
                }
            );
            let requested = matches!(expression.kind, hir::ExprKind::Comptime { .. })
                || (id == owner.body
                    && matches!(
                        definition.kind,
                        hir::DefinitionKind::Constant
                            | hir::DefinitionKind::Static
                            | hir::DefinitionKind::LocalStatic
                    ));
            if !mandatory && !(requested && program.late[instance][index]) {
                continue;
            }
            let key = LateKey {
                instance: digest_of(&summary.mono_key),
                expression: id.0,
                ty: program
                    .ty(instance, owner.expression_types[index])
                    .map_err(|e| vec![e])?,
                closure: program.closure(instance, id).map_err(|e| vec![e])?,
            };
            requests.insert(key, (instance, id));
        }
    }
    let mut results = Vec::with_capacity(requests.len());
    for (key, (instance, id)) in requests {
        let input = serde_json::to_vec(&(&key, world.universe.fingerprint, registry::summary()))
            .expect("late query 身份可序列化");
        let query_key = QueryKey::new(QueryKind::EvaluateLateComptime, 1, input);
        let outcome = queries
            .compute(query_key, |query| {
                query.record_dependency(freeze_key.clone(), frozen.fingerprint());
                let value = Evaluator::new(&program)
                    .evaluate(instance, id)
                    .and_then(|value| {
                        super::validate_value(&value, &key.ty, &world.universe)?;
                        Ok(value)
                    })
                    .map_err(|error| {
                        let location =
                            &program.owner(instance).expect("已选择的 owner").expressions
                                [id.index()]
                            .location;
                        let span = sources
                            .file_id(&module.sources[location.source as usize].path)
                            .and_then(|file| {
                                sources
                                    .span(
                                        file,
                                        location.start as usize,
                                        location.end as usize,
                                        crate::ExpansionId::new(location.expansion),
                                    )
                                    .ok()
                            });
                        store_errors(&[Diagnostic::error(error.code(), error.message(), span)])
                    })?;
                Ok((
                    serde_json::to_vec(&value).expect("late 值可序列化"),
                    Vec::new(),
                ))
            })
            .map_err(|error| restore_errors(error, sources))?;
        let value = serde_json::from_slice(outcome.payload())
            .map_err(|_| vec![invalid("late 缓存 schema 不合法")])?;
        results.push(LateResult { key, value });
    }
    let mut table = LateTable {
        universe: world.universe.fingerprint,
        results,
        fingerprint: [0; 32],
    };
    table.fingerprint = table.fingerprint();
    table.verify(world).map_err(|error| vec![error])?;
    world.late = table;
    Ok(())
}

fn freeze(world: &MonoWorldV1) -> Result<TypeUniverse, Diagnostic> {
    let mut types = BTreeMap::new();
    for record in world.instances.iter().flat_map(|i| &i.types) {
        if let Some(old) = types.insert(record.key, record.clone()) {
            if old != *record {
                return Err(invalid("相同 StableTypeKey 的类型记录不一致"));
            }
        }
    }
    let mut universe = TypeUniverse {
        records: types.into_values().collect(),
        vtables: Vec::new(),
        fingerprint: [0; 32],
    };
    for root in world.instances.iter().flat_map(|i| &i.vtable_roots) {
        let key = hash_domain("gugu-mono-v1", &root.self_type);
        let concrete = universe
            .type_id(&key)
            .ok_or_else(|| invalid("vtable payload 类型没有进入冻结集合"))?;
        universe.vtables.push(VtableReference {
            interface: root.interface,
            concrete,
        });
    }
    universe.vtables.sort_unstable();
    universe.vtables.dedup();
    universe.fingerprint = universe.fingerprint();
    universe.verify()?;
    Ok(universe)
}
