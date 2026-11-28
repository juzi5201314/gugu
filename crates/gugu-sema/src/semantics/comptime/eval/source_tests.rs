use super::*;
use crate::{ParsedModule, ast::ItemKind, cfg::CfgContext, source::SourceSnapshot};

const SOURCE_QUERIES: &str = "pub fn file() string { panic(\"source query stub\") }\n\
    pub fn line() int { panic(\"source query stub\") }\n\
    pub fn column() int { panic(\"source query stub\") }\n\
    pub fn caller() int { panic(\"source query stub\") }";

fn with_model(source: &str, test: impl FnOnce(&Model<'_>, &[ParsedModule])) {
    let inputs = [("std/src.gg", SOURCE_QUERIES), ("main.gg", source)];
    let sources = SourceMap::new(
        inputs
            .iter()
            .map(|(path, text)| SourceSnapshot::from_str(path, text).unwrap())
            .collect(),
    )
    .unwrap();
    let cfg = CfgContext::target_only(crate::TargetName::X86_64Linux);
    let modules: Vec<_> = inputs
        .iter()
        .map(|(path, text)| {
            let file = sources.file_id(path).unwrap();
            let snapshot = sources.snapshot(file).unwrap();
            let mut lexed = crate::lex(snapshot, &sources, file);
            assert!(lexed.diagnostics.is_empty());
            let parsed = crate::parse(text, &sources, file, &mut lexed.buffer);
            assert!(parsed.diagnostics.is_empty());
            let configured = crate::cfg::configure(
                snapshot,
                &sources,
                &parsed.file,
                &parsed.arena,
                &lexed.buffer,
                &cfg,
            )
            .unwrap();
            ParsedModule {
                package: None,
                path: if *path == "main.gg" {
                    "main"
                } else {
                    "std.src"
                }
                .to_owned(),
                file: parsed.file,
                arena: parsed.arena,
                tokens: lexed.buffer,
                configured,
                derived_impls: Vec::new(),
                builtin: *path != "main.gg",
            }
        })
        .collect();
    let names =
        crate::names::analyze_with_prefix("tests/source@1", &Default::default(), &modules, None)
            .unwrap();
    let model = Model::new(&modules, &names, &Default::default()).unwrap();
    test(&model, &modules);
}

fn initializer(module: &ParsedModule, name: &str) -> ExprId {
    module
        .arena
        .items
        .iter()
        .find_map(|item| match item.kind {
            ItemKind::Const { value, .. }
                if item
                    .name
                    .is_some_and(|symbol| module.tokens.intern.get_str(symbol) == name) =>
            {
                value
            }
            _ => None,
        })
        .unwrap()
}

#[test]
fn imported_source_queries_evaluate_in_early_const_and_source_expand() {
    for (imports, file, line, column) in [
        ("use std.src.{file, line, column}", "file", "line", "column"),
        ("use std.src as src", "src.file", "src.line", "src.column"),
        (
            "use std.src.{file as f, line as l, column as c}",
            "f",
            "l",
            "c",
        ),
    ] {
        let source = format!(
            "{imports}\nconst F: string = {file}()\nconst L: int = {line}()\nconst C: int = {column}()"
        );
        with_model(&source, |model, modules| {
            let queries = QueryEngine::new();
            let host = ExpandHost { queries: &queries };
            for (name, ty, expected) in [
                ("F", Ty::String, ConstantValue::String("main.gg".to_owned())),
                ("L", Ty::int(), ConstantValue::Int(3)),
                ("C", Ty::int(), ConstantValue::Int(16)),
            ] {
                let expression = initializer(&modules[1], name);
                assert_eq!(
                    model.eval_early_const(1, expression, &ty).unwrap(),
                    expected
                );
                assert_eq!(
                    model
                        .eval_source_macro(1, expression, SourceSlot::Expression, &host)
                        .unwrap()
                        .value,
                    expected
                );
            }
        });
    }
}

#[test]
fn local_values_shadow_imported_source_queries_during_comptime() {
    with_model(
        "use std.src.{line}\nconst N: int = { let line = 7\n line() }",
        |model, modules| {
            let expression = initializer(&modules[1], "N");
            let error = model
                .eval_early_const(1, expression, &Ty::int())
                .unwrap_err();
            assert_eq!(error.code(), DiagnosticCode::InvalidExpression);
            assert!(error.message().contains("局部"));
            let queries = QueryEngine::new();
            let error = model
                .eval_source_macro(
                    1,
                    expression,
                    SourceSlot::Expression,
                    &ExpandHost { queries: &queries },
                )
                .err()
                .unwrap();
            assert_eq!(error.code(), DiagnosticCode::InvalidExpression);
        },
    );
}

#[test]
fn local_values_shadow_imported_caller_in_both_comptime_domains() {
    with_model(
        "use std.src.{caller}\nconst N: int = { let caller = 7\n caller }",
        |model, modules| {
            let expression = initializer(&modules[1], "N");
            assert_eq!(
                model.eval_early_const(1, expression, &Ty::int()).unwrap(),
                ConstantValue::Int(7)
            );
            let queries = QueryEngine::new();
            assert_eq!(
                model
                    .eval_source_macro(
                        1,
                        expression,
                        SourceSlot::Expression,
                        &ExpandHost { queries: &queries },
                    )
                    .unwrap()
                    .value,
                ConstantValue::Int(7)
            );
        },
    );
}

#[test]
fn imported_caller_keeps_the_same_comptime_capability_boundary() {
    for expression in ["std.src.caller()", "src.caller()", "caller()", "site()"] {
        let source = format!(
            "use std.src as src\nuse std.src.{{caller, caller as site}}\nconst N: int = {expression}"
        );
        with_model(&source, |model, modules| {
            let expression = initializer(&modules[1], "N");
            let error = model
                .eval_early_const(1, expression, &Ty::int())
                .unwrap_err();
            assert_eq!(error.code(), DiagnosticCode::ComptimeCapability);
        });
    }
}
