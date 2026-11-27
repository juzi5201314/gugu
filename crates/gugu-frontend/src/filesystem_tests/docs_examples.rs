//! `docs/src/` 的全部 ````gugu` / ````gugu-test` 围栏示例必须可编译、可运行。
//!
//! 文档围栏是唯一真实的示例集合：本测试逐块提取并在进程内编译，失败的块带
//! 文件名、序号与行号报错。约定（标注写在围栏前一行，mdBook 不渲染）：
//!
//! - 无标注：编译至已验证 LIR。无 `fn main` 的声明片段补空入口。`gugu-test` 在普通域
//!   编译同一源码（`#[test]` 项在 bin 域是普通函数），域语义由 spec/testing.md 负责。
//! - `<!-- docs-example: parse -->`：只要求语法合法，用于语言/标准库自身声明的
//!   展示（预导入 trait、lang item）与刻意省略上下文的骨架。
//! - `<!-- docs-example: gap 诊断片段 -->`：已知实现缺口，必须仍以该诊断失败；
//!   实现补齐后本测试失败，提醒删除标注。
//! - 围栏前一行 `` `name.gg`： `` 把连续的块组成一个多文件包，在 `main.gg` 处整体编译。
//! - 紧随程序之后、标注 `<!-- docs-example: output -->` 的 ````text` 围栏是该程序的
//!   标准输出：程序在 LIR 解释器中执行，捕获的 stdout 必须逐字节一致。
//!
//! 每个用例只取少量块以落入默认套件的 3 秒上限；`registry_covers_every_block`
//! 保证新增围栏必须登记。
mod extract;

use std::{collections::BTreeSet, fs, ops::RangeInclusive, path::PathBuf};

use self::extract::{Block, Mode, blocks};
use super::{harness::Harness, printing::capture};
use crate::{
    Compilation, CompileRequest, Compiler, Project, TargetKind, TargetName, format_source,
};

fn docs_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/gugu-frontend 必须位于仓库根下")
        .join("docs/src")
}

fn read(relative: &str) -> Vec<Block> {
    let text = fs::read_to_string(docs_root().join(relative))
        .unwrap_or_else(|error| panic!("{relative} 必须可读: {error}"));
    blocks(&text)
}

fn check(relative: &str, range: RangeInclusive<usize>) {
    let all = read(relative);
    for block in all.iter().filter(|block| range.contains(&block.ordinal)) {
        let at = format!(
            "{relative} 第 {} 个围栏（第 {} 行）",
            block.ordinal, block.line
        );
        match &block.mode {
            Mode::Parse => {
                if let Err(error) = format_source("docs_example.gg", &block.code) {
                    panic!("{at} 语法错误: {error:?}");
                }
            }
            Mode::Gap(message) => {
                let output = compile_single(block);
                assert!(
                    !output.is_success(),
                    "{at} 已能编译：删除 gap 标注并把它当作普通示例检查"
                );
                assert!(
                    output
                        .diagnostics()
                        .items()
                        .iter()
                        .any(|item| item.message().contains(message.as_str())),
                    "{at} 的失败原因已变化（期望含 `{message}`）: {:?}",
                    output.diagnostics().items()
                );
            }
            Mode::Compile => match block.file.as_deref() {
                Some("main.gg") => run(
                    &at,
                    block,
                    &compile_package(&all, block, Compiler::compile_to_lir),
                ),
                Some(_) => {}
                None => run(&at, block, &compile_single(block)),
            },
        }
    }
}

fn compile_single(block: &Block) -> Compilation {
    let mut source = block.code.clone();
    if !source.contains("fn main") {
        source.push_str("\nfn main() {}\n");
    }
    Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ))
}

pub(super) fn compile_overview_image() -> Compilation {
    let all = read("spec/overview.md");
    let main = all
        .iter()
        .find(|block| block.file.as_deref() == Some("main.gg"))
        .expect("概述示例必须包含 main.gg");
    compile_package(&all, main, Compiler::compile_full_for_test)
}

/// 与 `main.gg` 相邻的带标签块组成一个包，在临时目录中按项目 bin target 编译。
fn compile_package(
    all: &[Block],
    main: &Block,
    compile: fn(&Compiler, CompileRequest) -> Compilation,
) -> Compilation {
    let index = main.ordinal - 1;
    let start = (0..index)
        .rev()
        .take_while(|&at| all[at].file.is_some())
        .last()
        .unwrap_or(index);
    let end = (index..all.len())
        .take_while(|&at| all[at].file.is_some())
        .last()
        .unwrap_or(index);
    let root = tempfile::tempdir().expect("tempdir");
    let src = root.path().join("src");
    fs::create_dir(&src).expect("create src");
    fs::write(
        root.path().join("gugu.toml"),
        "[package]\nname = \"docs\"\nversion = \"0.0.0\"\n",
    )
    .expect("write manifest");
    for block in &all[start..=end] {
        let file = block.file.as_deref().expect("包内块都有文件标签");
        fs::write(src.join(file), &block.code).expect("write module");
    }
    let project = Project::discover(root.path()).expect("discover project");
    let package = project.current_package().expect("current package");
    let target = package
        .targets()
        .iter()
        .find(|target| target.kind() == TargetKind::Bin)
        .expect("bin target");
    compile(
        &Compiler::shared(),
        CompileRequest::project_target(
            package,
            target,
            "docs@0.0.0 (path+.)",
            TargetName::X86_64Linux,
            vec!["default".to_owned()],
            BTreeSet::new(),
        ),
    )
}

fn run(at: &str, block: &Block, output: &Compilation) {
    assert!(
        output.is_success(),
        "{at} 编译失败: {:?}",
        output.diagnostics().items()
    );
    let Some(expected) = &block.output else {
        return;
    };
    let mut harness = Harness::new(output);
    harness.machine_hook = Some(capture);
    let entry = harness
        .names
        .values()
        .find(|(path, name)| {
            *name == "main" && path.ends_with("main.gg") && !path.starts_with("std/")
        })
        .map(|(path, _)| (*path).to_owned())
        .expect("入口 main.gg::main");
    harness.invoke(&entry, "main", &[], &mut vec![0; 262144]);
    let stdout: Vec<u8> = harness
        .calls
        .iter()
        .filter(|(fd, _)| *fd == 1)
        .flat_map(|(_, bytes)| bytes.iter().map(|byte| *byte as u8))
        .collect();
    assert_eq!(
        std::str::from_utf8(&stdout).unwrap_or_else(|error| panic!("{at} 输出无效: {error}")),
        expected.as_str(),
        "{at} 的输出与文档不符"
    );
}

macro_rules! doc_example_groups {
    ($(
        $group:ident {
            $($file:literal: $start:literal ..= $end:literal;)*
        }
    )*) => {
        const REGISTERED: &[(&str, usize, usize)] = &[
            $($(($file, $start, $end),)*)*
        ];
        $(
            #[test]
            fn $group() {
                $(check($file, $start..=$end);)*
            }
        )*
    };
}

// 按组在单进程内复用 `Compiler::shared()` 与已解码内建前缀，避免每 1–2 个文档块起一个独立进程；
// 同时保持每组编译量在 2–4 个块以内，使每组稳定落在默认 3 秒单测上限内。
doc_example_groups! {
    guide_comptime_macros {
        "guide/comptime-macros.md": 1..=3;
    }
    guide_concurrency_1 {
        "guide/concurrency.md": 1..=2;
    }
    guide_concurrency_2 {
        "guide/concurrency.md": 3..=4;
    }
    guide_ffi_and_testing {
        "guide/ffi-unsafe.md": 1..=2;
        "guide/testing.md": 1..=1;
    }
    guide_getting_started {
        "guide/getting-started.md": 1..=3;
    }
    guide_values_handles {
        "guide/values-handles.md": 1..=4;
    }
    reference_cheatsheet_and_traits {
        "reference/syntax-cheatsheet.md": 1..=3;
        "reference/types-traits.md": 1..=1;
    }
    spec_comptime_1 {
        "spec/comptime.md": 1..=3;
    }
    spec_comptime_2_and_concurrency {
        "spec/comptime.md": 4..=5;
        "spec/concurrency.md": 1..=1;
    }
    spec_declarations_and_expressions_1 {
        "spec/declarations.md": 1..=4;
        "spec/expressions.md": 1..=4;
    }
    spec_expressions_2 {
        "spec/expressions.md": 5..=8;
    }
    spec_functions {
        "spec/functions.md": 1..=3;
    }
    spec_lexical {
        "spec/lexical.md": 1..=3;
    }
    spec_overview {
        "spec/overview.md": 1..=2;
    }
    spec_passing_1 {
        "spec/passing.md": 1..=3;
    }
    spec_passing_2_and_patterns {
        "spec/passing.md": 4..=5;
        "spec/patterns.md": 1..=3;
    }
    spec_platform_and_program_model {
        "spec/platform-abi.md": 1..=1;
        "spec/program-model.md": 1..=2;
    }
    spec_stdlib_testing_and_unsafe {
        "spec/standard-library.md": 1..=1;
        "spec/testing.md": 1..=1;
        "spec/unsafe.md": 1..=1;
    }
    spec_traits_1 {
        "spec/traits.md": 1..=4;
    }
    spec_traits_2 {
        "spec/traits.md": 5..=8;
    }
    spec_types {
        "spec/types.md": 1..=5;
    }
    parse_only_pages {
        "internals/comptime-analysis.md": 1..=1;
        "spec/memory.md": 1..=1;
    }
}

fn markdown_files(directory: &std::path::Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("docs 目录可读") {
        let path = entry.expect("目录项").path();
        if path.is_dir() {
            markdown_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
}

#[test]
fn registry_covers_every_block() {
    let root = docs_root();
    let mut files = Vec::new();
    markdown_files(&root, &mut files);
    let mut missing = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("docs 内路径")
            .to_string_lossy()
            .replace('\\', "/");
        let count = read(&relative).len();
        let mut next = 1;
        for &(_, start, end) in REGISTERED.iter().filter(|(file, ..)| *file == relative) {
            assert_eq!(start, next, "{relative} 的登记区间必须从 {next} 连续开始");
            next = end + 1;
        }
        if next != count + 1 {
            missing.push(format!(
                "{relative}: 已登记 {} / 共 {count} 个围栏",
                next - 1
            ));
        }
    }
    assert!(missing.is_empty(), "docs 围栏未完整登记: {missing:#?}");
}
