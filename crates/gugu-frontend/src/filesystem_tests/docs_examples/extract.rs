//! 从 mdBook 源码提取 ````gugu` / ````gugu-test` 围栏与紧邻的检查标注。

/// 块的检查方式；由围栏前一行的 `<!-- docs-example: ... -->` 决定。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Mode {
    /// 完整编译；无 `fn main` 的声明片段补空入口。
    Compile,
    /// 只要求语法合法：语言/标准库自身声明的展示或刻意不完整的骨架。
    Parse,
    /// 已知实现缺口：必须带该诊断片段失败，修好后标注必须同步删掉。
    Gap(String),
}

#[derive(Clone, Debug)]
pub(super) struct Block {
    pub(super) ordinal: usize,
    pub(super) line: usize,
    pub(super) mode: Mode,
    /// 围栏前一行 `` `name.gg`： `` 给出的多文件示例模块名。
    pub(super) file: Option<String>,
    pub(super) code: String,
    /// 紧随其后、标注为 `output` 的 ````text` 围栏：完整程序的标准输出。
    pub(super) output: Option<String>,
}

const MARKER: &str = "<!-- docs-example:";

pub(super) fn blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let info = lines[index].trim_start().strip_prefix("```");
        let Some(info) = info.map(str::trim) else {
            index += 1;
            continue;
        };
        let (marker, label) = context(&lines[..index]);
        let end = closing(&lines, index);
        let code: String = lines[index + 1..end]
            .iter()
            .flat_map(|line| [*line, "\n"])
            .collect();
        match info {
            "gugu" | "gugu-test" => blocks.push(Block {
                ordinal: blocks.len() + 1,
                line: index + 1,
                mode: mode(marker, index),
                file: label,
                code,
                output: None,
            }),
            "text" if marker == Some("output") => {
                let program = program(&mut blocks, index);
                assert!(
                    program.output.is_none(),
                    "第 {} 行：重复的 output",
                    index + 1
                );
                program.output = Some(code);
            }
            _ => assert!(
                marker.is_none(),
                "第 {} 行：标注只能修饰 gugu 围栏",
                index + 1
            ),
        }
        index = end + 1;
    }
    blocks
}

/// output 归属最近的程序；多文件包归属其 `main.gg`。
fn program(blocks: &mut [Block], index: usize) -> &mut Block {
    let last = blocks
        .len()
        .checked_sub(1)
        .expect("output 标注必须跟在 gugu 围栏之后");
    let main = (0..=last)
        .rev()
        .take_while(|&at| blocks[at].file.is_some())
        .find(|&at| blocks[at].file.as_deref() == Some("main.gg"));
    assert!(
        main.is_some() || blocks[last].file.is_none(),
        "第 {} 行：多文件包缺少 main.gg",
        index + 1
    );
    &mut blocks[main.unwrap_or(last)]
}

fn closing(lines: &[&str], open: usize) -> usize {
    (open + 1..lines.len())
        .find(|&line| lines[line].trim_start().starts_with("```"))
        .unwrap_or_else(|| panic!("第 {} 行的围栏没有闭合", open + 1))
}

/// 围栏前的最近非空行：标注注释或多文件标签（标签可位于标注之前）。
fn context<'a>(before: &[&'a str]) -> (Option<&'a str>, Option<String>) {
    let mut previous = before
        .iter()
        .rev()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());
    let mut marker = None;
    let mut line = previous.next();
    if let Some(text) = line.and_then(|text| text.strip_prefix(MARKER)) {
        marker = Some(text.strip_suffix("-->").expect("标注必须单行闭合").trim());
        line = previous.next();
    }
    let label = line
        .and_then(|text| text.strip_prefix('`'))
        .and_then(|text| text.strip_suffix("`：").or_else(|| text.strip_suffix("`:")))
        .filter(|name| name.ends_with(".gg"))
        .map(str::to_owned);
    (marker, label)
}

fn mode(marker: Option<&str>, index: usize) -> Mode {
    match marker {
        None => Mode::Compile,
        Some("parse") => Mode::Parse,
        Some(text) => match text.strip_prefix("gap ") {
            Some(message) => Mode::Gap(message.trim().to_owned()),
            None => panic!("第 {} 行：未知标注 `{text}`", index + 1),
        },
    }
}
