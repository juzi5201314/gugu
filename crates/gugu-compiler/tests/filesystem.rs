//! 只编译内存中的固定输入并检查镜像，不运行文件操作或子进程。
use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

#[test]
fn filesystem_api_links_linux() {
    check(TargetName::X86_64Linux);
}

#[test]
fn filesystem_api_links_windows() {
    check(TargetName::X86_64Windows);
}

#[test]
fn filesystem_api_links_macos() {
    check(TargetName::X86_64Macos);
}

#[test]
fn borrowed_array_slice_preserves_referent() {
    let output =
        Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/array_slice.gg"),
            TargetName::X86_64Linux,
        ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.dump_lir().expect("LIR");
    let body = lir
        .split("fn borrow_array ")
        .nth(1)
        .expect("借用转换函数")
        .split("\n}\n")
        .next()
        .unwrap();
    let call = body
        .lines()
        .find(|line| line.trim_start().starts_with("invoke Instance("))
        .expect("切片写入调用");
    let argument = call
        .split("[ValueId(")
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap();
    let origin = body
        .lines()
        .find(|line| {
            line.trim_start()
                .starts_with(&format!("v{argument} ValueType"))
        })
        .expect("实参 provenance");
    assert!(
        !origin.contains("origin=Stack("),
        "切片指针必须是引用指向的数组，不能是保存引用的栈槽：{origin}"
    );
}

fn check(target: TargetName) {
    let source = include_str!("fixtures/fs_api.gg");
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS)
        .compile(CompileRequest::single_file("main.gg", source, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let plan = output.image_plan().expect("完整 API 必须形成镜像");
    let bytes = match target {
        TargetName::X86_64Linux => plan.linux_image(),
        TargetName::X86_64Windows => plan.windows_image(),
        TargetName::X86_64Macos => plan.macos_image(),
        TargetName::Aarch64Linux | TargetName::Aarch64Macos => unreachable!(),
    };
    assert!(!bytes.is_empty());
    let lir = output.dump_lir().expect("LIR");
    assert!(
        lir.contains("fn next_entry "),
        "目录迭代必须链接实际读取实现"
    );
    assert!(lir.contains("FsStart"), "文件操作必须提交原生 worker");
    assert!(
        lir.contains("Runtime(StringCompare)"),
        "字符串不能按 backing 地址比较"
    );
    assert!(
        lir.contains("Runtime(ValueForget)"),
        "内部资源移动必须转移来源租约"
    );
}
