//! compiler-owned 的封闭 comptime capability registry。
//!
//! 条目按解析后的规范路径（lang item 或 intrinsic 身份）登记，固定保存允许的执行域、
//! 效果集合、显式输入、结果种类和 evaluator revision。禁止按函数体自动授予能力；
//! 本表之外的路径在任何 comptime 执行域都不可调用。

use blake3::Hasher;

/// registry 的整体 revision；增删能力组必须提升该值并进入编译输入。
pub(crate) const REGISTRY_REVISION: u32 = 1;

/// `(revision, summary)` 的空表初值，供空 package 输出使用。
pub(crate) const EARLY_REGISTRY_IDENTITY: (u32, [u8; 32]) = (REGISTRY_REVISION, [0; 32]);

/// evaluator 核心的 revision；registry 条目各自声明其依赖的版本。
pub(crate) const EVALUATOR_REVISION: u32 = 1;

/// comptime 执行域位掩码。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Domain(u8);

impl Domain {
    /// 早期常量域：const、数组长度、布局参数与泛型实参。
    pub(crate) const EARLY_CONST: Self = Self(1);
    /// 源码宏展开域。
    pub(crate) const SOURCE_EXPAND: Self = Self(2);
    /// late 常量域：只读冻结 type universe。
    pub(crate) const LATE_CONST: Self = Self(4);

    /// 判断当前域是否获准。
    #[cfg(test)]
    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 规范名称，用于诊断与摘要编码。
    pub(crate) fn name(self) -> &'static str {
        match self.0 {
            1 => "EarlyConst",
            2 => "SourceExpand",
            4 => "LateConst",
            _ => "未知执行域",
        }
    }
}

/// 条目允许声明的效果集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Effect(u8);

impl Effect {
    /// 纯计算。
    pub(crate) const PURE: Self = Self(1);
    /// 使用 evaluator heap 分配。
    pub(crate) const HEAP: Self = Self(2);
    /// 读取显式登记的文件输入。
    pub(crate) const FILE_INPUT: Self = Self(4);
    /// 使用解析器 schema 解析源码。
    pub(crate) const SOURCE_PARSE: Self = Self(8);
    /// 构造可物化的初始同步位状态。
    pub(crate) const INITIAL_BITS: Self = Self(16);
}

/// 条目结果种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ResultKind {
    /// 规范标量叶值。
    Scalar,
    /// evaluator string 值。
    String,
    /// 固定形状聚合的初始位状态。
    InitialBits,
    /// 编译器拥有的解析片段。
    ParsedSource,
    /// 不产生值，只终止当前求值。
    Terminates,
}

/// 一条封闭登记的能力。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapabilityEntry {
    /// 允许调用该能力的执行域集合。
    pub(crate) domains: DomainSet,
    /// 声明的效果集合。
    pub(crate) effects: EffectSet,
    /// 允许读取的显式输入。
    pub(crate) explicit_inputs: &'static [&'static str],
    /// 结果种类。
    pub(crate) result_kind: ResultKind,
    /// 该条目依赖的 evaluator revision。
    pub(crate) evaluator_revision: u32,
}

/// 多域能力集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DomainSet(u8);

impl DomainSet {
    /// 由若干执行域组成的能力集合。
    pub(crate) const fn new(domains: &[Domain]) -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < domains.len() {
            bits |= domains[index].0;
            index += 1;
        }
        Self(bits)
    }

    /// 判断指定执行域是否获准。
    pub(crate) const fn allows(self, domain: Domain) -> bool {
        self.0 & domain.0 != 0
    }
}

/// 多效果集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectSet(u8);

impl EffectSet {
    /// 由若干效果组成的能力集合。
    pub(crate) const fn new(effects: &[Effect]) -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < effects.len() {
            bits |= effects[index].0;
            index += 1;
        }
        Self(bits)
    }
}

const ALL_DOMAINS: DomainSet = DomainSet::new(&[
    Domain::EARLY_CONST,
    Domain::SOURCE_EXPAND,
    Domain::LATE_CONST,
]);
const EARLY_AND_EXPAND: DomainSet = DomainSet::new(&[Domain::EARLY_CONST, Domain::SOURCE_EXPAND]);
const EXPAND_ONLY: DomainSet = DomainSet::new(&[Domain::SOURCE_EXPAND]);
const LATE_ONLY: DomainSet = DomainSet::new(&[Domain::LATE_CONST]);
const PURE_ONLY: EffectSet = EffectSet::new(&[Effect::PURE]);
const HEAP_ONLY: EffectSet = EffectSet::new(&[Effect::HEAP]);

/// 精确路径条目；规范要求封闭登记，不使用前缀猜测。
const EXACT_ENTRIES: &[(&str, CapabilityEntry)] = &[
    (
        "panic",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Terminates,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "size_of",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "align_of",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "offset_of",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "type_id",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "TypeId.name",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::String,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "type_id_count",
        CapabilityEntry {
            domains: LATE_ONLY,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "TypeId.as_int",
        CapabilityEntry {
            domains: LATE_ONLY,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.src.file",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: PURE_ONLY,
            explicit_inputs: &["source_location"],
            result_kind: ResultKind::String,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.src.line",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: PURE_ONLY,
            explicit_inputs: &["source_location"],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.src.column",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: PURE_ONLY,
            explicit_inputs: &["source_location"],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.mem.embed_file",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::FILE_INPUT.0),
            explicit_inputs: &["package_embedded_file"],
            result_kind: ResultKind::String,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.syntax.parse_source",
        CapabilityEntry {
            domains: EXPAND_ONLY,
            effects: EffectSet(Effect::SOURCE_PARSE.0),
            explicit_inputs: &["source_slot"],
            result_kind: ResultKind::ParsedSource,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.syntax.parse_expr",
        CapabilityEntry {
            domains: EXPAND_ONLY,
            effects: EffectSet(Effect::SOURCE_PARSE.0),
            explicit_inputs: &["source_slot"],
            result_kind: ResultKind::ParsedSource,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.syntax.parse_items",
        CapabilityEntry {
            domains: EXPAND_ONLY,
            effects: EffectSet(Effect::SOURCE_PARSE.0),
            explicit_inputs: &["source_slot"],
            result_kind: ResultKind::ParsedSource,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.syntax.parse_type",
        CapabilityEntry {
            domains: EXPAND_ONLY,
            effects: EffectSet(Effect::SOURCE_PARSE.0),
            explicit_inputs: &["source_slot"],
            result_kind: ResultKind::ParsedSource,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.syntax.parse_pattern",
        CapabilityEntry {
            domains: EXPAND_ONLY,
            effects: EffectSet(Effect::SOURCE_PARSE.0),
            explicit_inputs: &["source_slot"],
            result_kind: ResultKind::ParsedSource,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "string",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: HEAP_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::String,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "std.fmt",
        CapabilityEntry {
            domains: ALL_DOMAINS,
            effects: HEAP_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::String,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "SyntaxError",
        CapabilityEntry {
            domains: EXPAND_ONLY,
            effects: PURE_ONLY,
            explicit_inputs: &[],
            result_kind: ResultKind::Scalar,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "Atomic::new",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::INITIAL_BITS.0),
            explicit_inputs: &[],
            result_kind: ResultKind::InitialBits,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "Mutex::new",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::INITIAL_BITS.0),
            explicit_inputs: &[],
            result_kind: ResultKind::InitialBits,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "RwLock::new",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::INITIAL_BITS.0),
            explicit_inputs: &[],
            result_kind: ResultKind::InitialBits,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "Condvar::new",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::INITIAL_BITS.0),
            explicit_inputs: &[],
            result_kind: ResultKind::InitialBits,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "OnceLock::new",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::INITIAL_BITS.0),
            explicit_inputs: &[],
            result_kind: ResultKind::InitialBits,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
    (
        "Lazy::new",
        CapabilityEntry {
            domains: EARLY_AND_EXPAND,
            effects: EffectSet(Effect::INITIAL_BITS.0),
            explicit_inputs: &[],
            result_kind: ResultKind::InitialBits,
            evaluator_revision: EVALUATOR_REVISION,
        },
    ),
];

/// 按解析后的规范身份查询条目；未登记返回 `None`。
pub(crate) fn lookup(canonical: &str) -> Option<&'static CapabilityEntry> {
    EXACT_ENTRIES
        .iter()
        .find(|(path, _)| *path == canonical)
        .map(|(_, entry)| entry)
}

/// 全部登记路径，按字典序排列，供摘要编码使用。
#[cfg(test)]
pub(crate) fn registered_paths() -> impl Iterator<Item = &'static str> {
    EXACT_ENTRIES.iter().map(|(path, _)| *path)
}

/// registry 规范摘要：revision + 全部条目的规范编码经域隔离 BLAKE3。
pub(crate) fn summary() -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(&REGISTRY_REVISION.to_le_bytes());
    for (path, entry) in EXACT_ENTRIES {
        hasher.update(&(path.len() as u32).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(&entry.domains.0.to_le_bytes());
        hasher.update(&entry.effects.0.to_le_bytes());
        hasher.update(&(entry.explicit_inputs.len() as u32).to_le_bytes());
        for input in entry.explicit_inputs {
            hasher.update(&(input.len() as u32).to_le_bytes());
            hasher.update(input.as_bytes());
        }
        hasher.update(&[match entry.result_kind {
            ResultKind::Scalar => 0,
            ResultKind::String => 1,
            ResultKind::InitialBits => 2,
            ResultKind::ParsedSource => 3,
            ResultKind::Terminates => 4,
        }]);
        hasher.update(&entry.evaluator_revision.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}
