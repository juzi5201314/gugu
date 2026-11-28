use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, Severity, TargetName};

fn compile(compiler: &Compiler, request: CompileRequest) -> gugu_compiler::Compilation {
    compiler.compile_sema_for_test(request)
}

fn single_file(compiler: &Compiler, source: &str) -> gugu_compiler::Compilation {
    compile(
        compiler,
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux),
    )
}

#[test]
fn doc_world_and_missing_docs_contracts() {
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);

    // 1. 公共 API 收集与成员过滤。
    let output = single_file(
        &compiler,
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
    assert!(point.members.iter().any(|member| member.name == "x"));
    assert!(!point.members.iter().any(|member| member.name == "y"));
    assert!(
        !module
            .items
            .iter()
            .any(|item| item.name == "private_helper"),
        "私有项不得进入文档"
    );
    assert!(module.items.iter().any(|item| item.name == "Hidden"));
    assert_eq!(module.source_path.as_str(), "main.gg");

    // 2. 默认 missing_docs 警告与位置。
    let warned = single_file(&compiler, "pub fn undocumented() {}\nfn main() {}\n");
    assert!(warned.is_success(), "{:?}", warned.diagnostics().items());
    let warning = warned
        .diagnostics()
        .items()
        .iter()
        .find(|diagnostic| diagnostic.code().to_string() == "E0063")
        .expect("缺少 missing_docs 警告");
    assert_eq!(warning.severity(), Severity::Warning);
    assert!(warning.message().contains("缺少"), "{}", warning.message());
    let span = warning.span().expect("lint 必须带位置");
    assert_eq!(span.line(), 1);

    // 3. allow / deny 级别切换。
    let allowed = single_file(
        &compiler,
        "#![allow(missing_docs)]\npub fn f() {}\nfn main() {}\n",
    );
    assert!(
        !allowed
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code().to_string() == "E0063"),
        "allow(missing_docs) 后不得报告 E0063"
    );
    let denied = single_file(
        &compiler,
        "#![deny(missing_docs)]\npub fn f() {}\nfn main() {}\n",
    );
    assert!(!denied.is_success());
    let error = denied
        .diagnostics()
        .items()
        .iter()
        .find(|diagnostic| diagnostic.code().to_string() == "E0063")
        .expect("deny 后必须报告 E0063");
    assert_eq!(error.severity(), Severity::Error);

    // 4. 公开字段与 trait 方法成员覆盖。
    let members = single_file(
        &compiler,
        "#![deny(missing_docs)]\n\
         /// 有文档。\n\
         pub struct S {\n    pub field: int\n}\n\
         /// 有文档。\n\
         pub trait T {\n    fn method(self: &Self)\n}\n\
         fn main() {}\n",
    );
    assert!(!members.is_success(), "{:?}", members.diagnostics().items());
    let missing: Vec<String> = members
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

    // 5. doctest 错误回锚定与宏展开链 note。
    let doctest_err = compile(
        &compiler,
        CompileRequest::single_file_test(
            "main.gg",
            "/// 文档。\n/// ```gugu\n/// nope()\n/// ```\npub fn f() {}\nfn main() {}\n",
            TargetName::X86_64Linux,
        ),
    );
    assert!(!doctest_err.is_success());
    let error = doctest_err
        .diagnostics()
        .items()
        .iter()
        .find(|diagnostic| diagnostic.severity() == Severity::Error)
        .expect("doctest 必须产生错误");
    let span = error.span().expect("错误必须带主位置");
    assert_eq!(span.path().to_str().unwrap_or(""), "main.gg", "{span:?}");
    assert_eq!(span.line(), 2, "错误必须锚定到围栏行：{span:?}");
    assert!(
        doctest_err
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.severity() == Severity::Note
                && diagnostic.message().contains("宏调用位置")),
        "必须给出宏展开链 note：{:?}",
        doctest_err.diagnostics().items()
    );

    // 6. 生成的 doctest 模块不触发 missing_docs。
    let doctest_ok = compile(
        &compiler,
        CompileRequest::single_file_test(
            "main.gg",
            "//! 主模块文档\n/// 文档。\n/// ```gugu\n/// if 1 != 1 { panic(\"x\") }\n/// ```\npub fn f() {}\nfn main() {}\n",
            TargetName::X86_64Linux,
        ),
    );
    assert!(
        doctest_ok.is_success(),
        "{:?}",
        doctest_ok.diagnostics().items()
    );
    assert!(
        !doctest_ok
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code().to_string() == "E0063"),
        "生成模块不得触发 missing_docs"
    );
}
