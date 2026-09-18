//! 候选回收平面与世界状态的适配：快照取样、动作执行与唯一 sweep 消费者。
//!
//! 归属边界：
//!
//! 1. **平面只给决议，执行在这里**：`CandidateAction` 是候选平面唯一的输出，本模块把它落成块
//!    记录的 lease/状态迁移与 `LocalHeap` 的清扫；`ReleaseBlock` 只发布 return 消息，物理清空
//!    推迟到 consume。
//! 2. **事实只有一份来源**：`incoming` 取自 `EdgePlane` 的 target 侧已应用计数，lease、世代与
//!    mutation 版本取自 `HeapBlockRecord`，pin/resource/标记数取自 header 与 mark 位图。
//! 3. **sweep 消费者唯一**：只有 `drive_candidates` 调用 `sweep_block`，且每次动作立刻回填
//!    `note_swept`/`note_released`，重复执行会被平面判成不变量失败。

use super::RawWorld;
use super::shared_heap_impl;
use crate::runtime::candidate::{
    BlockPairCount, CandidateAction, CandidateInputs, CandidateProgress, CandidateReport,
    CandidateStats,
};
use crate::runtime::candidate_schema::CandidateSnapshot;
use crate::runtime::local_heap::{BlockRef, CycleReport, ManagedBlockId};
use crate::runtime::local_heap_schema::{HEAP_BLOCK_EVAC_SOURCE, HeapBlockState};
use crate::runtime::slab::RawInvariant;
use crate::runtime::startup_schema::{ExitCategory, ReportEvent, ReportReason};
use crate::runtime::termination::ReportSpec;

impl RawWorld {
    /// 启用候选回收平面；与 LocalHeap、边平面一起配置。
    pub(super) fn configure_candidates(&mut self) {
        self.candidates = Some(super::super::candidate::CandidatePlane::new());
    }

    /// 返回候选平面是否已启用。
    pub(crate) fn candidates_configured(&self) -> bool {
        self.candidates.is_some()
    }

    /// 取消全部进行中的 GC 工作：候选 job 全部退回、`candidate_job` 与状态复位。
    ///
    /// 失败路径必须先取消再上报：半完成的 job 会带着绑定与 `candidate` 状态活到下一个 cycle，
    /// 而下一个 cycle 的发现相位会把它当作“已经在组里”而跳过检查。返回被取消的 job 数。
    pub(crate) fn cancel_gc_work(&mut self, reason: &str) -> Result<u32, RawInvariant> {
        if self.candidates.is_none() {
            return Ok(0);
        }
        let planned = self
            .candidates
            .as_mut()
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?
            .cancel_all();
        if planned == 0 {
            return Ok(0);
        }
        // 推进一轮把退回动作执行掉：`Invalidate` 相位按预算逐个成员退回并解绑。
        let _ = self.drive_candidates(u32::MAX);
        debug_assert_eq!(
            self.candidate_job_count()?,
            0,
            "取消 {reason} 后不得残留候选 job"
        );
        Ok(planned)
    }

    /// 把一次 GC 失败接到既有诊断链上：走 RT0 的报告账本，reason 使用运行时内部不变量。
    ///
    /// RT0 尚未启动时没有账本可写，返回 `None`；调用方据此知道失败只以 `RawInvariant` 形式返回。
    pub(crate) fn report_gc_failure(
        &mut self,
        message: String,
    ) -> Result<Option<u64>, RawInvariant> {
        let Some(rt0) = self.rt0.as_mut() else {
            return Ok(None);
        };
        let epoch = rt0.emit_runtime_report(ReportSpec {
            event: ReportEvent::Panic,
            class: ExitCategory::RuntimeFailure,
            reason: ReportReason::RuntimeInvariant,
            message: Some(message),
            location: None,
            exit_code: 101,
        });
        Ok(Some(epoch))
    }

    /// 返回候选平面的活跃 job 数。
    pub(crate) fn candidate_job_count(&self) -> Result<usize, RawInvariant> {
        self.candidates
            .as_ref()
            .map(super::super::candidate::CandidatePlane::job_count)
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))
    }

    /// 返回候选平面的统计快照。
    pub(crate) fn candidate_stats(&self) -> Result<CandidateStats, RawInvariant> {
        self.candidates
            .as_ref()
            .map(super::super::candidate::CandidatePlane::stats)
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))
    }

    /// 返回一个 block 当前绑定的候选 job。
    pub(crate) fn candidate_job_of(&self, id: ManagedBlockId) -> Result<Option<u32>, RawInvariant> {
        self.candidates
            .as_ref()
            .map(|plane| plane.job_of_block(id))
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))
    }

    /// 返回全部活跃候选 job 的进度；调用方据此报告真实进度。
    pub(crate) fn candidate_progress(&self) -> Result<Vec<CandidateProgress>, RawInvariant> {
        let plane = self
            .candidates
            .as_ref()
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?;
        Ok(plane
            .job_phases()
            .into_iter()
            .map(|(job, phase)| CandidateProgress {
                job,
                phase,
                blocks: plane.job_blocks(job),
                pending_edges: plane.job_edge_cursor(job),
                work_units: plane.job_work(job),
            })
            .collect())
    }

    /// 通知候选平面某个 block 已被改动。
    pub(crate) fn note_candidate_dirty(&mut self, id: ManagedBlockId) -> Result<(), RawInvariant> {
        self.candidates
            .as_mut()
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?
            .note_dirty(id);
        Ok(())
    }

    /// 推进候选回收：取样、推进平面、执行动作并回填确认。
    pub(crate) fn drive_candidates(
        &mut self,
        quantum: u32,
    ) -> Result<CandidateReport, RawInvariant> {
        if self.candidates.is_none() {
            return Err(RawInvariant::new("候选平面尚未配置"));
        }
        let snapshots = self.candidate_snapshots()?;
        let edges = self.candidate_edges()?;
        let inputs = CandidateInputs {
            snapshots: &snapshots,
            edges: &edges,
        };
        let report = self
            .candidates
            .as_mut()
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?
            .advance(quantum, &inputs)?;
        // 快照已经取走：边平面的 dirty 位在本轮被消费，否则它会无限累积，让后续每轮都重复
        // 处理同一批已经判定过的 block。
        let consumed: Vec<ManagedBlockId> = self
            .edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
            .dirty_blocks()
            .collect();
        for block in consumed {
            self.edges
                .as_mut()
                .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
                .clear_dirty(block);
        }
        for action in report.actions.clone() {
            self.execute_candidate_action(action)?;
        }
        Ok(report)
    }

    /// 收集候选判定需要的 block 快照。
    ///
    /// 范围是「候选平面已知的 block」加「边平面记过入边或 dirty 的 block」：前者保证已绑定成员
    /// 一定有快照，后者保证新候选能被发现；缺失的事实不会被臆断成零，而是取样失败。
    fn candidate_snapshots(&self) -> Result<Vec<CandidateSnapshot>, RawInvariant> {
        let plane = self
            .candidates
            .as_ref()
            .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?;
        let edges = self
            .edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?;
        let mut ids: std::collections::BTreeSet<ManagedBlockId> =
            plane.tracked_blocks().into_iter().collect();
        for (_, destination, _) in edges.applied_pairs() {
            ids.insert(destination.id);
        }
        ids.extend(edges.dirty_blocks());
        // 候选回收的域是 LocalHeap 的块：共享 block 没有 arena 类别与 LocalHeap 块状态，它的字节
        // 归还由 block return 路径负责，因此不进入候选快照，也不参与试验删除与 SCC 判定。
        ids.retain(|id| !shared_heap_impl::is_shared_descriptor(id.arena()));
        let mut snapshots = Vec::with_capacity(ids.len());
        for id in ids {
            snapshots.push(self.candidate_snapshot(id)?);
        }
        Ok(snapshots)
    }

    /// 取样一个 block 的候选判定事实。
    fn candidate_snapshot(&self, id: ManagedBlockId) -> Result<CandidateSnapshot, RawInvariant> {
        let arena = self.managed_arena_by_descriptor(id.arena())?;
        let heap = self.heap(arena.heap_owner)?;
        let record = heap.block_record(id).map_err(heap_error)?;
        let block_ref = BlockRef {
            id,
            generation: record.generation,
        };
        let incoming = self
            .edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
            .incoming_applied(block_ref);
        let (pinned, resources) = heap.block_pin_and_resource_counts(id).map_err(heap_error)?;
        let marked = heap.block_marked_objects(id).map_err(heap_error)?;
        Ok(CandidateSnapshot {
            block: id,
            generation: record.generation,
            incoming,
            incoming_leases: record.incoming_leases,
            allocator_leases: record.allocator_leases,
            scanner_leases: record.scanner_leases,
            evacuation_leases: record.evacuation_leases,
            mutation_version: record.mutation_version,
            pinned,
            resources,
            marked,
        })
    }

    /// 收集 block 对的计数。
    ///
    /// 计数取自边平面的已应用值：它已经是 target 侧的真实入边计数，候选相位因此不需要重扫
    /// payload；逐对象精确追踪由 mark/trace 平面负责，候选只做 block 级试验删除。
    fn candidate_edges(&self) -> Result<Vec<BlockPairCount>, RawInvariant> {
        let edges = self
            .edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?;
        edges
            .applied_pairs()
            .into_iter()
            .map(|(source, destination, count)| {
                // 已应用计数必须能用 u32 表示：候选平面的计数、试验删除与出边减量都按它推理，
                // 放不下就是真的不变量破损，不能截断成一个偏大的值继续判定。
                Ok(BlockPairCount {
                    source: source.id,
                    target: destination.id,
                    count: u32::try_from(count)
                        .map_err(|_| RawInvariant::new("已应用边计数超出候选平面的 u32 范围"))?,
                })
            })
            .collect()
    }

    /// 执行一条候选动作；每一步都立刻回填平面要求的确认。
    fn execute_candidate_action(&mut self, action: CandidateAction) -> Result<(), RawInvariant> {
        match action {
            CandidateAction::BindBlock { block, job } => {
                self.heap_mut_for(block)?
                    .mark_block_candidate(block, job)
                    .map_err(heap_error)?;
            }
            CandidateAction::UnbindBlock { block, job } => {
                let heap = self.heap_mut_for(block)?;
                let record = heap.block_record(block).map_err(heap_error)?;
                if record.candidate_job == job {
                    heap.unmark_block_candidate(block).map_err(heap_error)?;
                }
            }
            CandidateAction::CommitGroup { blocks } => {
                for (block, generation) in blocks {
                    let heap = self.heap_mut_for(block)?;
                    let record = heap.block_record(block).map_err(heap_error)?;
                    if record.generation != generation {
                        return Err(RawInvariant::new("提交的 block 世代已经变化"));
                    }
                    // 决议已确定：成员绑定解除。evacuation 来源位由 `unmark` 保留，
                    // 随后按该位选择 Sweeping 或直接 ReturnPending。
                    heap.unmark_block_candidate(block).map_err(heap_error)?;
                    let mut record = heap.block_record(block).map_err(heap_error)?;
                    let evac = record.reserved & HEAP_BLOCK_EVAC_SOURCE != 0;
                    let live_lines = heap.block_live_lines(block).map_err(heap_error)?;
                    if evac {
                        if live_lines != 0 {
                            return Err(RawInvariant::new(
                                "evacuating 源块仍有 live line，不得进入 ReturnPending",
                            ));
                        }
                        record.reserved &= !HEAP_BLOCK_EVAC_SOURCE;
                        record.state = HeapBlockState::ReturnPending.raw();
                        heap.update_block_record(block, record)
                            .map_err(heap_error)?;
                        heap.promote_empty_large_span(block).map_err(heap_error)?;
                    } else {
                        record.state = HeapBlockState::Sweeping.raw();
                        heap.update_block_record(block, record)
                            .map_err(heap_error)?;
                        let mut cycle = CycleReport::default();
                        heap.sweep_block(block, &mut cycle).map_err(heap_error)?;
                        let live_lines = heap.block_live_lines(block).map_err(heap_error)?;
                        let mut record = heap.block_record(block).map_err(heap_error)?;
                        if live_lines == 0 {
                            record.state = HeapBlockState::ReturnPending.raw();
                            heap.update_block_record(block, record)
                                .map_err(heap_error)?;
                            heap.promote_empty_large_span(block).map_err(heap_error)?;
                        } else {
                            record.state = HeapBlockState::Allocating.raw();
                            heap.update_block_record(block, record)
                                .map_err(heap_error)?;
                            self.queue_line_runs_after_sweep(block)?;
                        }
                    }
                    self.candidates
                        .as_mut()
                        .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?
                        .note_swept(block, generation)?;
                }
            }
            CandidateAction::DropOutgoing {
                source,
                target,
                count,
            } => {
                let source_ref = self.block_ref_of(source)?;
                let target_ref = self.block_ref_of(target)?;
                self.edges
                    .as_mut()
                    .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
                    .drop_applied(source_ref, target_ref, count)?;
            }
            CandidateAction::ReleaseBlock { block, generation } => {
                let heap = self.heap_mut_for(block)?;
                let record = heap.block_record(block).map_err(heap_error)?;
                if record.generation != generation {
                    return Err(RawInvariant::new("释放的 block 世代已经变化"));
                }
                if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::ReturnPending) {
                    return Err(RawInvariant::new("ReleaseBlock 要求块已处于 ReturnPending"));
                }
                self.queue_heap_block_return(block)?;
                self.candidates
                    .as_mut()
                    .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?
                    .note_released(block, generation)?;
            }
            CandidateAction::InvalidateGroup { blocks } => {
                for block in blocks {
                    let heap = self.heap_mut_for(block)?;
                    let mut record = heap.block_record(block).map_err(heap_error)?;
                    if HeapBlockState::from_raw(record.state) == Some(HeapBlockState::Candidate) {
                        if record.reserved & HEAP_BLOCK_EVAC_SOURCE != 0 {
                            record.state = HeapBlockState::Evacuating.raw();
                        } else {
                            record.state = HeapBlockState::Allocating.raw();
                        }
                        heap.update_block_record(block, record)
                            .map_err(heap_error)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// 按 block 身份解析它所属 owner 的 LocalHeap（可变）。
    pub(super) fn heap_mut_for(
        &mut self,
        id: ManagedBlockId,
    ) -> Result<&mut super::super::local_heap::LocalHeap, RawInvariant> {
        let owner = self.managed_arena_by_descriptor(id.arena())?.heap_owner;
        self.heap_mut(owner)
    }

    /// 按 block 身份解析它所属 owner 的 LocalHeap。
    pub(super) fn heap_for(
        &self,
        id: ManagedBlockId,
    ) -> Result<&super::super::local_heap::LocalHeap, RawInvariant> {
        let owner = self.managed_arena_by_descriptor(id.arena())?.heap_owner;
        self.heap(owner)
    }

    /// 返回一个 block 当前的稳定身份。
    pub(super) fn block_ref_of(&self, id: ManagedBlockId) -> Result<BlockRef, RawInvariant> {
        let arena = self.managed_arena_by_descriptor(id.arena())?;
        let record = self
            .heap(arena.heap_owner)?
            .block_record(id)
            .map_err(heap_error)?;
        Ok(BlockRef {
            id,
            generation: record.generation,
        })
    }
}

/// LocalHeap 错误转成平面不变量：候选路径上的容量不足意味着状态机与堆不一致。
pub(super) use super::heap_impl::heap_error;
