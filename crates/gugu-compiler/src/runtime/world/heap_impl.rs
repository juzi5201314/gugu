//! RawWorld 上的 LocalHeap 接入：按契约配置 arena、分配与读写 managed 对象、pin 与分代 cycle。
//!
//! arena 的物理页仍走 extent 层：每个 32 KiB block 由 provider 提交，因此 managed heap 不进入
//! `OwnerAccounting`，`runtime_committed_bytes` 与 `spec/runtime.md` 的口径保持不变。指针是
//! arena 内的直接地址（`LOCAL_DIRECT`），根是模型根槽数组，跨 owner 引用在本阶段是不变量失败。

use super::super::barrier::{BarrierFlushReason, BarrierSite};
use super::super::extent::class_for_bytes;
use super::super::gc_metadata_schema::GcRootKindV1;
use super::super::gc_metadata_section::{GcRuntimeMetadata, decode_sections};
use super::super::local_heap::{
    BlockRef, CycleReport, GENERATION_OLD, HeapArenaKind, HeapCounters, HeapError, HeapObject,
    LocalHeap, ManagedBlockId,
};
use super::super::local_heap_schema::{HEAP_BLOCKS_PER_ARENA, LocalHeapRuntimeContract};
use super::super::slab::{MemoryDomainId, OwnerToken, RawInvariant};
use super::RawWorld;
use super::shared_heap_impl;

/// managed 分配位置；与 `PlacementKind` 的 managed 子集一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPlacement {
    /// 当前 turn 私有的 nursery 快路径。
    Nursery,
    /// 需要在 old region 分配的对象；old slow edge 必须真正可达。
    Old,
    /// 需要在 old region 固定的对象。
    Pinned,
    /// resource lease 对象；不进入 nursery。
    Resource,
    /// 跨 owner handle；本阶段的拒绝路径。
    SharedHeap,
}

impl ManagedPlacement {
    /// 返回 placement 名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Nursery => "local-heap",
            Self::Old => "old",
            Self::Pinned => "pinned",
            Self::Resource => "resource",
            Self::SharedHeap => "shared-heap",
        }
    }

    /// 返回目标 arena 类别。
    const fn arena(self) -> HeapArenaKind {
        match self {
            Self::Nursery | Self::SharedHeap => HeapArenaKind::Nursery,
            Self::Old => HeapArenaKind::Old,
            Self::Pinned => HeapArenaKind::Pinned,
            Self::Resource => HeapArenaKind::Resource,
        }
    }
}

/// 一个全局 managed arena 的登记项。
///
/// descriptor 由世界级稠密表分配，生命周期内不复用：card table、mark ticket 与 block 身份
/// 都按它索引。管理权转移只更新 `manager`，payload 仍由这里的 extent arena 定位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManagedArena {
    pub(crate) descriptor: u32,
    pub(crate) heap_owner: u32,
    pub(crate) heap_slot: usize,
    pub(crate) extent_arena: u32,
    pub(crate) base: u64,
    pub(crate) kind: HeapArenaKind,
    pub(crate) manager: OwnerToken,
}

/// 把 `HeapError` 转换成本平面的不变量错误。
pub(super) fn heap_error(error: HeapError) -> RawInvariant {
    error.into_invariant()
}

/// SharedHeap 参照实现失败在运行时平面上就是不变量失败：没有可恢复分支。
pub(super) fn shared_heap_error(error: super::super::shared_heap::SharedHeapError) -> RawInvariant {
    RawInvariant::new(error.message())
}

impl RawWorld {
    /// 分配并发布一个共享对象：payload 建立在 SharedHeap，block 身份登记在世界级 registry。
    ///
    /// 返回 stable handle；`block`/`offset` 由 registry 预留，因此共享 block descriptor 与
    /// LocalHeap arena descriptor 不会重合，payload 记录里保存的也是同一个世界级身份。
    pub(crate) fn allocate_shared_object(
        &mut self,
        owner: u32,
        bytes: u32,
    ) -> Result<super::super::shared_heap_schema::SharedHandle, RawInvariant> {
        let (block, block_offset) = self.shared_registry.reserve(owner, bytes)?;
        let descriptor = block.id.arena();
        let payload = self
            .shared_heap_mut()?
            .allocate_payload(owner, descriptor, block_offset, bytes)
            .map_err(shared_heap_error)?;
        let handle = self
            .shared_heap_mut()?
            .resolve_payload(payload)
            .map_err(shared_heap_error)?;
        let entry =
            self.shared_registry
                .insert(handle, payload.id, owner, bytes, block, block_offset)?;
        // 共享 block 与 LocalHeap arena 走同一套 card table 与 edge 记账：manager 是 payload
        // 的 owner，因此别的 owner 写进这块 payload 时卡确实会以 CardMark 消息投给它。
        let manager = self.token(owner);
        self.barrier_mut().register_arena(
            u64::from(descriptor),
            manager,
            block.generation,
            u64::from(super::super::gc_metadata_contract::GC_BLOCK_BYTES),
        )?;
        debug_assert_eq!(entry.block, block);
        Ok(handle)
    }

    /// 建立一个覆盖多次字段访问的共享 access guard；返回非零 token。
    ///
    /// 一个 token 对应 LIR 的一个 `SharedAccessBegin`/`SharedAccessEnd` 区间：区间内的读写共用
    /// 同一个已解析身份，因此 guard 结束前捕获的 payload 始终有效，搬迁不会让同一区间内的两次
    /// 读落到不同 payload 上。token 只在 world 生命周期内单调分配，绝不复用。
    pub(crate) fn begin_shared_access(
        &mut self,
        handle: super::super::shared_heap_schema::SharedHandle,
    ) -> Result<u32, RawInvariant> {
        let token = self.next_shared_access_token()?;
        let access = {
            let heap = self.shared_heap_mut()?;
            heap.begin_access(token, handle)
                .map_err(shared_heap_error)?;
            heap.resolve_access(token).map_err(shared_heap_error)?
        };
        let index = usize::try_from(token - 1).expect("token 下标适配 usize");
        if index >= self.shared_accesses.len() {
            self.shared_accesses.resize(index + 1, None);
        }
        self.shared_accesses[index] = Some(access);
        Ok(token)
    }

    /// 在 active guard 内读取共享 payload 的一个 64-bit 字段。
    pub(crate) fn load_shared_field_with(
        &mut self,
        token: u32,
        offset: u32,
    ) -> Result<u64, RawInvariant> {
        let access = self.shared_access(token)?;
        let bytes = self
            .shared_heap_mut()?
            .load(access, offset, 8)
            .map_err(shared_heap_error)?;
        let raw: [u8; 8] = bytes
            .try_into()
            .map_err(|_| RawInvariant::new("共享字段读取字节数不是 8"))?;
        Ok(u64::from_le_bytes(raw))
    }

    /// 在 active guard 内写入共享 payload 的一个 64-bit 字段。
    pub(crate) fn store_shared_field_with(
        &mut self,
        token: u32,
        offset: u32,
        value: u64,
    ) -> Result<(), RawInvariant> {
        let access = self.shared_access(token)?;
        self.shared_heap_mut()?
            .store(access, offset, &value.to_le_bytes())
            .map(|_| ())
            .map_err(shared_heap_error)
    }

    /// 结束 access guard 并释放 token。
    pub(crate) fn end_shared_access(&mut self, token: u32) -> Result<(), RawInvariant> {
        let access = self.shared_access(token)?;
        self.shared_heap_mut()?
            .end_access(access.token)
            .map_err(shared_heap_error)?;
        let index = usize::try_from(token - 1).expect("token 下标适配 usize");
        self.shared_accesses[index] = None;
        Ok(())
    }

    /// 取回一个 active guard 的已解析身份；token 未建立或已结束时失败。
    fn shared_access(
        &self,
        token: u32,
    ) -> Result<super::super::shared_heap::SharedAccessToken, RawInvariant> {
        let offset = token
            .checked_sub(1)
            .ok_or_else(|| RawInvariant::new("共享 access token 必须非零"))?;
        let index = usize::try_from(offset).expect("token 下标适配 usize");
        self.shared_accesses
            .get(index)
            .copied()
            .flatten()
            .ok_or_else(|| RawInvariant::new("共享 access token 未建立或已结束"))
    }

    /// 把一个 payload lease 提升为非移动 lease；它阻止 forwarding，直到 `unpin_shared_handle`。
    pub(crate) fn pin_shared_handle(
        &mut self,
        handle: super::super::shared_heap_schema::SharedHandle,
    ) -> Result<(), RawInvariant> {
        self.shared_heap_mut()?
            .pin(handle)
            .map_err(shared_heap_error)
    }

    /// 释放一个共享 pin lease；没有未结清 lease 时失败。
    pub(crate) fn unpin_shared_handle(
        &mut self,
        handle: super::super::shared_heap_schema::SharedHandle,
    ) -> Result<(), RawInvariant> {
        self.shared_heap_mut()?
            .unpin(handle)
            .map_err(shared_heap_error)
    }

    /// 写入一个共享 payload 的字段，并接入与本地字段同一条 hybrid barrier 记账路径。
    ///
    /// 顺序与 `store_managed_field` 一致：先读旧值，再算 old/new 目标 block，再写 payload，
    /// 最后执行 shade/card/edge 记账。区别有两点：旧值读取与新值写入共用一个 access guard，
    /// 因此搬迁不会让「读到的旧值」与「写入的目标」落在不同 payload 上；目标 block 是 registry
    /// 的世界级共享 block，因此 card 键与 edge 端点既能区分 owner，也不会与 LocalHeap 撞车。
    ///
    /// 失败路径不结束 guard：共享平面的失败都是不变量失败，世界已经不可用，泄漏的 guard 不会
    /// 被观察到；成功路径保证 guard 在一个函数内精确结清。
    pub(crate) fn store_shared_managed_field(
        &mut self,
        owner: u32,
        processor: usize,
        handle: super::super::shared_heap_schema::SharedHandle,
        offset: u32,
        value: u64,
        value_owner: Option<u32>,
    ) -> Result<(), RawInvariant> {
        let record = *self.shared_payload_block(handle)?;
        let descriptor = u64::from(record.block.id.arena());
        let card_offset = u64::from(record.block_offset)
            .checked_add(u64::from(offset))
            .ok_or_else(|| RawInvariant::new("共享字段 card 偏移溢出"))?;
        let token = self.begin_shared_access(handle)?;
        let old = self.load_shared_field_with(token, offset)?;
        let old_target = if old == 0 {
            None
        } else {
            let old_owner = self.owner_of(old)?;
            Some(self.managed_block_ref(old_owner, old)?)
        };
        let new_target = if value == 0 {
            None
        } else {
            let new_owner = value_owner
                .ok_or_else(|| RawInvariant::new("共享字段的 managed 新值缺少所属 owner"))?;
            Some(self.managed_block_ref(new_owner, value)?)
        };
        let new_in_nursery = match (value, value_owner) {
            (0, _) => false,
            (_, Some(new_owner)) => self.heap(new_owner)?.in_nursery(value),
            (_, None) => false,
        };
        let arena_generation = self
            .barrier()
            .table(descriptor)
            .map(|table| table.arena_generation())
            .ok_or_else(|| RawInvariant::new("共享 block 缺 card table 登记"))?;
        self.store_shared_field_with(token, offset, value)?;
        self.end_shared_access(token)?;
        let site = BarrierSite {
            arena_descriptor: descriptor,
            arena_generation,
            offset: card_offset,
            cycle_epoch: self.cycle_epoch,
            source: record.block,
            old: old_target,
            new: new_target,
            new_in_nursery,
            // 共享 payload 是稳定存储：它不随 minor 搬迁，因此永远按 old generation 记账。
            owner_old: true,
            marking: self.mark_active,
            stack_grey: true,
        };
        let outcome = self.perform_barrier(processor, site)?;
        if outcome.shaded_old && old != 0 {
            // Yuasa deletion：被覆盖掉的旧引用仍可能只有这一条通路，必须染灰后再失去它。
            let old_owner = self.owner_of(old)?;
            self.shade_address(old_owner, old)?;
        }
        if outcome.shaded_new && value != 0 {
            // Dijkstra insertion：新引用指向的对象必须进入灰色集合。
            let new_owner = self.owner_of(value)?;
            self.shade_address(new_owner, value)?;
        }
        if let Some(reason) = outcome.flush {
            // 字段写入已经发生，这里只补记账：先 flush 再把未记入的边变更原样重放。
            self.flush_barrier(owner, processor, reason)?;
            self.barrier_mut()
                .replay_pending_edges(processor, &outcome.pending_edges);
        }
        Ok(())
    }

    /// 返回下一个共享 access token；token 在 world 生命周期内唯一。
    fn next_shared_access_token(&mut self) -> Result<u32, RawInvariant> {
        self.shared_access_token = self
            .shared_access_token
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("shared access token 溢出"))?;
        Ok(self.shared_access_token)
    }

    /// 返回一个 handle 的共享 block 登记项；handle 过期或未登记时失败。
    pub(crate) fn shared_payload_block(
        &self,
        handle: super::super::shared_heap_schema::SharedHandle,
    ) -> Result<&shared_heap_impl::SharedPayloadBlock, RawInvariant> {
        self.shared_registry
            .get(handle)
            .ok_or_else(|| RawInvariant::new("共享 payload 未在世界 registry 登记"))
    }

    /// 返回共享 block 登记表的只读视图。
    pub(crate) fn shared_registry(&self) -> &shared_heap_impl::SharedRegistry {
        &self.shared_registry
    }
}

impl RawWorld {
    /// 按已验证契约配置每个 owner 的 LocalHeap 与 mark 平面。
    ///
    /// 两段配置必须一起完成：credit 池上界由 `policy`（shard × batch item）与根槽数共同推导，
    /// 单独的 `LocalHeapRuntimeContract` 无法得到合法 mark 契约，因此入口只接受整体契约。
    pub(crate) fn configure_gc(
        &mut self,
        raw: &super::super::RuntimeRawContractV1,
    ) -> Result<(), RawInvariant> {
        let contract = raw.local_heap();
        let types = decode_sections(
            &raw.gc_metadata().type_section,
            &raw.gc_metadata().metadata_section,
        )
        .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        let block_class = class_for_bytes(u64::from(contract.block_bytes()))
            .ok_or_else(|| RawInvariant::new("32 KiB block 不在 extent 阶梯中"))?;
        let owners = self.owners.len();
        self.local_heaps = Some((0..owners).map(|_| LocalHeap::new(contract)).collect());
        self.local_heap_contract = Some(contract.clone());
        // SharedHeap 表在配置期建立：handle 身份与 payload 记录只按已验证契约初始化，
        // 之后的 allocate/resolve/forward 都是幂等的表操作。
        let shared_contract = raw.shared_heap();
        if shared_contract.schema() != super::super::shared_heap_schema::SHARED_HEAP_SCHEMA {
            return Err(RawInvariant::new("SharedHeap 契约 schema 与运行时不一致"));
        }
        self.shared_heap = Some(super::super::shared_heap::SharedHeap::new(
            0,
            shared_contract,
        ));
        self.shared_heap_contract = Some(shared_contract.clone());
        self.gc_types = Some(types);
        self.managed_roots.clear();
        self.managed_root_kinds.clear();
        self.cycle_epoch = 0;
        self.heap_block_class = block_class;
        self.configure_mark(raw.mark())?;
        self.edges = Some(super::super::edge::EdgePlane::new());
        let edge = raw.edge();
        self.verify_edge_contract(edge)?;
        self.edge_contract = Some(edge.clone());
        self.configure_candidates();
        Ok(())
    }

    /// 校验运行时实现与 edge 契约逐项一致。
    ///
    /// 契约自身的 `verify` 只保证契约内部自洽；这里核对的是**运行时实现**：候选相位目录、
    /// block 状态目录、候选决议 schema 与 `job_of_block` 的保留取值。不一致必须在配置期失败，
    /// 否则错配要等到第一个 cycle 才以「相位名对不上」或「job 编号撞车」的形式爆出来。
    pub(crate) fn verify_edge_contract(
        &self,
        edge: &super::super::edge_schema::EdgeRuntimeContract,
    ) -> Result<(), RawInvariant> {
        use super::super::candidate_schema::{CANDIDATE_SCHEMA, CandidatePhase};
        use super::super::local_heap_schema::HEAP_BLOCK_STATE_NAMES;
        if edge.schema() != super::super::edge_schema::EDGE_SCHEMA {
            return Err(RawInvariant::new("edge 契约 schema 与运行时不一致"));
        }
        if edge.phases.len() != CandidatePhase::ALL.len() {
            return Err(RawInvariant::new("edge 契约的相位数量与运行时不一致"));
        }
        for (index, phase) in CandidatePhase::ALL.iter().enumerate() {
            if edge.phases[index] != phase.name() {
                return Err(RawInvariant::new("edge 契约的相位名与运行时不一致"));
            }
        }
        if edge.states.len() != HEAP_BLOCK_STATE_NAMES.len() {
            return Err(RawInvariant::new(
                "edge 契约的 block 状态数量与运行时不一致",
            ));
        }
        for (index, name) in HEAP_BLOCK_STATE_NAMES.iter().enumerate() {
            if edge.states[index] != *name {
                return Err(RawInvariant::new("edge 契约的 block 状态名与运行时不一致"));
            }
        }
        if edge.candidate_schema() != CANDIDATE_SCHEMA {
            return Err(RawInvariant::new("候选决议 schema 与运行时不一致"));
        }
        if edge.candidate_quantum() == 0 {
            return Err(RawInvariant::new("候选 quantum 不得为零"));
        }
        if edge.no_job() != super::super::edge_schema::EDGE_NO_JOB {
            return Err(RawInvariant::new("job_of_block 保留取值与运行时不一致"));
        }
        if edge.trace_executor_revision() == 0 {
            return Err(RawInvariant::new("精确追踪执行器 revision 不得为零"));
        }
        // scratch 预留与 shade 上界的关系由契约自身的 `verify` 逐值核对：没有 shade 站点的程序
        // 合法地拥有零预留，因此这里不再重复（重复会引入“有站点就必须有预留”的错误假设）。
        Ok(())
    }

    /// 返回已校对的 edge 契约。未配置时失败。
    pub(crate) fn edge_contract(
        &self,
    ) -> Result<&super::super::edge_schema::EdgeRuntimeContract, RawInvariant> {
        self.edge_contract
            .as_ref()
            .ok_or_else(|| RawInvariant::new("edge 契约尚未配置"))
    }

    /// 返回 LocalHeap 是否已配置。
    pub(crate) fn local_heap_configured(&self) -> bool {
        self.local_heaps.is_some()
    }

    pub(super) fn heap(&self, owner: u32) -> Result<&LocalHeap, RawInvariant> {
        self.local_heaps
            .as_ref()
            .and_then(|heaps| heaps.get(owner as usize))
            .ok_or_else(|| RawInvariant::new("LocalHeap 未按契约配置"))
    }

    pub(super) fn heap_mut(&mut self, owner: u32) -> Result<&mut LocalHeap, RawInvariant> {
        self.local_heaps
            .as_mut()
            .and_then(|heaps| heaps.get_mut(owner as usize))
            .ok_or_else(|| RawInvariant::new("LocalHeap 未按契约配置"))
    }

    pub(super) fn types(&self) -> Result<&GcRuntimeMetadata, RawInvariant> {
        self.gc_types
            .as_ref()
            .ok_or_else(|| RawInvariant::new("运行时可读 GC 类型表未解码"))
    }

    /// 返回某个 owner 的累计计数。
    pub(crate) fn managed_counters(&self, owner: u32) -> Result<HeapCounters, RawInvariant> {
        Ok(self.heap(owner)?.counters())
    }

    /// 返回全部 owner 的 live heap 字节数。
    pub(crate) fn managed_live_bytes(&self) -> u64 {
        self.local_heaps.as_ref().map_or(0, |heaps| {
            heaps
                .iter()
                .map(|heap| heap.counters().live_bytes)
                .fold(0_u64, u64::saturating_add)
        })
    }

    /// 返回 nursery 自上次 minor 起的分配字节数。
    pub(crate) fn nursery_bytes(&self, owner: u32) -> Result<u64, RawInvariant> {
        Ok(self.heap(owner)?.nursery_bytes())
    }

    /// 返回按契约配置的 SharedHeap 表；未配置时拒绝。
    pub(crate) fn shared_heap(
        &self,
    ) -> Result<&super::super::shared_heap::SharedHeap, RawInvariant> {
        self.shared_heap
            .as_ref()
            .ok_or_else(|| RawInvariant::new("SharedHeap 未按契约配置"))
    }

    /// 返回按契约配置的 SharedHeap 表（可变）。
    pub(crate) fn shared_heap_mut(
        &mut self,
    ) -> Result<&mut super::super::shared_heap::SharedHeap, RawInvariant> {
        self.shared_heap
            .as_mut()
            .ok_or_else(|| RawInvariant::new("SharedHeap 未按契约配置"))
    }

    /// 返回 SharedHeap 契约快照；未配置时拒绝。
    pub(crate) fn shared_heap_contract(
        &self,
    ) -> Result<&super::super::shared_heap_schema::SharedHeapRuntimeContract, RawInvariant> {
        self.shared_heap_contract
            .as_ref()
            .ok_or_else(|| RawInvariant::new("SharedHeap 契约未配置"))
    }

    /// 分配一个 managed 对象。
    pub(crate) fn allocate_managed(
        &mut self,
        owner: u32,
        type_index: u32,
        payload_bytes: u64,
        placement: ManagedPlacement,
    ) -> Result<u64, RawInvariant> {
        if placement == ManagedPlacement::SharedHeap {
            return Err(RawInvariant::new(
                "SharedHeap placement 需要跨 owner handle，不属于 LocalHeap 分配路径",
            ));
        }
        let contract = self
            .local_heap_contract
            .clone()
            .ok_or_else(|| RawInvariant::new("LocalHeap 未按契约配置"))?;
        let footprint = crate::runtime::local_heap::OBJECT_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or_else(|| RawInvariant::new("对象 footprint 溢出"))?;
        let block_bytes = u64::from(contract.block_bytes());
        let need = u32::try_from(footprint.div_ceil(block_bytes))
            .map_err(|_| RawInvariant::new("对象 block 数超过 u32"))?;
        // footprint 超过单个 Immix block 的请求走 non-moving large slow path。
        let kind = if need > 1 {
            HeapArenaKind::Large
        } else {
            placement.arena()
        };
        let arena = self.ensure_heap_space(owner, kind, need, need > 1)?;
        let align = kind_align(kind, &contract);
        let mut attempt = self
            .heap_mut(owner)?
            .allocate(arena, type_index, payload_bytes, align);
        if kind == HeapArenaKind::Nursery && matches!(attempt, Err(HeapError::NoCapacity)) {
            // 当前 TLAB span 在分配过程中用尽：重新取 span（必要时提交新 block）后重试一次。
            let arena = self.ensure_heap_space(owner, kind, need, false)?;
            attempt = self
                .heap_mut(owner)?
                .allocate(arena, type_index, payload_bytes, align);
        }
        let address = attempt.map_err(heap_error)?;
        // incremental marking 期间新对象必须进入本轮 cycle 的灰色集合：否则它在 cycle 收尾时
        // 仍未标记，会被 sweep 当成垃圾回收。
        self.shade_address(owner, address)?;
        Ok(address)
    }

    /// 读取对象 header 描述。
    pub(crate) fn managed_object(&self, address: u64) -> Result<HeapObject, RawInvariant> {
        let owner = self.owner_of(address)?;
        self.heap(owner)?.object_at(address).map_err(heap_error)
    }

    /// 读写对象 payload 中的 managed 字段。
    pub(crate) fn load_managed_field(
        &self,
        address: u64,
        offset: u64,
    ) -> Result<u64, RawInvariant> {
        let owner = self.owner_of(address)?;
        self.heap(owner)?.field(address, offset).map_err(heap_error)
    }

    /// 写入对象 payload 中的 managed 字段，并执行 hybrid barrier。
    ///
    /// source、old 与 new 三个身份都在写入之前从真实 heap metadata 解析：source 来自被写对象
    /// 自身，old 来自被覆盖的 word，new 来自新值。null 记为 `None`，合法 interior pointer 先
    /// 回表到原对象。card 键用字段的 arena 内偏移，而不是 payload 字段偏移。
    pub(crate) fn store_managed_field(
        &mut self,
        owner: u32,
        processor: usize,
        address: u64,
        offset: u64,
        value: u64,
    ) -> Result<(), RawInvariant> {
        let object = self.heap(owner)?.object_at(address).map_err(heap_error)?;
        let old = self
            .heap(owner)?
            .field(address, offset)
            .map_err(heap_error)?;
        let source = self.heap(owner)?.block_ref(address).map_err(heap_error)?;
        let (_, descriptor) = self.heap(owner)?.arena_of(address).map_err(heap_error)?;
        let card_offset = self
            .heap(owner)?
            .field_offset(address, offset)
            .map_err(heap_error)?;
        let old_target = if old == 0 {
            None
        } else {
            let old_owner = self.owner_of(old)?;
            Some(self.managed_block_ref(old_owner, old)?)
        };
        let new_target = if value == 0 {
            None
        } else {
            let new_owner = self.owner_of(value)?;
            Some(self.managed_block_ref(new_owner, value)?)
        };
        let new_in_nursery = match new_target {
            Some(_) => {
                let new_owner = self.owner_of(value)?;
                self.heap(new_owner)?.in_nursery(value)
            }
            None => false,
        };
        let arena_generation = self
            .barrier()
            .table(descriptor)
            .map(|table| table.arena_generation())
            .ok_or_else(|| RawInvariant::new("managed arena 缺 card table 登记"))?;
        self.heap_mut(owner)?
            .set_field(address, offset, value)
            .map_err(heap_error)?;
        let site = BarrierSite {
            arena_descriptor: descriptor,
            arena_generation,
            offset: card_offset,
            cycle_epoch: self.cycle_epoch,
            source,
            old: old_target,
            new: new_target,
            new_in_nursery,
            owner_old: object.generation >= GENERATION_OLD,
            marking: self.mark_active,
            stack_grey: true,
        };
        let outcome = self.perform_barrier(processor, site)?;
        if outcome.shaded_old && old != 0 {
            // Yuasa deletion：被覆盖掉的旧引用仍可能只有这一条通路，必须染灰后再失去它。
            let old_owner = self.owner_of(old)?;
            self.shade_address(old_owner, old)?;
        }
        if outcome.shaded_new && value != 0 {
            // Dijkstra insertion：新引用指向的对象必须进入灰色集合，否则本轮 cycle 会漏掉它。
            let new_owner = self.owner_of(value)?;
            self.shade_address(new_owner, value)?;
        }
        if let Some(reason) = outcome.flush {
            // 字段写入已经发生，这里只补记账：先 flush 再把未记入的边变更原样重放。
            self.flush_barrier(owner, processor, reason)?;
            self.barrier_mut()
                .replay_pending_edges(processor, &outcome.pending_edges);
        }
        Ok(())
    }

    /// 把全部「lease 归零且非空」的 managed block 登记为候选。
    ///
    /// incoming lease 归零本身就是候选的产生条件，因此 major cycle 不能只依赖 mutator 的 dirty
    /// 集合：那会把「已经没有外部引用的块」留到下一次写入才被发现。这里按真实事实筛：状态为
    /// `active`、四类 lease 全为零、非 nursery、且块内仍有对象（空块直接跳过，避免每轮重复
    /// 建组—释放空块），并排除块内仍有本轮标记对象的块。返回登记的块数。
    ///
    /// 调用时序固定在 `finish_mark_cycle` 之后、`sweep_owner` 之前：标记 gate 读的是**本轮**
    /// cycle 的标记位，而 arena 的 `mark_epoch` 只在 `begin_mark_cycle` 前进、收尾与 sweep 都不
    /// 推进它，因此此刻读到的仍是本轮结果。提前到收尾之前会读到尚未收敛的中间标记，退到 sweep
    /// 之后则无法区分「本轮清空的块」与「本来就没有对象的块」。
    pub(crate) fn seed_zero_lease_candidates(&mut self) -> Result<u64, RawInvariant> {
        if self.candidates.is_none() {
            return Ok(0);
        }
        let arenas: Vec<(u32, u32)> = self
            .managed_arenas
            .iter()
            .map(|arena| (arena.descriptor, arena.heap_owner))
            .collect();
        let mut seeded = 0_u64;
        for (descriptor, heap_owner) in arenas {
            let blocks = {
                let heap = self.heap(heap_owner)?;
                heap.committed_blocks_of(u64::from(descriptor))
                    .map_err(heap_error)?
            };
            for index in blocks {
                let id = ManagedBlockId::new(descriptor, index)?;
                let (record, kind, live_lines) = {
                    let heap = self.heap(heap_owner)?;
                    (
                        heap.block_record(id).map_err(heap_error)?,
                        heap.block_arena_kind(id).map_err(heap_error)?,
                        heap.block_live_lines(id).map_err(heap_error)?,
                    )
                };
                if record.state != 0
                    || kind == HeapArenaKind::Nursery
                    || live_lines == 0
                    || record.incoming_leases != 0
                    || record.allocator_leases != 0
                    || record.scanner_leases != 0
                    || record.evacuation_leases != 0
                {
                    continue;
                }
                // 块内仍有本 epoch 标记的对象说明它确实活着：不该进入候选，否则每轮 cycle 都会
                // 建一个必然被 `validate` 退回的 job。零标记的块才是「本轮 sweep 会清空」的块。
                {
                    let heap = self.heap(heap_owner)?;
                    if heap.block_marked_objects(id).map_err(heap_error)? != 0 {
                        continue;
                    }
                }
                if self.candidate_job_of(id)?.is_some() {
                    continue;
                }
                self.note_candidate_dirty(id)?;
                seeded = seeded.saturating_add(1);
            }
        }
        Ok(seeded)
    }

    /// 把一个 managed 对象染灰：它进入所属 owner 的 mark worklist。
    ///
    /// 只在 mark cycle 打开时生效：没有进行中的 cycle 时不存在灰色集合，写屏障与分配都不需要
    /// 染色（下一次 cycle 会从根与 card 重新开始）。去重由 mark pass 的标记位负责。
    pub(super) fn shade_address(&mut self, owner: u32, address: u64) -> Result<(), RawInvariant> {
        if !self.mark_active {
            return Ok(());
        }
        let object = self.heap(owner)?.object_at(address).map_err(heap_error)?;
        self.mark_worklists[owner as usize].push(object.object_start);
        Ok(())
    }

    pub(crate) fn pin_managed(
        &mut self,
        owner: u32,
        address: u64,
    ) -> Result<(u64, u32), RawInvariant> {
        self.ensure_heap_space(owner, HeapArenaKind::Pinned, 1, false)?;
        let types = self.types()?.clone();
        let mut roots = std::mem::take(&mut self.managed_roots);
        let result = self
            .heap_mut(owner)?
            .pin(address, &types, &mut roots)
            .map_err(heap_error);
        self.managed_roots = roots;
        result
    }

    /// 取消 pin。
    pub(crate) fn unpin_managed(&mut self, owner: u32, address: u64) -> Result<u32, RawInvariant> {
        self.heap_mut(owner)?.unpin(address).map_err(heap_error)
    }

    /// 登记一个 managed 根槽；返回槽位下标。
    pub(crate) fn register_managed_root(
        &mut self,
        kind: GcRootKindV1,
        type_index: u32,
    ) -> Result<u32, RawInvariant> {
        let slot = u32::try_from(self.managed_roots.len())
            .map_err(|_| RawInvariant::new("根槽数量超过 u32"))?;
        self.managed_roots.push(0);
        self.managed_root_kinds.push((kind as u32, type_index));
        Ok(slot)
    }

    /// 写入一个根槽。
    pub(crate) fn set_managed_root(&mut self, slot: u32, value: u64) -> Result<(), RawInvariant> {
        let entry = self
            .managed_roots
            .get_mut(slot as usize)
            .ok_or_else(|| RawInvariant::new("根槽下标越界"))?;
        *entry = value;
        Ok(())
    }

    /// 读取一个根槽。
    pub(crate) fn managed_root(&self, slot: u32) -> Result<u64, RawInvariant> {
        self.managed_roots
            .get(slot as usize)
            .copied()
            .ok_or_else(|| RawInvariant::new("根槽下标越界"))
    }

    /// 返回根槽数量。
    pub(crate) fn managed_root_count(&self) -> u32 {
        u32::try_from(self.managed_roots.len()).expect("根槽数量适配 u32")
    }

    /// 触发一次 minor cycle。
    pub(crate) fn collect_minor(&mut self, owner: u32) -> Result<CycleReport, RawInvariant> {
        let nursery = self.heap(owner)?.nursery_bytes();
        let block_bytes = u64::from(self.heap_contract()?.block_bytes());
        let blocks = u32::try_from(nursery.div_ceil(block_bytes)).expect("block 数适配 u32");
        self.ensure_heap_space(owner, HeapArenaKind::Old, blocks.max(1), false)?;
        self.ensure_heap_space(owner, HeapArenaKind::Pinned, 1, false)?;
        self.flush_remembered_set(owner)?;
        let dirty = self.drain_dirty_cards(owner)?;
        let types = self.types()?.clone();
        let mut roots = std::mem::take(&mut self.managed_roots);
        let result = self
            .heap_mut(owner)?
            .collect_minor(&types, &mut roots, &dirty);
        self.managed_roots = roots;
        // cycle 边界前先按真实搬迁重建 block 对计数：旧目标 block 的入边必须随对象搬到新目标，
        // 否则候选判定会按已经不存在的 block 做试验删除。
        let relocations = self.heap_mut(owner)?.take_relocations();
        self.rebuild_edges_after_relocation(relocations)?;
        self.advance_cycle_epoch(owner)?;
        result.map_err(heap_error)
    }

    /// 返回 managed 地址所属的 owner；跨 owner 引用必须由 world 级 mark pass 解析。
    pub(crate) fn owner_of(&self, address: u64) -> Result<u32, RawInvariant> {
        let heaps = self
            .local_heaps
            .as_ref()
            .ok_or_else(|| RawInvariant::new("LocalHeap 未按契约配置"))?;
        for (owner, heap) in heaps.iter().enumerate() {
            if heap.contains(address) {
                return Ok(u32::try_from(owner).expect("owner 下标适配 u32"));
            }
        }
        Err(RawInvariant::new(
            "managed 地址不属于任何已配置的 LocalHeap",
        ))
    }

    pub(super) fn heap_contract(&self) -> Result<&LocalHeapRuntimeContract, RawInvariant> {
        self.local_heap_contract
            .as_ref()
            .ok_or_else(|| RawInvariant::new("LocalHeap 未按契约配置"))
    }

    /// 冲刷全部 processor 的 barrier buffer，把 card 键写进 arena card table。
    ///
    /// 这是 minor/major cycle 的第一步：remembered set 必须先落到 arena 再被扫描。
    pub(crate) fn flush_remembered_set(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        let processors = self.barrier.processor_count();
        let mut batches = 0;
        for processor in 0..processors {
            batches += self.flush_barrier(owner, processor, BarrierFlushReason::MinorStop)?;
        }
        Ok(batches)
    }

    /// 返回尚未消费的 dirty card 数量（arena 下标, card 序号）；测试与诊断使用。
    pub(crate) fn pending_dirty_cards(
        &mut self,
        owner: u32,
    ) -> Result<Vec<(usize, u32)>, RawInvariant> {
        self.drain_dirty_cards(owner)
    }

    /// 取走每个 arena 的 dirty card。
    pub(super) fn drain_dirty_cards(
        &mut self,
        owner: u32,
    ) -> Result<Vec<(usize, u32)>, RawInvariant> {
        let descriptors = self.heap(owner)?.arena_descriptors();
        let mut cards = Vec::new();
        for (index, descriptor) in descriptors {
            if let Some(table) = self.barrier.table_mut(descriptor) {
                for card in table.swap_dirty() {
                    cards.push((index, card));
                }
            }
        }
        Ok(cards)
    }

    /// 确保目标类别的 arena 具备一次分配所需的 block；返回可分配 heap arena 下标。
    ///
    /// nursery 需要整个 TLAB span 连续；`blocks` 只描述本次请求的容量需求，不是候选图容量上界。
    fn ensure_heap_space(
        &mut self,
        owner: u32,
        kind: HeapArenaKind,
        blocks: u32,
        large: bool,
    ) -> Result<usize, RawInvariant> {
        let span = if kind == HeapArenaKind::Nursery && !large {
            self.heap(owner)?.tlab_span_blocks()
        } else {
            blocks.max(1)
        };
        if let Some(slot) = self.heap(owner)?.allocatable_arena(kind, span) {
            return Ok(slot);
        }
        let slot = self.register_heap_arena(owner, kind)?;
        let block_bytes = u64::from(self.heap_contract()?.block_bytes());
        let class = self.heap_block_class;
        for _ in 0..HEAP_BLOCKS_PER_ARENA {
            if self.heap(owner)?.has_allocatable(slot, span) {
                return Ok(slot);
            }
            let extent_arena = self.managed_arena(owner, slot)?.extent_arena;
            let (_, offset) = self.commit_managed_block(owner, extent_arena, class)?;
            let block = u32::try_from(offset / block_bytes)
                .map_err(|_| RawInvariant::new("block 下标超出 u32"))?;
            self.heap_mut(owner)?
                .commit_block(slot, block)
                .map_err(heap_error)?;
        }
        Err(RawInvariant::new(
            "LocalHeap arena 无法提供所需的连续 block",
        ))
    }

    /// 登记一个 managed arena：分配全局稠密 descriptor、打开 extent arena 并挂到 owner heap。
    fn register_heap_arena(
        &mut self,
        owner: u32,
        kind: HeapArenaKind,
    ) -> Result<usize, RawInvariant> {
        if let Some(slot) = self.heap(owner)?.uncommitted_arena(kind) {
            return Ok(slot);
        }
        let contract = self.heap_contract()?.clone();
        let manager = self.token(owner);
        let extent_arena = self.open_arena(owner, manager, MemoryDomainId::MANAGED_LOCAL)?;
        let base = self
            .extents
            .arena_base(extent_arena)
            .ok_or_else(|| RawInvariant::new("LocalHeap arena 缺少基址"))?;
        let descriptor = u32::try_from(self.managed_arenas.len() + 1)
            .map_err(|_| RawInvariant::new("managed arena 数量超过 u32"))?;
        let heap_slot = self
            .heap_mut(owner)?
            .attach_arena(kind, descriptor, base, &contract);
        self.heap_mut(owner)?
            .set_arena_manager(heap_slot, manager.owner_id.raw());
        self.managed_arenas.push(ManagedArena {
            descriptor,
            heap_owner: owner,
            heap_slot,
            extent_arena,
            base,
            kind,
            manager,
        });
        self.register_managed_arena(owner, descriptor, 0)?;
        Ok(heap_slot)
    }

    /// 按 owner 与 heap 内下标取 managed arena 登记项。
    pub(crate) fn managed_arena(
        &self,
        owner: u32,
        heap_slot: usize,
    ) -> Result<&ManagedArena, RawInvariant> {
        self.managed_arenas
            .iter()
            .find(|arena| arena.heap_owner == owner && arena.heap_slot == heap_slot)
            .ok_or_else(|| RawInvariant::new("managed arena 未登记"))
    }

    /// 按全局 descriptor 取 managed arena 登记项。
    pub(crate) fn managed_arena_by_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<&ManagedArena, RawInvariant> {
        self.managed_arenas
            .iter()
            .find(|arena| arena.descriptor == descriptor)
            .ok_or_else(|| RawInvariant::new("managed arena descriptor 未登记"))
    }

    /// 返回全部 managed arena 登记项；mark 与 candidate 平面按 descriptor 索引它们。
    pub(crate) fn managed_arenas(&self) -> &[ManagedArena] {
        &self.managed_arenas
    }

    /// 解析一个 payload 地址所属的稳定 block 身份。
    pub(crate) fn managed_block_ref(
        &self,
        owner: u32,
        address: u64,
    ) -> Result<BlockRef, RawInvariant> {
        self.heap(owner)?.block_ref(address).map_err(heap_error)
    }

    /// 按稳定 block 身份取它的当前 generation。
    pub(crate) fn managed_block_generation(&self, id: ManagedBlockId) -> Result<u32, RawInvariant> {
        let arena = self.managed_arena_by_descriptor(id.arena())?;
        self.heap(arena.heap_owner)?
            .block_generation(id)
            .map_err(heap_error)
    }
}

/// 返回类别的最小对齐。
fn kind_align(kind: HeapArenaKind, contract: &LocalHeapRuntimeContract) -> u64 {
    match kind {
        HeapArenaKind::Large => u64::from(contract.page_bytes),
        _ => u64::from(contract.granule_bytes),
    }
}
