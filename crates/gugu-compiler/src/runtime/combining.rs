//! typed combining 冷操作的确定性参照实现。
//!
//! `CombiningPlane` 是 `combining_schema` 契约的对偶：
//!
//! - 记录池是非移动 metadata：按 chunk 整块补充，chunk 内记录永不移动，已发放的
//!   `OpTicket` 只是「chunk/slot/generation」，槽位回收时推进 generation；
//! - 每个 combiner 一条 FIFO 的 MCS 记录链（`mcs_next` 单链），争用请求挂在链尾；
//! - 无争用时单原子 claim 字直接认领（fast path），争用时挂链并在后续轮次被认领；
//! - 一轮最多认领 `round_item_budget` 条、`round_byte_budget` 字节（每条按记录规范槽
//!   加请求字节计价），连续同类 `(tag, merge_key)` 记录按 `merge_limit` 合并成组；
//! - 取消只允许发生在认领之前；等待到 `timeout_rounds` 轮的记录以 `TimedOut` 收尾；
//! - response 只在记录完成时发布一次，`release` 回收槽位并推进 generation。
//!
//! 平面本身不执行任何 handler：它只回答「本轮认领了哪些操作、哪些操作可以合并成一次
//! 执行」，真实动作由 world 在 `execute_combining_operation` 里按 tag 分派。因此 combiner
//! 永远不会执行用户 closure、drop glue 或跨 safepoint 持锁。

use super::combining_schema::{
    COMBINING_RECORD_BYTES, CombiningMode, CombiningRuntimeContract, OPERATION_OUTCOMES,
    OPERATION_STATES, OPERATION_TAGS,
};
use super::slab::RawInvariant;

/// 冷操作 tag；判别值顺序与契约的 `OPERATION_TAGS` 目录逐项一致。
///
/// 名字直接取自契约目录（`OPERATION_TAGS[tag as usize]`），因此列目录与名字不可能分叉；
/// 顺序一致性由 `CombiningPlane::new` 的 `debug_assert!` 与契约测试共同锁定。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationTag {
    /// 平台页 trim：撤销一个 extent 的物理页。
    GlobalRangeRefill,
    /// extent coalescing：把已撤销物理页的 extent 合回 buddy 阶梯。
    ExtentCoalesce,
    /// topology 目录重建。
    TopologyRebuild,
    /// 平台 trim 的页撤销动作。
    PlatformTrim,
}

impl OperationTag {
    /// 登记顺序全表；契约目录与判别值顺序的一致性由它逐项核对。
    pub(crate) const ALL: [Self; 4] = [
        Self::GlobalRangeRefill,
        Self::ExtentCoalesce,
        Self::TopologyRebuild,
        Self::PlatformTrim,
    ];

    /// 返回稠密下标；也是统计与按 tag 计数的下标。
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// 返回契约目录中的 tag 名。
    pub(crate) const fn name(self) -> &'static str {
        OPERATION_TAGS[self.index()]
    }
}

/// operation record 的生命周期状态；判别值顺序与契约的 `OPERATION_STATES` 一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationState {
    /// 槽位空闲，可由新请求复用。
    Free,
    /// 已发布并入链，等待 combiner 认领。
    Published,
    /// 已被 fast path 或一轮 combiner 认领，等待 handler 结果。
    Claimed,
    /// handler 已结束并发布 response，等待请求者回收槽位。
    Completed,
    /// 认领之前被取消；由下一轮 combiner 跳过并回收，没有任何 handler。
    Cancelled,
}

impl OperationState {
    /// 返回契约目录中的状态名。
    pub(crate) const fn name(self) -> &'static str {
        OPERATION_STATES[self as usize]
    }
}

/// 一次冷操作的结局；判别值顺序与契约的 `OPERATION_OUTCOMES` 一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationOutcome {
    /// handler 已执行，`bytes` 是它本次作用（或登记）的字节数。
    Applied { bytes: u64 },
    /// 认领之前被取消，handler 从未执行。
    Cancelled,
    /// 在链上等待到超时轮数，handler 从未执行。
    TimedOut,
}

impl OperationOutcome {
    /// 返回契约目录中的结局名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Applied { .. } => OPERATION_OUTCOMES[0],
            Self::Cancelled => OPERATION_OUTCOMES[1],
            Self::TimedOut => OPERATION_OUTCOMES[2],
        }
    }

    /// 返回已作用字节数；从未执行的结局返回 `None`。
    pub(crate) const fn applied_bytes(self) -> Option<u64> {
        match self {
            Self::Applied { bytes } => Some(bytes),
            Self::Cancelled | Self::TimedOut => None,
        }
    }
}

/// 一个操作记录在池内的地址；chunk 分配后永不移动，因此它可以长期持有。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpId {
    chunk: usize,
    slot: usize,
}

/// operation record 的句柄；generation 使回收后的旧句柄必然失效。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpTicket {
    id: OpId,
    generation: u32,
}

impl OpTicket {
    /// 返回槽位当前的 generation；回收后推进，因此旧句柄必然读到更大值。
    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }
}

/// 提交给平面的冷操作请求；四个 tag 的标量参数都在这里。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationRequest {
    pub(crate) tag: OperationTag,
    /// combiner 槽位；每个 owner token 一个队列。
    pub(crate) owner: u32,
    /// 同类合并键；同 tag 内按它分组。
    pub(crate) merge_key: u64,
    /// stable descriptor id（extent 编号、epoch 等）。
    pub(crate) descriptor: u64,
    /// 标量参数（extent class 等）。
    pub(crate) scalar: u64,
    /// 请求字节数；参与轮次字节预算。
    pub(crate) bytes: u64,
}

/// 一条被认领的操作；world 用它执行 handler。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Operation {
    pub(crate) tag: OperationTag,
    pub(crate) descriptor: u64,
    pub(crate) scalar: u64,
    pub(crate) bytes: u64,
    pub(crate) merge_key: u64,
}

impl Operation {
    /// `global-range-refill` 的规范载荷。
    ///
    /// refill 是「记录池自身」的 GlobalRange 操作：它不引用任何 extent，handler 恰好
    /// 补充一个 chunk，bytes 由新增槽位数按记录规范槽计算。
    pub(crate) const fn global_range_refill() -> Self {
        Self {
            tag: OperationTag::GlobalRangeRefill,
            descriptor: 0,
            scalar: 0,
            bytes: 0,
            merge_key: 0,
        }
    }
}

/// `publish` 的分流结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublishOutcome {
    /// direct 模式：请求者上下文直接执行，不创建记录。
    Direct,
    /// 无争用：claim 字已由本次请求占用，记录处于 `claimed`。
    FastPath(OpTicket),
    /// 有争用：记录已挂到 combiner 链尾，等待后续轮次认领。
    Parked(OpTicket),
}

/// 一组同类请求；组内成员共享一次执行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MergeGroup {
    tag: OperationTag,
    merge_key: u64,
    tickets: Vec<OpTicket>,
}

/// 平面统计；字段顺序与契约的 `COMBINING_STATISTICS` 目录一致。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CombiningStats {
    /// 进入记录池的冷操作请求数；direct 模式不创建记录，因此不计数。
    pub(crate) requests: u64,
    /// 无争用路径直接占用 claim 字的次数。
    pub(crate) fast_path_claims: u64,
    /// 因争用挂链的次数。
    pub(crate) contended_parkings: u64,
    /// 因为合并而省下的执行次数。
    pub(crate) merged_requests: u64,
    /// 打开的 combiner 轮次数。
    pub(crate) rounds: u64,
    /// handler 执行次数（按组计，快速路径一次算一次）。
    pub(crate) executions: u64,
    /// 认领之前被取消的次数。
    pub(crate) cancellations: u64,
    /// 等待到超时轮数的记录数。
    pub(crate) timeouts: u64,
    /// 记录池补充次数（含构造时的首个 chunk）。
    pub(crate) refills: u64,
}

/// 一轮 combiner 的结果。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RoundReport {
    /// 本轮的执行次数（按合并组计）。
    pub(crate) executions: u32,
    /// 本轮认领的记录数。
    pub(crate) items: u32,
    /// 本轮认领记录携带的请求字节。
    pub(crate) bytes: u64,
    /// 本轮因为合并省下的执行次数。
    pub(crate) merged: u32,
    /// 本轮跳过并回收的已取消记录数（取消时已经计过统计）。
    pub(crate) cancellations: u32,
    /// 本轮因超时收尾的记录数。
    pub(crate) timeouts: u32,
    /// 本轮发布 response 的挂链记录数；等于需要平台唤醒的等待者上界。
    pub(crate) woken: u32,
}

/// 平面的预算与池参数；来源是契约，因此两处不会各有一套数字。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CombiningLimits {
    pub(crate) merge_limit: u32,
    pub(crate) round_item_budget: u32,
    pub(crate) round_byte_budget: u64,
    pub(crate) timeout_rounds: u32,
    pub(crate) record_bytes: u32,
    pub(crate) record_chunk_items: u32,
    pub(crate) refill_reserve_slots: u32,
    pub(crate) max_operation_chunks: u32,
}

impl CombiningLimits {
    /// 由已验证的契约取全部参数。
    pub(crate) fn from_contract(contract: &CombiningRuntimeContract) -> Self {
        Self {
            merge_limit: contract.merge_limit(),
            round_item_budget: contract.round_item_budget(),
            round_byte_budget: contract.round_byte_budget(),
            timeout_rounds: contract.timeout_rounds(),
            record_bytes: contract.record_bytes(),
            record_chunk_items: contract.record_chunk_items(),
            refill_reserve_slots: contract.refill_reserve_slots(),
            max_operation_chunks: contract.max_operation_chunks(),
        }
    }

    /// `RawWorld::new` 在契约可用之前使用的默认参数集；与契约常量同源。
    pub(crate) const fn from_contract_default() -> Self {
        Self {
            merge_limit: super::combining_schema::COMBINING_MERGE_LIMIT,
            round_item_budget: super::combining_schema::COMBINING_ROUND_ITEM_BUDGET,
            round_byte_budget: super::combining_schema::COMBINING_ROUND_BYTE_BUDGET,
            timeout_rounds: super::combining_schema::COMBINING_TIMEOUT_ROUNDS,
            record_bytes: COMBINING_RECORD_BYTES,
            record_chunk_items: super::combining_schema::COMBINING_RECORD_CHUNK_ITEMS,
            refill_reserve_slots: super::combining_schema::COMBINING_REFILL_RESERVE_SLOTS,
            max_operation_chunks: super::combining_schema::COMBINING_MAX_OPERATION_CHUNKS,
        }
    }
}

/// 一条 operation record：全部字段都是标量、stable descriptor id 与 response slot，
/// 没有任何 managed/raw 地址，因此记录池可以是非移动的纯 metadata。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OperationRecord {
    tag: OperationTag,
    state: OperationState,
    /// 槽位重用时推进；旧 ticket 因此读到 generation 不匹配。
    generation: u32,
    /// combiner 槽位。
    owner: u32,
    merge_key: u64,
    descriptor: u64,
    scalar: u64,
    bytes: u64,
    /// 只在完成或被取消后写入一次；`None` 表示 response 尚未发布。
    response: Option<OperationOutcome>,
    /// 该记录在链上等过的轮数。
    waited_rounds: u32,
    /// MCS 记录链的下一跳。
    mcs_next: Option<OpId>,
}

impl OperationRecord {
    const fn free(generation: u32) -> Self {
        Self {
            tag: OperationTag::GlobalRangeRefill,
            state: OperationState::Free,
            generation,
            owner: 0,
            merge_key: 0,
            descriptor: 0,
            scalar: 0,
            bytes: 0,
            response: None,
            waited_rounds: 0,
            mcs_next: None,
        }
    }

    /// 槽位是否被某个请求占用。
    const fn is_available(&self) -> bool {
        matches!(self.state, OperationState::Free)
    }
}

/// 一个 combiner 的 MCS 记录链；`head` 是最老的等待者，`tail` 是最新的。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CombinerQueues {
    head: Option<OpId>,
    tail: Option<OpId>,
}

/// typed combining 的确定性执行平面。
#[derive(Debug)]
pub(crate) struct CombiningPlane {
    mode: CombiningMode,
    limits: CombiningLimits,
    /// 非移动记录池；chunk 只在 `grow_operation_pool` 里整块 push，记录永不移动。
    chunks: Vec<Box<[OperationRecord]>>,
    /// 普通空闲槽位。
    free: Vec<OpId>,
    /// 每个 chunk 尾部的 refill 预留槽位；只有 `global-range-refill` 可以取用。
    reserved: Vec<OpId>,
    /// 每个 combiner 槽位一条 FIFO 链。
    combiners: Vec<CombinerQueues>,
    /// 单原子 claim 字：持有一轮的 combiner 槽位。
    ///
    /// 参照实现用 `Option<u32>` 表示这个字：它的取值变化只有「无争用占用」与「轮次结束
    /// 释放」两次，与真实实现里的 compare-exchange 一一对应，因此 fast path 的语义
    /// （谁先认领谁执行）在模型里仍然成立。
    claim: Option<u32>,
    /// 本轮认领的记录；`end_round` 用它结清全部已认领操作。
    round_claimed: Vec<OpTicket>,
    /// 本轮已消费的请求字节；轮次字节预算按它判定。
    ///
    /// 记录规范槽 `COMBINING_RECORD_BYTES` 是记录池的固定开销，不重复计入请求字节；
    /// 一轮满条目预算的规范槽开销由契约的 `round_byte_budget >= record_bytes * round_item_budget`
    /// 在上界上保证，因此字节预算不会因为记录开销而挤掉应有的一轮条目数。
    round_charge: u64,
    /// 本轮跳过并回收的已取消记录数。
    round_cancellations: u32,
    /// 按 tag 统计的请求数；世界用它证明某条冷路径确实经过了平面。
    tag_requests: [u64; OPERATION_TAGS.len()],
    stats: CombiningStats,
}

impl CombiningPlane {
    /// 由模式、combiner 槽位数与预算创建平面，并立即补充首个 chunk。
    ///
    /// refill 预留槽位只在池满时留给 `global-range-refill` 自己，因此「池满」不会让
    /// 补充记录池这条唯一的出路也被卡住。
    pub(crate) fn new(mode: CombiningMode, queue_count: u32, limits: CombiningLimits) -> Self {
        debug_assert!(limits.refill_reserve_slots < limits.record_chunk_items);
        debug_assert!(
            OperationTag::ALL.iter().enumerate().all(|(index, tag)| {
                tag.index() == index && OPERATION_TAGS[index] == tag.name()
            })
        );
        let mut plane = Self {
            mode,
            limits,
            chunks: Vec::new(),
            free: Vec::new(),
            reserved: Vec::new(),
            combiners: vec![
                CombinerQueues::default();
                usize::try_from(queue_count).expect("combiner 槽位数适配 usize")
            ],
            claim: None,
            round_claimed: Vec::new(),
            round_charge: 0,
            round_cancellations: 0,
            tag_requests: [0; OPERATION_TAGS.len()],
            stats: CombiningStats::default(),
        };
        plane
            .grow_operation_pool()
            .expect("首个 chunk 必然可以补充");
        plane
    }

    /// 返回 combining 模式。
    pub(crate) const fn mode(&self) -> CombiningMode {
        self.mode
    }

    /// 返回统计快照。
    pub(crate) const fn stats(&self) -> CombiningStats {
        self.stats
    }

    /// 返回某个 tag 的累计请求数。
    pub(crate) const fn tag_requests(&self, tag: OperationTag) -> u64 {
        self.tag_requests[tag.index()]
    }

    /// 返回单个记录的规范槽字节；refill 记账与轮次计价共用它。
    pub(crate) const fn record_bytes(&self) -> u32 {
        self.limits.record_bytes
    }

    /// 返回 chunk 数。
    pub(crate) fn chunk_count(&self) -> u32 {
        u32::try_from(self.chunks.len()).expect("chunk 数量适配 u32")
    }

    /// 返回记录池的槽位总数。
    pub(crate) fn pool_slots(&self) -> u32 {
        u32::try_from(self.chunks.len() * self.chunk_items()).expect("记录池槽位数适配 u32")
    }

    /// 返回仍被请求占用的记录数。
    pub(crate) fn pending_records(&self) -> u32 {
        let available =
            u32::try_from(self.free.len() + self.reserved.len()).expect("空闲槽位数适配 u32");
        debug_assert!(self.pool_slots() >= available);
        self.pool_slots().saturating_sub(available)
    }

    /// 返回仍被请求占用的记录携带的请求字节数。
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.iter())
            .filter(|record| !record.is_available())
            .map(|record| record.bytes)
            .sum()
    }

    /// 判断 claim 字是否已被某一轮占用。
    pub(crate) const fn claim_held(&self) -> bool {
        self.claim.is_some()
    }

    /// 判断当前是否有 combiner 正在执行一轮。
    pub(crate) const fn round_open(&self) -> bool {
        self.claim.is_some()
    }

    /// 判断某个 combiner 现在能否开一轮：没有打开的轮次，且本队列还有等待记录。
    ///
    /// 这是 owner service 里的 O(1) 检查：空队列与已打开的轮次都不会产生空轮次，
    /// 因此热路径不会因为「每次都开一轮」而污染统计。
    pub(crate) fn queue_open(&self, slot: u32) -> bool {
        if self.claim.is_some() {
            return false;
        }
        let Ok(index) = usize::try_from(slot) else {
            return false;
        };
        self.combiners
            .get(index)
            .is_some_and(|queue| queue.head.is_some())
    }

    /// 提交一条请求：direct 模式直接返回、无争用走单原子 fast path、有争用挂链等待。
    pub(crate) fn publish(
        &mut self,
        request: OperationRequest,
    ) -> Result<PublishOutcome, RawInvariant> {
        if self.mode == CombiningMode::Direct {
            // direct 模式不创建记录：同一份 handler 由请求者在自己的上下文执行。
            return Ok(PublishOutcome::Direct);
        }
        let id = self.reserve_slot(request.tag)?;
        let ticket = self.init_record(id, &request);
        self.note_request(request.tag);
        // fast path 只在本队列没有更老的等待者时成立：否则后来的请求会越过 FIFO 链。
        let uncontended =
            self.claim.is_none() && self.combiner_queue(request.owner)?.head.is_none();
        if uncontended {
            self.claim = Some(request.owner);
            self.chunks[id.chunk][id.slot].state = OperationState::Claimed;
            self.stats.fast_path_claims += 1;
            return Ok(PublishOutcome::FastPath(ticket));
        }
        self.push_parked(id, request.owner)?;
        self.stats.contended_parkings += 1;
        Ok(PublishOutcome::Parked(ticket))
    }

    /// 批量提交：为了先积累同类请求、再由一轮 combiner 合并执行，这里永远入链。
    pub(crate) fn enqueue(&mut self, request: OperationRequest) -> Result<OpTicket, RawInvariant> {
        let id = self.reserve_slot(request.tag)?;
        let ticket = self.init_record(id, &request);
        self.note_request(request.tag);
        self.push_parked(id, request.owner)?;
        self.stats.contended_parkings += 1;
        Ok(ticket)
    }

    /// 开一轮并认领记录；已取消的记录在这里被跳过并回收。
    pub(crate) fn begin_round(&mut self, slot: u32) -> Result<Vec<MergeGroup>, RawInvariant> {
        if self.claim.is_some() {
            return Err(RawInvariant::new(
                "combiner 轮次不得嵌套；range lock 不得跨第二次 combiner 执行",
            ));
        }
        let index = usize::try_from(slot).expect("combiner 槽位适配 usize");
        if index >= self.combiners.len() {
            return Err(RawInvariant::new("combiner 轮次引用了未登记的槽位"));
        }
        self.claim = Some(slot);
        self.round_claimed.clear();
        self.round_charge = 0;
        self.round_cancellations = 0;
        let mut groups: Vec<MergeGroup> = Vec::new();
        while u32::try_from(self.round_claimed.len()).expect("认领记录数适配 u32")
            < self.limits.round_item_budget
        {
            let Some(head) = self.combiners[index].head else {
                break;
            };
            // 已取消的记录没有 handler：跳过、回收，并且不占本轮预算。
            if self.chunks[head.chunk][head.slot].state == OperationState::Cancelled {
                self.pop_head(index);
                self.recycle(head);
                self.round_cancellations += 1;
                continue;
            }
            let charge = self.chunks[head.chunk][head.slot].bytes;
            // 第一条记录即使超过字节预算也必须被认领，否则永远无法推进。
            if !self.round_claimed.is_empty()
                && self.round_charge.saturating_add(charge) > self.limits.round_byte_budget
            {
                break;
            }
            self.pop_head(index);
            self.round_charge = self.round_charge.saturating_add(charge);
            let tag = self.chunks[head.chunk][head.slot].tag;
            let merge_key = self.chunks[head.chunk][head.slot].merge_key;
            let ticket = OpTicket {
                id: head,
                generation: self.chunks[head.chunk][head.slot].generation,
            };
            self.chunks[head.chunk][head.slot].state = OperationState::Claimed;
            self.round_claimed.push(ticket);
            match groups.last_mut() {
                Some(group)
                    if group.tag == tag
                        && group.merge_key == merge_key
                        && u32::try_from(group.tickets.len()).expect("组成员数适配 u32")
                            < self.limits.merge_limit =>
                {
                    group.tickets.push(ticket);
                }
                _ => groups.push(MergeGroup {
                    tag,
                    merge_key,
                    tickets: vec![ticket],
                }),
            }
        }
        self.stats.rounds += 1;
        Ok(groups)
    }

    /// 返回一个合并组内成员的操作视图；执行入口只认这些操作。
    pub(crate) fn group_operations(
        &self,
        group: &MergeGroup,
    ) -> Result<Vec<Operation>, RawInvariant> {
        group
            .tickets
            .iter()
            .map(|ticket| self.operation(*ticket))
            .collect()
    }

    /// 返回合并组成员数。
    pub(crate) fn groups_members(&self, group: &MergeGroup) -> u32 {
        u32::try_from(group.tickets.len()).expect("合并组成员数适配 u32")
    }

    /// 返回合并组的 tag。
    pub(crate) fn groups_tag(&self, group: &MergeGroup) -> OperationTag {
        group.tag
    }

    /// 按 ticket 取操作；只有处于已认领状态的记录才能被执行。
    pub(crate) fn operation(&self, ticket: OpTicket) -> Result<Operation, RawInvariant> {
        let record = self.record(ticket)?;
        if record.state != OperationState::Claimed {
            return Err(RawInvariant::new("combining 操作不处于已认领状态"));
        }
        Ok(Operation {
            tag: record.tag,
            descriptor: record.descriptor,
            scalar: record.scalar,
            bytes: record.bytes,
            merge_key: record.merge_key,
        })
    }

    /// 结束 fast path：发布 response、释放 claim 字并计入一次执行。
    pub(crate) fn complete(
        &mut self,
        ticket: OpTicket,
        outcome: OperationOutcome,
    ) -> Result<(), RawInvariant> {
        let owner = self.record(ticket)?.owner;
        let claimed = self.record(ticket)?.state == OperationState::Claimed;
        if !claimed || self.claim != Some(owner) {
            return Err(RawInvariant::new(
                "combining fast path 完成的操作不处于本轮认领状态",
            ));
        }
        {
            let record = self.record_mut(ticket)?;
            record.response = Some(outcome);
            record.state = OperationState::Completed;
        }
        self.claim = None;
        self.stats.executions += 1;
        Ok(())
    }

    /// 结束一轮：发布各成员 response、老化等待者并释放 claim 字。
    pub(crate) fn end_round(
        &mut self,
        slot: u32,
        executions: &[(MergeGroup, Vec<OperationOutcome>)],
    ) -> Result<RoundReport, RawInvariant> {
        if self.claim != Some(slot) {
            return Err(RawInvariant::new("combiner 轮次结束与打开的轮次不一致"));
        }
        let mut report = RoundReport {
            items: u32::try_from(self.round_claimed.len()).expect("认领记录数适配 u32"),
            // 报告口径是本轮认领记录携带的请求字节；轮次预算的记账含记录规范槽，两者不同。
            bytes: self
                .round_claimed
                .iter()
                .map(|ticket| self.chunks[ticket.id.chunk][ticket.id.slot].bytes)
                .sum(),
            cancellations: self.round_cancellations,
            ..RoundReport::default()
        };
        let mut completed: Vec<OpTicket> = Vec::with_capacity(self.round_claimed.len());
        for (group, outcomes) in executions {
            let members = self.groups_members(group);
            if usize::try_from(members).expect("组成员数适配 usize") != outcomes.len() {
                return Err(RawInvariant::new("combiner 执行结果与合并组成员数不一致"));
            }
            report.executions = report.executions.saturating_add(1);
            report.merged = report.merged.saturating_add(members - 1);
            for (ticket, outcome) in group.tickets.iter().zip(outcomes.iter()) {
                if !self.round_claimed.contains(ticket) || completed.contains(ticket) {
                    return Err(RawInvariant::new("combiner 执行了不属于本轮认领集合的操作"));
                }
                {
                    let record = self.record_mut(*ticket)?;
                    if record.state != OperationState::Claimed {
                        return Err(RawInvariant::new("combiner 试图完成未认领的操作"));
                    }
                    record.response = Some(*outcome);
                    record.state = OperationState::Completed;
                }
                completed.push(*ticket);
                report.woken = report.woken.saturating_add(1);
            }
        }
        if completed.len() != self.round_claimed.len() {
            return Err(RawInvariant::new("combiner 轮次未结清全部已认领操作"));
        }
        let timed_out = self.age_waiting_records(&mut report);
        report.woken = report.woken.saturating_add(timed_out);
        self.round_claimed.clear();
        self.round_charge = 0;
        self.round_cancellations = 0;
        self.claim = None;
        self.stats.executions += u64::from(report.executions);
        self.stats.merged_requests += u64::from(report.merged);
        Ok(report)
    }

    /// 读取 response；未发布时返回 `None`，槽位已回收时按 generation 拒绝。
    pub(crate) fn response(
        &self,
        ticket: OpTicket,
    ) -> Result<Option<OperationOutcome>, RawInvariant> {
        Ok(self.record(ticket)?.response)
    }

    /// 判断记录是否已经完成：response 已发布且槽位等待回收。
    ///
    /// 已取消的记录不是 completed：它仍挂在 combiner 链上，只能由下一轮摘除并回收。
    pub(crate) fn is_completed(&self, ticket: OpTicket) -> Result<bool, RawInvariant> {
        Ok(self.record(ticket)?.state == OperationState::Completed)
    }

    /// 回收一个已完成记录的槽位并推进 generation。
    ///
    /// 取消的记录不在这里回收：它仍然挂在 combiner 链上，必须由下一轮从链上摘除，
    /// 否则链会指向已被复用的槽位。
    pub(crate) fn release(&mut self, ticket: OpTicket) -> Result<(), RawInvariant> {
        if self.record(ticket)?.state != OperationState::Completed {
            return Err(RawInvariant::new("combining 操作尚未完成，不能回收槽位"));
        }
        self.recycle(ticket.id);
        Ok(())
    }

    /// 认领之前取消一条操作；已认领或已完成的操作不能取消。
    pub(crate) fn cancel(&mut self, ticket: OpTicket) -> Result<OperationOutcome, RawInvariant> {
        match self.record(ticket)?.state {
            OperationState::Published => {}
            _ => {
                return Err(RawInvariant::new("combining 操作已被认领，不能取消"));
            }
        }
        {
            let record = self.record_mut(ticket)?;
            record.response = Some(OperationOutcome::Cancelled);
            record.state = OperationState::Cancelled;
        }
        self.stats.cancellations += 1;
        Ok(OperationOutcome::Cancelled)
    }

    /// 取消某个 combiner 队列头部的等待记录，返回它的句柄与结局。
    ///
    /// 取消不改变链结构：记录仍留在链上，由下一轮 combiner 从链头摘除并回收，因此
    /// 链永远不会指向已经被复用的槽位。
    pub(crate) fn cancel_parked(
        &mut self,
        slot: u32,
    ) -> Result<(OpTicket, OperationOutcome), RawInvariant> {
        let index = usize::try_from(slot).expect("combiner 槽位适配 usize");
        let head = self
            .combiners
            .get(index)
            .and_then(|queue| queue.head)
            .ok_or_else(|| RawInvariant::new("combiner 队列没有等待中的操作"))?;
        let ticket = OpTicket {
            id: head,
            generation: self.chunks[head.chunk][head.slot].generation,
        };
        let outcome = self.cancel(ticket)?;
        Ok((ticket, outcome))
    }

    /// 判断普通空闲槽位是否已经用尽；world 据此先执行一次 `global-range-refill`。
    pub(crate) fn needs_refill(&self) -> bool {
        self.free.is_empty()
    }

    /// 补充一个 chunk；达到 chunk 上限时按不变量失败。
    pub(crate) fn grow_operation_pool(&mut self) -> Result<u32, RawInvariant> {
        let chunk_index = self.chunks.len();
        if u32::try_from(chunk_index).expect("chunk 数量适配 u32")
            >= self.limits.max_operation_chunks
        {
            return Err(RawInvariant::new("operation 记录池到达 chunk 上限"));
        }
        let items = self.chunk_items();
        let mut chunk = Vec::with_capacity(items);
        for slot in 0..items {
            let id = OpId {
                chunk: chunk_index,
                slot,
            };
            chunk.push(OperationRecord::free(1));
            if u32::try_from(slot).expect("槽位编号适配 u32") < self.general_slots() {
                self.free.push(id);
            } else {
                self.reserved.push(id);
            }
        }
        self.chunks.push(chunk.into_boxed_slice());
        self.stats.refills += 1;
        Ok(u32::try_from(items).expect("chunk 槽位数适配 u32"))
    }

    /// 返回记录池的队列数（combiner 槽位数）。
    pub(crate) fn queue_count(&self) -> u32 {
        u32::try_from(self.combiners.len()).expect("combiner 槽位数适配 u32")
    }

    /// 校验记录池的槽位分区不变量；只供确定性测试与契约核对调用。
    #[cfg(test)]
    pub(crate) fn verify_pool(&self) -> Result<(), RawInvariant> {
        let items = self.chunk_items();
        let expected = self.chunks.len() * items;
        let listed = self.free.len() + self.reserved.len();
        if listed > expected {
            return Err(RawInvariant::new("空闲槽位列表长于记录池"));
        }
        for id in self.free.iter().chain(self.reserved.iter()) {
            let Some(record) = self
                .chunks
                .get(id.chunk)
                .and_then(|chunk| chunk.get(id.slot))
            else {
                return Err(RawInvariant::new("空闲槽位列表引用了池外记录"));
            };
            if record.state != OperationState::Free
                || record.response.is_some()
                || record.mcs_next.is_some()
                || record.waited_rounds != 0
            {
                return Err(RawInvariant::new("空闲槽位仍带有上一次请求的残留状态"));
            }
        }
        for (chunk_index, chunk) in self.chunks.iter().enumerate() {
            for (slot_index, record) in chunk.iter().enumerate() {
                if record.is_available() {
                    continue;
                }
                let id = OpId {
                    chunk: chunk_index,
                    slot: slot_index,
                };
                if self.free.contains(&id) || self.reserved.contains(&id) {
                    return Err(RawInvariant::new("在飞记录出现在了空闲槽位列表中"));
                }
            }
        }
        Ok(())
    }

    /// 返回每个 chunk 的槽位数。
    fn chunk_items(&self) -> usize {
        usize::try_from(self.limits.record_chunk_items).expect("chunk 槽位数适配 usize")
    }

    /// 返回每个 chunk 里进入普通空闲列表的槽位数。
    fn general_slots(&self) -> u32 {
        self.limits.record_chunk_items - self.limits.refill_reserve_slots
    }

    /// 判断一个槽位是否是 chunk 尾部的 refill 预留槽位。
    fn is_reserved_slot(&self, id: OpId) -> bool {
        u32::try_from(id.slot).expect("槽位编号适配 u32") >= self.general_slots()
    }

    /// 记录一次请求进入平面。
    fn note_request(&mut self, tag: OperationTag) {
        self.stats.requests += 1;
        self.tag_requests[tag.index()] += 1;
    }

    /// 由空闲列表取一个槽位；`global-range-refill` 在池满时可以取预留槽位。
    fn reserve_slot(&mut self, tag: OperationTag) -> Result<OpId, RawInvariant> {
        if let Some(id) = self.free.pop() {
            return Ok(id);
        }
        if tag == OperationTag::GlobalRangeRefill
            && let Some(id) = self.reserved.pop()
        {
            return Ok(id);
        }
        Err(RawInvariant::new(
            "operation 记录池已满；调用方必须先执行 global-range-refill",
        ))
    }

    /// 初始化一个新占用的记录并返回它的句柄。
    fn init_record(&mut self, id: OpId, request: &OperationRequest) -> OpTicket {
        let record = &mut self.chunks[id.chunk][id.slot];
        record.tag = request.tag;
        record.state = OperationState::Published;
        record.owner = request.owner;
        record.merge_key = request.merge_key;
        record.descriptor = request.descriptor;
        record.scalar = request.scalar;
        record.bytes = request.bytes;
        record.response = None;
        record.waited_rounds = 0;
        record.mcs_next = None;
        OpTicket {
            id,
            generation: record.generation,
        }
    }

    /// 返回某个 combiner 的链状态；槽位越界按不变量失败。
    fn combiner_queue(&self, owner: u32) -> Result<&CombinerQueues, RawInvariant> {
        let index = usize::try_from(owner).expect("combiner 槽位适配 usize");
        self.combiners
            .get(index)
            .ok_or_else(|| RawInvariant::new("operation 引用了未登记的 combiner 槽位"))
    }

    /// 把记录挂到 combiner 链尾并置为已发布。
    fn push_parked(&mut self, id: OpId, owner: u32) -> Result<(), RawInvariant> {
        let index = usize::try_from(owner).expect("combiner 槽位适配 usize");
        if index >= self.combiners.len() {
            return Err(RawInvariant::new("operation 引用了未登记的 combiner 槽位"));
        }
        self.chunks[id.chunk][id.slot].state = OperationState::Published;
        self.chunks[id.chunk][id.slot].mcs_next = None;
        match self.combiners[index].tail {
            Some(tail) => {
                self.chunks[tail.chunk][tail.slot].mcs_next = Some(id);
                self.combiners[index].tail = Some(id);
            }
            None => {
                self.combiners[index].head = Some(id);
                self.combiners[index].tail = Some(id);
            }
        }
        Ok(())
    }

    /// 从某个 combiner 的链头摘下一个记录。
    fn pop_head(&mut self, index: usize) -> Option<OpId> {
        let head = self.combiners[index].head?;
        let next = self.chunks[head.chunk][head.slot].mcs_next;
        self.combiners[index].head = next;
        if next.is_none() {
            self.combiners[index].tail = None;
        }
        self.chunks[head.chunk][head.slot].mcs_next = None;
        Some(head)
    }

    /// 槽位回到空闲列表：清空请求状态并推进 generation。
    fn recycle(&mut self, id: OpId) {
        let reserved = self.is_reserved_slot(id);
        {
            let record = &mut self.chunks[id.chunk][id.slot];
            record.state = OperationState::Free;
            record.response = None;
            record.mcs_next = None;
            record.waited_rounds = 0;
            record.generation = record.generation.wrapping_add(1);
        }
        if reserved {
            self.reserved.push(id);
        } else {
            self.free.push(id);
        }
    }

    /// 给仍在链上等待的记录计一轮；达到超时轮数的记录以 `TimedOut` 收尾并摘链。
    ///
    /// 返回本轮因超时发布 response 的记录数。
    fn age_waiting_records(&mut self, report: &mut RoundReport) -> u32 {
        let timeout_rounds = self.limits.timeout_rounds;
        let mut woken = 0_u32;
        for index in 0..self.combiners.len() {
            let mut previous: Option<OpId> = None;
            let mut cursor = self.combiners[index].head;
            while let Some(id) = cursor {
                let next = self.chunks[id.chunk][id.slot].mcs_next;
                let waited = self.chunks[id.chunk][id.slot]
                    .waited_rounds
                    .saturating_add(1);
                self.chunks[id.chunk][id.slot].waited_rounds = waited;
                if waited >= timeout_rounds {
                    {
                        let record = &mut self.chunks[id.chunk][id.slot];
                        record.response = Some(OperationOutcome::TimedOut);
                        record.state = OperationState::Completed;
                        record.mcs_next = None;
                    }
                    match previous {
                        Some(prev) => {
                            self.chunks[prev.chunk][prev.slot].mcs_next = next;
                        }
                        None => {
                            self.combiners[index].head = next;
                        }
                    }
                    if self.combiners[index].tail == Some(id) {
                        self.combiners[index].tail = previous;
                    }
                    report.timeouts = report.timeouts.saturating_add(1);
                    woken = woken.saturating_add(1);
                } else {
                    previous = Some(id);
                }
                cursor = next;
            }
        }
        self.stats.timeouts += u64::from(report.timeouts);
        woken
    }

    /// 按 ticket 取记录；槽位越界或 generation 过期都按不变量失败。
    fn record(&self, ticket: OpTicket) -> Result<&OperationRecord, RawInvariant> {
        let record = self
            .chunks
            .get(ticket.id.chunk)
            .and_then(|chunk| chunk.get(ticket.id.slot))
            .ok_or_else(|| RawInvariant::new("combining ticket 引用了记录池外的槽位"))?;
        if record.generation != ticket.generation {
            return Err(RawInvariant::new("combining ticket 属于已被回收的记录槽"));
        }
        Ok(record)
    }

    /// 按 ticket 取可变记录；校验与 `record` 相同。
    fn record_mut(&mut self, ticket: OpTicket) -> Result<&mut OperationRecord, RawInvariant> {
        let record = self
            .chunks
            .get_mut(ticket.id.chunk)
            .and_then(|chunk| chunk.get_mut(ticket.id.slot))
            .ok_or_else(|| RawInvariant::new("combining ticket 引用了记录池外的槽位"))?;
        if record.generation != ticket.generation {
            return Err(RawInvariant::new("combining ticket 属于已被回收的记录槽"));
        }
        Ok(record)
    }

    /// 操作状态名；供世界与确定性测试断言状态机位置。
    pub(crate) fn state_name(&self, ticket: OpTicket) -> Result<&'static str, RawInvariant> {
        Ok(self.record(ticket)?.state.name())
    }
}
