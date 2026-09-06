//! query 身份包含源码和 cfg 输入，缓存诊断只保存逻辑位置并重绑定当前 SourceMap。
use super::{CheckedSemantics, checker, model::Model, output::SCHEMA_VERSION};
use crate::query::{QueryEngine, QueryError, QueryKey, QueryKind};
use crate::{Diagnostic, DiagnosticCode, SourceMap};

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredDiagnostic {
    severity: crate::Severity,
    sequence: u32,
    code: DiagnosticCode,
    message: String,
    location: Option<(String, u32, u32, u32)>,
}

pub(super) fn check(
    model: &Model<'_>,
    sources: &SourceMap,
    cfg: &super::super::cfg::CfgContext,
    queries: &QueryEngine,
    early: &super::comptime::EarlyConstTable,
    early_dependency: &crate::query::DependencyFingerprint,
) -> Result<(CheckedSemantics, crate::query::DependencyFingerprint), Vec<Diagnostic>> {
    let mut hash = blake3::Hasher::new_derive_key("gugu-type-check-input-v1");
    let configuration = format!("{cfg:?}");
    hash.update(configuration.as_bytes());
    for source in sources.snapshots() {
        hash.update(&(source.logical_path().len() as u64).to_le_bytes());
        hash.update(source.logical_path().as_bytes());
        hash.update(&source.content_hash());
    }
    hash.update(&model.name_fingerprint());
    hash.update(&super::comptime::registry::summary());
    let input_fingerprint = *hash.finalize().as_bytes();
    let key = QueryKey::new(QueryKind::TypeCheck, SCHEMA_VERSION, input_fingerprint);
    let result = queries.compute(key.clone(), |context| {
        for source in sources.snapshots() {
            context.record_dependency(
                QueryKey::new(QueryKind::SourceSnapshot, 1, source.logical_path()),
                source.content_hash(),
            );
        }
        context.record_dependency(
            QueryKey::new(QueryKind::Configure, 1, b"cfg"),
            *blake3::hash(configuration.as_bytes()).as_bytes(),
        );
        context.record_dependency(
            QueryKey::new(QueryKind::ResolveImports, 1, b"names"),
            model.name_fingerprint(),
        );
        context.record_dependency(
            early_dependency.key().clone(),
            early_dependency.fingerprint(),
        );
        match checker::check(model, early) {
            Ok(mut output) => {
                output.input_fingerprint = input_fingerprint;
                Ok((
                    serde_json::to_vec(&output).expect("语义 schema 序列化"),
                    Vec::new(),
                ))
            }
            Err(errors) => Err(store_errors(&errors)),
        }
    });
    match result {
        Ok(result) => {
            let output: CheckedSemantics =
                serde_json::from_slice(result.payload()).map_err(|_| {
                    vec![Diagnostic::error(
                        DiagnosticCode::InvalidType,
                        "类型 query 缓存 schema 不合法",
                        None,
                    )]
                })?;
            output.verify(model).map_err(|error| vec![error])?;
            Ok((
                output,
                crate::query::DependencyFingerprint::new(key, result.fingerprint()),
            ))
        }
        Err(error) => Err(restore_errors(error, sources)),
    }
}

pub(crate) fn store_errors(errors: &[Diagnostic]) -> QueryError {
    let stored: Vec<_> = errors
        .iter()
        .map(|error| StoredDiagnostic {
            severity: error.severity(),
            sequence: error.sequence(),
            code: error.code(),
            message: error.message().to_owned(),
            location: error.span().map(|span| {
                (
                    span.path().to_string_lossy().into_owned(),
                    span.start(),
                    span.end(),
                    span.expansion().as_u32(),
                )
            }),
        })
        .collect();
    QueryError::Failed(serde_json::to_string(&stored).expect("诊断 schema 序列化"))
}

pub(crate) fn restore_errors(error: QueryError, sources: &SourceMap) -> Vec<Diagnostic> {
    let QueryError::Failed(stored) = error else {
        return vec![Diagnostic::error(
            DiagnosticCode::InvalidType,
            error.to_string(),
            None,
        )];
    };
    let Ok(errors) = serde_json::from_str::<Vec<StoredDiagnostic>>(&stored) else {
        return vec![Diagnostic::error(
            DiagnosticCode::InvalidType,
            "查询诊断缓存 schema 不合法",
            None,
        )];
    };
    errors
        .into_iter()
        .map(|error| {
            let span = match error.location {
                Some((path, start, end, expansion)) => {
                    let Some(file) = sources.file_id(&path) else {
                        return Diagnostic::error(
                            DiagnosticCode::InvalidType,
                            "查询诊断引用未知源码",
                            None,
                        );
                    };
                    match sources.span(
                        file,
                        start as usize,
                        end as usize,
                        crate::ExpansionId::new(expansion),
                    ) {
                        Ok(span) => Some(span),
                        Err(_) => {
                            return Diagnostic::error(
                                DiagnosticCode::InvalidType,
                                "查询诊断源码位置不合法",
                                None,
                            );
                        }
                    }
                }
                None => None,
            };
            Diagnostic::new(
                error.severity,
                error.code,
                error.message,
                span,
                error.sequence,
            )
        })
        .collect()
}
