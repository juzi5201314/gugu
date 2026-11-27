use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, Severity, TargetName};

fn compile(request: CompileRequest) -> gugu_compiler::Compilation {
    Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_to_lir(request)
}

fn single_file(source: &str) -> gugu_compiler::Compilation {
    compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ))
}

#[test]
fn doc_world_collects_public_api() {
    let output = single_file(
        "//! 模块文档\n\
         /// 加一。\n\
         /// 细节第二行。\n\
         pub fn add(a: int) int = a + 1\n\
         /// 点。\n\
         pub struct Point {\n    pub x: int,\n    y: int\n}\n\
         fn private_helper() {}\n\
         pub struct Hidden {\n    z: int\n}\n\
         fn main() {}\n",
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let world = output.docs().expect("成功编译必须产出文档世界");
    let module = world
        .modules
        .iter()
        .find(|module| module.path == "main")
        .expect("缺少 main 模块");
    assert!(module.docs.contains("模块文档"), "{}", module.docs);
    let add = module
        .items
        .iter()
        .find(|item| item.name == "add")
        .expect("缺少 add 项");
    assert_eq!(add.kind, "function");
    assert!(add.signature.contains("add"), "{}", add.signature);
    assert!(add.docs.contains("加一。"), "{}", add.docs);
    assert!(add.docs.contains("细节第二行。"), "{}", add.docs);
    assert_eq!(add.line, 4);
    let point = module
        .items
        .iter()
        .find(|item| item.name == "Point")
        .expect("缺少 Point 项");
    assert_eq!(point.kind, "struct");
    // 公开字段进入成员表，私有字段被过滤。
    assert!(point.members.iter().any(|member| member.name == "x"));
    assert!(!point.members.iter().any(|member| member.name == "y"));
    assert!(
        !module
            .items
            .iter()
            .any(|item| item.name == "private_helper"),
        "私有项不得进入文档"
    );
    // `pub struct Hidden` 未写文档，仍收录但会触发 missing_docs。
    assert!(module.items.iter().any(|item| item.name == "Hidden"));
    assert_eq!(module.source_path.as_str(), "main.gg");
}

#[test]
fn missing_docs_warns_by_default() {
    let output = single_file("pub fn undocumented() {}\nfn main() {}\n");
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let warning = output
        .diagnostics()
        .items()
        .iter()
        .find(|diagnostic| diagnostic.code().to_string() == "E0063")
        .expect("缺少 missing_docs 警告");
    assert_eq!(warning.severity(), Severity::Warning);
    assert!(warning.message().contains("缺少"), "{}", warning.message());
    let span = warning.span().expect("lint 必须带位置");
    assert_eq!(span.line(), 1);
}

#[test]
fn missing_docs_respects_lint_levels() {
    let allowed = single_file("#![allow(missing_docs)]\npub fn f() {}\nfn main() {}\n");
    assert!(
        !allowed
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code().to_string() == "E0063"),
        "allow(missing_docs) 后不得报告 E0063"
    );

    let denied = single_file("#![deny(missing_docs)]\npub fn f() {}\nfn main() {}\n");
    assert!(!denied.is_success());
    let error = denied
        .diagnostics()
        .items()
        .iter()
        .find(|diagnostic| diagnostic.code().to_string() == "E0063")
        .expect("deny 后必须报告 E0063");
    assert_eq!(error.severity(), Severity::Error);
}

#[test]
fn missing_docs_covers_public_members() {
    let output = single_file(
        "#![deny(missing_docs)]\n\
         /// 有文档。\n\
         pub struct S {\n    pub field: int\n}\n\
         /// 有文档。\n\
         pub trait T {\n    fn method(self: &Self)\n}\n\
         fn main() {}\n",
    );
    assert!(!output.is_success(), "{:?}", output.diagnostics().items());
    let missing: Vec<String> = output
        .diagnostics()
        .items()
        .iter()
        .filter(|diagnostic| diagnostic.code().to_string() == "E0063")
        .map(|diagnostic| diagnostic.message().to_owned())
        .collect();
    assert!(
        missing.iter().any(|message| message.contains("field")),
        "{missing:?}"
    );
    assert!(
        missing.iter().any(|message| message.contains("method")),
        "{missing:?}"
    );
}

#[test]
fn doctest_error_reanchors_to_fence() {
    // 围栏开在 `/// ```gugu` 行（第 2 行）；生成模块中的语义错误必须
    // 重锚定回围栏位置并给出宏展开链 note。
    let output = compile(CompileRequest::single_file_test(
        "main.gg",
        "/// 文档。\n/// ```gugu\n/// nope()\n/// ```\npub fn f() {}\nfn main() {}\n",
        TargetName::X86_64Linux,
    ));
    assert!(!output.is_success());
    let error = output
        .diagnostics()
        .items()
        .iter()
        .find(|diagnostic| diagnostic.severity() == Severity::Error)
        .expect("doctest 必须产生错误");
    let span = error.span().expect("错误必须带主位置");
    assert_eq!(span.path().to_str().unwrap_or(""), "main.gg", "{span:?}");
    assert_eq!(span.line(), 2, "错误必须锚定到围栏行：{span:?}");
    assert!(
        output
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.severity() == Severity::Note
                && diagnostic.message().contains("宏调用位置")),
        "必须给出宏展开链 note：{:?}",
        output.diagnostics().items()
    );
}

#[test]
fn generated_doctest_module_skips_missing_docs() {
    // doctest 生成模块自身不带文档头，但 generated 模块不得触发 missing_docs。
    let output = compile(CompileRequest::single_file_test(
        "main.gg",
        "//! 主模块文档\n/// 文档。\n/// ```gugu\n/// if 1 != 1 { panic(\"x\") }\n/// ```\npub fn f() {}\nfn main() {}\n",
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    assert!(
        !output
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code().to_string() == "E0063"),
        "生成模块不得触发 missing_docs"
    );
}
