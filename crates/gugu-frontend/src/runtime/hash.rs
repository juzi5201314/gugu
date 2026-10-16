//! `std.hash` 的确定性参照模型。
//!
//! `Hash` 把值的语义字段按声明顺序馈送给 `Hasher`；`a == b` 必须产生相同输入。默认
//! HashMap 使用随机化 FoldHash-fast，SecureHashMap 使用 OS CSPRNG 初始化的 SipHash 1-3，
//! 两者的输出都不是持久格式。`XxHash3_64` / `XxHash3_128` 是算法命名的稳定非密码 hash，
//! 相同字节输入跨进程与工具链产生相同结果。

use std::hash::{BuildHasher, Hasher as _};

/// 集合与摘要可选择的哈希族。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HashFamily {
    /// 随机化 FoldHash-fast；seed 来自进程熵，只在同一进程内一致。
    FoldHashFast { seed: u64 },
    /// 由 OS CSPRNG 初始化的 SipHash 1-3；供不可信键使用。
    SipHash13 { key: [u64; 2] },
    /// 稳定的 XXH3 64 位输出。
    XxHash3_64,
    /// 稳定的 XXH3 128 位输出。
    XxHash3_128,
}

/// 一次 hash 的输出位宽。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HashOutput {
    Bits64(u64),
    Bits128(u128),
}

impl HashFamily {
    /// 默认族：用 8 字节进程熵初始化 FoldHash-fast。
    pub(crate) fn default_from_entropy(entropy: [u8; 8]) -> Self {
        Self::FoldHashFast {
            seed: u64::from_le_bytes(entropy),
        }
    }

    /// 安全族：用 16 字节 CSPRNG 输出初始化 SipHash 1-3 的两个 key。
    pub(crate) fn secure_from_entropy(entropy: [u8; 16]) -> Self {
        let (low, high) = entropy.split_at(8);
        Self::SipHash13 {
            key: [
                u64::from_le_bytes(low.try_into().expect("8 字节")),
                u64::from_le_bytes(high.try_into().expect("8 字节")),
            ],
        }
    }

    /// 该族是否承诺跨进程与工具链稳定。
    pub(crate) fn is_persistent(self) -> bool {
        matches!(self, Self::XxHash3_64 | Self::XxHash3_128)
    }

    /// 对已馈送的语义字节求 hash。
    pub(crate) fn finish(self, input: &[u8]) -> HashOutput {
        match self {
            Self::FoldHashFast { seed } => {
                let mut hasher = foldhash::fast::FixedState::with_seed(seed).build_hasher();
                hasher.write(input);
                HashOutput::Bits64(hasher.finish())
            }
            Self::SipHash13 { key } => {
                let mut hasher = siphasher::sip::SipHasher13::new_with_keys(key[0], key[1]);
                hasher.write(input);
                HashOutput::Bits64(hasher.finish())
            }
            Self::XxHash3_64 => HashOutput::Bits64(xxhash_rust::xxh3::xxh3_64(input)),
            Self::XxHash3_128 => HashOutput::Bits128(xxhash_rust::xxh3::xxh3_128(input)),
        }
    }
}

/// `Hash::hash` 的馈送目标。字段按语义顺序进入；变长数据带长度前缀以保持前缀无关。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Hasher {
    bytes: Vec<u8>,
}

impl Hasher {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 有符号整数按 64 位小端馈送。
    pub(crate) fn write_int(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// 无符号整数按 64 位小端馈送。
    pub(crate) fn write_uint(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// bool 馈送一个字节。
    pub(crate) fn write_bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    /// char 按 Unicode 标量值馈送。
    pub(crate) fn write_char(&mut self, value: char) {
        self.bytes
            .extend_from_slice(&u32::from(value).to_le_bytes());
    }

    /// string 按原始 UTF-8 字节馈送，不随 Unicode 表改变。
    pub(crate) fn write_str(&mut self, value: &str) {
        self.write_bytes(value.as_bytes());
    }

    /// 变长字节：长度前缀 + 内容。
    pub(crate) fn write_bytes(&mut self, value: &[u8]) {
        self.write_uint(u64::try_from(value.len()).expect("长度可编码"));
        self.bytes.extend_from_slice(value);
    }

    /// 已馈送的语义字节。
    pub(crate) fn input(&self) -> &[u8] {
        &self.bytes
    }

    /// 用指定哈希族结束。
    pub(crate) fn finish(&self, family: HashFamily) -> HashOutput {
        family.finish(&self.bytes)
    }
}

/// `Eq + Hash + StableHash` 键：Eq 与 Hash 的可观察结果不能被外部别名改变。
///
/// 只有标量、按字节比较的文本/字节快照以及元素全部稳定的元组/数组由编译器给出该 marker；
/// 可变身份句柄与资源不能安全实现。
pub(crate) trait StableKey: Clone + Eq {
    fn feed(&self, hasher: &mut Hasher);

    /// 按哈希族求键 hash。
    fn stable_hash(&self, family: HashFamily) -> HashOutput {
        let mut hasher = Hasher::new();
        self.feed(&mut hasher);
        hasher.finish(family)
    }
}

/// `Ord + StableOrd` 键：比较顺序不能被外部别名改变。
pub(crate) trait StableOrdKey: Clone + Ord {}

impl StableKey for i64 {
    fn feed(&self, hasher: &mut Hasher) {
        hasher.write_int(*self);
    }
}
impl StableKey for u64 {
    fn feed(&self, hasher: &mut Hasher) {
        hasher.write_uint(*self);
    }
}
impl StableKey for bool {
    fn feed(&self, hasher: &mut Hasher) {
        hasher.write_bool(*self);
    }
}
impl StableKey for char {
    fn feed(&self, hasher: &mut Hasher) {
        hasher.write_char(*self);
    }
}
impl StableKey for String {
    fn feed(&self, hasher: &mut Hasher) {
        hasher.write_str(self);
    }
}
impl StableKey for Vec<u8> {
    fn feed(&self, hasher: &mut Hasher) {
        hasher.write_bytes(self);
    }
}
impl<A: StableKey, B: StableKey> StableKey for (A, B) {
    fn feed(&self, hasher: &mut Hasher) {
        self.0.feed(hasher);
        self.1.feed(hasher);
    }
}
impl<T: StableKey, const N: usize> StableKey for [T; N] {
    fn feed(&self, hasher: &mut Hasher) {
        for item in self {
            item.feed(hasher);
        }
    }
}

impl StableOrdKey for i64 {}
impl StableOrdKey for u64 {}
impl StableOrdKey for bool {}
impl StableOrdKey for char {}
impl StableOrdKey for String {}
impl StableOrdKey for Vec<u8> {}
impl<A: StableOrdKey, B: StableOrdKey> StableOrdKey for (A, B) {}
impl<T: StableOrdKey, const N: usize> StableOrdKey for [T; N] {}
