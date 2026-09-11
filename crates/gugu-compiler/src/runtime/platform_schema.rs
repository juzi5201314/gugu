//! 平台范围的契约 schema：操作目录、range 迁移表、extent 阶梯、策略与故障映射。
//!
//! 这些类型把 `PlatformRange` 的固定操作集合、二次幂 extent 阶梯、huge-page hint、zero 与
//! dump policy 以及 Linux/Windows 两个 profile 的失败映射固定成可缓存、可校验的契约；状态机
//! 与 buddy 阶梯的参照实现见 `platform` 与 `extent` 模块。

use serde::{Deserialize, Serialize};

use super::extent::EXTENT_CLASS_LADDER;
use super::model::RawModelError;
use super::platform::PlatformProfile;
use super::provider::{FaultClass, ProviderError, RangeCost, RangeState};

/// 平台范围契约段的 schema 版本。
pub(crate) const PLATFORM_SCHEMA: u32 = 1;

/// 平台 range 的固定操作。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum PlatformOp {
    /// 预留对齐的虚拟范围。
    ReserveAligned,
    /// 提交物理页。
    Commit,
    /// 撤销物理页；只在 lease 与 grace 结束后按批执行。
    Decommit,
    /// 释放虚拟范围；编号不复用。
    Release,
    /// 把范围尾部保护为 guard。
    ProtectGuard,
    /// 取消 guard 保护。
    Unprotect,
    /// 在字上睡眠。
    Wait,
    /// 唤醒字上的等待者。
    Wake,
    /// 取得平台 entropy。
    Entropy,
    /// 显式清零已提交范围。
    Zero,
    /// 设置 core dump 归属策略。
    SetDumpPolicy,
    /// 低内存提示；可选能力。
    LowMemoryHint,
    /// huge-page hint；可选能力，不是正确性保证。
    HugePageHint,
}

impl PlatformOp {
    /// 全部操作的稠密登记顺序；契约编码按它排序。
    pub const ALL: [Self; 13] = [
        Self::ReserveAligned,
        Self::Commit,
        Self::Decommit,
        Self::Release,
        Self::ProtectGuard,
        Self::Unprotect,
        Self::Wait,
        Self::Wake,
        Self::Entropy,
        Self::Zero,
        Self::SetDumpPolicy,
        Self::LowMemoryHint,
        Self::HugePageHint,
    ];

    /// 返回操作名；契约编码、GIR/LIR 与诊断都使用它。
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReserveAligned => "reserve_aligned",
            Self::Commit => "commit",
            Self::Decommit => "decommit",
            Self::Release => "release",
            Self::ProtectGuard => "protect_guard",
            Self::Unprotect => "unprotect",
            Self::Wait => "wait",
            Self::Wake => "wake",
            Self::Entropy => "entropy",
            Self::Zero => "zero",
            Self::SetDumpPolicy => "set_dump_policy",
            Self::LowMemoryHint => "low_memory_hint",
            Self::HugePageHint => "huge_page_hint",
        }
    }

    /// 判断该操作是否改变 range 或字的可观察状态。
    ///
    /// 只读操作仍受 `NoSafepointRegion` 约束（平台调用一律禁止进入该区域），但它们不推进
    /// 生命周期，也不会使 `decommit` 门禁失效。
    pub(crate) const fn mutating(self) -> bool {
        matches!(
            self,
            Self::ReserveAligned
                | Self::Commit
                | Self::Decommit
                | Self::Release
                | Self::ProtectGuard
                | Self::Unprotect
                | Self::Zero
                | Self::SetDumpPolicy
                | Self::HugePageHint
                | Self::LowMemoryHint
        )
    }

    /// 判断该操作是否可能阻塞当前协程。
    pub const fn blocking(self) -> bool {
        matches!(self, Self::Wait)
    }

    /// 返回该操作在失败时可能映射到的类别集合。
    ///
    /// 类别由该操作的失败集合推导，排序去重后进入契约编码。
    pub(crate) fn fault_classes(self) -> Vec<FaultClass> {
        let mut classes: Vec<FaultClass> = self
            .faults()
            .iter()
            .map(|error| error.fault_class())
            .collect();
        classes.sort();
        classes.dedup();
        classes
    }

    /// 返回该操作可能返回的失败集合。
    pub(crate) const fn faults(self) -> &'static [ProviderError] {
        match self {
            Self::ReserveAligned => &[
                ProviderError::ZeroBytes,
                ProviderError::NonPowerOfTwoAlignment,
                ProviderError::ArithmeticOverflow,
                ProviderError::OutOfSpace,
                ProviderError::MappingLimit,
            ],
            Self::Commit => &[
                ProviderError::UnknownRange,
                ProviderError::AlreadyCommitted,
                ProviderError::DoubleRelease,
            ],
            Self::Decommit => &[
                ProviderError::UnknownRange,
                ProviderError::NotCommitted,
                ProviderError::DoubleRelease,
            ],
            Self::Release => &[ProviderError::UnknownRange, ProviderError::DoubleRelease],
            Self::ProtectGuard => &[
                ProviderError::UnknownRange,
                ProviderError::NotCommitted,
                ProviderError::GuardOverlap,
            ],
            Self::Unprotect => &[ProviderError::UnknownRange, ProviderError::NotGuarded],
            Self::Wait => &[ProviderError::UnknownWaitWord],
            Self::Wake => &[ProviderError::UnknownWaitWord],
            Self::Entropy => &[ProviderError::EntropyUnavailable],
            Self::Zero => &[ProviderError::UnknownRange, ProviderError::NotCommitted],
            Self::SetDumpPolicy => &[
                ProviderError::UnknownRange,
                ProviderError::DoubleRelease,
                ProviderError::DumpPolicyRejected,
            ],
            Self::LowMemoryHint | Self::HugePageHint => {
                &[ProviderError::UnknownRange, ProviderError::DoubleRelease]
            }
        }
    }
}

impl std::fmt::Display for PlatformOp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// 一个操作目录项。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlatformOpEntry {
    pub(crate) name: String,
    /// 操作是否改变可观察状态。
    pub(crate) mutating: bool,
    /// 操作是否可能阻塞。
    pub(crate) blocking: bool,
    /// 失败映射到的统一分类，按类别名稳定排序。
    pub(crate) fault_classes: Vec<String>,
}

/// 平台操作目录。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlatformOpSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) ops: Vec<PlatformOpEntry>,
}

impl PlatformOpSchemaV1 {
    /// 由参照实现的规范操作表构造。
    pub(crate) fn fixed() -> Self {
        // 目录按名字稳定排序，使编码与 dump 与枚举声明顺序解耦。
        let mut ops: Vec<PlatformOpEntry> = PlatformOp::ALL
            .into_iter()
            .map(|op| PlatformOpEntry {
                name: op.name().to_owned(),
                mutating: op.mutating(),
                blocking: op.blocking(),
                fault_classes: op
                    .fault_classes()
                    .into_iter()
                    .map(|class| class.name().to_owned())
                    .collect(),
            })
            .collect();
        ops.sort_by(|left, right| left.name.cmp(&right.name));
        Self {
            schema: PLATFORM_SCHEMA,
            ops,
        }
    }

    /// 校验版本、非空、按名字稳定排序，且与 `PlatformOp` 全枚举一一对应。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != PLATFORM_SCHEMA {
            return Err(RawModelError::new("平台操作目录 schema 版本不匹配"));
        }
        if self.ops.is_empty() {
            return Err(RawModelError::new("平台操作目录不能为空"));
        }
        for pair in self.ops.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(RawModelError::new("平台操作目录没有按名字稳定排序"));
            }
        }
        if self.ops.len() != PlatformOp::ALL.len() {
            return Err(RawModelError::new("平台操作目录与登记的操集合数量不一致"));
        }
        for op in PlatformOp::ALL {
            if !self.ops.iter().any(|entry| entry.name == op.name()) {
                return Err(RawModelError::new(format!(
                    "平台操作目录缺少 `{}`",
                    op.name()
                )));
            }
        }
        for entry in &self.ops {
            let op = PlatformOp::ALL
                .into_iter()
                .find(|op| op.name() == entry.name)
                .ok_or_else(|| RawModelError::new("平台操作目录含未登记的操作"))?;
            if entry.mutating != op.mutating() || entry.blocking != op.blocking() {
                return Err(RawModelError::new(format!(
                    "平台操作 `{}` 的读写或阻塞分类与实现不一致",
                    entry.name
                )));
            }
            for class in &entry.fault_classes {
                if !FaultClass::ALL
                    .into_iter()
                    .any(|candidate| candidate.name() == class)
                {
                    return Err(RawModelError::new(format!(
                        "平台操作 `{}` 登记了未分类的失败 `{class}`",
                        entry.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.ops.len() * 32);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.ops.len() as u32).to_le_bytes());
        for entry in &self.ops {
            bytes.extend_from_slice(entry.name.as_bytes());
            bytes.push(0);
            bytes.push(u8::from(entry.mutating) | u8::from(entry.blocking) << 1);
            bytes.push(u8::try_from(entry.fault_classes.len()).expect("失败类别数量适配 u8"));
            for class in &entry.fault_classes {
                bytes.extend_from_slice(class.as_bytes());
                bytes.push(0);
            }
        }
        bytes
    }
}

/// 一条 range 状态迁移。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RangeTransitionV1 {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) trigger: String,
}

/// 一个状态在字节口径上的归属规则。
///
/// `Committed` 状态的 range 可以只提交一部分页，因此它的字节按 commit 位图拆到两个口径：
/// 未提交的剩余页计入 `range_reserved_bytes`，已提交的页计入 `runtime_committed_bytes`。
/// `split_by_commit` 精确记录这件事；字节级互斥由 commit 位图保证。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RangeStateCostV1 {
    pub(crate) state: String,
    /// 归属规则名，与 `RangeCost::name()` 一致。
    pub(crate) rule: String,
    /// 该状态的字节是否按 commit 位图拆分到两个口径。
    pub(crate) split_by_commit: bool,
}

/// range 的状态集合与迁移表。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RangeStateSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) states: Vec<String>,
    pub(crate) transitions: Vec<RangeTransitionV1>,
    /// 每个状态的字节口径归属，按 `states` 顺序排列。
    pub(crate) costs: Vec<RangeStateCostV1>,
}

impl RangeStateSchemaV1 {
    /// 返回参照实现允许的迁移表。
    ///
    /// `Released` 是终态：任何从 `Released` 出发的迁移都不存在，重复 release 只会得到
    /// `DoubleRelease`。
    pub(crate) fn fixed() -> Self {
        let states: Vec<String> = [
            RangeState::Reserved,
            RangeState::Committed,
            RangeState::Decommitted,
            RangeState::Released,
        ]
        .into_iter()
        .map(|state| state.name().to_owned())
        .collect();
        let costs = [
            RangeState::Reserved,
            RangeState::Committed,
            RangeState::Decommitted,
            RangeState::Released,
        ]
        .into_iter()
        .map(|state| RangeStateCostV1 {
            state: state.name().to_owned(),
            rule: state.cost().name().to_owned(),
            split_by_commit: state.cost() == RangeCost::Committed,
        })
        .collect();
        Self {
            schema: PLATFORM_SCHEMA,
            states,
            costs,
            transitions: vec![
                RangeTransitionV1 {
                    from: "reserved".to_owned(),
                    to: "committed".to_owned(),
                    trigger: "commit".to_owned(),
                },
                RangeTransitionV1 {
                    from: "committed".to_owned(),
                    to: "decommitted".to_owned(),
                    trigger: "decommit-after-lease-and-grace".to_owned(),
                },
                RangeTransitionV1 {
                    from: "decommitted".to_owned(),
                    to: "committed".to_owned(),
                    trigger: "commit".to_owned(),
                },
                RangeTransitionV1 {
                    from: "reserved".to_owned(),
                    to: "released".to_owned(),
                    trigger: "release".to_owned(),
                },
                RangeTransitionV1 {
                    from: "committed".to_owned(),
                    to: "released".to_owned(),
                    trigger: "release".to_owned(),
                },
                RangeTransitionV1 {
                    from: "decommitted".to_owned(),
                    to: "released".to_owned(),
                    trigger: "release".to_owned(),
                },
            ],
        }
    }

    /// 校验状态集合、迁移表与实现一致：四状态齐全、released 是终态、没有自环。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != PLATFORM_SCHEMA {
            return Err(RawModelError::new("range 状态 schema 版本不匹配"));
        }
        let expected: Vec<String> = [
            RangeState::Reserved,
            RangeState::Committed,
            RangeState::Decommitted,
            RangeState::Released,
        ]
        .into_iter()
        .map(|state| state.name().to_owned())
        .collect();
        if self.states != expected {
            return Err(RawModelError::new("range 状态集合与登记的迁移实现不一致"));
        }
        for transition in &self.transitions {
            if !self.states.contains(&transition.from) || !self.states.contains(&transition.to) {
                return Err(RawModelError::new("range 迁移表引用了未登记的状态"));
            }
            if transition.from == transition.to {
                return Err(RawModelError::new("range 迁移表不能包含自环"));
            }
            if transition.from == RangeState::Released.name() {
                return Err(RawModelError::new(
                    "released 是终态，不能作为任何迁移的起点",
                ));
            }
        }
        for state in &self.states {
            if state == RangeState::Released.name() {
                continue;
            }
            if !self
                .transitions
                .iter()
                .any(|transition| &transition.from == state)
            {
                return Err(RawModelError::new(format!(
                    "状态 `{state}` 没有任何离开路径"
                )));
            }
        }
        for state in [RangeState::Committed, RangeState::Decommitted] {
            if !self
                .transitions
                .iter()
                .any(|transition| transition.to == state.name())
            {
                return Err(RawModelError::new(format!(
                    "状态 `{}` 没有任何进入路径",
                    state.name()
                )));
            }
        }
        if self.costs.len() != self.states.len() {
            return Err(RawModelError::new("range 状态的口径归属没有覆盖全部状态"));
        }
        for cost in &self.costs {
            let state = [
                RangeState::Reserved,
                RangeState::Committed,
                RangeState::Decommitted,
                RangeState::Released,
            ]
            .into_iter()
            .find(|state| state.name() == cost.state)
            .ok_or_else(|| RawModelError::new("range 状态口径引用了未登记的状态"))?;
            if cost.rule != state.cost().name() {
                return Err(RawModelError::new(format!(
                    "状态 `{}` 的字节口径与实现不一致",
                    cost.state
                )));
            }
            if cost.split_by_commit != (state.cost() == RangeCost::Committed) {
                return Err(RawModelError::new(format!(
                    "状态 `{}` 的 commit 拆分标记与实现不一致",
                    cost.state
                )));
            }
        }
        // 每个活跃状态都必须至少贡献一个口径，否则那部分字节会从内存压力统计里消失。
        for state in [
            RangeState::Reserved,
            RangeState::Committed,
            RangeState::Decommitted,
        ] {
            if state.cost() == RangeCost::None {
                return Err(RawModelError::new(format!(
                    "状态 `{}` 没有归属任何字节口径",
                    state.name()
                )));
            }
        }
        if RangeState::Released.cost() != RangeCost::None {
            return Err(RawModelError::new("released 状态不能计入任何字节口径"));
        }
        if self
            .costs
            .iter()
            .filter(|cost| cost.split_by_commit)
            .count()
            != 1
        {
            return Err(RawModelError::new(
                "只有一个状态可以按 commit 位图拆分字节口径",
            ));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        for state in &self.states {
            bytes.extend_from_slice(state.as_bytes());
            bytes.push(0);
        }
        for cost in &self.costs {
            bytes.extend_from_slice(cost.state.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(cost.rule.as_bytes());
            bytes.push(0);
            bytes.push(u8::from(cost.split_by_commit));
        }
        bytes.extend_from_slice(&(self.transitions.len() as u32).to_le_bytes());
        for transition in &self.transitions {
            bytes.extend_from_slice(transition.from.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(transition.to.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(transition.trigger.as_bytes());
            bytes.push(0);
        }
        bytes
    }
}

/// 一个二次幂 extent class。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExtentClassV1 {
    pub(crate) bytes: u64,
    pub(crate) alignment: u64,
    /// 该 class 是否达到 huge-page 阈值。
    pub(crate) huge_page: bool,
}

/// extent class 阶梯。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExtentClassSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) classes: Vec<ExtentClassV1>,
}

impl ExtentClassSchemaV1 {
    /// 由参照实现的阶梯构造。
    pub(crate) fn fixed() -> Self {
        Self {
            schema: PLATFORM_SCHEMA,
            classes: EXTENT_CLASS_LADDER
                .into_iter()
                .map(|bytes| ExtentClassV1 {
                    bytes,
                    alignment: bytes,
                    huge_page: bytes >= 2 * 1024 * 1024,
                })
                .collect(),
        }
    }

    /// 校验阶梯严格倍增、每级二次幂、对齐等于大小，且最小项是平台页。
    pub(crate) fn verify(
        &self,
        page_bytes: u64,
        huge_page_bytes: u64,
    ) -> Result<(), RawModelError> {
        if self.schema != PLATFORM_SCHEMA {
            return Err(RawModelError::new("extent class schema 版本不匹配"));
        }
        if self.classes.is_empty() {
            return Err(RawModelError::new("extent class 阶梯不能为空"));
        }
        if self.classes[0].bytes != page_bytes {
            return Err(RawModelError::new(
                "extent class 阶梯的最小项必须等于平台页大小",
            ));
        }
        if self.classes.len() != EXTENT_CLASS_LADDER.len() {
            return Err(RawModelError::new(
                "extent class 阶梯与登记的阶梯长度不一致",
            ));
        }
        for (index, class) in self.classes.iter().enumerate() {
            if !class.bytes.is_power_of_two() || class.alignment != class.bytes {
                return Err(RawModelError::new(
                    "extent class 必须是二次幂且对齐等于大小",
                ));
            }
            if class.huge_page != (class.bytes >= huge_page_bytes) {
                return Err(RawModelError::new(
                    "extent class 的 huge-page 标记与阈值不一致",
                ));
            }
            if index != 0 && class.bytes != self.classes[index - 1].bytes * 2 {
                return Err(RawModelError::new("extent class 阶梯必须逐级倍增"));
            }
        }
        if self.classes.last().map(|class| class.bytes) != Some(huge_page_bytes) {
            return Err(RawModelError::new(
                "extent class 阶梯的最大项必须等于 huge-page 阈值",
            ));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + self.classes.len() * 17);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.classes.len() as u32).to_le_bytes());
        for class in &self.classes {
            bytes.extend_from_slice(&class.bytes.to_le_bytes());
            bytes.extend_from_slice(&class.alignment.to_le_bytes());
            bytes.push(u8::from(class.huge_page));
        }
        bytes
    }
}

/// 平台策略：清零、entropy、dump policy 与可选 hint。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlatformPolicyV1 {
    pub(crate) profile: String,
    pub(crate) page_bytes: u64,
    pub(crate) huge_page_bytes: u64,
    pub(crate) guard_bytes: u64,
    pub(crate) mapping_limit: u64,
    /// 平台是否保证 commit 得到的匿名页已清零。
    pub(crate) commit_zeroes: bool,
    /// entropy 源名；没有平台 entropy 时使用启动期闭世界 seed。
    pub(crate) entropy_source: String,
    /// 平台 entropy 是否可用。
    pub(crate) entropy_available: bool,
    /// dump policy 的默认值。
    pub(crate) dump_policy_default: String,
    /// huge-page hint 是否可用；hint 不是正确性保证。
    pub(crate) huge_page_hint: bool,
    /// 低内存提示是否可用。
    pub(crate) low_memory_hint: bool,
}

impl PlatformPolicyV1 {
    /// 由 profile 常量构造策略。
    pub(crate) fn fixed(profile: PlatformProfile) -> Self {
        let constants = profile.constants();
        Self {
            profile: profile.name().to_owned(),
            page_bytes: constants.page_bytes,
            huge_page_bytes: constants.huge_page_bytes,
            guard_bytes: constants.guard_bytes,
            mapping_limit: constants.mapping_limit,
            commit_zeroes: true,
            entropy_source: "runtime-seed".to_owned(),
            entropy_available: constants.entropy_available,
            dump_policy_default: constants.dump_policy_default.name().to_owned(),
            huge_page_hint: true,
            low_memory_hint: true,
        }
    }

    /// 校验策略与 profile 常量、阶梯与 dump policy 取值一致。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        let profile = PlatformProfile::ALL
            .into_iter()
            .find(|profile| profile.name() == self.profile)
            .ok_or_else(|| RawModelError::new("平台策略引用未登记的 profile"))?;
        let constants = profile.constants();
        if !constants.is_valid() {
            return Err(RawModelError::new("平台常量不满足页与对齐规则"));
        }
        if self.page_bytes != constants.page_bytes
            || self.huge_page_bytes != constants.huge_page_bytes
            || self.guard_bytes != constants.guard_bytes
            || self.mapping_limit != constants.mapping_limit
        {
            return Err(RawModelError::new(
                "平台策略的页、huge page、guard 或 mapping 上限与 profile 常量不一致",
            ));
        }
        if self.guard_bytes > self.page_bytes {
            return Err(RawModelError::new("guard 字节数不能超过平台页"));
        }
        if !matches!(self.dump_policy_default.as_str(), "excluded" | "included") {
            return Err(RawModelError::new("dump policy 默认值未登记"));
        }
        if self.entropy_source.is_empty() {
            return Err(RawModelError::new("entropy 源名不能为空"));
        }
        if self.entropy_available != constants.entropy_available {
            return Err(RawModelError::new("entropy 可用性与 profile 常量不一致"));
        }
        if !self.entropy_available && self.entropy_source != "runtime-seed" {
            return Err(RawModelError::new(
                "平台 entropy 不可用时必须回退到启动期闭世界 seed",
            ));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(self.profile.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&self.page_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.huge_page_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.guard_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.mapping_limit.to_le_bytes());
        bytes.push(u8::from(self.commit_zeroes));
        bytes.extend_from_slice(self.entropy_source.as_bytes());
        bytes.push(0);
        bytes.push(u8::from(self.entropy_available));
        bytes.extend_from_slice(self.dump_policy_default.as_bytes());
        bytes.push(0);
        bytes.push(u8::from(self.huge_page_hint));
        bytes.push(u8::from(self.low_memory_hint));
        bytes
    }
}

/// 由编译产物推导出的平台范围需求视图。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlatformRangeDemand {
    /// 承载 raw slab span 的 extent 数量下界。
    pub payload_extents: u32,
    /// 承载 stack span 的 extent 数量下界。
    pub stack_extents: u32,
    /// 承载 owner directory、range descriptor 与 message node 的 extent 数量下界。
    pub metadata_extents: u32,
    /// 需要 guard 页的 extent 数量下界。
    pub guard_extents: u32,
    /// 参与范围管理的 owner 数量下界。
    pub owners: u32,
}

impl PlatformRangeDemand {
    /// 由 raw 平面的 owner 与记录需求推导下界。
    ///
    /// 规则固定：每个 owner 至少各需要一个 payload 与一个 metadata extent；每个协程创建点
    /// 至少需要一个 stack extent；每个需要 guard 的 extent 由 stack 与 metadata 需求派生。
    /// 这些是**下界**，不是容量承诺；huge-page hint 与 trim 都不改变它们。
    pub(crate) fn derive(
        owners: u32,
        coroutine_sites: u32,
        resource_sites: u32,
        runtime_raw_sites: u32,
    ) -> Self {
        let owners = owners.max(1);
        let payload_extents = owners
            .saturating_add(resource_sites)
            .saturating_add(runtime_raw_sites);
        let metadata_extents = owners.saturating_mul(2);
        let stack_extents = coroutine_sites.max(owners);
        Self {
            payload_extents,
            stack_extents,
            metadata_extents,
            guard_extents: stack_extents.saturating_add(metadata_extents),
            owners,
        }
    }

    /// 校验需求视图的下界关系：每个 owner 至少各有一个 payload 与 metadata extent。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.owners == 0 {
            return Err(RawModelError::new("平台范围需求必须至少登记一个 owner"));
        }
        if self.payload_extents < self.owners {
            return Err(RawModelError::new(
                "payload extent 需求低于 owner 数量的下界",
            ));
        }
        if self.metadata_extents < self.owners {
            return Err(RawModelError::new(
                "metadata extent 需求低于 owner 数量的下界",
            ));
        }
        if self.stack_extents < self.owners {
            return Err(RawModelError::new("stack extent 需求低于 owner 数量的下界"));
        }
        if self.guard_extents < self.stack_extents {
            return Err(RawModelError::new(
                "guard extent 需求不能低于 stack extent 需求",
            ));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20);
        bytes.extend_from_slice(&self.payload_extents.to_le_bytes());
        bytes.extend_from_slice(&self.stack_extents.to_le_bytes());
        bytes.extend_from_slice(&self.metadata_extents.to_le_bytes());
        bytes.extend_from_slice(&self.guard_extents.to_le_bytes());
        bytes.extend_from_slice(&self.owners.to_le_bytes());
        bytes
    }
}

/// 平台范围契约段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlatformRangeSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) ops: PlatformOpSchemaV1,
    pub(crate) states: RangeStateSchemaV1,
    pub(crate) classes: ExtentClassSchemaV1,
    pub(crate) policy: PlatformPolicyV1,
    pub(crate) demand: PlatformRangeDemand,
    /// 两个 profile 的失败映射，按 profile 名稳定排序。
    pub(crate) fault_map: Vec<PlatformFaultEntry>,
}

/// 一个 profile 下某个失败的统一分类。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlatformFaultEntry {
    pub(crate) profile: String,
    pub(crate) error: String,
    pub(crate) class: String,
}

impl PlatformRangeSchemaV1 {
    /// 由 profile 与需求视图构建契约段。
    pub(crate) fn build(
        profile: PlatformProfile,
        demand: PlatformRangeDemand,
    ) -> Result<Self, RawModelError> {
        // 映射表按 (profile, 失败名) 稳定排序，使编码与枚举声明顺序解耦。
        let mut fault_map: Vec<PlatformFaultEntry> = PlatformProfile::ALL
            .into_iter()
            .flat_map(|candidate| {
                ProviderError::ALL
                    .into_iter()
                    .map(move |error| PlatformFaultEntry {
                        profile: candidate.name().to_owned(),
                        error: error.name().to_owned(),
                        class: candidate.fault_class(error).name().to_owned(),
                    })
            })
            .collect();
        fault_map.sort_by(|left, right| {
            (left.profile.as_str(), left.error.as_str())
                .cmp(&(right.profile.as_str(), right.error.as_str()))
        });
        let section = Self {
            schema: PLATFORM_SCHEMA,
            ops: PlatformOpSchemaV1::fixed(),
            states: RangeStateSchemaV1::fixed(),
            classes: ExtentClassSchemaV1::fixed(),
            policy: PlatformPolicyV1::fixed(profile),
            demand,
            fault_map,
        };
        section.verify()?;
        Ok(section)
    }

    /// 校验全部子段、两个 profile 的失败映射一致性与需求下界。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != PLATFORM_SCHEMA {
            return Err(RawModelError::new("平台范围契约 schema 版本不匹配"));
        }
        self.ops.verify()?;
        self.states.verify()?;
        self.policy.verify()?;
        self.classes
            .verify(self.policy.page_bytes, self.policy.huge_page_bytes)?;
        self.demand.verify()?;
        if self.fault_map.len() != PlatformProfile::ALL.len() * ProviderError::ALL.len() {
            return Err(RawModelError::new(
                "失败映射没有覆盖全部 profile 与失败类别的组合",
            ));
        }
        for pair in self.fault_map.windows(2) {
            if (pair[0].profile.as_str(), pair[0].error.as_str())
                >= (pair[1].profile.as_str(), pair[1].error.as_str())
            {
                return Err(RawModelError::new("失败映射没有稳定排序"));
            }
        }
        for error in ProviderError::ALL {
            let classes: Vec<&str> = self
                .fault_map
                .iter()
                .filter(|entry| entry.error == error.name())
                .map(|entry| entry.class.as_str())
                .collect();
            if classes.len() != PlatformProfile::ALL.len() {
                return Err(RawModelError::new(format!(
                    "失败 `{}` 没有在两个 profile 上登记",
                    error.name()
                )));
            }
            if classes.iter().any(|class| *class != classes[0]) {
                return Err(RawModelError::new(format!(
                    "失败 `{}` 在两个 profile 上的映射不一致",
                    error.name()
                )));
            }
            if classes[0] != error.fault_class().name() {
                return Err(RawModelError::new(format!(
                    "失败 `{}` 的映射与实现不一致",
                    error.name()
                )));
            }
        }
        if !FaultClass::ALL.into_iter().all(|class| {
            self.fault_map
                .iter()
                .any(|entry| entry.class == class.name())
        }) {
            return Err(RawModelError::new("失败映射没有覆盖全部统一分类"));
        }
        Ok(())
    }

    /// 返回操作数量。
    pub(crate) fn op_count(&self) -> u32 {
        u32::try_from(self.ops.ops.len()).expect("平台操作数量适配 u32")
    }

    /// 返回 extent class 数量。
    pub(crate) fn class_count(&self) -> u32 {
        u32::try_from(self.classes.classes.len()).expect("extent class 数量适配 u32")
    }

    /// 返回 profile 名。
    pub(crate) fn profile(&self) -> &str {
        &self.policy.profile
    }

    /// 返回平台页大小。
    pub(crate) const fn page_bytes(&self) -> u64 {
        self.policy.page_bytes
    }

    /// 返回 huge-page 阈值。
    pub(crate) const fn huge_page_bytes(&self) -> u64 {
        self.policy.huge_page_bytes
    }

    /// 返回 guard 字节数。
    pub(crate) const fn guard_bytes(&self) -> u64 {
        self.policy.guard_bytes
    }

    /// 返回 dump policy 默认值。
    pub(crate) fn dump_policy_default(&self) -> &str {
        &self.policy.dump_policy_default
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.ops.canonical_bytes());
        bytes.extend_from_slice(&self.states.canonical_bytes());
        bytes.extend_from_slice(&self.classes.canonical_bytes());
        bytes.extend_from_slice(&self.policy.canonical_bytes());
        bytes.extend_from_slice(&self.demand.canonical_bytes());
        bytes.extend_from_slice(&(self.fault_map.len() as u32).to_le_bytes());
        for entry in &self.fault_map {
            bytes.extend_from_slice(entry.profile.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(entry.error.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(entry.class.as_bytes());
            bytes.push(0);
        }
        bytes
    }
}
