//! owner-directed return 的 temporal radix fan-out 契约段。
//!
//! 本段把「路由模式目录」「固定 `2^k` bucket 与有限 levels」「转发 hop 上限」
//! 「maintenance 相位序列」「由 raw 平面需求推导的编译期上界」固定成带版本的对象，
//! 与 `compression_schema` / `block_return_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! direct 是默认模式，不分配任何 bucket 表；radix 只在显式 profile 开启后启用，
//! 且只拦截 return 族消息。mode 是 runtime tuning profile，不是用户可观察行为。

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::model::RawModelError;

/// routing 契约段 schema。
pub(crate) const ROUTING_SCHEMA: u32 = 1;

/// 内建 routing profile 名。
pub(crate) const ROUTING_PROFILE_NAME: &str = "mosaic-routing";
/// profile revision；模式目录、bucket 参数、hop 上限或相位序列变化都必须递增。
pub(crate) const ROUTING_PROFILE_REVISION: u32 = 1;

/// 路由模式目录；顺序即登记顺序。
pub(crate) const ROUTE_MODES: [&str; 2] = ["direct", "radix"];

/// radix bucket 的位数 `k`；高 fan-out profile 默认 `k = 6`。
pub(crate) const RADIX_BUCKET_LOG2: u32 = 6;
/// radix bucket 数量；固定 `2^k`。
pub(crate) const RADIX_BUCKETS: u32 = 1 << RADIX_BUCKET_LOG2;
/// 路由目录最多使用的固定层数；超过目录容量的解析退回 domain injection。
pub(crate) const MAX_RADIX_LEVELS: u32 = 2;
/// 转发 hop 的固定上限；必须不小于目录层数。
pub(crate) const RADIX_HOP_LIMIT: u32 = 4;

/// radix 模式拦截的消息族；其余消息族保持 direct 路径。
pub(crate) const ROUTED_FAMILIES: [&str; 1] = ["return"];

/// 维护相位目录；顺序即两条切换序列的合法推进顺序。
pub(crate) const ROUTING_MAINTENANCE_PHASES: [&str; 7] = [
    "idle",
    "freeze-target-cache",
    "flush-staging",
    "drain-buckets",
    "drain-forwarding",
    "publish-mode",
    "restore-target-cache",
];
/// direct → radix 的切换序列；先冻结 target cache 并冲刷 partial batch，再发布新 mode。
pub(crate) const DIRECT_TO_RADIX_PHASES: [&str; 3] =
    ["freeze-target-cache", "flush-staging", "publish-mode"];
/// radix → direct 的切换序列；先排空 bucket 与 forwarding chain，再恢复 target cache。
pub(crate) const RADIX_TO_DIRECT_PHASES: [&str; 4] = [
    "drain-buckets",
    "drain-forwarding",
    "publish-mode",
    "restore-target-cache",
];

/// 路由统计名；顺序即登记顺序，与 `RoutingStats` 的字段顺序一致。
pub(crate) const ROUTING_STATISTICS: [&str; 5] = [
    "remote-return-hops",
    "radix-batches",
    "maintenance-switches",
    "old-topology-drained-batches",
    "injection-fallbacks",
];

/// 路由模式；direct 是默认，radix 只在 profile 显式开启后启用。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteMode {
    /// direct owner routing：target inbox 直达，不分配 bucket 表。
    #[default]
    Direct,
    /// temporal radix fan-out：固定 `2^k` bucket 分层 staging，按 hop 上限转发。
    Radix,
}

impl RouteMode {
    /// 返回登记目录中的模式名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Radix => "radix",
        }
    }
}

/// 路由 profile 的显式开关；默认 direct 表示现有 owner inbox 直达语义。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RoutingPolicyV1 {
    /// 路由模式。
    pub mode: RouteMode,
}

impl RoutingPolicyV1 {
    /// direct 模式：不分配 bucket 表，return 消息直达 owner inbox。
    pub(crate) const fn direct() -> Self {
        Self {
            mode: RouteMode::Direct,
        }
    }
}

/// 由 raw 平面需求推导的路由需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RoutingDemand {
    /// 需要进入路由目录的 owner 数量；来自 raw 平面需求的 owner 计数。
    pub owners: u32,
    /// 可能发布跨 owner return 的站点上界；raw 与 resource 站点之和。
    pub return_publish_sites: u32,
}

impl RoutingDemand {
    /// 由 raw 平面需求的 owner 数与 raw/resource 分配站点数推导编译期上界。
    pub(crate) fn derive(owners: u32, runtime_raw_sites: u32, resource_sites: u32) -> Self {
        Self {
            owners,
            return_publish_sites: runtime_raw_sites.saturating_add(resource_sites),
        }
    }

    /// 校验需求自身：两个字段都是独立的编译期上界，没有交叉约束。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        Ok(())
    }
}

/// 已验证的 temporal radix fan-out runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RoutingRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// 路由模式。
    pub mode: RouteMode,
    /// radix bucket 的位数 `k`。
    pub bucket_log2: u32,
    /// radix bucket 数量；固定 `2^k`。
    pub bucket_count: u32,
    /// 路由目录最大层数。
    pub max_levels: u32,
    /// 转发 hop 上限。
    pub hop_limit: u32,
    /// radix 模式拦截的消息族目录。
    pub routed_families: Vec<String>,
    /// 维护相位目录。
    pub maintenance_phases: Vec<String>,
    /// direct → radix 的切换序列。
    pub direct_to_radix_phases: Vec<String>,
    /// radix → direct 的切换序列。
    pub radix_to_direct_phases: Vec<String>,
    /// 统计口径目录。
    pub statistics: Vec<String>,
    /// 上游需求视图。
    pub demand: RoutingDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl RoutingRuntimeContract {
    /// 由需求视图与路由模式构建契约；direct 模式仍建立完整契约状态。
    pub(crate) fn build(
        demand: RoutingDemand,
        policy: RoutingPolicyV1,
    ) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: ROUTING_SCHEMA,
            profile: ROUTING_PROFILE_NAME.to_owned(),
            profile_revision: ROUTING_PROFILE_REVISION,
            mode: policy.mode,
            bucket_log2: RADIX_BUCKET_LOG2,
            bucket_count: RADIX_BUCKETS,
            max_levels: MAX_RADIX_LEVELS,
            hop_limit: RADIX_HOP_LIMIT,
            routed_families: ROUTED_FAMILIES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            maintenance_phases: ROUTING_MAINTENANCE_PHASES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            direct_to_radix_phases: DIRECT_TO_RADIX_PHASES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            radix_to_direct_phases: RADIX_TO_DIRECT_PHASES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            statistics: ROUTING_STATISTICS
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

    /// 返回路由模式。
    pub const fn mode(&self) -> RouteMode {
        self.mode
    }

    /// 返回 radix bucket 位数。
    pub const fn bucket_log2(&self) -> u32 {
        self.bucket_log2
    }

    /// 返回 radix bucket 数量。
    pub const fn bucket_count(&self) -> u32 {
        self.bucket_count
    }

    /// 返回路由目录最大层数。
    pub const fn max_levels(&self) -> u32 {
        self.max_levels
    }

    /// 返回转发 hop 上限。
    pub const fn hop_limit(&self) -> u32 {
        self.hop_limit
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> RoutingDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 校验契约：模式、bucket 参数、hop 上限、相位序列与需求闭合。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != ROUTING_SCHEMA
            || self.profile != ROUTING_PROFILE_NAME
            || self.profile_revision != ROUTING_PROFILE_REVISION
        {
            return Err(RawModelError::new("routing profile 参数与登记值不一致"));
        }
        if !ROUTE_MODES.contains(&self.mode.name()) {
            return Err(RawModelError::new("routing 模式不在登记目录中"));
        }
        if self.bucket_log2 != RADIX_BUCKET_LOG2 || self.bucket_count != RADIX_BUCKETS {
            return Err(RawModelError::new(
                "radix bucket 参数与登记值不一致；bucket 数必须是固定 2^k",
            ));
        }
        if self.max_levels != MAX_RADIX_LEVELS || self.max_levels == 0 {
            return Err(RawModelError::new("路由目录层数与登记值不一致"));
        }
        if self.hop_limit != RADIX_HOP_LIMIT || self.hop_limit < self.max_levels {
            return Err(RawModelError::new(
                "转发 hop 上限与登记值不一致，且不得小于目录层数",
            ));
        }
        expect_directory(&self.routed_families, &ROUTED_FAMILIES, "radix 拦截消息族")?;
        expect_directory(
            &self.maintenance_phases,
            &ROUTING_MAINTENANCE_PHASES,
            "维护相位",
        )?;
        expect_sequence(
            &self.direct_to_radix_phases,
            &DIRECT_TO_RADIX_PHASES,
            &self.maintenance_phases,
            "direct → radix 切换序列",
        )?;
        expect_sequence(
            &self.radix_to_direct_phases,
            &RADIX_TO_DIRECT_PHASES,
            &self.maintenance_phases,
            "radix → direct 切换序列",
        )?;
        expect_directory(&self.statistics, &ROUTING_STATISTICS, "路由统计")?;
        self.demand.verify()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("routing 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("routing 契约可序列化")
    }

    /// 计算契约指纹。
    ///
    /// `canonical_bytes` 是整段 serde_json（含 `fingerprint` 字段），因此这里对指纹字段
    /// 清零的副本求 hash：否则「先写指纹、再校验相等」永远不可能成立。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.fingerprint = [0; 32];
        *blake3::Hasher::new_derive_key("gugu-routing-runtime-v1")
            .update(&canonical.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回人类可读的契约 dump。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "routing schema={} profile={} revision={} mode={} buckets={} levels={} hop-limit={}",
            self.schema,
            self.profile,
            self.profile_revision,
            self.mode.name(),
            self.bucket_count,
            self.max_levels,
            self.hop_limit
        );
        let _ = writeln!(out, "routing-families {}", self.routed_families.join(","));
        let _ = writeln!(out, "routing-phases {}", self.maintenance_phases.join(","));
        let _ = writeln!(
            out,
            "routing-switch direct-to-radix={}",
            self.direct_to_radix_phases.join(",")
        );
        let _ = writeln!(
            out,
            "routing-switch radix-to-direct={}",
            self.radix_to_direct_phases.join(",")
        );
        let _ = writeln!(out, "routing-statistics {}", self.statistics.join(","));
        let _ = writeln!(
            out,
            "routing-demand owners={} return-publish-sites={}",
            self.demand.owners, self.demand.return_publish_sites
        );
        let _ = writeln!(out, "routing-fingerprint {}", hex_lower(self.fingerprint));
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

/// 校验切换序列与登记序列一致，且每个相位都在维护相位目录中。
fn expect_sequence(
    actual: &[String],
    expected: &[&str],
    directory: &[String],
    label: &str,
) -> Result<(), RawModelError> {
    expect_directory(actual, expected, label)?;
    for phase in actual {
        if !directory.iter().any(|known| known == phase) {
            return Err(RawModelError::new(format!(
                "{label}引用了不在维护相位目录中的相位：{phase}"
            )));
        }
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
