//! 需要完整 `Compiler` 的 GIR 层回归：action key、image plan 计数、GIR dump 与大拷贝 lint。

use crate::{CompileRequest, Compiler, TargetName};

/// 大拷贝诊断：一次编译一个用例，单项不超过测试耗时上限。
fn large_copy_compilation(source: &str) -> crate::Compilation {
    Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ))
}

#[test]
fn large_copy_warns_by_default() {
    let compilation =
        large_copy_compilation("fn take(xs: [uint; 9]) { _ = xs }\nfn main() { take([0; 9]) }");
    assert!(compilation.is_success());
    assert!(compilation.image_plan().is_some());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(
                |diagnostic| diagnostic.code() == crate::DiagnosticCode::LargeCopy
                    && diagnostic.severity() == crate::Severity::Warning
            )
    );
}

#[test]
fn large_copy_allow_suppresses_the_lint() {
    let compilation = large_copy_compilation(
        "fn take(xs: [uint; 9]) { _ = xs }\n#[allow(large_copy)]\nfn main() { take([0; 9]) }",
    );
    assert!(compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::LargeCopy),
        "允许标记之后不得再报大拷贝：{:?}",
        compilation.diagnostics().items()
    );
}

#[test]
fn large_copy_exact_limit_is_not_reported() {
    let compilation =
        large_copy_compilation("fn take(xs: [uint; 8]) { _ = xs }\nfn main() { take([0; 8]) }");
    assert!(compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::LargeCopy)
    );
}

#[test]
fn large_copy_deny_fails_without_an_image_plan() {
    let compilation = large_copy_compilation(
        "#![deny(large_copy)]\nfn take(xs: [uint; 9]) { _ = xs }\nfn main() { take([0; 9]) }",
    );
    assert!(!compilation.is_success());
    assert!(compilation.image_plan().is_none());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(
                |diagnostic| diagnostic.code() == crate::DiagnosticCode::LargeCopy
                    && diagnostic.severity() == crate::Severity::Error
            )
    );
}

#[test]
#[ignore = "slow full compilation"]
fn borrowed_fields_keep_original_places_without_large_copy() {
    let source = "#[repr(C, align(64))]\nstruct Large { head: uint, tail: [uint; 9] }\n#[used]\n#[deny(large_copy)]\nfn field(value: &Large) &uint = &value.head\nfn main() {}";
    let result = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(result.is_success(), "{:?}", result.diagnostics().items());
    assert!(
        result
            .diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::LargeCopy),
        "借用字段不得产生大拷贝：{:?}",
        result.diagnostics().items()
    );

    let copying = "struct Large { bytes: [uint; 9] }\n#[used]\n#[deny(large_copy)]\nfn copy(value: &Large) Large = *value\nfn main() {}";
    let result = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        copying,
        TargetName::X86_64Linux,
    ));
    assert!(!result.is_success());
    assert!(result.image_plan().is_none());
    assert!(
        result
            .diagnostics()
            .items()
            .iter()
            .any(
                |diagnostic| diagnostic.code() == crate::DiagnosticCode::LargeCopy
                    && diagnostic.severity() == crate::Severity::Error
            )
    );
}

/// string 按值复制要密封：`main` 的 body 必须出现 `cow_snapshot`。
#[test]
fn string_copy_seals() {
    let string =
        "fn take(s: string) { _ = s }\nfn main() { let t: string = \"ok\"\n take(t)\n _ = t }";
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        string,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let dump = compilation.dump_gir().expect("string GIR");
    let string_main = body_named(&dump, "main", "String");
    assert!(string_main.contains("cow_snapshot"), "{string_main}");
}

/// chan 共享身份：复制不密封，`main` 的 body 不得出现 `cow_snapshot`。
#[test]
fn chan_shares_identity() {
    let chan = "fn take(c: chan[int]) { _ = c }\nfn main() { let c: chan[int] = chan[int](1)\n take(c)\n _ = c }";
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        chan,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let dump = compilation.dump_gir().expect("chan GIR");
    let chan_main = body_named(&dump, "main", "Chan(");
    assert!(!chan_main.contains("cow_snapshot"), "{chan_main}");
    assert!(chan_main.contains("ValueAction Copy"), "{chan_main}");
}

fn body_named<'a>(dump: &'a str, owner: &str, marker: &str) -> &'a str {
    dump.split("body owner=")
        .find(|body| body.starts_with(owner) && body.contains(marker))
        .unwrap_or_else(|| panic!("缺少 {owner} body（{marker}）"))
}

#[test]
#[ignore = "slow full compilation"]
fn image_plan_reports_gir_counts() {
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { _ = 1 }",
        TargetName::X86_64Linux,
    ));
    let plan = compilation.image_plan().expect("镜像计划");
    assert!(plan.gir_body_count() >= 1);
    assert!(plan.gir_block_count() >= 1);
    assert!(plan.placement_count() >= 1);
    assert_ne!(plan.gir_fingerprint(), [0; 32]);
    assert_ne!(plan.placement_fingerprint(), [0; 32]);
}

#[test]
#[ignore = "slow full compilation"]
fn action_key_includes_generic_gir() {
    let first = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { _ = 1 }",
        TargetName::X86_64Linux,
    ));
    let second = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { _ = 2 }",
        TargetName::X86_64Linux,
    ));
    assert_ne!(first.action_key(), second.action_key());
    assert_ne!(first.gir_fingerprint(), second.gir_fingerprint());
}
