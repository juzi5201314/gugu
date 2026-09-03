use std::{
    cmp::Ordering,
    fmt,
    path::{Component, Path, PathBuf},
};

/// 单个源码文件允许的最大字节数（不含终止符）。
pub const MAX_SOURCE_BYTES: usize = u32::MAX as usize;

/// 编译 action 中的稳定源码文件编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceFileId(u32);

impl SourceFileId {
    /// 从稠密编号构造文件 ID。
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// 返回文件 ID 的稠密下标。
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// 返回文件 ID 的原始整数。
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// 源码宏展开记录的稳定 action 内编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExpansionId(u32);

impl ExpansionId {
    /// 根源码的展开 ID。
    pub const ROOT: Self = Self(0);

    /// 从稠密编号构造展开 ID。
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// 返回展开 ID 的稠密下标。
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// 返回展开 ID 的原始整数。
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// 源码宏可以插入的语法片段位置。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SourceSlot {
    /// 模块 item 列表。
    Item,
    /// 块语句列表。
    Statement,
    /// 表达式位置。
    Expression,
    /// 类型位置。
    Type,
    /// 模式位置。
    Pattern,
}

impl fmt::Display for SourceSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Item => "item",
            Self::Statement => "statement",
            Self::Expression => "expression",
            Self::Type => "type",
            Self::Pattern => "pattern",
        };
        formatter.write_str(name)
    }
}

/// 源码快照或逻辑路径校验失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceError {
    /// 源文件超过 `u32` 字节范围。
    TooLarge {
        /// 逻辑路径。
        path: String,
    },
    /// 源文件不是合法 UTF-8。
    InvalidUtf8 {
        /// 逻辑路径。
        path: String,
        /// 首个非法字节偏移。
        offset: u32,
    },
    /// 源文件含有 UTF-8 BOM。
    Bom {
        /// 逻辑路径。
        path: String,
    },
    /// 逻辑路径不是 package-relative UTF-8 路径。
    InvalidPath {
        /// 原始路径文本。
        path: String,
    },
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { path } => write!(formatter, "源文件 `{path}` 超过 u32 字节范围"),
            Self::InvalidUtf8 { path, offset } => {
                write!(formatter, "源文件 `{path}` 含有非法 UTF-8（字节 {offset}）")
            }
            Self::Bom { path } => write!(formatter, "源文件 `{path}` 不能包含 UTF-8 BOM"),
            Self::InvalidPath { path } => {
                write!(
                    formatter,
                    "源码逻辑路径 `{path}` 不是合法的 package-relative 路径"
                )
            }
        }
    }
}

impl std::error::Error for SourceError {}

/// UTF-8 源码中的一行一列位置，均从 1 开始。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineColumn {
    /// 行号。
    pub line: u32,
    /// 按 UTF-8 字节计的列号。
    pub column: u32,
}

/// 固定一次编译可见的 UTF-8 源码字节和逻辑路径。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshot {
    logical_path: String,
    path: PathBuf,
    content: String,
    content_hash: [u8; 32],
    line_starts: Vec<u32>,
}

impl SourceSnapshot {
    /// 从 UTF-8 字节创建源码快照。
    pub fn from_bytes(
        path: impl AsRef<Path>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<Self, SourceError> {
        let logical_path = normalize_logical_path(path.as_ref())?;
        let bytes = bytes.as_ref();
        if bytes.len() >= MAX_SOURCE_BYTES {
            return Err(SourceError::TooLarge { path: logical_path });
        }
        if bytes.starts_with(b"\xef\xbb\xbf") {
            return Err(SourceError::Bom { path: logical_path });
        }
        let content = std::str::from_utf8(bytes).map_err(|error| SourceError::InvalidUtf8 {
            path: logical_path.clone(),
            offset: checked_u32(error.valid_up_to()),
        })?;
        let line_starts = build_line_starts(bytes);
        Ok(Self {
            path: PathBuf::from(&logical_path),
            logical_path,
            content: content.to_owned(),
            content_hash: *blake3::hash(bytes).as_bytes(),
            line_starts,
        })
    }

    /// 从 UTF-8 字符串创建源码快照。
    pub fn from_str(path: impl AsRef<Path>, source: &str) -> Result<Self, SourceError> {
        Self::from_bytes(path, source.as_bytes())
    }

    /// 返回 package-relative 的规范逻辑路径。
    pub fn logical_path(&self) -> &str {
        &self.logical_path
    }

    /// 返回逻辑路径的 `Path` 视图。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 返回不可变源码文本。
    pub fn content(&self) -> &str {
        &self.content
    }

    /// 返回源码原始字节。
    pub fn bytes(&self) -> &[u8] {
        self.content.as_bytes()
    }

    /// 返回 BLAKE3-256 内容摘要。
    pub fn content_hash(&self) -> [u8; 32] {
        self.content_hash
    }

    /// 返回按 UTF-8 字节排列的规范行首表。
    pub fn line_starts(&self) -> &[u32] {
        &self.line_starts
    }

    /// 把半开字节偏移映射为 1-based 行列。
    pub fn line_column(&self, offset: usize) -> Result<LineColumn, SpanError> {
        if offset > self.content.len() {
            return Err(SpanError::OutOfBounds {
                path: self.logical_path.clone(),
                start: checked_u32(offset),
                end: checked_u32(offset),
                length: checked_u32(self.content.len()),
            });
        }
        let line = self
            .line_starts
            .partition_point(|&start| start as usize <= offset)
            - 1;
        let column = offset - self.line_starts[line] as usize;
        Ok(LineColumn {
            line: checked_u32(line + 1),
            column: checked_u32(column + 1),
        })
    }

    /// 创建属于根源码的半开 span。
    pub fn span(&self, file: SourceFileId, start: usize, end: usize) -> Result<Span, SpanError> {
        Span::from_snapshot(file, self, start, end, ExpansionId::ROOT)
    }
}

/// 源码范围的校验错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpanError {
    /// span 不是合法的半开范围。
    OutOfBounds {
        /// 逻辑路径。
        path: String,
        /// 起点。
        start: u32,
        /// 终点。
        end: u32,
        /// 源文件字节长度。
        length: u32,
    },
    /// 引用的文件 ID 不属于当前源码表。
    UnknownFile(SourceFileId),
    /// 引用的展开 ID 不属于当前源码表。
    UnknownExpansion(ExpansionId),
}

impl fmt::Display for SpanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds {
                path,
                start,
                end,
                length,
            } => write!(
                formatter,
                "span `{path}` [{start}, {end}) 超出 {length} 字节源码范围"
            ),
            Self::UnknownFile(file) => write!(formatter, "未知源码文件 ID {}", file.as_u32()),
            Self::UnknownExpansion(expansion) => {
                write!(formatter, "未知源码展开 ID {}", expansion.as_u32())
            }
        }
    }
}

impl std::error::Error for SpanError {}

/// 一个带源码身份和展开上下文的半开字节范围。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    file: SourceFileId,
    path: PathBuf,
    start: u32,
    end: u32,
    expansion: ExpansionId,
    line: u32,
    column: u32,
}

impl Span {
    fn from_snapshot(
        file: SourceFileId,
        snapshot: &SourceSnapshot,
        start: usize,
        end: usize,
        expansion: ExpansionId,
    ) -> Result<Self, SpanError> {
        if start > end || end > snapshot.content.len() {
            return Err(SpanError::OutOfBounds {
                path: snapshot.logical_path.clone(),
                start: checked_u32(start),
                end: checked_u32(end),
                length: checked_u32(snapshot.content.len()),
            });
        }
        let position = snapshot.line_column(start)?;
        Ok(Self {
            file,
            path: snapshot.path.clone(),
            start: checked_u32(start),
            end: checked_u32(end),
            expansion,
            line: position.line,
            column: position.column,
        })
    }

    pub(crate) fn detached(path: &Path, start: usize, end: usize) -> Self {
        Self {
            file: SourceFileId::new(0),
            path: path.to_path_buf(),
            start: checked_u32(start),
            end: checked_u32(end),
            expansion: ExpansionId::ROOT,
            line: 1,
            column: checked_u32(start.saturating_add(1)),
        }
    }

    /// 返回源码文件 ID。
    pub fn file(&self) -> SourceFileId {
        self.file
    }

    /// 返回该范围所属的逻辑源码路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 返回半开字节范围的起点。
    pub fn start(&self) -> u32 {
        self.start
    }

    /// 返回半开字节范围的终点。
    pub fn end(&self) -> u32 {
        self.end
    }

    /// 返回源码宏展开上下文。
    pub fn expansion(&self) -> ExpansionId {
        self.expansion
    }

    /// 返回 1-based 行号。
    pub fn line(&self) -> u32 {
        self.line
    }

    /// 返回 1-based 字节列号。
    pub fn column(&self) -> u32 {
        self.column
    }
}

/// 注册源码宏展开时使用的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpansionInput {
    /// 父展开记录。
    pub parent: ExpansionId,
    /// 宏调用位置。
    pub macro_call: Span,
    /// 宏定义位置。
    pub macro_definition: Span,
    /// 生成源码所在文件。
    pub generated_source: SourceFileId,
    /// 生成片段类别。
    pub fragment_kind: SourceSlot,
    /// 展开轮次。
    pub round: u32,
    /// 同一调用中的生成片段顺序。
    pub fragment_order: u32,
}

/// 一次成功源码宏展开的不可变记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpansionRecord {
    id: ExpansionId,
    parent: ExpansionId,
    macro_call: Span,
    macro_definition: Span,
    generated_source: SourceFileId,
    fragment_kind: SourceSlot,
    source_hash: [u8; 32],
}

impl ExpansionRecord {
    /// 返回展开 ID。
    pub fn id(&self) -> ExpansionId {
        self.id
    }

    /// 返回父展开 ID。
    pub fn parent(&self) -> ExpansionId {
        self.parent
    }

    /// 返回宏调用位置。
    pub fn macro_call(&self) -> &Span {
        &self.macro_call
    }

    /// 返回宏定义位置。
    pub fn macro_definition(&self) -> &Span {
        &self.macro_definition
    }

    /// 返回生成源码文件 ID。
    pub fn generated_source(&self) -> SourceFileId {
        self.generated_source
    }

    /// 返回生成片段类别。
    pub fn fragment_kind(&self) -> SourceSlot {
        self.fragment_kind
    }

    /// 返回生成源码 BLAKE3-256 摘要。
    pub fn source_hash(&self) -> [u8; 32] {
        self.source_hash
    }
}

/// 一个编译 action 的稳定源码与展开表。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SourceMap {
    snapshots: Vec<SourceSnapshot>,
    expansions: Vec<ExpansionRecord>,
}

impl SourceMap {
    /// 按规范逻辑路径排序并建立稠密文件 ID。
    pub fn new(mut snapshots: Vec<SourceSnapshot>) -> Result<Self, SourceMapError> {
        snapshots.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        for pair in snapshots.windows(2) {
            if pair[0].logical_path == pair[1].logical_path {
                return Err(SourceMapError::DuplicatePath(pair[0].logical_path.clone()));
            }
        }
        checked_u32(snapshots.len());
        Ok(Self {
            snapshots,
            expansions: Vec::new(),
        })
    }

    /// 返回空源码表。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 返回按稳定文件 ID 排列的源码快照。
    pub fn snapshots(&self) -> &[SourceSnapshot] {
        &self.snapshots
    }

    /// 按逻辑路径查找稳定文件 ID。
    pub fn file_id(&self, logical_path: &str) -> Option<SourceFileId> {
        self.snapshots
            .binary_search_by(|snapshot| snapshot.logical_path.as_str().cmp(logical_path))
            .ok()
            .map(|index| SourceFileId::new(checked_u32(index)))
    }

    /// 按稳定文件 ID 取得源码快照。
    pub fn snapshot(&self, file: SourceFileId) -> Option<&SourceSnapshot> {
        self.snapshots.get(file.index())
    }

    /// 创建带当前源码表身份的 span。
    pub fn span(
        &self,
        file: SourceFileId,
        start: usize,
        end: usize,
        expansion: ExpansionId,
    ) -> Result<Span, SpanError> {
        let snapshot = self.snapshot(file).ok_or(SpanError::UnknownFile(file))?;
        if expansion != ExpansionId::ROOT && expansion.index() > self.expansions.len() {
            return Err(SpanError::UnknownExpansion(expansion));
        }
        Span::from_snapshot(file, snapshot, start, end, expansion)
    }

    /// 注册一个成功的源码宏展开。
    pub fn register_expansion(
        &mut self,
        input: ExpansionInput,
    ) -> Result<ExpansionId, SourceMapError> {
        self.validate_expansion(&input)?;
        let id = ExpansionId::new(checked_u32(self.expansions.len() + 1));
        let source_hash = self
            .snapshot(input.generated_source)
            .expect("validated generated source")
            .content_hash();
        self.expansions.push(ExpansionRecord {
            id,
            parent: input.parent,
            macro_call: input.macro_call,
            macro_definition: input.macro_definition,
            generated_source: input.generated_source,
            fragment_kind: input.fragment_kind,
            source_hash,
        });
        Ok(id)
    }

    /// 按调用位置、轮次和片段顺序稳定注册多个展开。
    pub fn register_expansions(
        &mut self,
        mut inputs: Vec<ExpansionInput>,
    ) -> Result<Vec<ExpansionId>, SourceMapError> {
        inputs.sort_by(expansion_input_order);
        inputs
            .into_iter()
            .map(|input| self.register_expansion(input))
            .collect()
    }

    /// 返回所有成功展开记录。
    pub fn expansions(&self) -> &[ExpansionRecord] {
        &self.expansions
    }

    fn validate_expansion(&self, input: &ExpansionInput) -> Result<(), SourceMapError> {
        if input.parent != ExpansionId::ROOT && input.parent.index() > self.expansions.len() {
            return Err(SourceMapError::UnknownExpansion(input.parent));
        }
        self.validate_span(&input.macro_call)?;
        self.validate_span(&input.macro_definition)?;
        if self.snapshot(input.generated_source).is_none() {
            return Err(SourceMapError::UnknownFile(input.generated_source));
        }
        Ok(())
    }

    fn validate_span(&self, span: &Span) -> Result<(), SourceMapError> {
        if self.snapshot(span.file).is_none() {
            return Err(SourceMapError::UnknownFile(span.file));
        }
        if span.expansion != ExpansionId::ROOT && span.expansion.index() > self.expansions.len() {
            return Err(SourceMapError::UnknownExpansion(span.expansion));
        }
        Ok(())
    }
}

/// 源码表构建或展开注册失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceMapError {
    /// 多个快照使用同一逻辑路径。
    DuplicatePath(String),
    /// 引用未知文件。
    UnknownFile(SourceFileId),
    /// 引用未知展开。
    UnknownExpansion(ExpansionId),
}

impl fmt::Display for SourceMapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePath(path) => write!(formatter, "源码逻辑路径 `{path}` 重复"),
            Self::UnknownFile(file) => write!(formatter, "未知源码文件 ID {}", file.as_u32()),
            Self::UnknownExpansion(expansion) => {
                write!(formatter, "未知源码展开 ID {}", expansion.as_u32())
            }
        }
    }
}

impl std::error::Error for SourceMapError {}

/// 把 package-relative 路径规范化为 `/` 分隔、不含 `.` 与 `..` 的逻辑路径。
pub fn normalize_logical_path(path: &Path) -> Result<String, SourceError> {
    let original = path.to_string_lossy().into_owned();
    if path.as_os_str().is_empty() || original.contains('\\') {
        return Err(SourceError::InvalidPath { path: original });
    }
    let mut components: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| SourceError::InvalidPath {
                    path: original.clone(),
                })?;
                if value.is_empty() {
                    return Err(SourceError::InvalidPath {
                        path: original.clone(),
                    });
                }
                components.push(value);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if components.pop().is_none() {
                    return Err(SourceError::InvalidPath { path: original });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(SourceError::InvalidPath { path: original });
            }
        }
    }
    if components.is_empty() {
        return Err(SourceError::InvalidPath { path: original });
    }
    Ok(components.join("/"))
}
fn build_line_starts(bytes: &[u8]) -> Vec<u32> {
    let mut starts = Vec::with_capacity(bytes.len().min(128));
    starts.push(0);
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\r' => {
                offset += 1;
                if bytes.get(offset) == Some(&b'\n') {
                    offset += 1;
                }
                starts.push(checked_u32(offset));
            }
            b'\n' => {
                offset += 1;
                starts.push(checked_u32(offset));
            }
            _ => offset += 1,
        }
    }
    starts
}

fn expansion_input_order(left: &ExpansionInput, right: &ExpansionInput) -> Ordering {
    (
        left.macro_call.file,
        left.macro_call.start,
        left.round,
        left.fragment_order,
        left.fragment_kind,
        left.generated_source,
    )
        .cmp(&(
            right.macro_call.file,
            right.macro_call.start,
            right.round,
            right.fragment_order,
            right.fragment_kind,
            right.generated_source,
        ))
}

fn checked_u32(value: usize) -> u32 {
    debug_assert!(value <= u32::MAX as usize);
    value as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(path: &str, source: &str) -> SourceSnapshot {
        SourceSnapshot::from_str(Path::new(path), source).expect("snapshot builds")
    }

    #[test]
    fn snapshot_rejects_bom_and_invalid_utf8() {
        let bom = SourceSnapshot::from_bytes(Path::new("src/main.gg"), b"\xef\xbb\xbffn main() {}");
        assert_eq!(
            bom.unwrap_err(),
            SourceError::Bom {
                path: "src/main.gg".to_owned()
            }
        );

        let invalid = SourceSnapshot::from_bytes(Path::new("src/main.gg"), b"fn main() {}\xff\xfe");
        assert_eq!(
            invalid.unwrap_err(),
            SourceError::InvalidUtf8 {
                path: "src/main.gg".to_owned(),
                offset: 12
            }
        );
    }

    #[test]
    fn snapshot_computes_blake3_content_hash() {
        let source = snapshot("src/main.gg", "fn main() {}\n");
        assert_eq!(
            source.content_hash(),
            *blake3::hash(b"fn main() {}\n").as_bytes()
        );
    }

    #[test]
    fn snapshot_maps_lf_crlf_and_cr_line_starts() {
        let source = snapshot("src/a.gg", "one\ntwo\r\nthree\rfour");
        assert_eq!(source.line_starts(), [0, 4, 9, 15]);
        assert_eq!(source.line_column(0), Ok(LineColumn { line: 1, column: 1 }));
        assert_eq!(source.line_column(4), Ok(LineColumn { line: 2, column: 1 }));
        assert_eq!(source.line_column(6), Ok(LineColumn { line: 2, column: 3 }));
        assert_eq!(
            source.line_column(source.content().len()),
            Ok(LineColumn { line: 4, column: 5 })
        );
        assert!(source.line_column(source.content().len() + 1).is_err());
    }

    #[test]
    fn logical_paths_are_normalized_and_validated() {
        assert_eq!(
            normalize_logical_path(Path::new("src/./a/../b.gg")).as_deref(),
            Ok("src/b.gg")
        );
        assert_eq!(
            normalize_logical_path(Path::new("src/main.gg")).as_deref(),
            Ok("src/main.gg")
        );
        for invalid in ["/abs/main.gg", "../escape.gg", "src\\main.gg", ""] {
            assert!(
                normalize_logical_path(Path::new(invalid)).is_err(),
                "`{invalid}` 必须被拒绝"
            );
        }
    }

    #[test]
    fn source_map_assigns_ids_by_logical_path_order() {
        let map = SourceMap::new(vec![
            snapshot("src/z.gg", "z"),
            snapshot("src/a.gg", "a"),
            snapshot("lib/b.gg", "b"),
        ])
        .expect("distinct paths");
        assert_eq!(
            map.snapshots()
                .iter()
                .map(SourceSnapshot::logical_path)
                .collect::<Vec<_>>(),
            ["lib/b.gg", "src/a.gg", "src/z.gg"]
        );
        assert_eq!(map.file_id("src/a.gg").map(SourceFileId::index), Some(1));
        assert_eq!(map.file_id("src/missing.gg"), None);
        assert!(SourceMap::new(vec![snapshot("dup.gg", "a"), snapshot("dup.gg", "b")]).is_err());
    }

    #[test]
    fn spans_are_half_open_and_validated() {
        let map =
            SourceMap::new(vec![snapshot("src/main.gg", "fn main() {}")]).expect("single source");
        let file = map.file_id("src/main.gg").expect("registered");
        let span = map.span(file, 3, 8, ExpansionId::ROOT).expect("valid span");
        assert_eq!(span.line(), 1);
        assert_eq!(span.column(), 4);
        assert_eq!(span.path(), Path::new("src/main.gg"));
        assert!(map.span(file, 8, 3, ExpansionId::ROOT).is_err());
        assert!(map.span(file, 0, 100, ExpansionId::ROOT).is_err());
        assert!(
            map.span(SourceFileId::new(9), 0, 0, ExpansionId::ROOT)
                .is_err()
        );
    }

    #[test]
    fn expansions_register_deterministically_with_parent_chain() {
        let mut map = SourceMap::new(vec![
            snapshot("src/main.gg", "comptime source { }"),
            snapshot("generated/one.gg", "fn one() {}"),
            snapshot("generated/two.gg", "fn two() {}"),
        ])
        .expect("distinct sources");
        let main = map.file_id("src/main.gg").expect("main registered");
        let one = map.file_id("generated/one.gg").expect("one registered");
        let two = map.file_id("generated/two.gg").expect("two registered");
        let call = map.span(main, 0, 18, ExpansionId::ROOT).expect("call span");
        let definition = map
            .span(main, 0, 8, ExpansionId::ROOT)
            .expect("definition span");

        // 故意乱序输入，注册必须按调用位置、轮次、片段顺序稳定分配。
        let ids = map
            .register_expansions(vec![
                ExpansionInput {
                    parent: ExpansionId::ROOT,
                    macro_call: call.clone(),
                    macro_definition: definition.clone(),
                    generated_source: two,
                    fragment_kind: SourceSlot::Item,
                    round: 1,
                    fragment_order: 1,
                },
                ExpansionInput {
                    parent: ExpansionId::ROOT,
                    macro_call: call.clone(),
                    macro_definition: definition.clone(),
                    generated_source: one,
                    fragment_kind: SourceSlot::Item,
                    round: 1,
                    fragment_order: 0,
                },
            ])
            .expect("expansions register");
        assert_eq!(ids.iter().map(|id| id.index()).collect::<Vec<_>>(), [1, 2]);
        let first = &map.expansions()[0];
        assert_eq!(first.parent(), ExpansionId::ROOT);
        assert_eq!(first.fragment_kind(), SourceSlot::Item);
        assert_eq!(
            first.source_hash(),
            map.snapshot(one).expect("one").content_hash()
        );

        // 第二轮展开挂在第一轮之下，父链成立。
        let child = map
            .register_expansion(ExpansionInput {
                parent: ids[0],
                macro_call: map
                    .span(two, 0, 9, ids[0])
                    .expect("span in generated source"),
                macro_definition: definition,
                generated_source: one,
                fragment_kind: SourceSlot::Expression,
                round: 2,
                fragment_order: 0,
            })
            .expect("child expansion registers");
        assert_eq!(map.expansions()[2].parent(), ids[0]);
        assert_eq!(child.index(), 3);

        // 未知父展开必须被拒绝。
        assert!(
            map.register_expansion(ExpansionInput {
                parent: ExpansionId::new(99),
                macro_call: call,
                macro_definition: map.span(two, 0, 1, ExpansionId::ROOT).expect("valid span"),
                generated_source: one,
                fragment_kind: SourceSlot::Statement,
                round: 2,
                fragment_order: 1,
            })
            .is_err()
        );
    }
}
