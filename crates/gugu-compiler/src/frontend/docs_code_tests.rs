//! 文档示例守卫：`docs/src` 下所有 ```gugu 围栏必须是完整、可被前端 lex+parse 的 Gugu 源。
//!
//! 约定：`gugu` 块放用户可写的完整程序；签名清单、编译器内部形状、含 `{ ... }`
//! 省略体的示意片段与故意非法的代码一律使用 `text` 等其它信息串。

use std::path::{Path, PathBuf};

use crate::source::{SourceMap, SourceSnapshot};

use super::{lex, parse};

fn repository_docs_src() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../docs/src")
        .canonicalize()
        .expect("docs/src 必须存在")
}

fn collect_markdown_pages(dir: &Path, pages: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("docs/src 必须可读")
        .map(|entry| entry.expect("docs/src 目录项必须可读").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_markdown_pages(&path, pages);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            pages.push(path);
        }
    }
}

/// 提取一个页面里的 ```gugu 围栏，返回 (块首行 1 基行号, 块内容)。
/// 开栏/闭栏按 CommonMark 匹配：围栏内所有行都是内容，闭栏是长度不小于
/// 开栏、只含反引号的行；非 gugu 围栏同样被跟踪，其内容不参与匹配。
fn gugu_blocks(text: &str) -> Vec<(usize, String)> {
    let mut blocks: Vec<(usize, String)> = Vec::new();
    let mut open_fence: Option<(usize, bool)> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let backticks = trimmed
            .chars()
            .take_while(|character| *character == '`')
            .count();
        let rest = &trimmed[backticks..];
        if let Some((open_length, collecting)) = open_fence {
            if backticks >= open_length && rest.trim_matches('`').is_empty() {
                open_fence = None;
            } else if collecting && let Some((_, content)) = blocks.last_mut() {
                content.push_str(line);
                content.push('\n');
            }
            continue;
        }
        if backticks >= 3 {
            let collecting = rest.trim() == "gugu";
            open_fence = Some((backticks, collecting));
            if collecting {
                blocks.push((index + 2, String::new()));
            }
        }
    }
    blocks
}

fn collect_block_problems(label: &str, source: &str, problems: &mut Vec<String>) {
    let snapshot = SourceSnapshot::from_str(label, source).expect("块内容必须是 UTF-8");
    let map = SourceMap::new(vec![snapshot.clone()]).expect("块路径唯一");
    let file = map.file_id(label).expect("块路径已登记");
    let lexed = lex(&snapshot, &map, file);
    for diagnostic in &lexed.diagnostics {
        problems.push(format!("词法: {diagnostic:?}"));
    }
    let mut buffer = lexed.buffer;
    let parsed = parse(snapshot.content(), &map, file, &mut buffer);
    for diagnostic in &parsed.diagnostics {
        problems.push(format!("语法: {diagnostic:?}"));
    }
}

#[test]
fn every_gugu_fence_in_docs_parses() {
    let docs_src = repository_docs_src();
    let mut pages = Vec::new();
    collect_markdown_pages(&docs_src, &mut pages);
    assert!(
        pages.len() >= 33,
        "应找到全部书籍页面，实际 {}",
        pages.len()
    );

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for page in &pages {
        let text = std::fs::read_to_string(page).expect("书籍页面必须可读");
        let relative = page
            .strip_prefix(&docs_src)
            .expect("页面在 docs/src 下")
            .to_string_lossy()
            .replace('\\', "/");
        for (start_line, block) in gugu_blocks(&text) {
            checked += 1;
            let label = format!("{relative}#{start_line}");
            let mut problems = Vec::new();
            collect_block_problems(&label, &block, &mut problems);
            for problem in problems {
                failures.push(format!("{label}: {problem}"));
            }
        }
    }

    assert!(checked >= 40, "应至少检查 40 个 gugu 块，实际 {checked}");
    assert!(
        failures.is_empty(),
        "{} 个文档 gugu 块解析失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
