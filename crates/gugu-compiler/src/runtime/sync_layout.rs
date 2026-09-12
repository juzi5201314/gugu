//! 从冻结 HIR 与具体 GIR 交叉校验同步控制块布局；不重新解释源码或名称解析。

use super::model::RawModelError;
use super::sync_schema::SyncRuntimeContract;
use crate::frontend::{
    gir::{
        GirWorldV1,
        concrete::{TypeKind, TypeLayout},
    },
    hir,
};
use std::collections::BTreeSet;

const SOURCE: &str = "std/runtime/sync.gg";

pub(super) fn verify_source(
    contract: &SyncRuntimeContract,
    module: &hir::Module,
    gir: &GirWorldV1,
) -> Result<(), RawModelError> {
    if module.sources.is_empty() {
        debug_assert!(gir.concrete.is_empty());
        return Ok(());
    }
    let mut found = vec![false; contract.records.len()];
    for concrete in &gir.concrete {
        let definition = &module.definitions[concrete.body.owner.index()];
        if !in_source(definition, module) {
            continue;
        }
        let parameters = &gir.bodies
            [usize::try_from(concrete.generic_body).expect("generic body下标")]
        .signature
        .parameters;
        if parameters.len() != concrete.body.signature.parameters.len() {
            return Err(RawModelError::new("同步入口具体签名与冻结HIR不一致"));
        }
        let mut verifier = Verifier {
            contract,
            module,
            types: &concrete.types,
            found: &mut found,
            visited: BTreeSet::new(),
        };
        for (source, target) in parameters.iter().zip(&concrete.body.signature.parameters) {
            verifier.walk(*source, target.0, false)?;
        }
    }
    if found.iter().any(|found| !found) {
        let missing: Vec<_> = contract
            .records
            .iter()
            .zip(&found)
            .filter_map(|(record, found)| (!found).then_some(record.name.as_str()))
            .collect();
        return Err(RawModelError::new(format!(
            "内建Gugu同步源缺少具体布局：{}",
            missing.join(", ")
        )));
    }
    Ok(())
}

fn in_source(definition: &hir::Definition, module: &hir::Module) -> bool {
    definition.location.as_ref().is_some_and(|location| {
        module.sources[usize::try_from(location.source).expect("source下标")].path == SOURCE
    })
}

struct Verifier<'a> {
    contract: &'a SyncRuntimeContract,
    module: &'a hir::Module,
    types: &'a [TypeLayout],
    found: &'a mut [bool],
    visited: BTreeSet<(hir::TypeId, u32)>,
}

impl Verifier<'_> {
    fn walk(&mut self, source: hir::TypeId, target: u32, field: bool) -> Result<(), RawModelError> {
        if !self.visited.insert((source, target)) {
            return Ok(());
        }
        let ty = &self.types[usize::try_from(target).expect("具体类型下标")];
        match (&self.module.types[source.index()], &ty.kind) {
            (hir::Type::Ref(inner), TypeKind::Reference(target)) if !field => {
                self.walk(*inner, *target, false)
            }
            (hir::Type::Ptr(inner), TypeKind::Pointer(target)) => self.walk(*inner, *target, false),
            (hir::Type::Array(inner, _), TypeKind::Array { element, .. }) => {
                self.walk(*inner, *element, field)
            }
            (
                hir::Type::Named { definition, .. },
                TypeKind::Aggregate {
                    tag_bytes: 0,
                    variants,
                },
            ) => {
                let definition_value = &self.module.definitions[definition.index()];
                if !in_source(definition_value, self.module) {
                    return Err(RawModelError::new("控制块包含未登记外部record"));
                }
                let aggregate = self
                    .module
                    .aggregates
                    .iter()
                    .find(|aggregate| aggregate.definition == *definition)
                    .ok_or_else(|| RawModelError::new("控制块缺少aggregate"))?;
                let fields = &aggregate.variants[0].fields;
                if variants.len() != 1 || fields.len() != variants[0].len() {
                    return Err(RawModelError::new("同步 record 字段集合不匹配"));
                }
                if let Some(index) = self
                    .contract
                    .records
                    .iter()
                    .position(|record| record.name == definition_value.name)
                {
                    let expected = &self.contract.records[index];
                    let layout = ty
                        .layout
                        .ok_or_else(|| RawModelError::new("同步 record 布局未形成"))?;
                    if layout.size != u64::from(expected.bytes)
                        || layout.align != u64::from(expected.alignment)
                    {
                        return Err(RawModelError::new(format!(
                            "{} 的Gugu布局为size={} align={}，machine要求size={} align={}",
                            expected.name,
                            layout.size,
                            layout.align,
                            expected.bytes,
                            expected.alignment
                        )));
                    }
                    for expected_field in &expected.fields {
                        let index = fields
                            .iter()
                            .position(|field| field.name == expected_field.name)
                            .ok_or_else(|| RawModelError::new("同步 record 缺少已登记字段"))?;
                        if variants[0][index].offset != u64::from(expected_field.offset) {
                            return Err(RawModelError::new(format!(
                                "{}.{} 的Gugu与machine字段偏移不一致",
                                expected.name, expected_field.name
                            )));
                        }
                    }
                    self.found[index] = true;
                }
                for (field, target) in fields.iter().zip(&variants[0]) {
                    self.walk(field.ty, target.ty, true)?;
                }
                Ok(())
            }
            (hir::Type::Int { .. } | hir::Type::Unit, _) => Ok(()),
            _ => Err(RawModelError::new(
                "同步 raw record 含未登记的 managed 字段或表示",
            )),
        }
    }
}
