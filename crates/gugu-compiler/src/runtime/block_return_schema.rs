//! owner-directed managed block return 的契约段。
//!
//! 本段把「四类 return unit 目录」「lease/grace 门禁目录」「Immix 尺寸与 grace 步数」
//! 「由优化后 LIR 推导的编译期上界」固定成带版本的对象，与 `local_heap_schema` /
//! `shared_heap_schema` 共用 `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump`
//! 闭环。
//!
//! 需求字段是每个分配点至多产生一次对应 unit 的编译期上界，不是运行时计数。

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use super::local_heap_schema::LocalHeapDemand;
use super::model::{GRACE_STEPS, RawModelError};
use super::shared_heap_schema::SharedHeapDemand;

/// block return 契约段 schema。
pub(crate) const BLOCK_RETURN_SCHEMA: u32 = 1;

/// 内建 block return profile 名。
pub(crate) const BLOCK_RETURN_PROFILE_NAME: &str = "mosaic-block-return";
/// profile revision；unit 目录、gate 目录或尺寸变化都必须递增。
pub(crate) const BLOCK_RETURN_PROFILE_REVISION: u32 = 1;

/// 连续空 line 短于该阈值时不发 `HeapLineRun`，由 bump 在 block 内复用。
pub(crate) const HEAP_LINE_RUN_MIN_LINES: u32 = 8;

/// 四类 return unit；顺序即登记顺序。
pub(crate) const BLOCK_RETURN_UNITS: [&str; 4] =
    ["heap-block", "heap-line-run", "heap-arena", "large-mapping"];

/// 发布前必须全部归零的门禁；顺序即登记顺序。
pub(crate) const BLOCK_RETURN_GATES: [&str; 8] = [
    "allocator-lease",
    "scanner-lease",
    "evacuation-lease",
    "incoming-edge",
    "pin",
    "resource",
    "handle-access",
    "queue-page-grace",
];

/// 从优化后 LIR 与冻结类型表推导的 block return 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BlockReturnDemand {
    /// LocalHeap 分配点上界；每个站点至多产生一次 `HeapBlock`。
    pub block_sites: u32,
    /// LocalHeap 分配点上界；每个站点至多产生一次 `HeapLineRun`。
    pub line_run_sites: u32,
    /// LocalHeap 分配点上界；每个站点至多产生一次 `HeapArena`。
    pub arena_sites: u32,
    /// 超过单个 Immix block 的类型数；每个 large 类型至多产生一次 `LargeMapping`。
    pub large_sites: u32,
    /// SharedHeap 分配点上界；每个站点至多产生一次共享 `HeapBlock`。
    pub shared_block_sites: u32,
    /// Resource placement 站点数；每个站点至多产生一次承载 resource 的 `HeapBlock`。
    pub resource_block_sites: u32,
    /// 单个 unit 的最大物理字节；与 arena 尺寸同源。
    pub max_unit_bytes: u64,
}

impl BlockReturnDemand {
    /// 由 LocalHeap / SharedHeap 需求推导编译期上界。
    pub(crate) fn derive(
        local: &LocalHeapDemand,
        shared: &SharedHeapDemand,
    ) -> Result<Self, RawModelError> {
        let demand = Self {
            block_sites: local.alloc_sites,
            line_run_sites: local.alloc_sites,
            arena_sites: local.alloc_sites,
            large_sites: local.large_types,
            shared_block_sites: shared.alloc_sites,
            resource_block_sites: local.resource_sites,
            max_unit_bytes: GC_ARENA_BYTES,
        };
        demand.verify()?;
        Ok(demand)
    }

    /// 返回需求视图的稳定指纹。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(32);
        bytes.extend_from_slice(&self.block_sites.to_le_bytes());
        bytes.extend_from_slice(&self.line_run_sites.to_le_bytes());
        bytes.extend_from_slice(&self.arena_sites.to_le_bytes());
        bytes.extend_from_slice(&self.large_sites.to_le_bytes());
        bytes.extend_from_slice(&self.shared_block_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_block_sites.to_le_bytes());
        bytes.extend_from_slice(&self.max_unit_bytes.to_le_bytes());
        crate::frontend::mono::keys::hash_domain("gugu-block-return-demand-v1", &bytes)
    }

    /// 校验需求自身的可证明关系。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.line_run_sites != self.block_sites || self.arena_sites != self.block_sites {
            return Err(RawModelError::new("block/line-run/arena 站点上界必须同源"));
        }
        if self.resource_block_sites > self.block_sites {
            return Err(RawModelError::new(
                "resource block 站点不得超过 LocalHeap 分配站点",
            ));
        }
        if self.large_sites > self.block_sites {
            return Err(RawModelError::new(
                "large mapping 站点不得超过 LocalHeap 分配站点",
            ));
        }
        if self.max_unit_bytes != GC_ARENA_BYTES {
            return Err(RawModelError::new(
                "max_unit_bytes 必须与 GC arena 尺寸同源",
            ));
        }
        Ok(())
    }
}

/// 已验证的 block return runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BlockReturnRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// return unit 目录。
    pub units: Vec<String>,
    /// 发布门禁目录。
    pub gates: Vec<String>,
    /// queue-page grace 步数。
    pub grace_steps: u32,
    /// Immix block 字节数。
    pub block_bytes: u32,
    /// Immix arena 字节数。
    pub arena_bytes: u64,
    /// Immix line 字节数。
    pub line_bytes: u32,
    /// 发 `HeapLineRun` 的最短连续空 line 数。
    pub line_run_min_lines: u32,
    /// 上游需求视图。
    pub demand: BlockReturnDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl BlockReturnRuntimeContract {
    /// 由需求视图构建契约；空需求仍建立完整契约状态。
    pub(crate) fn build(demand: BlockReturnDemand) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: BLOCK_RETURN_SCHEMA,
            profile: BLOCK_RETURN_PROFILE_NAME.to_owned(),
            profile_revision: BLOCK_RETURN_PROFILE_REVISION,
            units: BLOCK_RETURN_UNITS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            gates: BLOCK_RETURN_GATES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            grace_steps: GRACE_STEPS,
            block_bytes: GC_BLOCK_BYTES,
            arena_bytes: GC_ARENA_BYTES,
            line_bytes: GC_LINE_BYTES,
            line_run_min_lines: HEAP_LINE_RUN_MIN_LINES,
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回内部 schema 版本。
    pub const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 profile 名。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回 unit 数量。
    pub fn unit_count(&self) -> u32 {
        u32::try_from(self.units.len()).expect("unit 数量适配 u32")
    }

    /// 返回 gate 数量。
    pub fn gate_count(&self) -> u32 {
        u32::try_from(self.gates.len()).expect("gate 数量适配 u32")
    }

    /// 返回 queue-page grace 步数。
    pub const fn grace_steps(&self) -> u32 {
        self.grace_steps
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> BlockReturnDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 校验契约：目录、尺寸、grace 与需求关系。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != BLOCK_RETURN_SCHEMA
            || self.profile != BLOCK_RETURN_PROFILE_NAME
            || self.profile_revision != BLOCK_RETURN_PROFILE_REVISION
        {
            return Err(RawModelError::new(
                "block return profile 参数与登记值不一致",
            ));
        }
        if self.units.len() != BLOCK_RETURN_UNITS.len()
            || self
                .units
                .iter()
                .zip(BLOCK_RETURN_UNITS.iter())
                .any(|(name, expected)| name != expected)
        {
            return Err(RawModelError::new("block return unit 目录与登记表不一致"));
        }
        if self.gates.len() != BLOCK_RETURN_GATES.len()
            || self
                .gates
                .iter()
                .zip(BLOCK_RETURN_GATES.iter())
                .any(|(name, expected)| name != expected)
        {
            return Err(RawModelError::new("block return gate 目录与登记表不一致"));
        }
        if self.grace_steps != GRACE_STEPS {
            return Err(RawModelError::new("block return grace 步数必须为 4"));
        }
        if self.block_bytes != GC_BLOCK_BYTES
            || self.arena_bytes != GC_ARENA_BYTES
            || self.line_bytes != GC_LINE_BYTES
            || self.line_run_min_lines != HEAP_LINE_RUN_MIN_LINES
        {
            return Err(RawModelError::new(
                "block return 尺寸与 GC metadata / line-run 阈值不一致",
            ));
        }
        if self.demand.max_unit_bytes < u64::from(self.block_bytes) {
            return Err(RawModelError::new(
                "max_unit_bytes 不得小于单个 Immix block",
            ));
        }
        self.demand.verify()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("block return 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.profile.len() as u32).to_le_bytes());
        bytes.extend_from_slice(self.profile.as_bytes());
        bytes.extend_from_slice(&self.profile_revision.to_le_bytes());
        for unit in &self.units {
            bytes.extend_from_slice(&(unit.len() as u32).to_le_bytes());
            bytes.extend_from_slice(unit.as_bytes());
        }
        for gate in &self.gates {
            bytes.extend_from_slice(&(gate.len() as u32).to_le_bytes());
            bytes.extend_from_slice(gate.as_bytes());
        }
        bytes.extend_from_slice(&self.grace_steps.to_le_bytes());
        bytes.extend_from_slice(&self.block_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.arena_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.line_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.line_run_min_lines.to_le_bytes());
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes
    }

    /// 计算契约指纹。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        crate::frontend::mono::keys::hash_domain(
            "gugu-block-return-contract-v1",
            &self.canonical_bytes(),
        )
    }

    /// 返回人类可读的契约 dump。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "block-return schema={} profile={} revision={}",
            self.schema, self.profile, self.profile_revision
        );
        let _ = writeln!(out, "block-return-units {}", self.units.join(","));
        let _ = writeln!(out, "block-return-gates {}", self.gates.join(","));
        let _ = writeln!(
            out,
            "block-return-sizes arena={} block={} line={} grace-steps={}",
            self.arena_bytes, self.block_bytes, self.line_bytes, self.grace_steps
        );
        let _ = writeln!(
            out,
            "block-return-demand block={} line-run={} arena={} large={} shared={} resource={} max-unit-bytes={}",
            self.demand.block_sites,
            self.demand.line_run_sites,
            self.demand.arena_sites,
            self.demand.large_sites,
            self.demand.shared_block_sites,
            self.demand.resource_block_sites,
            self.demand.max_unit_bytes
        );
        let _ = writeln!(
            out,
            "block-return-fingerprint {}",
            hex_lower(self.fingerprint)
        );
        out
    }
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
