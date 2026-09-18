//! `EdgeDelta` 消息与候选回收的契约段。
//!
//! 归属边界：
//!
//! 1. **字段目录与 fingerprint 在这里**：`EdgeDelta` 的字段集合逐项与规范列表比较（名字、
//!    种类与顺序），内容 fingerprint 随契约进入 ImagePlan，编译期消费者按它判断边消息格式
//!    是否变化。
//! 2. **需求从既有需求派生**：边站点数取自 barrier 的 `edge_summary_sites`，预留槽取自
//!    barrier 的 `shade_slots`（每条写入最多两项，与 shade 上界同源），并与
//!    `MarkDemand.edge_delta_sites` 交叉校验，不新写一套站点统计。
//! 3. **预算不新增平行字段**：候选推进的 quantum 是 profile 参数，逐轮实际额度仍由 mark
//!    credit 授权给出，因此这里不声明第二份“每轮预算”。

use serde::{Deserialize, Serialize};

use super::barrier_schema::{BarrierDemand, MessageFamilyTag};
use super::local_heap_schema::HEAP_BLOCK_STATE_NAMES;
use super::mark_schema::{MarkDemand, MarkRuntimeContract};
use super::model::{MessageSchemaV1, RawModelError};

/// edge 契约段的 schema 版本。
pub(crate) const EDGE_SCHEMA: u32 = 1;

/// 内建 edge profile 名。
pub(crate) const EDGE_PROFILE_NAME: &str = "mosaic-edge";
/// edge profile 的 revision；任何派生值、相位目录或消息目录变化都必须递增。
pub(crate) const EDGE_PROFILE_REVISION: u32 = 2;

/// 候选推进的默认 quantum：每次 `advance_block_candidates` 至多消费的工作量单位。
pub(crate) const EDGE_CANDIDATE_QUANTUM: u32 = 4096;
/// 每个 processor 的 edge scratch 项数；与 barrier 契约的同一值保持同源。
pub(crate) const EDGE_BUFFER_ENTRIES: u32 = 512;
/// 一条 hybrid barrier 写入最多产生的边变更数。
pub(crate) const EDGE_DELTAS_PER_WRITE: u32 = 2;
/// 精确追踪执行器的 revision；Bitmap/SWITCH 修正与可恢复游标变化时递增。
pub(crate) const EDGE_TRACE_EXECUTOR_REVISION: u32 = 2;
/// 候选决议结构的 schema 版本。
pub(crate) const EDGE_CANDIDATE_SCHEMA: u32 = 1;

/// 候选 job 的固定相位名；顺序即状态机推进顺序。
pub(crate) const EDGE_PHASES: [&str; 10] = [
    "discover",
    "trace",
    "trial",
    "scc",
    "validate",
    "commit",
    "sweep",
    "release",
    "complete",
    "invalidate",
];

/// 未绑定 job 的 block 在 `job_of_block` 中的取值。
pub(crate) const EDGE_NO_JOB: u32 = u32::MAX;

/// 候选游标的工作预算单位名。
pub(crate) const EDGE_WORK_UNIT: &str = "edge-work-unit";

/// 候选回收的需求视图。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeDemand {
    /// 可能触发 edge summary 的 managed store 站点数。
    pub edge_sites: u32,
    /// barrier permit 覆盖的 shade 额度总和；它同时是 edge scratch 的预留证明。
    pub reserve_slots: u32,
}

impl EdgeDemand {
    /// 从 barrier 需求派生，并与 mark 需求交叉校验。
    pub(crate) fn derive(
        barrier: &BarrierDemand,
        mark: &MarkDemand,
    ) -> Result<Self, RawModelError> {
        if mark.edge_delta_sites != barrier.edge_summary_sites {
            return Err(RawModelError::new("mark 与 barrier 的 edge 站点计数不一致"));
        }
        Ok(Self {
            edge_sites: barrier.edge_summary_sites,
            // 每次写入最多两项边变更，与 barrier 的 shade 上界同源：不需要第二份额度字段。
            reserve_slots: barrier.shade_slots,
        })
    }
}

/// 返回 `EdgeDelta` 的规范字段布局：`(字段名, 字段种类)`，直接取自消息 schema 的登记值。
///
/// 契约已经把字段集合注册成规范列表（名字稳定排序、不含地址、覆盖全部身份字段），因此这里只做
/// 投影，不另写一份字段表。
pub(crate) fn edge_delta_layout() -> Vec<(String, &'static str)> {
    super::model::MessageSchemaV1::edge_delta()
        .fields
        .iter()
        .map(|field| (field.name.clone(), field.kind.name()))
        .collect()
}

/// `EdgeDelta` 消息与候选回收的契约段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeRuntimeContract {
    /// 契约段 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub revision: u32,
    /// 候选推进的默认 quantum。
    pub candidate_quantum: u32,
    /// 每个 processor 的 edge scratch 项数。
    pub edge_buffer_entries: u32,
    /// 一条写入最多产生的边变更数。
    pub deltas_per_write: u32,
    /// 精确追踪执行器的 revision。
    pub trace_executor_revision: u32,
    /// 候选决议结构的 schema 版本。
    pub candidate_schema: u32,
    /// candidate job 的相位目录。
    pub phases: Vec<String>,
    /// block 候选状态目录；与 `HeapBlockRecord.state` 的判别值同源。
    pub states: Vec<String>,
    /// `EdgeDelta` 的规范字段集合；对外只暴露字段数，不暴露内部 schema 类型。
    pub(crate) edge_delta_fields: MessageSchemaV1,
    /// 契约内容 fingerprint。
    pub fingerprint: [u8; 32],
    demand: EdgeDemand,
}

impl EdgeRuntimeContract {
    /// 按派生需求构建契约并校验全部字段。
    pub(crate) fn build(
        demand: EdgeDemand,
        barrier: &super::barrier_schema::BarrierRuntimeContract,
        mark: &MarkRuntimeContract,
    ) -> Result<Self, RawModelError> {
        let edge_delta_fields = MessageSchemaV1::edge_delta();
        let mut contract = Self {
            schema: EDGE_SCHEMA,
            profile: EDGE_PROFILE_NAME.to_owned(),
            revision: EDGE_PROFILE_REVISION,
            candidate_quantum: EDGE_CANDIDATE_QUANTUM,
            edge_buffer_entries: EDGE_BUFFER_ENTRIES,
            deltas_per_write: EDGE_DELTAS_PER_WRITE,
            trace_executor_revision: EDGE_TRACE_EXECUTOR_REVISION,
            candidate_schema: EDGE_CANDIDATE_SCHEMA,
            phases: EDGE_PHASES.iter().map(|name| (*name).to_owned()).collect(),
            states: HEAP_BLOCK_STATE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            edge_delta_fields,
            fingerprint: [0_u8; 32],
            demand,
        };
        contract.fingerprint = contract.derive_fingerprint();
        contract.verify(barrier, mark)?;
        Ok(contract)
    }

    /// 返回需求视图。
    pub(crate) const fn demand(&self) -> &EdgeDemand {
        &self.demand
    }

    /// 返回 `EdgeDelta` 的规范字段数；报告与镜像计划只暴露计数，不暴露内部 schema 类型。
    pub fn edge_delta_field_count(&self) -> usize {
        self.edge_delta_fields.fields.len()
    }

    /// 返回契约段 schema 版本。
    pub(crate) const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 `job_of_block` 的保留取值。
    pub(crate) const fn no_job(&self) -> u32 {
        EDGE_NO_JOB
    }

    /// 返回候选推进的默认 quantum。
    pub(crate) const fn candidate_quantum(&self) -> u32 {
        self.candidate_quantum
    }

    /// 返回精确追踪执行器的 revision。
    pub(crate) const fn trace_executor_revision(&self) -> u32 {
        self.trace_executor_revision
    }

    /// 返回候选决议结构的 schema 版本。
    pub(crate) const fn candidate_schema(&self) -> u32 {
        self.candidate_schema
    }

    /// 校验派生值、目录与消息字段集合。
    ///
    /// 字段比较是**逐项**的：名字、种类与顺序都必须与规范列表一致，只检查“存在某种字段”
    /// 会让错误车道静默通过。
    pub(crate) fn verify(
        &self,
        barrier: &super::barrier_schema::BarrierRuntimeContract,
        mark: &MarkRuntimeContract,
    ) -> Result<(), RawModelError> {
        if self.schema != EDGE_SCHEMA
            || self.profile != EDGE_PROFILE_NAME
            || self.revision != EDGE_PROFILE_REVISION
        {
            return Err(RawModelError::new("edge profile 与登记值不一致"));
        }
        if self.candidate_quantum != EDGE_CANDIDATE_QUANTUM
            || self.edge_buffer_entries != EDGE_BUFFER_ENTRIES
            || self.deltas_per_write != EDGE_DELTAS_PER_WRITE
            || self.trace_executor_revision != EDGE_TRACE_EXECUTOR_REVISION
            || self.candidate_schema != EDGE_CANDIDATE_SCHEMA
        {
            return Err(RawModelError::new("edge 契约参数与登记值不一致"));
        }
        // 边 scratch 的预留与 shade 上界同源：两者必须逐值相等，否则 region 内的容量证明不成立。
        if self.deltas_per_write != barrier.shade_slots_per_write
            || self.edge_buffer_entries != barrier.edge_buffer_entries
        {
            return Err(RawModelError::new(
                "edge 额度必须与 barrier 的 shade 上界和 scratch 容量同源",
            ));
        }
        if self.phases != EDGE_PHASES.map(str::to_owned).to_vec() {
            return Err(RawModelError::new("candidate 相位目录与登记值不一致"));
        }
        if self.states != HEAP_BLOCK_STATE_NAMES.map(str::to_owned).to_vec() {
            return Err(RawModelError::new("block 候选状态目录与登记值不一致"));
        }
        if self.demand.edge_sites != barrier.demand.edge_summary_sites
            || self.demand.reserve_slots != barrier.demand.shade_slots
        {
            return Err(RawModelError::new("edge 需求与 barrier 需求不一致"));
        }
        if self.demand.edge_sites != mark.demand.edge_delta_sites {
            return Err(RawModelError::new("edge 站点数与 mark 需求不一致"));
        }
        // 预算是按 region 预留的：permit 覆盖的 shade 额度同时是该 region 的 edge scratch
        // 预留，因此只能要求它按“每条写入两项”整除，不能要求它覆盖未进入 region 的站点。
        if self.demand.reserve_slots != 0 && self.demand.reserve_slots % self.deltas_per_write != 0
        {
            return Err(RawModelError::new(
                "edge 预留槽必须是每条写入边变更数的整数倍",
            ));
        }
        if self.edge_buffer_entries < self.deltas_per_write {
            return Err(RawModelError::new("edge scratch 容量小于单次写入的边上界"));
        }
        self.edge_delta_fields
            .verify_family(MessageFamilyTag::EdgeDelta)?;
        let expected = MessageSchemaV1::edge_delta();
        if self.edge_delta_fields.fields.len() != expected.fields.len()
            || self
                .edge_delta_fields
                .fields
                .iter()
                .zip(&expected.fields)
                .any(|(actual, want)| actual.name != want.name || actual.kind != want.kind)
        {
            return Err(RawModelError::new("EdgeDelta 字段目录与规范列表逐项不一致"));
        }
        if self.fingerprint != self.derive_fingerprint() {
            return Err(RawModelError::new("edge 契约 fingerprint 与内容不一致"));
        }
        Ok(())
    }

    /// 返回契约内容的规范字节序列；fingerprint 就是它的摘要。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(self.profile.as_bytes());
        bytes.extend_from_slice(&self.revision.to_le_bytes());
        bytes.extend_from_slice(&self.candidate_quantum.to_le_bytes());
        bytes.extend_from_slice(&self.edge_buffer_entries.to_le_bytes());
        bytes.extend_from_slice(&self.deltas_per_write.to_le_bytes());
        bytes.extend_from_slice(&self.trace_executor_revision.to_le_bytes());
        bytes.extend_from_slice(&self.candidate_schema.to_le_bytes());
        bytes.extend_from_slice(&self.demand.edge_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.reserve_slots.to_le_bytes());
        for name in &self.phases {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        for name in &self.states {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        for field in &self.edge_delta_fields.fields {
            bytes.extend_from_slice(field.name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(field.kind.name().as_bytes());
            bytes.push(0);
        }
        bytes
    }

    /// 返回内容 fingerprint 的十六进制表示。
    pub(crate) fn fingerprint_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// 渲染契约与需求，进入 runtime dump 与镜像计划报告。
    pub(crate) fn dump_into(&self, output: &mut String) {
        use std::fmt::Write;
        writeln!(
            output,
            "edge schema={} profile={} revision={} quantum={} edge-buffer={} deltas-per-write={} trace-executor={} candidate-schema={} edge-delta-fields={}",
            self.schema,
            self.profile,
            self.revision,
            self.candidate_quantum,
            self.edge_buffer_entries,
            self.deltas_per_write,
            self.trace_executor_revision,
            self.candidate_schema,
            self.edge_delta_fields.fields.len(),
        )
        .expect("String写入");
        writeln!(
            output,
            "edge-demand edge-sites={} reserve-slots={}",
            self.demand.edge_sites, self.demand.reserve_slots,
        )
        .expect("String写入");
        writeln!(output, "edge-phases {}", self.phases.join(",")).expect("String写入");
        writeln!(output, "edge-states {}", self.states.join(",")).expect("String写入");
        writeln!(output, "edge-fingerprint {}", self.fingerprint_hex()).expect("String写入");
    }

    fn derive_fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-edge-runtime-v1");
        hasher.update(&self.canonical_bytes());
        *hasher.finalize().as_bytes()
    }
}
