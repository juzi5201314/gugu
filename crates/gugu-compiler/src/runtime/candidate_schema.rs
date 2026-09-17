//! 候选回收的决议结构与相位目录。
//!
//! 归属边界：
//!
//! 1. **相位目录只有一份**：名字与顺序直接取自 edge 契约的 `EDGE_PHASES`，本模块只给出稠密
//!    下标，避免出现第二套状态机目录；`phase_names_match_contract` 测试钉住两者逐项相等。
//! 2. **决议是私有的**：`CandidateVerdict` 只描述「哪一组 block 已经判定死亡以及依据」，
//!    它不携带任何释放权限；只有持有该 block sweep lease 的唯一消费者能按它执行释放。
//! 3. **快照只含堆侧事实**：`CandidateSnapshot` 的每个字段都由 `LocalHeap` 的既有 header、
//!    object-start 位图与 `HeapBlockRecord` 直接读出，本模块不新增计数来源。

use super::edge_schema::{EDGE_CANDIDATE_SCHEMA, EDGE_PHASES, EDGE_WORK_UNIT};
use super::local_heap::ManagedBlockId;

/// 候选推进的工作单位名；与 edge 契约同源，不新写第二份。
pub(crate) const CANDIDATE_WORK_UNIT: &str = EDGE_WORK_UNIT;

/// 候选进度游标对外的字段布局：`(字段, 位宽)`。
///
/// dump 与验收核对按这张表逐字段渲染，因此「游标有哪些字段、各占多少位」只有一份登记值，
/// 不需要在诊断代码里另写一遍。
pub(crate) const CANDIDATE_CURSOR_LAYOUT: [(&str, u32); 5] = [
    ("job", 32),
    ("phase", 8),
    ("blocks", 32),
    ("pending_edges", 32),
    ("work_units", 64),
];

/// 候选决议结构的 schema 版本；必须与 edge 契约登记的 `candidate_schema` 相等。
pub(crate) const CANDIDATE_SCHEMA: u32 = EDGE_CANDIDATE_SCHEMA;

/// 候选 job 的固定相位；顺序即状态机推进顺序，与契约目录逐项同源。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) enum CandidatePhase {
    /// 从 dirty 集合与零入边 block 出发，按无规模截断的弱连通扩张组成 job。
    Discover = 0,
    /// 精确追踪组内对象，得到组内 block 对计数与指向组外的出边。
    Trace = 1,
    /// block 对试验删除：减去组内入边后仍为零才能继续判定。
    Trial = 2,
    /// 可暂停迭代 Tarjan：得到组内 SCC 与「无出组边」的闭合判定。
    Scc = 3,
    /// 私有验证：lease、pin、resource、mutation 版本与当前 epoch 标记全部复核。
    Validate = 4,
    /// 整组线性化提交：一次发出该组的 sweep 决议。
    Commit = 5,
    /// 唯一消费者执行恰好一次 sweep。
    Sweep = 6,
    /// 提交后按恰好一次释放 block 回 owner free structure。
    Release = 7,
    /// job 收尾：解绑 block、清 dirty 位、计入统计。
    Complete = 8,
    /// 受预算约束的局部失效：退回 active 并等待下一次真实改动重新进入候选。
    Invalidate = 9,
}

impl CandidatePhase {
    /// 全部相位，顺序与契约目录一致。
    pub(crate) const ALL: [Self; 10] = [
        Self::Discover,
        Self::Trace,
        Self::Trial,
        Self::Scc,
        Self::Validate,
        Self::Commit,
        Self::Sweep,
        Self::Release,
        Self::Complete,
        Self::Invalidate,
    ];

    /// 返回契约中的相位名。
    pub(crate) const fn name(self) -> &'static str {
        EDGE_PHASES[self as usize]
    }

    /// 按下标取相位。
    pub(crate) const fn from_index(index: usize) -> Option<Self> {
        if index < Self::ALL.len() {
            Some(Self::ALL[index])
        } else {
            None
        }
    }
}

/// 一个 block 在候选判定中使用的快照；全部字段来自堆侧既有状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CandidateSnapshot {
    pub block: ManagedBlockId,
    pub generation: u32,
    /// `EdgePlane` 的已应用入边计数之和（target 侧真实值）。
    pub incoming: i64,
    /// block 记录的 `incoming_leases`。
    pub incoming_leases: u64,
    pub allocator_leases: u32,
    pub scanner_leases: u32,
    pub evacuation_leases: u32,
    /// `mutation_version`；与 job 建组时的值比较，判断快照是否失效。
    pub mutation_version: u64,
    /// block 内带 `PINNED` 位的对象数。
    pub pinned: u32,
    /// block 内带 resource 实例位的对象数。
    pub resources: u32,
    /// 本 block 内处于当前 mark epoch 的对象数。
    pub marked: u32,
}

impl CandidateSnapshot {
    /// 返回所有 lease 是否归零。
    pub(crate) const fn leases_idle(&self) -> bool {
        self.incoming_leases == 0
            && self.allocator_leases == 0
            && self.scanner_leases == 0
            && self.evacuation_leases == 0
    }

    /// 返回该 block 是否可以进入候选组。
    ///
    /// 资格只看 lease：`incoming_leases` 是所有外部引用都已在途归零的证明；pin 与 resource
    /// 不阻止建组，它们在验证相位作为 gate 让整组失效，因此这两个事实不会被静默丢掉。
    pub(crate) const fn allows_candidate(&self) -> bool {
        self.leases_idle()
    }
}

/// 判定一个 block 组仍然存活的原因；进入诊断与 dump。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidateAliveReason {
    /// 组内 block 仍有来自组外的已应用入边。
    ExternalIncoming,
    /// 组内仍有本 epoch 标记过的对象，说明有组外根可达它。
    Marked,
    /// 组内还有 pin 对象。
    Pinned,
    /// 组内还有含 resource 实例的对象。
    Resource,
    /// lease 未归零，候选不能推进。
    LeaseBusy,
    /// 快照之后 block 又被改动，判定必须从发现的重新开始。
    Mutated,
    /// 组内 SCC 仍有指向组外的出边，无法闭合。
    OpenComponent,
}

/// 渲染一条候选进度记录：字段与位宽都来自上面的布局登记值。
///
/// dump 只对真实存在的 cursor 调用，因此每一行都对应一个活的 job。
pub(crate) fn render_cursor_row(progress: &super::candidate::CandidateProgress) -> String {
    let values = [
        ("job", u64::from(progress.job)),
        ("phase", progress.phase as u64),
        ("blocks", u64::from(progress.blocks)),
        ("pending_edges", u64::from(progress.pending_edges)),
        ("work_units", progress.work_units),
    ];
    let mut rendered = String::new();
    for ((name, width), (value_name, value)) in CANDIDATE_CURSOR_LAYOUT.iter().zip(values) {
        debug_assert_eq!(*name, value_name, "游标布局与渲染取值必须同名同序");
        rendered.push_str(&format!("{name}={value}/{width}bit "));
    }
    rendered.push_str(&format!("unit={CANDIDATE_WORK_UNIT}"));
    rendered
}

/// 候选决议：私有结构，只有持有 sweep lease 的唯一消费者能按它执行释放。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CandidateVerdict {
    /// 整组判定死亡；`blocks` 按稳定顺序给出身份与世代。
    Dead(Vec<(ManagedBlockId, u32)>),
    /// 整组判定存活，附带第一个命中的原因与证据 block。
    Alive {
        reason: CandidateAliveReason,
        evidence: ManagedBlockId,
    },
}

impl CandidateVerdict {
    /// 返回决议是否判定死亡。
    pub(crate) const fn is_dead(&self) -> bool {
        matches!(self, Self::Dead(_))
    }
}
