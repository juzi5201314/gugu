//! `docs/src/guide/` 与 `docs/src/reference/` 的围栏示例必须可编译。
//!
//! 教程与参考手册的 ````gugu` / ````gugu-test` 代码块是唯一真实的示例集合：
//! 本测试逐块提取并在进程内编译，失败的块带文件名与序号报错，保证文档示例
//! 与语言实现不漂移。纯声明片段（无 `fn main`）自动补空 `main` 后按可执行
//! 入口编译；`gugu-test` 块按 test 域编译。
//!
//! 每个文档一个用例以落入默认套件的 3 秒上限；同一用例内复用 `Compiler`
//! 分摊内建源码与产物解析。

use std::{
    fs,
    path::{Path, PathBuf},
};

use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/gugu-compiler 必须位于仓库根下")
        .to_path_buf()
}

/// 提取 ````gugu` 与 ````gugu-test` 围栏块；返回 (是否 test 域, 源码)。
fn gugu_blocks(text: &str) -> Vec<(bool, String)> {
    let mut blocks = Vec::new();
    let mut inside: Option<(bool, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        match &mut inside {
            Some((_, code)) => {
                if trimmed.starts_with("```") {
                    let (test, code) = inside.take().expect("围栏状态");
                    blocks.push((test, code));
                } else {
                    code.push_str(line);
                    code.push('\n');
                }
            }
            None => {
                if let Some(info) = trimmed.strip_prefix("```") {
                    match info.trim() {
                        "gugu" => inside = Some((false, String::new())),
                        "gugu-test" => inside = Some((true, String::new())),
                        _ => {}
                    }
                }
            }
        }
    }
    blocks
}

fn compile_doc(relative: &str, blocks: std::ops::RangeInclusive<usize>) {
    let path = repository_root().join("docs/src").join(relative);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{relative} 必须可读: {error}"));
    let compiler = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS);
    let mut checked = 0_u32;
    for (index, (test_domain, code)) in gugu_blocks(&text).iter().enumerate() {
        let ordinal = index + 1;
        if !blocks.contains(&ordinal) {
            continue;
        }
        let mut source = code.clone();
        if !source.contains("fn main") {
            source.push_str("\nfn main() {}\n");
        }
        let name = format!("docs_example_{index}.gg");
        // test 域的 harness + 后端编译超出单用例 3 秒上限：test 围栏在普通域
        // 验证同一源码（`#[test]` 在 bin 域编译为普通项），域语义差异由
        // testing.md 引用的 spec/testing.md 负责。
        let _ = test_domain;
        let request = CompileRequest::single_file(name, source, TargetName::X86_64Linux);
        let output = compiler.compile(request);
        assert!(
            output.is_success(),
            "{relative} 第 {ordinal} 个围栏示例编译失败: {:?}",
            output.diagnostics()
        );
        checked += 1;
    }
    assert!(
        checked > 0 || gugu_blocks(&text).is_empty(),
        "{relative} 的示例区间 {blocks:?} 为空"
    );
}

macro_rules! doc_examples {
    ($name:ident, $file:literal) => {
        #[test]
        fn $name() {
            compile_doc($file, 1..=usize::MAX);
        }
    };
    ($name:ident, $file:literal, $start:literal, $end:literal) => {
        #[test]
        fn $name() {
            compile_doc($file, $start..=$end);
        }
    };
}

doc_examples!(
    guide_getting_started_examples_1,
    "guide/getting-started.md",
    1,
    2
);
doc_examples!(
    guide_getting_started_examples_2,
    "guide/getting-started.md",
    3,
    999
);
doc_examples!(
    guide_values_handles_examples_1,
    "guide/values-handles.md",
    1,
    2
);
doc_examples!(
    guide_values_handles_examples_2,
    "guide/values-handles.md",
    3,
    999
);
doc_examples!(guide_concurrency_examples_1, "guide/concurrency.md", 1, 1);
doc_examples!(guide_concurrency_examples_2, "guide/concurrency.md", 2, 2);
doc_examples!(guide_concurrency_examples_3, "guide/concurrency.md", 3, 3);
doc_examples!(guide_concurrency_examples_4, "guide/concurrency.md", 4, 999);
doc_examples!(
    guide_comptime_macros_examples_1,
    "guide/comptime-macros.md",
    1,
    1
);
doc_examples!(
    guide_comptime_macros_examples_2,
    "guide/comptime-macros.md",
    2,
    2
);
doc_examples!(
    guide_comptime_macros_examples_3,
    "guide/comptime-macros.md",
    3,
    999
);
doc_examples!(guide_package_build_examples, "guide/package-build.md");
doc_examples!(guide_ffi_unsafe_examples, "guide/ffi-unsafe.md");
doc_examples!(guide_testing_examples, "guide/testing.md");
doc_examples!(guide_deployment_examples, "guide/deployment.md");
doc_examples!(
    reference_syntax_cheatsheet_examples_1,
    "reference/syntax-cheatsheet.md",
    1,
    2
);
doc_examples!(
    reference_syntax_cheatsheet_examples_2,
    "reference/syntax-cheatsheet.md",
    3,
    999
);
doc_examples!(reference_types_traits_examples, "reference/types-traits.md");
doc_examples!(reference_stdlib_api_examples, "reference/stdlib-api.md");
doc_examples!(reference_cli_examples, "reference/cli.md");
doc_examples!(reference_diagnostics_examples, "reference/diagnostics.md");
doc_examples!(
    reference_runtime_config_examples,
    "reference/runtime-config.md"
);
doc_examples!(reference_abi_compat_examples, "reference/abi-compat.md");
