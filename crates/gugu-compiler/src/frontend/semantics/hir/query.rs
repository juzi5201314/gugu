use super::*;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

pub(super) fn lower(
    model: &Model<'_>,
    names: &NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
    entry: Option<CallableId>,
    dependency: &DependencyFingerprint,
    queries: &QueryEngine,
) -> Result<hir::Validated, Vec<Diagnostic>> {
    let mut hash = blake3::Hasher::new_derive_key("gugu-lower-hir-input-v1");
    hash.update(&dependency.fingerprint());
    if let Some(entry) = entry {
        hash.update(model.modules[entry.module].path.as_bytes());
        hash.update(b"main");
    }
    for expansion in sources.expansions() {
        hash.update(&expansion.parent().as_u32().to_le_bytes());
        hash.update(&expansion.source_hash());
        hash.update(expansion.macro_call().path().as_os_str().as_encoded_bytes());
        hash.update(&expansion.macro_call().start().to_le_bytes());
        hash.update(&expansion.macro_call().end().to_le_bytes());
    }
    let input_fingerprint = *hash.finalize().as_bytes();
    let key = QueryKey::new(QueryKind::LowerHir, 1, input_fingerprint);
    let mut fresh = None;
    let result = queries.compute(key, |context| {
        context.record_dependency(dependency.key().clone(), dependency.fingerprint());
        let formed = Builder::new(model, names, checked, sources, entry)
            .and_then(Builder::build)
            .and_then(|mut module| {
                module.input_fingerprint = input_fingerprint;
                hir::Validated::freeze(module)
            });
        match formed {
            Ok((hir, bytes)) => {
                fresh = Some(hir);
                Ok((bytes, Vec::new()))
            }
            Err(error) => Err(super::super::query::store_errors(&[error])),
        }
    });
    let result = result.map_err(|error| super::super::query::restore_errors(error, sources))?;
    if let Some(fresh) = fresh {
        return Ok(fresh);
    }
    let module: hir::Module = serde_json::from_slice(result.payload())
        .map_err(|_| vec![invalid("HIR query 缓存 schema 不合法")])?;
    if module.input_fingerprint != input_fingerprint {
        return Err(vec![invalid("HIR query 输入身份不匹配")]);
    }
    let (validated, canonical) = hir::Validated::freeze(module).map_err(|error| vec![error])?;
    if canonical != result.payload() {
        return Err(vec![invalid("HIR query 结果不是规范序列化")]);
    }
    Ok(validated)
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
