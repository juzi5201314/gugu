use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Session 内 intern 的 UTF-8 字符串身份。
///
/// 标识符文本是稀疏键，不能用稠密下标当查找键；`Symbol` 本身是插入序稠密
/// `u32`，供 token payload 与后续 AST 点查。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Symbol(u32);

/// 连续字节池 + 哈希点查的 intern 表。
///
/// 访问模式是词法扫描时 intern、之后按 `Symbol` 取切片；元素数有 `u32` 上界。
#[derive(Clone, Debug, Default)]
pub(crate) struct SymbolInterner {
    data: Vec<u8>,
    spans: Vec<(u32, u32)>,
    by_hash: HashMap<u64, Vec<u32>>,
}

impl SymbolInterner {
    pub(crate) fn intern(&mut self, bytes: &[u8]) -> Symbol {
        let hash = hash_bytes(bytes);
        if let Some(symbol) = self.lookup_hashed(bytes, hash) {
            return symbol;
        }
        debug_assert!(
            self.spans.len() < u32::MAX as usize,
            "symbol intern 达到 u32 上界"
        );
        debug_assert!(
            self.data
                .len()
                .checked_add(bytes.len())
                .is_some_and(|end| end <= u32::MAX as usize),
            "intern 字节池达到 u32 上界"
        );
        let index = self.spans.len() as u32;
        let start = self.data.len() as u32;
        self.data.extend_from_slice(bytes);
        self.spans.push((start, bytes.len() as u32));
        self.by_hash.entry(hash).or_default().push(index);
        Symbol(index)
    }

    pub(crate) fn intern_str(&mut self, text: &str) -> Symbol {
        self.intern(text.as_bytes())
    }

    pub(crate) fn get(&self, symbol: Symbol) -> &[u8] {
        self.bytes_at(symbol.0)
    }

    pub(crate) fn get_str(&self, symbol: Symbol) -> &str {
        std::str::from_utf8(self.get(symbol)).expect("intern 只保存 UTF-8")
    }

    pub(crate) fn lookup_str(&self, text: &str) -> Option<Symbol> {
        self.lookup_hashed(text.as_bytes(), hash_bytes(text.as_bytes()))
    }

    fn lookup_hashed(&self, bytes: &[u8], hash: u64) -> Option<Symbol> {
        self.by_hash
            .get(&hash)?
            .iter()
            .copied()
            .find(|&index| self.bytes_at(index) == bytes)
            .map(Symbol)
    }

    fn bytes_at(&self, index: u32) -> &[u8] {
        let (start, len) = self.spans[index as usize];
        let start = start as usize;
        &self.data[start..start + len as usize]
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
