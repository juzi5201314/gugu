use gugu_compiler::{
    ActionKind, ActionStatus, CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName,
};

fn compile_to_lir(request: CompileRequest) -> gugu_compiler::Compilation {
    Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_to_lir(request)
}

fn generated(output: &gugu_compiler::Compilation, marker: &str) -> String {
    output
        .source_map()
        .snapshots()
        .iter()
        .find(|snapshot| snapshot.logical_path().contains(marker))
        .map(|snapshot| snapshot.content().to_owned())
        .unwrap_or_else(|| panic!("缺少生成模块 `{marker}`"))
}

fn names_table(manifest: &str) -> Vec<String> {
    manifest
        .split("pub fn __gugu_test_name")
        .nth(1)
        .expect("缺少 __gugu_test_name 表")
        .split("pub fn __gugu_test_flags")
        .next()
        .expect("缺少 __gugu_test_flags 表")
        .lines()
        .filter(|line| line.contains("=>") && line.contains('"'))
        .filter(|line| !line.trim_start().starts_with('_'))
        .map(|line| {
            line.split('"')
                .nth(1)
                .expect("name 表项缺少字符串")
                .to_owned()
        })
        .collect()
}

#[test]
fn test_domain_collects_sorts_and_wires_runner() {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file_test(
            "main.gg",
            include_str!("../fixtures/harness_tests.gg"),
            TargetName::X86_64Linux,
        ),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());

    let manifest = generated(&output, "__gugu_harness");
    // 按身份排序的收集表：非 doctest 用例在 doctest 前缀项之前按字典序稳定。
    let names = names_table(&manifest);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "测试表必须按身份排序");
    assert!(names.iter().any(|n| n == "main.aardvark_panics_eq"));
    assert!(names.iter().any(|n| n == "main.cfg_test_only"));
    assert!(names.iter().any(|n| n == "main.mid_panics"));
    assert!(names.iter().any(|n| n == "main.result_ok"));
    assert!(names.iter().any(|n| n == "main.zulu"));
    assert!(names.iter().any(|n| n == "main.alpha_ignored"));
    assert!(names.iter().any(|n| n.starts_with("doctest:")));
    // flags 位：ignore=1、should_panic=2、expect=4。
    assert!(manifest.contains("__gugu_test_flags"));
    assert!(manifest.contains("__gugu_test_expect"));
    assert!(manifest.contains("__synthetic_panic"));
    assert!(manifest.contains("__run_tests"));
    assert!(!manifest.contains("+ __run_bench("));

    // doctest 模块生成：gugu 围栏包装为测试，独立程序围栏保留 fn main 重命名。
    let doctest = generated(&output, "__gugu_doctest_");
    assert!(doctest.contains("fn __gugu_dt_case"), "{doctest}");

    // LIR 真实编译：生成的用例表、spawn 调度与 panic 边界进入最终世界。
    let lir = output.dump_lir().expect("成功编译必须有 LIR");
    for marker in [
        "__gugu_test_spawn",
        "__gugu_test_name",
        "__gugu_test_flags",
        "__gugu_test_expect",
        "join_panic",
        "__gugu_dt_case",
    ] {
        assert!(lir.contains(marker), "LIR 缺少 `{marker}`");
    }
    assert!(output.image_plan().is_some());
}

#[test]
fn bench_domain_collects_benches() {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file_bench(
            "bench.gg",
            include_str!("../fixtures/harness_bench.gg"),
            TargetName::X86_64Linux,
        ),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let manifest = generated(&output, "__gugu_harness");
    assert!(manifest.contains("__gugu_bwrap_0"));
    assert!(manifest.contains("__gugu_bwrap_1"));
    assert!(manifest.contains("__bench_run"));
    assert!(manifest.contains("__gugu_bench_spawn"));
    assert!(manifest.contains("let failed = __run_bench("));
    assert!(!manifest.contains("let failed = __run_tests("));
    // bench 域不生成 doctest 模块：fixture 带 gugu 围栏时 `#[test]` 项会被裁除，
    // manifest 继续引用它们就会构建失败（回归）。
    assert!(
        !output
            .source_map()
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.logical_path().contains("__gugu_doctest")),
        "bench 域不得生成 __gugu_doctest_ 模块"
    );
    assert!(output.dump_lir().is_some());
    assert!(output.image_plan().is_some());
}

#[test]
fn project_library_and_test_targets_use_the_requested_harness_domain() {
    use gugu_compiler::{Package, Target, TargetKind};

    let directory =
        std::env::temp_dir().join(format!("gugu-harness-domains-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let root = directory.as_path();
    let entry = root.join("lib.gg");
    std::fs::write(&entry, include_str!("../fixtures/harness_library.gg")).unwrap();
    let package = Package::new(
        root.to_path_buf(),
        root.join("gugu.toml"),
        None,
        "library".to_owned(),
        "0.1.0".to_owned(),
        vec![],
        vec![],
    );
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    for (kind, test) in [
        (TargetKind::Lib, true),
        (TargetKind::Lib, false),
        (TargetKind::Test, false),
    ] {
        let target = Target::new(
            kind,
            "library".to_owned(),
            entry.clone(),
            root.to_path_buf(),
            vec![],
            true,
        );
        let request = CompileRequest::project_target(
            &package,
            &target,
            "library",
            TargetName::X86_64Linux,
            vec![],
            Default::default(),
        );
        let request = if test {
            request.with_test_domain()
        } else {
            request.with_bench_domain()
        };
        let output = compiler.compile_sema_for_test(request);
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        let manifest = generated(&output, "__gugu_harness");
        assert_eq!(manifest.contains("library_test as"), test);
        assert_eq!(manifest.contains("library_bench as"), !test);
        assert_eq!(manifest.contains("let failed = __run_tests("), test);
        assert_eq!(manifest.contains("let failed = __run_bench("), !test);
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn non_test_domain_drops_marked_items() {
    // 非测试构建中 #[test]/#[bench] 项按 cfg 规则裁除：不收集、不生成 manifest。
    let output = compile_to_lir(CompileRequest::single_file(
        "main.gg",
        "fn main() {}\n#[test]\nfn skipped() { panic(\"x\") }\n#[bench]\nfn b() {}\n",
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    assert!(
        !output
            .source_map()
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.logical_path().contains("__gugu_")),
        "非 harness 域不得生成 __gugu_ 模块"
    );
    let stages = output.action_graph().nodes();
    let build_ir = stages
        .iter()
        .position(|stage| stage.kind() == ActionKind::BuildIr)
        .expect("action graph 必须登记 BuildIr");
    assert_eq!(stages[build_ir].status(), ActionStatus::Complete);
    assert!(
        stages[build_ir + 1..]
            .iter()
            .all(|stage| stage.status() == ActionStatus::Skipped)
    );
    assert!(output.image_plan().is_none());
    assert!(output.action_key().is_none());
}

#[test]
fn harness_invalid_markers_and_signatures_fail_compile() {
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);

    let bad_test = compiler.compile_sema_for_test(CompileRequest::single_file_test(
        "main.gg",
        "#[test]\nfn bad(arg: int) {}\n",
        TargetName::X86_64Linux,
    ));
    assert!(!bad_test.is_success());
    let text = format!("{:?}", bad_test.diagnostics().items());
    assert!(text.contains("无参数"), "{text}");

    let dup = compiler.compile_sema_for_test(CompileRequest::single_file_test(
        "main.gg",
        "#[test]\nfn dup() {}\n#[test]\nfn dup() {}\n",
        TargetName::X86_64Linux,
    ));
    assert!(!dup.is_success());
    let text = format!("{:?}", dup.diagnostics().items());
    assert!(text.contains("重复的测试/benchmark 身份"), "{text}");

    let non_fn = compiler.compile_sema_for_test(CompileRequest::single_file_test(
        "main.gg",
        "#[test]\nstruct Marker { x: int }\n",
        TargetName::X86_64Linux,
    ));
    assert!(!non_fn.is_success());

    let bad_bench = compiler.compile_sema_for_test(CompileRequest::single_file_bench(
        "bench.gg",
        "#[bench]\nfn bad() {}\n",
        TargetName::X86_64Linux,
    ));
    assert!(!bad_bench.is_success());
    let text = format!("{:?}", bad_bench.diagnostics().items());
    assert!(text.contains("#[bench] 签名"), "{text}");
}
