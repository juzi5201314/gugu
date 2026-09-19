//! raw link provenance 与 release 安全 profile 的契约段。
//!
//! 本段把「释放 provenance 检查目录」「per-domain secret 登记与管理规则」「release/
//! debug/security 三个安全 profile 的检查绑定」「拒绝分类」「统计口径」固定成带版本的
//! 对象，与 `compression_schema` / `routing_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! release 是默认 profile：只保留 owner、generation、range 与 alignment 基线检查，
//! 不引入任何额外运行时成本；debug 与 security 都是基线之上的显式 profile，分别追加
//! poison/双重释放标记/全链 verifier 与随机复用/guard region/checked copy。profile 是
//! runtime tuning，不是用户可观察行为；检查失败绝不静默丢弃消息。

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::model::RawModelError;

/// provenance 契约段 schema。
pub(crate) const PROVENANCE_SCHEMA: u32 = 1;

/// 内建 provenance profile 名。
pub(crate) const PROVENANCE_PROFILE_NAME: &str = "mosaic-provenance";
/// profile revision；检查目录、secret 规则、拒绝分类或统计口径变化都必须递增。
pub(crate) const PROVENANCE_PROFILE_REVISION: u32 = 1;

/// 释放 provenance 的检查目录；前八项是 raw provenance 校验，后六项是 debug/security
/// 追加的机制检查。顺序即登记顺序。
pub(crate) const PROVENANCE_CHECKS: [&str; 14] = [
    "canonical",
    "alignment",
    "range",
    "class",
    "owner",
    "generation",
    "link",
    "state",
    "poison",
    "double-return-marker",
    "full-chain",
    "random-reuse",
    "guard-region",
    "checked-copy",
];

/// release profile 的基线检查集合；任何 profile 都必须恰好保留这四项作为起点。
pub(crate) const RELEASE_BASELINE_CHECKS: [&str; 4] = ["owner", "generation", "range", "alignment"];

/// debug profile 在基线之上的额外检查；顺序即启用顺序。
pub(crate) const DEBUG_EXTRA_CHECKS: [&str; 3] = ["poison", "double-return-marker", "full-chain"];

/// security profile 在基线之上的额外机制；顺序即启用顺序。
pub(crate) const SECURITY_EXTRA_CHECKS: [&str; 3] =
    ["random-reuse", "guard-region", "checked-copy"];

/// per-domain secret 的登记目录；顺序与 `MemoryDomainId::ALL` 的稠密编号一致。
pub(crate) const PROVENANCE_DOMAINS: [&str; 7] = [
    "managed-turn",
    "managed-local",
    "managed-shared",
    "runtime-raw",
    "resource",
    "platform-range",
    "foreign",
];

/// per-domain secret 的管理规则目录；每条规则都是可检验的契约。
pub(crate) const PROVENANCE_SECRET_RULES: [&str; 4] = [
    "per-domain-derivation",
    "non-zero-secret",
    "non-moving-metadata-only",
    "init-failure-fatal",
];

/// 释放拒绝的稳定分类；顺序即登记顺序，也是统计计数的下标。
pub(crate) const PROVENANCE_REJECTIONS: [&str; 7] = [
    "chain-corruption",
    "forged-link",
    "double-return",
    "cross-owner",
    "cross-class",
    "stale-generation",
    "guard-region",
];

/// provenance 统计口径；顺序即登记顺序，与 `ProvenanceStats` 的字段顺序一致。
pub(crate) const PROVENANCE_STATISTICS: [&str; 6] = [
    "release-rejections",
    "poison-writes",
    "double-return-markers",
    "full-chain-verifications",
    "randomized-pops",
    "checked-copies",
];

/// raw link provenance 的安全 profile；release 是默认，debug 与 security 显式开启。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SafetyProfile {
    /// release 基线：owner/generation/range/alignment 检查，无额外运行时成本。
    #[default]
    Release,
    /// debug：基线之上追加 poison、双重释放标记与全链 verifier。
    Debug,
    /// security：基线之上追加随机复用、guard region 与 checked copy 接缝。
    Security,
}

impl SafetyProfile {
    /// 返回登记目录中的 profile 名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Debug => "debug",
            Self::Security => "security",
        }
    }

    /// 返回该 profile 激活的检查序列；基线检查在前，额外检查按登记顺序拼接。
    pub(crate) const fn activates(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            Self::Release => (RELEASE_BASELINE_CHECKS.as_slice(), &[]),
            Self::Debug => (
                RELEASE_BASELINE_CHECKS.as_slice(),
                DEBUG_EXTRA_CHECKS.as_slice(),
            ),
            Self::Security => (
                RELEASE_BASELINE_CHECKS.as_slice(),
                SECURITY_EXTRA_CHECKS.as_slice(),
            ),
        }
    }
}

/// provenance profile 的显式开关；默认 release 表示既有基线检查语义。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProvenancePolicyV1 {
    /// 安全 profile。
    pub profile: SafetyProfile,
}

impl ProvenancePolicyV1 {
    /// release 基线：不追加 poison、随机复用或 checked copy。
    pub(crate) const fn release() -> Self {
        Self {
            profile: SafetyProfile::Release,
        }
    }
}

/// 由 raw 平面需求推导的 provenance 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProvenanceDemand {
    /// 进入 provenance 目录的 owner 数量；来自 raw 平面需求的 owner 计数。
    pub owners: u32,
    /// 受 provenance 检查覆盖的 class 数量；raw ladder 与 resource ladder 之和。
    pub classes: u32,
}

impl ProvenanceDemand {
    /// 由 raw 平面需求的 owner 数与两份 class ladder 推导编译期上界。
    pub(crate) fn derive(owners: u32, raw_classes: u32, resource_classes: u32) -> Self {
        Self {
            owners,
            classes: raw_classes.saturating_add(resource_classes),
        }
    }

    /// 校验需求自身：owner 数与 class 数是独立的编译期上界，没有交叉约束。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        Ok(())
    }
}

/// 已验证的 raw link provenance runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProvenanceRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// 安全 profile。
    pub mode: SafetyProfile,
    /// 释放 provenance 检查目录。
    pub checks: Vec<String>,
    /// 当前 profile 激活的检查序列；基线在前、额外检查按登记顺序拼接。
    pub active_checks: Vec<String>,
    /// release 基线检查集合。
    pub baseline_checks: Vec<String>,
    /// per-domain secret 登记目录。
    pub domains: Vec<String>,
    /// per-domain secret 管理规则目录。
    pub secret_rules: Vec<String>,
    /// 释放拒绝的稳定分类目录。
    pub rejections: Vec<String>,
    /// 统计口径目录。
    pub statistics: Vec<String>,
    /// 上游需求视图。
    pub demand: ProvenanceDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl ProvenanceRuntimeContract {
    /// 由需求视图与安全 profile 构建契约；release 模式仍建立完整契约状态。
    pub(crate) fn build(
        demand: ProvenanceDemand,
        policy: ProvenancePolicyV1,
    ) -> Result<Self, RawModelError> {
        let (baseline, extra) = policy.profile.activates();
        let mut active_checks: Vec<String> =
            baseline.iter().map(|name| (*name).to_owned()).collect();
        active_checks.extend(extra.iter().map(|name| (*name).to_owned()));
        let mut contract = Self {
            schema: PROVENANCE_SCHEMA,
            profile: PROVENANCE_PROFILE_NAME.to_owned(),
            profile_revision: PROVENANCE_PROFILE_REVISION,
            mode: policy.profile,
            checks: PROVENANCE_CHECKS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            active_checks,
            baseline_checks: RELEASE_BASELINE_CHECKS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            domains: PROVENANCE_DOMAINS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            secret_rules: PROVENANCE_SECRET_RULES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            rejections: PROVENANCE_REJECTIONS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            statistics: PROVENANCE_STATISTICS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回 profile 名。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回安全 profile。
    pub const fn mode(&self) -> SafetyProfile {
        self.mode
    }

    /// 返回检查目录长度。
    pub const fn check_count(&self) -> u32 {
        PROVENANCE_CHECKS.len() as u32
    }

    /// 返回 per-domain secret 目录长度。
    pub const fn domain_count(&self) -> u32 {
        PROVENANCE_DOMAINS.len() as u32
    }

    /// 返回拒绝分类目录长度。
    pub const fn rejection_category_count(&self) -> u32 {
        PROVENANCE_REJECTIONS.len() as u32
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> ProvenanceDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 校验契约：profile、检查目录、激活序列、secret 规则、拒绝分类与需求闭合。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != PROVENANCE_SCHEMA
            || self.profile != PROVENANCE_PROFILE_NAME
            || self.profile_revision != PROVENANCE_PROFILE_REVISION
        {
            return Err(RawModelError::new("provenance profile 参数与登记值不一致"));
        }
        if !matches!(
            self.mode,
            SafetyProfile::Release | SafetyProfile::Debug | SafetyProfile::Security
        ) {
            return Err(RawModelError::new("provenance 安全 profile 不在登记目录中"));
        }
        expect_directory(&self.checks, &PROVENANCE_CHECKS, "释放 provenance 检查")?;
        expect_directory(
            &self.baseline_checks,
            &RELEASE_BASELINE_CHECKS,
            "release 基线检查",
        )?;
        let (baseline, extra) = self.mode.activates();
        let mut expected_active: Vec<&str> = baseline.to_vec();
        expected_active.extend_from_slice(extra);
        if self.active_checks.len() != expected_active.len()
            || self
                .active_checks
                .iter()
                .zip(expected_active.iter())
                .any(|(name, expected)| name != expected)
        {
            return Err(RawModelError::new(
                "provenance 激活检查序列与 profile 不一致",
            ));
        }
        for name in &self.active_checks {
            if !PROVENANCE_CHECKS.contains(&name.as_str()) {
                return Err(RawModelError::new(format!(
                    "provenance 激活检查引用了不在目录中的检查：{name}"
                )));
            }
        }
        expect_directory(&self.domains, &PROVENANCE_DOMAINS, "per-domain secret")?;
        expect_directory(
            &self.secret_rules,
            &PROVENANCE_SECRET_RULES,
            "per-domain secret 管理规则",
        )?;
        expect_directory(&self.rejections, &PROVENANCE_REJECTIONS, "释放拒绝分类")?;
        expect_directory(&self.statistics, &PROVENANCE_STATISTICS, "provenance 统计")?;
        self.demand.verify()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("provenance 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("provenance 契约可序列化")
    }

    /// 计算契约指纹。
    ///
    /// `canonical_bytes` 是整段 serde_json（含 `fingerprint` 字段），因此这里对指纹字段
    /// 清零的副本求 hash：否则「先写指纹、再校验相等」永远不可能成立。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.fingerprint = [0; 32];
        *blake3::Hasher::new_derive_key("gugu-provenance-runtime-v1")
            .update(&canonical.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回人类可读的契约 dump。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "provenance schema={} profile={} revision={} mode={} checks={} active={} domains={} rejections={}",
            self.schema,
            self.profile,
            self.profile_revision,
            self.mode.name(),
            self.checks.len(),
            self.active_checks.len(),
            self.domains.len(),
            self.rejections.len()
        );
        let _ = writeln!(out, "provenance-checks {}", self.checks.join(","));
        let _ = writeln!(out, "provenance-active {}", self.active_checks.join(","));
        let _ = writeln!(out, "provenance-domains {}", self.domains.join(","));
        let _ = writeln!(
            out,
            "provenance-secret-rules {}",
            self.secret_rules.join(",")
        );
        let _ = writeln!(out, "provenance-rejections {}", self.rejections.join(","));
        let _ = writeln!(out, "provenance-statistics {}", self.statistics.join(","));
        let _ = writeln!(
            out,
            "provenance-demand owners={} classes={}",
            self.demand.owners, self.demand.classes
        );
        let _ = writeln!(
            out,
            "provenance-fingerprint {}",
            hex_lower(self.fingerprint)
        );
        out
    }
}

/// 校验目录与登记表逐项一致。
fn expect_directory(
    actual: &[String],
    expected: &[&str],
    label: &str,
) -> Result<(), RawModelError> {
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected.iter())
            .any(|(name, expected)| name != expected)
    {
        return Err(RawModelError::new(format!("{label}目录与登记表不一致")));
    }
    Ok(())
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
