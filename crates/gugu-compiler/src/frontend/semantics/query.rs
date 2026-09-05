//! query 身份包含源码和 cfg 输入，缓存诊断只保存逻辑位置并重绑定当前 SourceMap。
use super::{CheckedSemantics, checker, model::Model, output::SCHEMA_VERSION};
use crate::query::{QueryEngine, QueryError, QueryKey, QueryKind};
use crate::{Diagnostic, DiagnosticCode, SourceMap};

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredDiagnostic {
    code: DiagnosticCode,
    message: String,
    location: Option<(String, u32, u32)>,
}

pub(super) fn check(
    model: &Model<'_>,
    sources: &SourceMap,
    cfg: &super::super::cfg::CfgContext,
    queries: &QueryEngine,
) -> Result<CheckedSemantics, Vec<Diagnostic>> {
    let mut hash = blake3::Hasher::new_derive_key("gugu-type-check-input-v1");
    let configuration = format!("{cfg:?}");
    hash.update(configuration.as_bytes());
    for source in sources.snapshots() {
        hash.update(&(source.logical_path().len() as u64).to_le_bytes());
        hash.update(source.logical_path().as_bytes());
        hash.update(&source.content_hash());
    }
    hash.update(&model.name_fingerprint());
    let input_fingerprint = *hash.finalize().as_bytes();
    let key = QueryKey::new(QueryKind::TypeCheck, SCHEMA_VERSION, input_fingerprint);
    let result = queries.compute(key, |context| {
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
        match checker::check(model) {
            Ok(mut output) => {
                output.input_fingerprint = input_fingerprint;
                Ok((
                    serde_json::to_vec(&output).expect("语义 schema 序列化"),
                    Vec::new(),
                ))
            }
            Err(errors) => {
                let stored: Vec<_> = errors
                    .iter()
                    .map(|error| StoredDiagnostic {
                        code: error.code(),
                        message: error.message().to_owned(),
                        location: error.span().map(|span| {
                            (
                                span.path().to_string_lossy().into_owned(),
                                span.start(),
                                span.end(),
                            )
                        }),
                    })
                    .collect();
                Err(QueryError::Failed(
                    serde_json::to_string(&stored).expect("诊断 schema 序列化"),
                ))
            }
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
            Ok(output)
        }
        Err(QueryError::Failed(stored)) => {
            let errors: Vec<StoredDiagnostic> =
                serde_json::from_str(&stored).expect("失败 query 由本 schema 写入");
            Err(errors
                .into_iter()
                .map(|error| {
                    let span = error.location.map(|(path, start, end)| {
                        sources
                            .span(
                                sources.file_id(&path).expect("相同 query 源路径"),
                                start as usize,
                                end as usize,
                                crate::source::ExpansionId::ROOT,
                            )
                            .expect("相同 query 的合法范围")
                    });
                    Diagnostic::error(error.code, error.message, span)
                })
                .collect())
        }
        Err(error) => Err(vec![Diagnostic::error(
            DiagnosticCode::InvalidType,
            error.to_string(),
            None,
        )]),
    }
}
