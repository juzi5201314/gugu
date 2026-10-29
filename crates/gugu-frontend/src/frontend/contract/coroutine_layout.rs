//! 从冻结HIR与具体GIR交叉校验Gugu控制块布局；不重新解释源码或名称解析。

use super::contract_view::source_parameters;
use crate::frontend::{
    gir::concrete::{RecordTypes, TypeKind, TypeLayout},
    hir,
};
use crate::runtime::{CoroutineRuntimeContract, RawModelError, RuntimeResources};
use std::collections::BTreeSet;

pub(crate) fn verify_source(
    contract: &CoroutineRuntimeContract,
    module: &hir::Module,
    gir: &[RecordTypes],
    resources: &RuntimeResources<'_>,
) -> Result<(), RawModelError> {
    // 空package没有runtime源图，也不生成镜像；固定machine布局仍由contract.verify校验。
    if module.sources.is_empty() {
        debug_assert!(gir.is_empty());
        return Ok(());
    }
    let mut found = vec![false; contract.records.len()];
    for concrete in gir {
        let definition = &module.definitions[concrete.definition.index()];
        if !in_source(definition, module, resources) {
            continue;
        }
        let parameters = source_parameters(module, concrete.definition)?;
        if parameters.len() != concrete.parameters.len() {
            return Err(RawModelError::new("协程入口具体签名与冻结HIR不一致"));
        }
        let mut verifier = Verifier {
            contract,
            module,
            types: &concrete.types,
            found: &mut found,
            visited: BTreeSet::new(),
            resources,
        };
        for (source, target) in parameters.iter().zip(&concrete.parameters) {
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
            "内建Gugu协程源缺少具体布局：{}",
            missing.join(", ")
        )));
    }
    Ok(())
}

fn in_source(
    definition: &hir::Definition,
    module: &hir::Module,
    resources: &RuntimeResources<'_>,
) -> bool {
    resources
        .runtime_paths()
        .any(|path| path == source_path(definition, module))
}

/// 定义所在的逻辑源路径。
fn source_path<'a>(definition: &hir::Definition, module: &'a hir::Module) -> &'a str {
    definition.location.as_ref().map_or("", |location| {
        module.sources[usize::try_from(location.source).expect("source下标")]
            .path
            .as_str()
    })
}

struct Verifier<'a> {
    contract: &'a CoroutineRuntimeContract,
    module: &'a hir::Module,
    types: &'a [TypeLayout],
    found: &'a mut [bool],
    visited: BTreeSet<(hir::TypeId, u32)>,
    resources: &'a RuntimeResources<'a>,
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
                if !in_source(definition_value, self.module, self.resources) {
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
                    return Err(RawModelError::new("协程record字段集合不匹配"));
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
                        .ok_or_else(|| RawModelError::new("协程record布局未形成"))?;
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
                            .ok_or_else(|| RawModelError::new("协程record缺少已登记字段"))?;
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
            // bool 在机器层是单字节标量，与整型一样不需要字段展开。
            (hir::Type::Bool, TypeKind::Bool) => Ok(()),
            // 函数句柄的机器表示固定为 code 与 environment 两个车道（规范 functions.md）。
            // environment 车道指向带描述符的受管闭包环境：形状由对象自带的描述符决定，
            // 不按静态类型展开，因此这里只校验两车道宽度本身。
            (hir::Type::Function { .. }, TypeKind::Function { .. }) => {
                let layout = ty
                    .layout
                    .ok_or_else(|| RawModelError::new("函数句柄布局未形成"))?;
                if layout.size != 16 || layout.align != 8 {
                    return Err(RawModelError::new(format!(
                        "函数句柄在raw record里必须占两个机器字，实际size={} align={}",
                        layout.size, layout.align
                    )));
                }
                Ok(())
            }
            (hir::Type::Int { .. } | hir::Type::Unit, _) => Ok(()),
            (source_type, kind) => Err(RawModelError::new(format!(
                "协程raw record含未登记的managed字段或表示：{source_type:?} -> {kind:?}"
            ))),
        }
    }
}
