//! 契约记录只消费冻结签名与类型布局，不消费可执行函数体。
use crate::Diagnostic;
use crate::runtime::RawModelError;
use crate::semantics::linkage::RootLinkageKind;
use crate::{gir::concrete::RecordTypes, hir, mono::keys::MonoContext};

pub fn collect(context: &MonoContext<'_>) -> Result<Vec<RecordTypes>, Diagnostic> {
    context
        .module
        .linkage
        .iter()
        .filter(|linkage| linkage.root_kind == RootLinkageKind::ContractRecord)
        .map(|linkage| crate::gir::concrete::record_types(context, linkage.definition))
        .collect()
}

pub fn source_parameters(
    module: &hir::Module,
    definition: hir::DefId,
) -> Result<&[hir::TypeId], RawModelError> {
    let signature = module.definitions[definition.index()]
        .signature
        .ok_or_else(|| RawModelError::new("契约记录缺少源签名"))?;
    let signature = match module.types[signature.index()] {
        hir::Type::Callable { signature, .. } => signature,
        _ => signature,
    };
    let hir::Type::Function { parameters, .. } = &module.types[signature.index()] else {
        return Err(RawModelError::new("契约记录签名不是函数"));
    };
    Ok(parameters)
}
