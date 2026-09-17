//! 从冻结 HIR 与具体 GIR 交叉校验 SharedHeap handle slot 与 payload record 布局；
//! 不重新解释源码或名称解析。

use super::model::RawModelError;
use super::shared_heap_schema::SharedHeapRuntimeContract;
use crate::frontend::{
    gir::{GirWorldV1, concrete::TypeKind, concrete::TypeLayout},
    hir,
};
use std::collections::BTreeSet;

const SOURCE: &str = "std/runtime/heap.gg";

pub(super) fn verify_source(
    contract: &SharedHeapRuntimeContract,
    module: &hir::Module,
    gir: &GirWorldV1,
) -> Result<(), RawModelError> {
    // 空 package 没有 runtime 源图，也不生成镜像；固定 machine 布局仍由 contract.verify 校验。
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
            [usize::try_from(concrete.generic_body).expect("generic body 下标")]
        .signature
        .parameters;
        if parameters.len() != concrete.body.signature.parameters.len() {
            return Err(RawModelError::new(
                "SharedHeap 入口具体签名与冻结 HIR 不一致",
            ));
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
            "内建 Gugu SharedHeap 源缺少具体布局：{}",
            missing.join(", ")
        )));
    }
    Ok(())
}

fn in_source(definition: &hir::Definition, module: &hir::Module) -> bool {
    definition.location.as_ref().is_some_and(|location| {
        module.sources[usize::try_from(location.source).expect("source 下标")].path == SOURCE
    })
}

struct Verifier<'a> {
    contract: &'a SharedHeapRuntimeContract,
    module: &'a hir::Module,
    types: &'a [TypeLayout],
    found: &'a mut [bool],
    visited: BTreeSet<(hir::TypeId, u32)>,
}

impl Verifier<'_> {
    /// 沿冻结 HIR 与具体 GIR 走一层：引用解引用、数组/指针穿透、aggregate 展开。
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
            ) => self.walk_aggregate(*definition, ty, variants),
            (hir::Type::Int { .. } | hir::Type::Unit, _) => Ok(()),
            _ => Err(RawModelError::new(
                "SharedHeap record 含未登记的 managed 字段或表示",
            )),
        }
    }

    /// 展开一个无 tag 的 record：字段集合、size/align 与逐字段 offset 都要与 machine 一致。
    fn walk_aggregate(
        &mut self,
        definition: hir::DefId,
        ty: &TypeLayout,
        variants: &[Vec<crate::frontend::gir::concrete::Field>],
    ) -> Result<(), RawModelError> {
        let definition_value = &self.module.definitions[definition.index()];
        if !in_source(definition_value, self.module) {
            return Err(RawModelError::new(
                "SharedHeap record 包含未登记外部 record",
            ));
        }
        let aggregate = self
            .module
            .aggregates
            .iter()
            .find(|aggregate| aggregate.definition == definition)
            .ok_or_else(|| RawModelError::new("SharedHeap record 缺少 aggregate"))?;
        let fields = &aggregate.variants[0].fields;
        if variants.len() != 1 || fields.len() != variants[0].len() {
            return Err(RawModelError::new("SharedHeap record 字段集合不匹配"));
        }
        if let Some(index) = self
            .contract
            .records
            .iter()
            .position(|record| record.name == definition_value.name)
        {
            self.verify_record(index, ty, fields, variants)?;
        }
        for (field, target) in fields.iter().zip(&variants[0]) {
            self.walk(field.ty, target.ty, true)?;
        }
        Ok(())
    }

    /// 校验一个已登记 record 的 size/align 与逐字段 offset。
    fn verify_record(
        &mut self,
        index: usize,
        ty: &TypeLayout,
        fields: &[hir::Field],
        variants: &[Vec<crate::frontend::gir::concrete::Field>],
    ) -> Result<(), RawModelError> {
        let expected = &self.contract.records[index];
        let layout = ty
            .layout
            .ok_or_else(|| RawModelError::new("SharedHeap record 布局未形成"))?;
        if layout.size != u64::from(expected.bytes) || layout.align != u64::from(expected.alignment)
        {
            return Err(RawModelError::new(format!(
                "{} 的 Gugu 布局为 size={} align={}，machine 要求 size={} align={}",
                expected.name, layout.size, layout.align, expected.bytes, expected.alignment
            )));
        }
        if fields.len() != expected.fields.len() {
            return Err(RawModelError::new(format!(
                "{} 的字段数量与 machine 登记不一致",
                expected.name
            )));
        }
        for (position, expected_field) in expected.fields.iter().enumerate() {
            if fields[position].name != expected_field.name {
                return Err(RawModelError::new(format!(
                    "{}.{} 的字段顺序与 machine 登记不一致",
                    expected.name, expected_field.name
                )));
            }
            if variants[0][position].offset != u64::from(expected_field.offset) {
                return Err(RawModelError::new(format!(
                    "{}.{} 的 Gugu 与 machine 字段偏移不一致",
                    expected.name, expected_field.name
                )));
            }
        }
        self.found[index] = true;
        Ok(())
    }
}
