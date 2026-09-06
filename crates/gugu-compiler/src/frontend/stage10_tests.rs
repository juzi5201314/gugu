use std::collections::{BTreeMap, BTreeSet};

use crate::{
    DiagnosticCode, Severity, SourceMap, SourceSnapshot, TargetName,
    frontend::{SourceInput, bootstrap, cfg::CfgContext},
};

fn analyze(
    sources: &[(&str, &str)],
    target: TargetName,
    declared_features: &[&str],
    enabled_features: &[&str],
    test: bool,
    bench: bool,
) -> Result<super::FrontendOutput, Vec<crate::Diagnostic>> {
    let cfg = CfgContext::new(
        target,
        declared_features
            .iter()
            .map(|feature| (*feature).to_owned()),
        enabled_features.iter().map(|feature| (*feature).to_owned()),
        test,
        bench,
        BTreeMap::new(),
    );
    analyze_with_cfg(sources, &cfg)
}

fn analyze_with_cfg(
    sources: &[(&str, &str)],
    cfg: &CfgContext,
) -> Result<super::FrontendOutput, Vec<crate::Diagnostic>> {
    let snapshots = sources
        .iter()
        .map(|(path, source)| SourceSnapshot::from_str(path, source).expect("valid source"))
        .collect();
    let mut source_map = SourceMap::new(snapshots).expect("unique source paths");
    bootstrap(
        SourceInput::Sources {
            source_map: &mut source_map,
            entry: "src/main.gg",
            source_root: "src",
            package_identity: "acme/demo@1.0.0",
            require_main: true,
            cfg,
            external_packages: &BTreeSet::new(),
        },
        &crate::query::QueryEngine::new(),
    )
}

fn codes(result: Result<super::FrontendOutput, Vec<crate::Diagnostic>>) -> Vec<DiagnosticCode> {
    result
        .expect_err("analysis must fail")
        .into_iter()
        .filter(|diagnostic| diagnostic.severity() == Severity::Error)
        .map(|diagnostic| diagnostic.code())
        .collect()
}

#[test]
fn cfg_selects_exactly_one_target_and_feature_definition() {
    let sources = [
        ("src/main.gg", "use platform.{boot}\nfn main() {}\n"),
        (
            "src/platform.gg",
            "#[cfg(all(os = \"linux\", feature = \"fast\"))]\npub fn boot() {}\n#[cfg(any(os = \"windows\", not(feature = \"fast\")))]\npub fn boot() {}\n",
        ),
    ];
    let linux = analyze(
        &sources,
        TargetName::X86_64Linux,
        &["default", "fast"],
        &["fast"],
        false,
        false,
    )
    .expect("linux feature branch is unique");
    let windows = analyze(
        &sources,
        TargetName::X86_64Windows,
        &["default", "fast"],
        &["fast"],
        false,
        false,
    )
    .expect("windows branch is unique");

    assert_eq!(linux.names.definitions.len(), 2);
    assert_eq!(windows.names.definitions.len(), 2);
    assert_eq!(linux.names.imports.len(), 1);
    assert_eq!(windows.names.imports.len(), 1);
}

#[test]
fn cfg_test_and_bench_are_separate_modes() {
    let source = [(
        "src/main.gg",
        "#[cfg(test)] fn mode() {}\n#[cfg(bench)] fn mode() {}\nfn main() {}\n",
    )];
    assert!(
        analyze(
            &source,
            TargetName::X86_64Linux,
            &["default"],
            &["default"],
            true,
            false,
        )
        .is_ok()
    );
    assert!(
        analyze(
            &source,
            TargetName::X86_64Linux,
            &["default"],
            &["default"],
            false,
            true,
        )
        .is_ok()
    );
}
#[test]
fn cfg_evaluates_registered_build_flags_and_values() {
    let mut custom = BTreeMap::new();
    custom.insert("fast_path".to_owned(), None);
    custom.insert("platform_api".to_owned(), Some("epoll".to_owned()));
    let cfg = CfgContext::new(
        TargetName::X86_64Linux,
        ["default".to_owned()],
        ["default".to_owned()],
        false,
        false,
        custom,
    );
    let configured = analyze_with_cfg(
        &[(
            "src/main.gg",
            "#[cfg(fast_path)] fn fast() {}\n#[cfg(platform_api = \"epoll\")] fn io() {}\nfn main() {}\n",
        )],
        &cfg,
    )
    .expect("registered custom cfg resolves");
    assert_eq!(configured.names.definitions.len(), 3);

    let unknown = analyze(
        &[("src/main.gg", "#[cfg(fast_path)] fn main() {}\n")],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(unknown), [DiagnosticCode::CfgInvalidPredicate]);
}

#[test]
fn cfg_prunes_declarations_statements_arms_and_list_members() {
    let configured = analyze(
        &[
            (
                "src/main.gg",
                r#"use worker.{#[cfg(false)] hidden, run}
struct Record {
    #[cfg(false)] secret: int
    pub shown: int
}
enum Choice {
    #[cfg(false)] Old
    Pair(#[cfg(false)] int, int)
    New
}
fn optional(#[cfg(false)] removed: int, kept: int) {}
fn main() {
    let value = 1
    #[cfg(false)] let removed = missing
    let values = [#[cfg(false)] missing, 1]
    let record = Record { #[cfg(false)] secret: missing, shown: 1 }
    match value {
        #[cfg(false)] 0 => missing
        _ => 1
    }
    _ = select {
        #[cfg(false)] _ => missing
        _ => 1
    }
}
"#,
            ),
            ("src/worker.gg", "pub fn hidden() {}\npub fn run() {}\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    )
    .expect("cfg 只删除完整序列成员");

    assert_eq!(configured.names.definitions.len(), 10);
    assert_eq!(configured.names.imports.len(), 1);
}

#[test]
fn cfg_rejects_deleting_a_required_singleton_expression() {
    let result = analyze(
        &[("src/main.gg", "fn main() = #[cfg(false)] 1\n")],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(result), [DiagnosticCode::CfgInvalidPredicate]);

    let newtype = analyze(
        &[(
            "src/main.gg",
            "struct Id(#[cfg(false)] int)\nfn main() {}\n",
        )],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(newtype), [DiagnosticCode::CfgInvalidPredicate]);
}

#[test]
fn cfg_rejects_undefined_features_and_removes_main_before_codegen() {
    let unknown = analyze(
        &[(
            "src/main.gg",
            "#[cfg(feature = \"missing\")] fn main() {}\n",
        )],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(unknown), [DiagnosticCode::CfgInvalidPredicate]);

    let removed = analyze(
        &[("src/main.gg", "#[cfg(os = \"windows\")] fn main() {}\n")],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(removed), [DiagnosticCode::MissingMain]);
}

#[test]
fn module_level_cfg_removes_the_module_from_import_resolution() {
    let result = analyze(
        &[
            ("src/main.gg", "use platform\nfn main() {}\n"),
            (
                "src/platform.gg",
                "#![cfg(os = \"windows\")]\npub fn boot() {}\n",
            ),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(result), [DiagnosticCode::ModuleNotFound]);
}

#[test]
fn imports_resolve_reexports_and_enforce_visibility() {
    let reexport = analyze(
        &[
            ("src/main.gg", "use facade.{run}\nfn main() {}\n"),
            ("src/facade.gg", "pub use worker.{run}\n"),
            ("src/worker.gg", "pub fn run() {}\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    )
    .expect("public re-export resolves");
    assert_eq!(reexport.names.imports.len(), 2);

    let private = analyze(
        &[
            ("src/main.gg", "use worker.{run}\nfn main() {}\n"),
            ("src/worker.gg", "fn run() {}\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(private), [DiagnosticCode::PrivateImport]);
}

#[test]
fn import_graph_rejects_cycles_case_mismatches_and_alias_conflicts() {
    let cycle = analyze(
        &[
            ("src/main.gg", "use worker\nfn main() {}\n"),
            ("src/worker.gg", "use main\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(cycle), [DiagnosticCode::ImportCycle]);

    let case = analyze(
        &[
            ("src/main.gg", "use Worker\nfn main() {}\n"),
            ("src/worker.gg", "pub fn run() {}\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(case), [DiagnosticCode::ModulePathCaseMismatch]);

    let conflict = analyze(
        &[
            (
                "src/main.gg",
                "use worker.{run}\nfn run() {}\nfn main() {}\n",
            ),
            ("src/worker.gg", "pub fn run() {}\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(conflict), [DiagnosticCode::ImportConflict]);
}

#[test]
fn duplicate_and_reserved_definitions_are_rejected_per_namespace() {
    let duplicate = analyze(
        &[(
            "src/main.gg",
            "fn repeated() {}\nfn repeated() {}\nfn main() {}\n",
        )],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(duplicate), [DiagnosticCode::DuplicateDefinition]);

    let reserved = analyze(
        &[("src/main.gg", "struct Option {}\nfn main() {}\n")],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(reserved), [DiagnosticCode::ReservedName]);

    let separate_namespaces = analyze(
        &[(
            "src/main.gg",
            "struct Widget {}\nfn Widget() {}\nfn main() {}\n",
        )],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert!(separate_namespaces.is_ok());
}

#[test]
fn module_file_forms_and_definition_ids_are_deterministic() {
    let conflict = analyze(
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("src/net.gg", "pub fn open() {}\n"),
            ("src/net/mod.gg", "pub fn close() {}\n"),
        ],
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    );
    assert_eq!(codes(conflict), [DiagnosticCode::ModuleInvalidPath]);

    let forward = [
        ("src/main.gg", "use worker.{run}\nfn main() {}\n"),
        ("src/worker.gg", "pub fn run() {}\n"),
    ];
    let reverse = [forward[1], forward[0]];
    let first = analyze(
        &forward,
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    )
    .expect("forward order resolves");
    let second = analyze(
        &reverse,
        TargetName::X86_64Linux,
        &["default"],
        &["default"],
        false,
        false,
    )
    .expect("reverse order resolves");
    let first_keys = first
        .names
        .definitions
        .iter()
        .map(|definition| (definition.id.index(), definition.stable_key))
        .collect::<Vec<_>>();
    let second_keys = second
        .names
        .definitions
        .iter()
        .map(|definition| (definition.id.index(), definition.stable_key))
        .collect::<Vec<_>>();
    assert_eq!(first_keys, second_keys);
}
