//! UTF-8/UTF-16、大小写、规范化、切分与 COW 字节缓冲的确定性参照模型。
//!
//! 数据版本是 Unicode 17.0.0：编码用宿主 `char`，规范化与 grapheme/word/line
//! 用同一版本的表，完整 case fold 用随附的 `CaseFolding.txt`。

use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

/// Unicode 数据版本。编码、大小写、规范化与切分都按这一版。
pub(crate) const UNICODE_VERSION: &str = "17.0.0";

/// 四种规范化形式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Norm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

/// 文本操作会在语言里 panic 的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextFault {
    /// 负的长度、容量或索引。
    Negative,
    /// 索引不在 UTF-8 标量边界上。
    Boundary,
    /// 索引超出当前字节长度。
    OutOfRange,
}

/// 严格 UTF-8。失败偏移是第一个非法字节。
pub(crate) fn utf8_decode(source: &[u8]) -> Result<String, usize> {
    match std::str::from_utf8(source) {
        Ok(text) => Ok(text.to_owned()),
        Err(error) => Err(error.valid_up_to()),
    }
}

/// 有损 UTF-8：每个 maximal ill-formed subpart 换成 U+FFFD。
pub(crate) fn utf8_decode_lossy(source: &[u8]) -> String {
    String::from_utf8_lossy(source).into_owned()
}

/// 严格 UTF-16。失败偏移是第一个未配对代理项的 code unit。
pub(crate) fn utf16_decode(source: &[u16]) -> Result<String, usize> {
    let mut out = String::new();
    for (index, item) in char::decode_utf16(source.iter().copied()).enumerate() {
        match item {
            Ok(scalar) => out.push(scalar),
            Err(_) => return Err(index),
        }
    }
    Ok(out)
}

/// 有损 UTF-16：每个未配对代理项换成一个 U+FFFD。
pub(crate) fn utf16_decode_lossy(source: &[u16]) -> String {
    char::decode_utf16(source.iter().copied())
        .map(|item| item.unwrap_or('\u{FFFD}'))
        .collect()
}

/// 把合法 UTF-8 编成 UTF-16 code unit。
pub(crate) fn utf16_encode(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// `byte_at`：负索引失败，越界是 `None`，不要求标量边界。
pub(crate) fn byte_at(text: &str, index: isize) -> Result<Option<u8>, TextFault> {
    if index < 0 {
        return Err(TextFault::Negative);
    }
    Ok(text.as_bytes().get(index as usize).copied())
}

/// 修改用的字节终点。`end` 为真时允许等于长度。
pub(crate) fn scalar_boundary(text: &str, index: isize, end: bool) -> Result<usize, TextFault> {
    if index < 0 {
        return Err(TextFault::Negative);
    }
    let index = index as usize;
    if index > text.len() || (!end && index == text.len()) {
        return Err(TextFault::OutOfRange);
    }
    if text.is_char_boundary(index) {
        Ok(index)
    } else {
        Err(TextFault::Boundary)
    }
}

/// 按 Unicode 标量序号取值。负索引失败，越界是 `None`。
pub(crate) fn char_at(text: &str, index: isize) -> Result<Option<char>, TextFault> {
    if index < 0 {
        return Err(TextFault::Negative);
    }
    Ok(text.chars().nth(index as usize))
}

/// 不可变字节快照或可变缓冲的逻辑值。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CowBytes {
    bytes: Vec<u8>,
    backing: u64,
    sealed: bool,
}

impl CowBytes {
    /// 空的 unique 缓冲。
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            backing: 1,
            sealed: false,
        }
    }

    /// 负容量失败。容量只影响预留，不改变内容身份。
    pub(crate) fn with_capacity(capacity: isize) -> Result<Self, TextFault> {
        if capacity < 0 {
            return Err(TextFault::Negative);
        }
        let mut value = Self::new();
        value.bytes.reserve(capacity as usize);
        Ok(value)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn backing(&self) -> u64 {
        self.backing
    }

    pub(crate) fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// 复制封存 backing，两份值共享同一 backing 编号。
    pub(crate) fn share(&mut self) -> Self {
        self.sealed = true;
        Self {
            bytes: self.bytes.clone(),
            backing: self.backing,
            sealed: true,
        }
    }

    /// `freeze`：快照封存 backing，缓冲保留内容并变为 sealed。
    pub(crate) fn freeze(&mut self) -> Self {
        self.share()
    }

    /// 从已封存快照建立缓冲。第一次写入才分离。
    pub(crate) fn thaw(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            backing: self.backing,
            sealed: true,
        }
    }

    /// 写入前，sealed 值分离到新 backing。
    pub(crate) fn push(&mut self, byte: u8) {
        self.detach();
        self.bytes.push(byte);
    }

    /// 返回前 `length` 个字节的快照，缓冲换绑到剩余区间并保持 sealed。
    pub(crate) fn split_to(&mut self, length: isize) -> Result<Self, TextFault> {
        if length < 0 {
            return Err(TextFault::Negative);
        }
        let length = length as usize;
        if length > self.bytes.len() {
            return Err(TextFault::OutOfRange);
        }
        self.sealed = true;
        let prefix = Self {
            bytes: self.bytes[..length].to_vec(),
            backing: self.backing,
            sealed: true,
        };
        self.bytes.drain(..length);
        Ok(prefix)
    }

    fn detach(&mut self) {
        if self.sealed {
            self.backing = self.backing.wrapping_add(1);
            self.sealed = false;
        }
    }
}

/// 默认大小写映射。结果可以比输入长。
pub(crate) fn to_lowercase(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// 默认大写映射。结果可以比输入长。
pub(crate) fn to_uppercase(text: &str) -> String {
    text.chars().flat_map(char::to_uppercase).collect()
}

/// 完整 case fold（`CaseFolding.txt` 的 C 与 F，不含 Turkic）。
pub(crate) fn case_fold(text: &str) -> String {
    let table = fold_table();
    let mut out = String::new();
    for scalar in text.chars() {
        match table.binary_search_by_key(&(u32::from(scalar)), |entry| entry.0) {
            Ok(index) => push_fold(&mut out, &table[index].1),
            Err(_) => out.push(scalar),
        }
    }
    out
}

/// NFC / NFD / NFKC / NFKD。
pub(crate) fn normalize(text: &str, form: Norm) -> String {
    match form {
        Norm::Nfc => text.nfc().collect(),
        Norm::Nfd => text.nfd().collect(),
        Norm::Nfkc => text.nfkc().collect(),
        Norm::Nfkd => text.nfkd().collect(),
    }
}

/// 扩展 grapheme cluster。`extended` 使用 UAX #29 默认规则。
pub(crate) fn graphemes(text: &str) -> Vec<String> {
    text.graphemes(true).map(str::to_owned).collect()
}

/// UAX #29 词边界，包含空白和标点片段。
pub(crate) fn words(text: &str) -> Vec<String> {
    text.split_word_bounds().map(str::to_owned).collect()
}

/// 强制换行：CR LF 不拆开，并在 LF、NEL、LS、PS 以及不跟 LF 的 CR 之后断开。
pub(crate) fn lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, scalar)) = chars.next() {
        let break_after = match scalar {
            '\r' => chars.peek().is_none_or(|(_, next)| *next != '\n'),
            '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}' => true,
            _ => false,
        };
        if break_after {
            let end = index + scalar.len_utf8();
            out.push(text[start..end].to_owned());
            start = end;
        }
    }
    if start < text.len() {
        out.push(text[start..].to_owned());
    } else if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 三套 Unicode 17 表是否一致。
pub(crate) fn unicode_tables_agree() -> bool {
    unicode_normalization::UNICODE_VERSION == (17, 0, 0)
        && unicode_segmentation::UNICODE_VERSION == (17, 0, 0)
        && char::UNICODE_VERSION == (17, 0, 0)
}

fn push_fold(out: &mut String, scalars: &[u32]) {
    for scalar in scalars {
        out.push(char::from_u32(*scalar).expect("case fold 目标是标量"));
    }
}

fn fold_table() -> &'static [(u32, Vec<u32>)] {
    static TABLE: OnceLock<Vec<(u32, Vec<u32>)>> = OnceLock::new();
    TABLE.get_or_init(parse_case_folding).as_slice()
}

fn parse_case_folding() -> Vec<(u32, Vec<u32>)> {
    let mut table = Vec::new();
    for line in include_str!("../../resources/unicode/CaseFolding.txt").lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(';').map(str::trim);
        let Some(source) = fields
            .next()
            .and_then(|text| u32::from_str_radix(text, 16).ok())
        else {
            continue;
        };
        let status = fields.next().unwrap_or("");
        if status != "C" && status != "F" {
            continue;
        }
        let mapped = fields
            .next()
            .unwrap_or("")
            .split_whitespace()
            .filter_map(|text| u32::from_str_radix(text, 16).ok())
            .collect::<Vec<_>>();
        insert_fold(&mut table, source, status, mapped);
    }
    table.sort_by_key(|entry| entry.0);
    table
}

fn insert_fold(table: &mut Vec<(u32, Vec<u32>)>, source: u32, status: &str, mapped: Vec<u32>) {
    if let Some(existing) = table.iter_mut().find(|entry| entry.0 == source) {
        if status == "F" {
            existing.1 = mapped;
        }
        return;
    }
    table.push((source, mapped));
}
