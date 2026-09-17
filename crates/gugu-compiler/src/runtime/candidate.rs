//! 候选回收平面：dirty → 无规模截断的弱连通组 → block 对试验删除 → 组内 SCC → 私有验证 →
//! 整组提交 → 恰好一次 sweep/release → 收尾；任一 gate 失败则受预算约束地局部失效。
//!
//! 归属边界：
//!
//! 1. **平面不读堆也不读边平面**：事实以 `CandidateSnapshot` 与 `BlockPairCount` 输入，效果以
//!    `CandidateAction` 输出，因此判定可逐单位复现，也不会绕过堆的 lease 与状态迁移。
//! 2. **组规模不受上限约束**：扩张按 `edge_cursor` 分批推进，只受工作预算约束；不按大小截断、
//!    不抽样，预算耗尽时保留游标，下一批从同一点继续。
//! 3. **相位顺序固定**：`Discover → Trace → Trial → Scc → Validate → Commit → Sweep → Release
//!    → Complete`，发现/追踪/试验/SCC/验证任一相位失败都转入 `Invalidate` 结束该 job；
//!    `Commit` 是唯一线性化点，之后只能继续 sweep/release，不能退回。
//! 4. **确认恰好一次**：`note_swept`/`note_released` 对同一 block 只接受一次，重复或世代不符
//!    都是不变量失败；决议 `CandidateVerdict` 只在报告里出现，释放权限不在平面内。

use std::collections::{BTreeMap, BTreeSet};

use super::candidate_schema::{
    CandidateAliveReason, CandidatePhase, CandidateSnapshot, CandidateVerdict,
};
use super::local_heap::ManagedBlockId;
use super::slab::RawInvariant;

/// 一条 block 对计数：已应用入边或精确追踪得到的组内引用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockPairCount {
    pub(crate) source: ManagedBlockId,
    pub(crate) target: ManagedBlockId,
    pub(crate) count: u32,
}

/// 候选平面发出的动作；只有世界层能执行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CandidateAction {
    /// 把一个 block 绑定到 job；堆侧据此写入 `candidate_job`。
    BindBlock { block: ManagedBlockId, job: u32 },
    /// 解绑；块记录里的 job 回到未绑定值。
    UnbindBlock { block: ManagedBlockId, job: u32 },
    /// 整组提交：一次给出该组全部死亡的 block 身份与世代，作为唯一线性化点。
    CommitGroup { blocks: Vec<(ManagedBlockId, u32)> },
    /// 组内 block 指向组外的引用必须随组死亡一起减掉。
    DropOutgoing {
        source: ManagedBlockId,
        target: ManagedBlockId,
        count: u32,
    },
    /// 恰好一次释放：把 block 交回 owner free structure。
    ReleaseBlock {
        block: ManagedBlockId,
        generation: u32,
    },
    /// 局部失效：组内 block 退回 active，等下一次真实改动再进入候选。
    InvalidateGroup { blocks: Vec<ManagedBlockId> },
}

/// 一个 job 的进度视图；调用方按它报告真实进度，而不是“已尝试推进几步”。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CandidateProgress {
    pub(crate) job: u32,
    pub(crate) phase: CandidatePhase,
    /// 组内 block 数。
    pub(crate) blocks: u32,
    /// 弱连通扩张仍待处理的边记录数。
    pub(crate) pending_edges: u32,
    /// 该 job 累计消费的工作单位。
    pub(crate) work_units: u64,
}

/// 候选平面的累计统计。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CandidateStats {
    pub(crate) jobs_started: u64,
    pub(crate) jobs_completed: u64,
    pub(crate) jobs_invalidated: u64,
    pub(crate) blocks_bound: u64,
    pub(crate) blocks_swept: u64,
    pub(crate) blocks_released: u64,
    pub(crate) dead_groups: u64,
    pub(crate) alive_groups: u64,
    pub(crate) outgoing_dropped: u64,
    pub(crate) work_units: u64,
}

/// 一次推进的输入。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CandidateInputs<'a> {
    /// 本批可用的 block 快照；已绑定 block 的快照必须齐全。
    pub(crate) snapshots: &'a [CandidateSnapshot],
    /// block 对计数；按 `edge_cursor` 逐条推进。
    pub(crate) edges: &'a [BlockPairCount],
}

/// 一次推进的结果。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CandidateReport {
    pub(crate) actions: Vec<CandidateAction>,
    pub(crate) progress: Vec<CandidateProgress>,
    /// 本批产生的私有决议：`(job, 决议)`。
    pub(crate) verdicts: Vec<(u32, CandidateVerdict)>,
    /// 本批真实消费的工作单位。
    pub(crate) work_units: u32,
}

/// 一个候选 job；成员按弱连通扩张形成，相位按契约目录推进。
#[derive(Clone, Debug, Eq, PartialEq)]
struct CandidateJob {
    id: u32,
    phase: CandidatePhase,
    /// 组内 block；有序，保证遍历与动作顺序稳定。
    blocks: BTreeSet<ManagedBlockId>,
    /// 建组时的快照，作为版本与世代的核对基准。
    snapshot: BTreeMap<ManagedBlockId, CandidateSnapshot>,
    /// 组内无向邻接：弱连通扩张与“是否有出组边”的判据。
    weak: BTreeMap<ManagedBlockId, BTreeSet<ManagedBlockId>>,
    /// 组内定向邻接：SCC 需要方向。
    directed: BTreeMap<ManagedBlockId, BTreeSet<ManagedBlockId>>,
    /// 组内 block 对计数：试验删除按它减去内部引用。
    internal_counts: BTreeMap<(ManagedBlockId, ManagedBlockId), u32>,
    /// 指向组外的引用计数。
    outgoing: BTreeMap<(ManagedBlockId, ManagedBlockId), u32>,
    /// 试验删除后的剩余入边数；SCC 之后必须仍全部为零。
    trial: BTreeMap<ManagedBlockId, i64>,
    /// 边输入的单次线性游标。
    edge_cursor: usize,
    /// 当前相位内部成员游标；换相位时归零，因此相位可跨批恢复且不会重复处理成员。
    cursor: usize,
    /// 建组期间被 mutator 改过的成员；失效后它们必须重新进入 dirty 集合。
    mutated: BTreeSet<ManagedBlockId>,
    /// 组内 SCC 的 Tarjan 状态；进入 `Scc` 时创建一次。
    tarjan: Option<TarjanState>,
    /// SCC 收缩图上的存活传播状态；Tarjan 完成后创建一次。
    liveness: Option<LivenessState>,
    /// 判定死亡的 block 与世代。
    dead: Vec<(ManagedBlockId, u32)>,
    /// 已确认 sweep / release 的 block；各自恰好一次。
    swept: BTreeSet<ManagedBlockId>,
    released: BTreeSet<ManagedBlockId>,
    work: u64,
}

/// 一次相位推进的结果。
enum PhaseStep {
    /// 本相位仍需继续推进。
    Continue,
    /// 本相位已完成，按固定顺序进入下一相位。
    Done,
    /// 本相位判定该 job 必须转入 `Invalidate` 收尾：退回动作由收尾相位按预算执行。
    Retreat,
}

/// 可暂停迭代 Tarjan：显式调用栈替代递归，节点与 SCC 顺序都按 block 稳定序确定。
#[derive(Clone, Debug, Eq, PartialEq)]
struct TarjanState {
    nodes: Vec<ManagedBlockId>,
    /// 每个节点的组内出边（稠密下标）。
    adjacency: Vec<Vec<usize>>,
    next_edge: Vec<usize>,
    indices: Vec<Option<u64>>,
    lowlink: Vec<u64>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    call_stack: Vec<(usize, usize)>,
    components: Vec<Vec<usize>>,
    root: usize,
    next_index: u64,
}

impl TarjanState {
    fn new(job: &CandidateJob) -> Self {
        let nodes: Vec<ManagedBlockId> = job.blocks.iter().copied().collect();
        let index_of: BTreeMap<ManagedBlockId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(index, block)| (*block, index))
            .collect();
        let adjacency = nodes
            .iter()
            .map(|block| {
                job.directed
                    .get(block)
                    .map(|targets| {
                        targets
                            .iter()
                            .filter_map(|target| index_of.get(target).copied())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect::<Vec<Vec<usize>>>();
        let count = nodes.len();
        Self {
            nodes,
            adjacency,
            next_edge: vec![0; count],
            indices: vec![None; count],
            lowlink: vec![0; count],
            on_stack: vec![false; count],
            stack: Vec::new(),
            call_stack: Vec::new(),
            components: Vec::new(),
            root: 0,
            next_index: 0,
        }
    }

    /// 推进至多 `budget` 个单位；返回是否已处理完全部节点。
    fn advance(&mut self, budget: &mut u32) -> bool {
        while self.root < self.nodes.len() {
            let node = self.root;
            if self.indices[node].is_none() {
                if !charge(budget) {
                    return false;
                }
                self.indices[node] = Some(self.next_index);
                self.lowlink[node] = self.next_index;
                self.next_index += 1;
                self.stack.push(node);
                self.on_stack[node] = true;
                self.call_stack.push((node, usize::MAX));
            }
            while let Some(&(current, parent)) = self.call_stack.last() {
                if self.next_edge[current] < self.adjacency[current].len() {
                    if !charge(budget) {
                        return false;
                    }
                    let next = self.adjacency[current][self.next_edge[current]];
                    self.next_edge[current] += 1;
                    if self.indices[next].is_none() {
                        self.indices[next] = Some(self.next_index);
                        self.lowlink[next] = self.next_index;
                        self.next_index += 1;
                        self.stack.push(next);
                        self.on_stack[next] = true;
                        self.call_stack.push((next, current));
                    } else if self.on_stack[next] {
                        let index = self.indices[next].expect("已访问节点必有下标");
                        self.lowlink[current] = self.lowlink[current].min(index);
                    }
                } else {
                    self.call_stack.pop();
                    if parent != usize::MAX {
                        self.lowlink[parent] = self.lowlink[parent].min(self.lowlink[current]);
                    }
                    let index = self.indices[current].expect("已访问节点必有下标");
                    if self.lowlink[current] == index {
                        let mut component = Vec::new();
                        while let Some(member) = self.stack.pop() {
                            self.on_stack[member] = false;
                            component.push(member);
                            if member == current {
                                break;
                            }
                        }
                        component.sort_unstable();
                        self.components.push(component);
                    }
                }
            }
            self.root += 1;
        }
        true
    }

    /// 返回全部 SCC 的 block 集合，按稳定序给出。
    fn components(&self) -> Vec<Vec<ManagedBlockId>> {
        let mut components: Vec<Vec<ManagedBlockId>> = self
            .components
            .iter()
            .map(|component| {
                component
                    .iter()
                    .map(|index| self.nodes[*index])
                    .collect::<Vec<ManagedBlockId>>()
            })
            .collect();
        components.sort();
        components
    }
}

/// 存活传播：SCC 收缩图上的可暂停 DFS。
///
/// 判定依据来自试验删除：含正剩余入边的 SCC 说明组外仍有引用进入，因而是存活源；存活沿
/// 定向边传播，未被传播到的 SCC 才是死亡组。这样同一弱连通组里的不同 SCC 可以得到不同结论，
/// 不需要把整组一起杀死或一起放弃。
#[derive(Clone, Debug, Eq, PartialEq)]
struct LivenessState {
    /// 每个 SCC 的出边（收缩图，稠密下标）。
    out_components: Vec<Vec<usize>>,
    live: Vec<bool>,
    /// 显式 DFS 栈：`(组件, 下一条出边游标)`。
    stack: Vec<(usize, usize)>,
    /// 待开始的存活种子。
    seeds: Vec<usize>,
    seed: usize,
}

impl LivenessState {
    fn new(job: &CandidateJob, components: &[Vec<ManagedBlockId>]) -> Self {
        let component_of: BTreeMap<ManagedBlockId, usize> = components
            .iter()
            .enumerate()
            .flat_map(|(index, component)| component.iter().map(move |block| (*block, index)))
            .collect();
        let mut out_components: Vec<Vec<usize>> = vec![Vec::new(); components.len()];
        for (source, targets) in &job.directed {
            let Some(from) = component_of.get(source).copied() else {
                continue;
            };
            for target in targets {
                let Some(to) = component_of.get(target).copied() else {
                    continue;
                };
                if from != to && !out_components[from].contains(&to) {
                    out_components[from].push(to);
                }
            }
        }
        for targets in &mut out_components {
            targets.sort_unstable();
        }
        let seeds = components
            .iter()
            .enumerate()
            .filter(|(_, component)| {
                component
                    .iter()
                    .any(|block| job.trial.get(block).copied().unwrap_or_default() > 0)
            })
            .map(|(index, _)| index)
            .collect();
        let live = vec![false; components.len()];
        Self {
            out_components,
            live,
            stack: Vec::new(),
            seeds,
            seed: 0,
        }
    }

    /// 推进至多 `budget` 个单位；返回是否已完成传播。
    fn advance(&mut self, budget: &mut u32) -> bool {
        loop {
            if let Some(&(component, cursor)) = self.stack.last() {
                if cursor < self.out_components[component].len() {
                    if !charge(budget) {
                        return false;
                    }
                    let next = self.out_components[component][cursor];
                    let top = self.stack.last_mut().expect("栈非空");
                    top.1 += 1;
                    if !self.live[next] {
                        self.live[next] = true;
                        self.stack.push((next, 0));
                    }
                    continue;
                }
                self.stack.pop();
                continue;
            }
            if self.seed >= self.seeds.len() {
                return true;
            }
            if !charge(budget) {
                return false;
            }
            let component = self.seeds[self.seed];
            self.seed += 1;
            if !self.live[component] {
                self.live[component] = true;
                self.stack.push((component, 0));
            }
        }
    }
}

/// 扣一个工作单位；返回是否仍有预算。
fn charge(budget: &mut u32) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    true
}

/// 候选回收平面。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CandidatePlane {
    jobs: BTreeMap<u32, CandidateJob>,
    job_of_block: BTreeMap<ManagedBlockId, u32>,
    dirty: BTreeSet<ManagedBlockId>,
    next_job: u32,
    stats: CandidateStats,
}

impl CandidatePlane {
    /// 创建一个空平面。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回统计快照。
    pub(crate) const fn stats(&self) -> CandidateStats {
        self.stats
    }

    /// 把全部活跃 job 强制转入失效收尾；返回被取消的 job 数。
    ///
    /// 失败取消必须走与本地失效同一条路径（`Invalidate` → 退回动作 → 解绑），因此它继承同一套
    /// 不变量：成员退回 `active`、块记录清掉 `candidate_job`、被改过的成员回到 dirty 集合。
    pub(crate) fn cancel_all(&mut self) -> u32 {
        let mut cancelled = 0;
        for state in self.jobs.values_mut() {
            if state.phase < CandidatePhase::Commit {
                state.phase = CandidatePhase::Invalidate;
                state.cursor = 0;
                cancelled += 1;
            }
        }
        cancelled
    }

    /// 返回仍待发现的 dirty block 数。
    pub(crate) fn dirty_count(&self) -> u64 {
        self.dirty.len() as u64
    }

    /// 返回绑定到 job 的 block 数。
    pub(crate) fn bound_count(&self) -> u64 {
        self.job_of_block.len() as u64
    }

    /// 返回活跃 job 数。
    pub(crate) fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// 返回一个 block 绑定的 job。
    pub(crate) fn job_of_block(&self, block: ManagedBlockId) -> Option<u32> {
        self.job_of_block.get(&block).copied()
    }

    /// 返回一个 job 当前所处的相位。
    pub(crate) fn phase_of(&self, job: u32) -> Option<CandidatePhase> {
        self.jobs.get(&job).map(|state| state.phase)
    }

    /// 返回候选平面跟踪的全部 block：已绑定成员加待发现的 dirty block。
    pub(crate) fn tracked_blocks(&self) -> Vec<ManagedBlockId> {
        let mut blocks: Vec<ManagedBlockId> = self.job_of_block.keys().copied().collect();
        blocks.extend(self.dirty.iter().copied());
        blocks.sort_unstable();
        blocks
    }

    /// 返回全部活跃 job 的 `(job, 相位)`，按 job 编号有序。
    pub(crate) fn job_phases(&self) -> Vec<(u32, CandidatePhase)> {
        self.jobs
            .iter()
            .map(|(job, state)| (*job, state.phase))
            .collect()
    }

    /// 返回一个 job 的成员数。
    pub(crate) fn job_blocks(&self, job: u32) -> u32 {
        self.jobs
            .get(&job)
            .map(|state| u32::try_from(state.blocks.len()).expect("成员数适配 u32"))
            .unwrap_or(0)
    }

    /// 返回一个 job 在发现相位仍待处理的边记录数。
    pub(crate) fn job_edge_cursor(&self, job: u32) -> u32 {
        self.jobs
            .get(&job)
            .map(|state| u32::try_from(state.edge_cursor).expect("游标适配 u32"))
            .unwrap_or(0)
    }

    /// 返回一个 job 累计消费的工作单位。
    pub(crate) fn job_work(&self, job: u32) -> u64 {
        self.jobs.get(&job).map(|state| state.work).unwrap_or(0)
    }

    /// 记录一个 block 已变化；重复标记只置位。
    ///
    /// 已绑定成员永远不进 dirty 集合：改动只记在 job 上。`Commit` 之前的 job 整体转入
    /// `Invalidate`（它的试验删除与 SCC 都是在旧快照上算出来的），失效收尾时被改过的成员重新回到
    /// dirty 集合等下一次发现；`Commit` 之后的成员已经在死亡组里，不存在合法的写入来源。
    pub(crate) fn note_dirty(&mut self, block: ManagedBlockId) {
        if let Some(job) = self.job_of_block.get(&block).copied()
            && let Some(state) = self.jobs.get_mut(&job)
        {
            state.mutated.insert(block);
            if state.phase < CandidatePhase::Commit {
                state.phase = CandidatePhase::Invalidate;
            }
            return;
        }
        self.dirty.insert(block);
    }

    /// 确认一个 block 已完成恰好一次 sweep。
    pub(crate) fn note_swept(
        &mut self,
        block: ManagedBlockId,
        generation: u32,
    ) -> Result<(), RawInvariant> {
        let job = self.job_in_phase(block, CandidatePhase::Sweep, "sweep")?;
        if job.phase != CandidatePhase::Sweep {
            return Err(RawInvariant::new("sweep 确认发生在 Sweep 相位之外"));
        }
        let expected = dead_generation(job, block, "sweep")?;
        if expected != generation {
            return Err(RawInvariant::new("sweep 确认的世代与决议不一致"));
        }
        if !job.swept.insert(block) {
            return Err(RawInvariant::new("同一 block 被 sweep 两次"));
        }
        self.stats.blocks_swept = self.stats.blocks_swept.saturating_add(1);
        Ok(())
    }

    /// 确认一个 block 已完成恰好一次 release。
    pub(crate) fn note_released(
        &mut self,
        block: ManagedBlockId,
        generation: u32,
    ) -> Result<(), RawInvariant> {
        let job = self.job_in_phase(block, CandidatePhase::Release, "release")?;
        let expected = dead_generation(job, block, "release")?;
        if expected != generation {
            return Err(RawInvariant::new("release 确认的世代与决议不一致"));
        }
        if !job.released.insert(block) {
            return Err(RawInvariant::new("同一 block 被 release 两次"));
        }
        self.stats.blocks_released = self.stats.blocks_released.saturating_add(1);
        Ok(())
    }

    /// 取出一个 block 所属 job，并核对它处于期待相位。
    fn job_in_phase(
        &mut self,
        block: ManagedBlockId,
        phase: CandidatePhase,
        what: &str,
    ) -> Result<&mut CandidateJob, RawInvariant> {
        let job_id =
            self.job_of_block.get(&block).copied().ok_or_else(|| {
                RawInvariant::new(format!("{what} 确认的 block 不属于任何候选 job"))
            })?;
        let job = self
            .jobs
            .get_mut(&job_id)
            .ok_or_else(|| RawInvariant::new(format!("{what} 确认的 job 不存在")))?;
        if job.phase != phase {
            return Err(RawInvariant::new(format!(
                "{what} 确认发生在 {phase:?} 相位之外"
            )));
        }
        Ok(job)
    }

    /// 按 `quantum` 个工作单位推进全部 job；预算耗尽时保留游标，下一批继续。
    pub(crate) fn advance(
        &mut self,
        quantum: u32,
        inputs: &CandidateInputs<'_>,
    ) -> Result<CandidateReport, RawInvariant> {
        let mut budget = quantum;
        let mut report = CandidateReport::default();
        self.open_jobs(inputs, &mut budget, &mut report)?;
        let ids: Vec<u32> = self.jobs.keys().copied().collect();
        for id in ids {
            if budget == 0 {
                break;
            }
            self.advance_job(id, inputs, &mut budget, &mut report)?;
        }
        for (id, job) in &self.jobs {
            report.progress.push(CandidateProgress {
                job: *id,
                phase: job.phase,
                blocks: u32::try_from(job.blocks.len()).expect("组内 block 数适配 u32"),
                pending_edges: u32::try_from(inputs.edges.len().saturating_sub(job.edge_cursor))
                    .expect("待处理边数适配 u32"),
                work_units: job.work,
            });
        }
        // 统计只记账本批真实消费的单位与真实产生的决议，不重复累计 job 的生命周期工作量。
        report.work_units = quantum.saturating_sub(budget);
        // 已绑定成员不再属于 dirty 集合：它的改动经 `note_dirty` 直接让 job 失效，而不是等下一次
        // 发现；否则失效结束后同一个 block 会立刻被当成新候选，形成发现—失效的活锁。
        self.dirty
            .retain(|block| !self.job_of_block.contains_key(block));
        self.stats.work_units = self
            .stats
            .work_units
            .saturating_add(u64::from(report.work_units));
        for (_, verdict) in &report.verdicts {
            if verdict.is_dead() {
                self.stats.dead_groups = self.stats.dead_groups.saturating_add(1);
            } else {
                self.stats.alive_groups = self.stats.alive_groups.saturating_add(1);
            }
        }
        for action in &report.actions {
            if let CandidateAction::DropOutgoing { count, .. } = action {
                self.stats.outgoing_dropped = self
                    .stats
                    .outgoing_dropped
                    .saturating_add(u64::from(*count));
            }
        }
        Ok(report)
    }

    /// 从 dirty 集合开新 job：只有全部 lease 空闲的 block 才能成为候选。
    ///
    /// 每个 block 只能属于一个 job：如果它与某个尚未提交的 job 相邻（按本批边记录），就由那个
    /// job 收养它并重跑发现相位，而不是另起一个 job；这样弱连通闭包只有一份所有者，也不会出现
    /// 两个 job 各自把同一 block 当成自己成员的重复计数。
    fn open_jobs(
        &mut self,
        inputs: &CandidateInputs<'_>,
        budget: &mut u32,
        report: &mut CandidateReport,
    ) -> Result<(), RawInvariant> {
        let seeds: Vec<ManagedBlockId> = self.dirty.iter().copied().collect();
        for seed in seeds {
            if self.job_of_block.contains_key(&seed) {
                self.dirty.remove(&seed);
                continue;
            }
            if !charge(budget) {
                return Ok(());
            }
            let snapshot = snapshot_of(inputs, seed)?;
            if !snapshot.allows_candidate() {
                // lease 未归零：保持 dirty，等下一次推进再判定，不能凭空建组。
                continue;
            }
            if let Some(owner) = self.adjacent_job(seed, inputs) {
                let job = self
                    .jobs
                    .get_mut(&owner)
                    .ok_or_else(|| RawInvariant::new("收养 seed 的候选 job 不存在"))?;
                adopt(job, seed, snapshot);
                self.dirty.remove(&seed);
                self.job_of_block.insert(seed, owner);
                self.stats.blocks_bound = self.stats.blocks_bound.saturating_add(1);
                report.actions.push(CandidateAction::BindBlock {
                    block: seed,
                    job: owner,
                });
                continue;
            }
            let id = self.next_job;
            self.next_job = self
                .next_job
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("候选 job 编号溢出"))?;
            let job = CandidateJob {
                id,
                phase: CandidatePhase::Discover,
                blocks: BTreeSet::from([seed]),
                snapshot: BTreeMap::from([(seed, snapshot)]),
                weak: BTreeMap::new(),
                directed: BTreeMap::new(),
                internal_counts: BTreeMap::new(),
                outgoing: BTreeMap::new(),
                trial: BTreeMap::new(),
                edge_cursor: 0,
                cursor: 0,
                mutated: BTreeSet::new(),
                tarjan: None,
                liveness: None,
                dead: Vec::new(),
                swept: BTreeSet::new(),
                released: BTreeSet::new(),
                work: 1,
            };
            self.dirty.remove(&seed);
            self.job_of_block.insert(seed, id);
            self.jobs.insert(id, job);
            self.stats.jobs_started = self.stats.jobs_started.saturating_add(1);
            self.stats.blocks_bound = self.stats.blocks_bound.saturating_add(1);
            report.actions.push(CandidateAction::BindBlock {
                block: seed,
                job: id,
            });
        }
        Ok(())
    }

    /// 返回与本 block 相邻、且还能继续扩张的 job 编号。
    fn adjacent_job(&self, seed: ManagedBlockId, inputs: &CandidateInputs<'_>) -> Option<u32> {
        let mut adjacent = Vec::new();
        for edge in inputs.edges {
            if edge.source == seed {
                adjacent.push(edge.target);
            } else if edge.target == seed {
                adjacent.push(edge.source);
            }
        }
        for neighbour in adjacent {
            if let Some(job) = self.job_of_block.get(&neighbour).copied()
                && let Some(state) = self.jobs.get(&job)
                && state.phase < CandidatePhase::Commit
            {
                return Some(job);
            }
        }
        None
    }

    /// 推进一个 job 的当前相位；相位完成后按固定顺序前进，`Complete`/`Invalidate` 结束 job。
    fn advance_job(
        &mut self,
        id: u32,
        inputs: &CandidateInputs<'_>,
        budget: &mut u32,
        report: &mut CandidateReport,
    ) -> Result<(), RawInvariant> {
        loop {
            if *budget == 0 {
                return Ok(());
            }
            let before = *budget;
            let bound: BTreeMap<ManagedBlockId, u32> = self.job_of_block.clone();
            let (phase, step) = {
                let job = self
                    .jobs
                    .get_mut(&id)
                    .ok_or_else(|| RawInvariant::new("推进的候选 job 不存在"))?;
                let step = match job.phase {
                    CandidatePhase::Discover => step_discover(job, inputs, budget, &bound)?,
                    CandidatePhase::Trace => step_trace(job, inputs, budget)?,
                    CandidatePhase::Trial => step_trial(job, budget)?,
                    CandidatePhase::Scc => step_scc(job, budget, report)?,
                    CandidatePhase::Validate => step_validate(job, inputs, budget, report)?,
                    CandidatePhase::Commit => step_commit(job, budget, report)?,
                    CandidatePhase::Sweep => step_sweep(job),
                    CandidatePhase::Release => step_release(job, budget, report)?,
                    CandidatePhase::Complete => step_complete(job, budget)?,
                    CandidatePhase::Invalidate => step_invalidate(job, budget, report)?,
                };
                (job.phase, step)
            };
            match step {
                PhaseStep::Continue => {
                    // 没有消费任何预算又不换相位，说明该 job 正在等外部确认（sweep/release）或输入
                    // 耗尽：本批不再推进它，否则会在同一批里空转。
                    if *budget == before {
                        return Ok(());
                    }
                    continue;
                }
                PhaseStep::Retreat => {
                    let job = self
                        .jobs
                        .get_mut(&id)
                        .ok_or_else(|| RawInvariant::new("推进的候选 job 不存在"))?;
                    // 退回必须真正走到 `Invalidate` 相位：收尾动作（把成员交回 active）在那里按
                    // 预算执行，直接结束 job 会让绑定与状态残留。
                    job.phase = CandidatePhase::Invalidate;
                    job.cursor = 0;
                    continue;
                }
                PhaseStep::Done => {}
            }
            match phase {
                CandidatePhase::Complete | CandidatePhase::Invalidate => {
                    let blocks: Vec<ManagedBlockId> = self
                        .jobs
                        .get(&id)
                        .map(|job| job.blocks.iter().copied().collect())
                        .unwrap_or_default();
                    // 解绑是两种收尾共有的动作：死亡路径在释放之后解绑，失效路径在退回之后解绑，
                    // 因此块记录里的 `candidate_job` 在两条路径上都会回到「未绑定」。
                    for block in blocks {
                        report
                            .actions
                            .push(CandidateAction::UnbindBlock { block, job: id });
                    }
                    self.finish_job(id, phase == CandidatePhase::Invalidate);
                    return Ok(());
                }
                _ => {
                    let next = CandidatePhase::from_index(phase as usize + 1)
                        .ok_or_else(|| RawInvariant::new("候选相位没有后继"))?;
                    let job = self
                        .jobs
                        .get_mut(&id)
                        .ok_or_else(|| RawInvariant::new("推进的候选 job 不存在"))?;
                    job.phase = next;
                    // 成员游标随相位归零：每个相位都从头处理自己那一份成员，且只处理一次。
                    job.cursor = 0;
                }
            }
            self.bind_new_members(id, report)?;
        }
    }

    /// 把 job 成员集合里还没绑定到它的 block 补上绑定。
    ///
    /// 发现相位会把 lease 空闲的邻居直接纳入成员集合，绑定表必须跟着成员集合走，否则后续
    /// sweep/release 的确认会被当成「不属于任何候选 job」。
    fn bind_new_members(
        &mut self,
        id: u32,
        report: &mut CandidateReport,
    ) -> Result<(), RawInvariant> {
        let unbound: Vec<ManagedBlockId> = {
            let job = self
                .jobs
                .get(&id)
                .ok_or_else(|| RawInvariant::new("补齐绑定的候选 job 不存在"))?;
            job.blocks
                .iter()
                .copied()
                .filter(|block| self.job_of_block.get(block).copied() != Some(id))
                .collect()
        };
        for block in unbound {
            if self
                .job_of_block
                .insert(block, id)
                .is_some_and(|previous| previous != id)
            {
                return Err(RawInvariant::new("block 同时被两个候选 job 拥有"));
            }
            self.stats.blocks_bound = self.stats.blocks_bound.saturating_add(1);
            report
                .actions
                .push(CandidateAction::BindBlock { block, job: id });
        }
        Ok(())
    }

    /// 结束一个 job：解绑全部成员、计入统计并移除状态。
    ///
    /// 失效路径把「建组期间被 mutator 改过」的成员放回 dirty 集合：它们的改动是真实发生的，
    /// 不重新发现就等于漏收；死亡路径不放回，因为成员已经释放。
    fn finish_job(&mut self, id: u32, invalidated: bool) {
        let Some(job) = self.jobs.remove(&id) else {
            return;
        };
        for block in &job.blocks {
            self.job_of_block.remove(block);
        }
        if invalidated {
            for block in &job.mutated {
                self.dirty.insert(*block);
            }
            self.stats.jobs_invalidated = self.stats.jobs_invalidated.saturating_add(1);
        } else {
            self.stats.jobs_completed = self.stats.jobs_completed.saturating_add(1);
        }
    }
}

/// 让一个 job 收养一个新成员：重跑发现相位，闭包按新成员重新展开。
///
/// 发现相位的派生结果（邻接、计数、出边、SCC 与存活状态）都必须清空重算：`internal_counts`
/// 是饱和累加，若在旧结果上再扫一遍就会重复计数。
fn adopt(job: &mut CandidateJob, block: ManagedBlockId, snapshot: CandidateSnapshot) {
    job.blocks.insert(block);
    job.snapshot.insert(block, snapshot);
    job.weak.clear();
    job.directed.clear();
    job.internal_counts.clear();
    job.outgoing.clear();
    job.trial.clear();
    job.tarjan = None;
    job.liveness = None;
    job.edge_cursor = 0;
    job.cursor = 0;
    job.phase = CandidatePhase::Discover;
}

/// 取出一个 block 的快照；缺失快照是调用方契约失败，不能当成“没有事实”。
fn snapshot_of(
    inputs: &CandidateInputs<'_>,
    block: ManagedBlockId,
) -> Result<CandidateSnapshot, RawInvariant> {
    inputs
        .snapshots
        .iter()
        .find(|snapshot| snapshot.block == block)
        .copied()
        .ok_or_else(|| RawInvariant::new("候选判定缺少 block 快照"))
}

/// 取一个死亡组内 block 的决议世代。
fn dead_generation(
    job: &CandidateJob,
    block: ManagedBlockId,
    what: &str,
) -> Result<u32, RawInvariant> {
    job.dead
        .iter()
        .find(|(candidate, _)| *candidate == block)
        .map(|(_, generation)| *generation)
        .ok_or_else(|| RawInvariant::new(format!("{what} 确认的 block 不在决议死亡组内")))
}

/// 发现相位：线性扫过边记录，把 lease 空闲的邻居纳入组，直到输入耗尽。
///
/// 组是弱连通闭包：任何一条边只要有一端已在组内，另一端在本批快照里 lease 空闲就进组；
/// 没有大小上限，也不会因为组变大而改变判定顺序（`blocks` 始终有序）。
fn step_discover(
    job: &mut CandidateJob,
    inputs: &CandidateInputs<'_>,
    budget: &mut u32,
    bound: &BTreeMap<ManagedBlockId, u32>,
) -> Result<PhaseStep, RawInvariant> {
    while job.edge_cursor < inputs.edges.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        let edge = inputs.edges[job.edge_cursor];
        job.edge_cursor += 1;
        job.work = job.work.saturating_add(1);
        let source_inside = job.blocks.contains(&edge.source);
        let target_inside = job.blocks.contains(&edge.target);
        if !source_inside && !target_inside {
            continue;
        }
        if source_inside && target_inside {
            link(job, edge.source, edge.target, edge.count);
            continue;
        }
        let (near, far) = if source_inside {
            (edge.source, edge.target)
        } else {
            (edge.target, edge.source)
        };
        match bound.get(&far).copied() {
            // 已经属于本 job：按成员处理。
            Some(owner) if owner == job.id => {
                link(job, near, far, edge.count);
            }
            // 属于别的 job：两个 job 各自独立判定，这条边对本 job 而言是出边。
            Some(_) => {
                job.outgoing
                    .entry((near, far))
                    .and_modify(|count| *count = count.saturating_add(edge.count))
                    .or_insert(edge.count);
            }
            None => {
                let far_snapshot = snapshot_of(inputs, far)?;
                if far_snapshot.allows_candidate() {
                    job.blocks.insert(far);
                    job.snapshot.insert(far, far_snapshot);
                    link(job, near, far, edge.count);
                } else {
                    // 邻居有在飞 lease：它不能进组，这条边成为指向组外的引用。
                    job.outgoing
                        .entry((near, far))
                        .and_modify(|count| *count = count.saturating_add(edge.count))
                        .or_insert(edge.count);
                }
            }
        }
    }
    Ok(PhaseStep::Done)
}

/// 建立一条组内连接：无向邻接、定向邻接与计数同时更新。
fn link(job: &mut CandidateJob, source: ManagedBlockId, target: ManagedBlockId, count: u32) {
    job.weak.entry(source).or_default().insert(target);
    job.weak.entry(target).or_default().insert(source);
    job.directed.entry(source).or_default().insert(target);
    job.internal_counts
        .entry((source, target))
        .and_modify(|existing| *existing = existing.saturating_add(count))
        .or_insert(count);
}

/// 追踪相位：核对成员快照仍然有效；世代或版本变化就转失效。
fn step_trace(
    job: &mut CandidateJob,
    inputs: &CandidateInputs<'_>,
    budget: &mut u32,
) -> Result<PhaseStep, RawInvariant> {
    let members: Vec<ManagedBlockId> = job.blocks.iter().copied().collect();
    while job.cursor < members.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        let block = members[job.cursor];
        job.cursor += 1;
        job.work = job.work.saturating_add(1);
        let current = snapshot_of(inputs, block)?;
        let baseline = baseline_of(job, block)?;
        if current.generation != baseline.generation
            || current.mutation_version != baseline.mutation_version
        {
            return Ok(PhaseStep::Retreat);
        }
    }
    Ok(PhaseStep::Done)
}

/// 试验删除相位：减去组内入边，结果为每个成员留下「组外仍有几条引用」。
///
/// 这里不直接判活判死：正剩余只说明该成员是存活种子，最终结论要由 SCC 收缩图上的存活传播给出，
/// 否则同一弱连通组里已死的 SCC 会被存活邻居连坐。
fn step_trial(job: &mut CandidateJob, budget: &mut u32) -> Result<PhaseStep, RawInvariant> {
    let members: Vec<ManagedBlockId> = job.blocks.iter().copied().collect();
    while job.cursor < members.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        let block = members[job.cursor];
        job.cursor += 1;
        job.work = job.work.saturating_add(1);
        let baseline = *baseline_of(job, block)?;
        let internal: u32 = job
            .internal_counts
            .iter()
            .filter(|((source, target), _)| *target == block && job.blocks.contains(source))
            .map(|(_, count)| *count)
            .fold(0_u32, u32::saturating_add);
        let remaining = baseline.incoming.saturating_sub(i64::from(internal));
        job.trial.insert(block, remaining);
    }
    Ok(PhaseStep::Done)
}

/// SCC 相位：Tarjan 求组内 SCC，再按试验删除结果做存活传播；未被传播到的 SCC 是死亡组。
fn step_scc(
    job: &mut CandidateJob,
    budget: &mut u32,
    report: &mut CandidateReport,
) -> Result<PhaseStep, RawInvariant> {
    if job.tarjan.is_none() {
        job.tarjan = Some(TarjanState::new(job));
    }
    let state = job.tarjan.as_mut().expect("刚创建的 Tarjan 状态必然存在");
    if !state.advance(budget) {
        return Ok(PhaseStep::Continue);
    }
    let components = state.components();
    if job.liveness.is_none() {
        job.liveness = Some(LivenessState::new(job, &components));
    }
    let liveness = job.liveness.as_mut().expect("刚创建的存活状态必然存在");
    if !liveness.advance(budget) {
        return Ok(PhaseStep::Continue);
    }
    let live = liveness.live.clone();
    let mut dead = Vec::new();
    for (index, component) in components.iter().enumerate() {
        if live[index] {
            continue;
        }
        for block in component {
            let snapshot = baseline_of(job, *block)?;
            dead.push((*block, snapshot.generation));
        }
    }
    dead.sort();
    if dead.is_empty() {
        // 全部 SCC 都存活：整组必须失效，不能杀死任何成员。证据优先点名「组外仍有引用」的成员。
        let evidence = job
            .blocks
            .iter()
            .copied()
            .find(|block| job.trial.get(block).copied().unwrap_or_default() > 0)
            .or_else(|| job.blocks.iter().copied().next())
            .ok_or_else(|| RawInvariant::new("候选 job 没有成员 block"))?;
        report.verdicts.push((
            job.id,
            CandidateVerdict::Alive {
                reason: CandidateAliveReason::ExternalIncoming,
                evidence,
            },
        ));
        return Ok(PhaseStep::Retreat);
    }
    job.dead = dead;
    Ok(PhaseStep::Done)
}

/// 验证相位：对死亡组逐项复核 lease、pin、resource、世代、版本与当前 epoch 标记。
///
/// 只验证要杀死的成员：同一组里被判存活的 SCC 已经退出判定，不需要再检查。
fn step_validate(
    job: &mut CandidateJob,
    inputs: &CandidateInputs<'_>,
    budget: &mut u32,
    report: &mut CandidateReport,
) -> Result<PhaseStep, RawInvariant> {
    let dead: Vec<(ManagedBlockId, u32)> = job.dead.clone();
    while job.cursor < dead.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        let block = dead[job.cursor].0;
        job.cursor += 1;
        job.work = job.work.saturating_add(1);
        let current = snapshot_of(inputs, block)?;
        let baseline = baseline_of(job, block)?;
        let reason = if current.generation != baseline.generation
            || current.mutation_version != baseline.mutation_version
        {
            Some(CandidateAliveReason::Mutated)
        } else if !current.leases_idle() {
            Some(CandidateAliveReason::LeaseBusy)
        } else if current.pinned != 0 {
            Some(CandidateAliveReason::Pinned)
        } else if current.resources != 0 {
            Some(CandidateAliveReason::Resource)
        } else if current.marked != 0 {
            Some(CandidateAliveReason::Marked)
        } else {
            None
        };
        if let Some(reason) = reason {
            report.verdicts.push((
                job.id,
                CandidateVerdict::Alive {
                    reason,
                    evidence: block,
                },
            ));
            return Ok(PhaseStep::Retreat);
        }
    }
    Ok(PhaseStep::Done)
}

/// 提交相位：死亡组在一次性扣费后线性化，同时放出指向组外的减量与存活成员的退场。
///
/// 该相位不在中途产生任何动作：只有走到最后一行才发布 `CommitGroup`，因此不存在“部分成员
/// 已提交”的中间状态。
fn step_commit(
    job: &mut CandidateJob,
    budget: &mut u32,
    report: &mut CandidateReport,
) -> Result<PhaseStep, RawInvariant> {
    let dead: Vec<(ManagedBlockId, u32)> = job.dead.clone();
    while job.cursor < dead.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        job.cursor += 1;
        job.work = job.work.saturating_add(1);
    }
    // 全部成员都已计费：本相位剩下的只有一次性的线性化动作。
    let dead_blocks: BTreeSet<ManagedBlockId> = job.dead.iter().map(|(block, _)| *block).collect();
    // 死亡组的引用必须一起减掉：目标是组外 block，或是同组里仍然存活的 SCC。组内两端都死亡的
    // 引用不需要减量——两者一起消失。
    let mut drops: Vec<(ManagedBlockId, ManagedBlockId, u32)> = job
        .internal_counts
        .iter()
        .filter(|((source, target), _)| {
            dead_blocks.contains(source) && !dead_blocks.contains(target)
        })
        .map(|((source, target), count)| (*source, *target, *count))
        .collect();
    drops.extend(
        job.outgoing
            .iter()
            .filter(|((source, _), _)| dead_blocks.contains(source))
            .map(|((source, target), count)| (*source, *target, *count)),
    );
    drops.sort_unstable();
    for (source, target, count) in drops {
        report.actions.push(CandidateAction::DropOutgoing {
            source,
            target,
            count,
        });
    }
    // 同组的存活成员退回 active：它们不参与释放，但也不再绑定在这个 job 上。
    let survivors: Vec<ManagedBlockId> = job
        .blocks
        .iter()
        .copied()
        .filter(|block| !dead_blocks.contains(block))
        .collect();
    if !survivors.is_empty() {
        report
            .actions
            .push(CandidateAction::InvalidateGroup { blocks: survivors });
    }
    report.actions.push(CandidateAction::CommitGroup {
        blocks: job.dead.clone(),
    });
    report
        .verdicts
        .push((job.id, CandidateVerdict::Dead(job.dead.clone())));
    Ok(PhaseStep::Done)
}

/// 取一个成员的建组快照。
fn baseline_of(
    job: &CandidateJob,
    block: ManagedBlockId,
) -> Result<&CandidateSnapshot, RawInvariant> {
    job.snapshot
        .get(&block)
        .ok_or_else(|| RawInvariant::new("候选 job 缺少建组快照"))
}

/// sweep 相位：等待唯一消费者确认全部死亡 block。
fn step_sweep(job: &mut CandidateJob) -> PhaseStep {
    if job.swept.len() == job.dead.len() {
        return PhaseStep::Done;
    }
    PhaseStep::Continue
}

/// release 相位：发出恰好一次释放动作，等消费者确认后收尾。
fn step_release(
    job: &mut CandidateJob,
    budget: &mut u32,
    report: &mut CandidateReport,
) -> Result<PhaseStep, RawInvariant> {
    if job.released.len() == job.dead.len() {
        return Ok(PhaseStep::Done);
    }
    let dead: Vec<(ManagedBlockId, u32)> = job.dead.clone();
    while job.cursor < dead.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        let (block, generation) = dead[job.cursor];
        job.cursor += 1;
        job.work = job.work.saturating_add(1);
        report
            .actions
            .push(CandidateAction::ReleaseBlock { block, generation });
    }
    Ok(PhaseStep::Continue)
}

/// 收尾相位：死亡组已经释放，相位只完成最后一次计费。
fn step_complete(job: &mut CandidateJob, budget: &mut u32) -> Result<PhaseStep, RawInvariant> {
    if !charge(budget) {
        return Ok(PhaseStep::Continue);
    }
    job.work = job.work.saturating_add(1);
    Ok(PhaseStep::Done)
}

/// 失效相位：按预算逐个成员退回，最后一次性发出失效动作。
fn step_invalidate(
    job: &mut CandidateJob,
    budget: &mut u32,
    report: &mut CandidateReport,
) -> Result<PhaseStep, RawInvariant> {
    let members: Vec<ManagedBlockId> = job.blocks.iter().copied().collect();
    while job.cursor < members.len() {
        if !charge(budget) {
            return Ok(PhaseStep::Continue);
        }
        job.cursor += 1;
        job.work = job.work.saturating_add(1);
    }
    report.actions.push(CandidateAction::InvalidateGroup {
        blocks: job.blocks.iter().copied().collect(),
    });
    Ok(PhaseStep::Done)
}
