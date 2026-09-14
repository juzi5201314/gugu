//! 文档链接与章节锚点契约。
//!
//! `docs/src` 的跨文件引用依赖标题 id；标题一旦改写，引用就会静默失效，而这种失效
//! 既不会让 mdBook 构建失败，也不会被编译测试发现。本测试逐条解析 markdown 链接、
//! 复算每个标题的 id，并把「被引用的标题必须声明显式锚点」作为可执行约束固定下来。
//!
//! 测试是确定性的、纯文件解析的，不构建 book、不访问网络、不启动子进程。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// 仓库根目录：`CARGO_MANIFEST_DIR` 指向 `crates/gugu-compiler`。
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/gugu-compiler 必须位于仓库根下")
        .to_path_buf()
}

/// `docs/src` 下的全部 markdown，键是相对 `docs/src` 的路径。
fn document_sources() -> BTreeMap<String, String> {
    let root = repository_root().join("docs/src");
    walk(&root, &root)
}

fn walk(directory: &Path, root: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} 必须可读: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("目录项必须可读").path();
        if path.is_dir() {
            files.extend(walk(&path, root));
        } else if path.extension().is_some_and(|extension| extension == "md") {
            let relative = path
                .strip_prefix(root)
                .expect("文档必须位于 docs/src 内")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{} 必须是 UTF-8: {error}", path.display()));
            files.insert(relative, text);
        }
    }
    files
}

fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// 去掉行内代码，避免把 `` `chan[T](n)` `` 这类示例误当成链接。
fn strip_inline_code(line: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    let mut stripped = String::with_capacity(line.len());
    let mut index = 0;
    while index < characters.len() {
        if characters[index] != '`' {
            stripped.push(characters[index]);
            index += 1;
            continue;
        }
        let opening = index;
        while index < characters.len() && characters[index] == '`' {
            index += 1;
        }
        let fence = index - opening;
        let mut probe = index;
        let mut closing = None;
        while probe < characters.len() {
            if characters[probe] == '`' {
                let start = probe;
                while probe < characters.len() && characters[probe] == '`' {
                    probe += 1;
                }
                if probe - start == fence {
                    closing = Some(probe);
                    break;
                }
            } else {
                probe += 1;
            }
        }
        match closing {
            Some(end) => index = end,
            None => {
                // 未闭合的围栏按字面保留，与 CommonMark 的段落级回退一致。
                stripped.extend(&characters[opening..index]);
            }
        }
    }
    stripped
}

/// mdBook 的标题 id 规则：小写；保留字母数字与 `-`、`_`；空白转 `-`；其余字符丢弃。
fn auto_id(heading: &str) -> String {
    let mut id = String::with_capacity(heading.len());
    for character in heading.to_lowercase().chars() {
        if character.is_alphanumeric() || character == '-' || character == '_' {
            id.push(character);
        } else if character == ' ' || character == '\t' {
            id.push('-');
        }
    }
    id
}

/// 拆出标题正文与 `{#id ...}` 属性块里的显式 id。
fn split_heading_attributes(heading: &str) -> (&str, Option<&str>) {
    let trimmed = heading.trim_end();
    let Some(open) = trimmed.strip_suffix('}').and_then(|_| trimmed.rfind('{')) else {
        return (trimmed, None);
    };
    let body = &trimmed[open + 1..trimmed.len() - 1];
    if body.contains(['{', '}', '<', '>', '\\']) {
        return (trimmed, None);
    }
    let mut explicit = None;
    for token in body.split_whitespace() {
        if let Some(id) = token.strip_prefix('#')
            && !id.is_empty()
        {
            explicit = Some(id);
        }
    }
    (trimmed[..open].trim_end(), explicit)
}

/// 一个页面里由标题产生的 id，以及哪些 id 是被显式锚点固定的。
#[derive(Default)]
struct PageIds {
    all: BTreeSet<String>,
    explicit: BTreeSet<String>,
}

fn page_ids(text: &str) -> PageIds {
    let mut ids = PageIds::default();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let heading = rest.trim_start_matches('#');
        if !heading.starts_with(' ') && !heading.starts_with('\t') {
            continue;
        }
        let (body, explicit) = split_heading_attributes(heading.trim());
        match explicit {
            Some(id) => {
                ids.all.insert(id.to_owned());
                ids.explicit.insert(id.to_owned());
            }
            None => {
                ids.all.insert(auto_id(body));
            }
        }
    }
    ids
}

/// 一行里 `](...)` 形式的链接目标，跳过行内代码与含空白的伪目标。
fn link_targets(line: &str) -> Vec<String> {
    let line = strip_inline_code(line);
    let characters: Vec<char> = line.chars().collect();
    let mut targets = Vec::new();
    let mut index = 0;
    while index + 1 < characters.len() {
        if characters[index] == ']' && characters[index + 1] == '(' {
            let start = index + 2;
            let end = characters[start..]
                .iter()
                .position(|character| *character == ')')
                .map_or(characters.len(), |offset| start + offset);
            let target: String = characters[start..end].iter().collect();
            if !target.is_empty() && !target.contains(char::is_whitespace) {
                targets.push(target);
            }
            index = end;
        }
        index += 1;
    }
    targets
}

fn percent_decode(target: &str) -> String {
    let bytes = target.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                (bytes[index + 1] as char).to_digit(16),
                (bytes[index + 2] as char).to_digit(16),
            )
        {
            decoded.push((high * 16 + low) as u8);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// 把链接目标解析成 `docs/src` 内的页面键与片段。
fn resolve(page: &str, target: &str) -> Option<(String, String)> {
    if target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
    {
        return None;
    }
    let (path, fragment) = match target.split_once('#') {
        Some((path, fragment)) => (path, fragment),
        None => (target, ""),
    };
    if path.is_empty() {
        return Some((page.to_owned(), percent_decode(fragment)));
    }
    let path = percent_decode(path);
    if !path.ends_with(".md") {
        // 指向源码文件的链接不进 book，由其他契约覆盖。
        return None;
    }
    let directory = Path::new(page).parent().unwrap_or(Path::new(""));
    let joined = directory.join(path);
    let mut normalized = Vec::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str().to_string_lossy().replace('\\', "/")),
        }
    }
    Some((normalized.join("/"), percent_decode(fragment)))
}

struct DocumentLink {
    page: String,
    line: usize,
    target: String,
    destination: String,
    fragment: String,
}

fn document_links(sources: &BTreeMap<String, String>) -> Vec<DocumentLink> {
    let mut links = Vec::new();
    for (page, text) in sources {
        let mut in_fence = false;
        for (index, line) in text.lines().enumerate() {
            if is_fence(line) {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            for target in link_targets(line) {
                if let Some((destination, fragment)) = resolve(page, &target) {
                    links.push(DocumentLink {
                        page: page.clone(),
                        line: index + 1,
                        target,
                        destination,
                        fragment,
                    });
                }
            }
        }
    }
    links
}

#[test]
fn every_document_link_resolves_to_a_heading() {
    let sources = document_sources();
    let ids: BTreeMap<&str, PageIds> = sources
        .iter()
        .map(|(page, text)| (page.as_str(), page_ids(text)))
        .collect();
    let mut problems = Vec::new();
    for link in document_links(&sources) {
        let Some(page) = ids.get(link.destination.as_str()) else {
            problems.push(format!(
                "{}:{} 链接 `{}` 指向 docs/src 外的页面 `{}`",
                link.page, link.line, link.target, link.destination
            ));
            continue;
        };
        if !link.fragment.is_empty() && !page.all.contains(&link.fragment) {
            problems.push(format!(
                "{}:{} 链接 `{}` 的锚点 `#{}` 在 `{}` 中不存在",
                link.page, link.line, link.target, link.fragment, link.destination
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "文档链接失效 {} 条：\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn referenced_headings_declare_explicit_anchors() {
    let sources = document_sources();
    let ids: BTreeMap<&str, PageIds> = sources
        .iter()
        .map(|(page, text)| (page.as_str(), page_ids(text)))
        .collect();
    let mut problems = Vec::new();
    for link in document_links(&sources) {
        if link.fragment.is_empty() {
            continue;
        }
        let Some(page) = ids.get(link.destination.as_str()) else {
            continue;
        };
        if page.explicit.contains(&link.fragment) {
            continue;
        }
        problems.push(format!(
            "{}:{} 链接 `{}` 依赖 `{}` 的自动 slug；被引用的标题必须写成 `## 标题 {{#{}}}`",
            link.page, link.line, link.target, link.destination, link.fragment
        ));
    }
    assert!(
        problems.is_empty(),
        "被引用标题缺少显式锚点 {} 处：\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn explicit_anchors_are_unique_and_do_not_shadow_auto_ids() {
    let sources = document_sources();
    let mut problems = Vec::new();
    for (page, text) in &sources {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut in_fence = false;
        for (index, line) in text.lines().enumerate() {
            if is_fence(line) {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            let Some(rest) = line.strip_prefix('#') else {
                continue;
            };
            let heading = rest.trim_start_matches('#');
            if !heading.starts_with(' ') && !heading.starts_with('\t') {
                continue;
            }
            let (body, explicit) = split_heading_attributes(heading.trim());
            let id = explicit.map_or_else(|| auto_id(body), str::to_owned);
            if let Some(previous) = seen.insert(id.clone(), index + 1) {
                problems.push(format!(
                    "{page}:{} 与 {page}:{previous} 产生重复的标题 id `{id}`，mdBook 会静默输出重复锚点",
                    index + 1
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "标题 id 冲突 {} 处：\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn document_sources_cover_the_whole_book() {
    let sources = document_sources();
    assert!(
        sources.len() >= 30,
        "docs/src 必须至少包含 30 个页面，实际 {}",
        sources.len()
    );
    for required in [
        "SUMMARY.md",
        "spec/runtime.md",
        "spec/standard-library.md",
        "internals/gc-metadata.md",
    ] {
        assert!(sources.contains_key(required), "缺少文档页面 {required}");
    }
    let root = repository_root();
    assert!(
        root.join("book.toml").is_file(),
        "{} 必须包含 book.toml",
        root.display()
    );
}
