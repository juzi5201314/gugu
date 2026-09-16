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
    CycleReport, GENERATION_OLD, HeapArenaKind, HeapCounters, HeapError, HeapObject, LocalHeap,
};
use super::super::local_heap_schema::LocalHeapRuntimeContract;
use super::super::slab::{MemoryDomainId, RawInvariant, SlabDescriptorId};
use super::RawWorld;

/// managed 分配位置；与 `PlacementKind` 的 managed 子集一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPlacement {
    /// 当前 turn 私有的 nursery 快路径。
    Nursery,
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
            Self::Pinned => "pinned",
            Self::Resource => "resource",
            Self::SharedHeap => "shared-heap",
        }
    }

    /// 返回目标 arena 类别。
    const fn arena(self) -> HeapArenaKind {
        match self {
            Self::Nursery | Self::SharedHeap => HeapArenaKind::Nursery,
            Self::Pinned => HeapArenaKind::Pinned,
            Self::Resource => HeapArenaKind::Resource,
        }
    }
}

/// 把 `HeapError` 转换成本平面的不变量错误。
fn heap_error(error: HeapError) -> RawInvariant {
    error.into_invariant()
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
        self.gc_types = Some(types);
        self.managed_roots.clear();
        self.managed_root_kinds.clear();
        self.heap_cycle_epoch = 0;
        self.heap_block_class = block_class;
        self.configure_mark(raw.mark())?;
        Ok(())
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
        self.ensure_heap_space(owner, kind, need, need > 1)?;
        let align = kind_align(kind, &contract);
        let mut attempt = self
            .heap_mut(owner)?
            .allocate(kind, type_index, payload_bytes, align);
        if kind == HeapArenaKind::Nursery && matches!(attempt, Err(HeapError::NoCapacity)) {
            // 当前 TLAB span 在分配过程中用尽：重新取 span（必要时提交新 block）后重试一次。
            self.ensure_heap_space(owner, kind, need, false)?;
            attempt = self
                .heap_mut(owner)?
                .allocate(kind, type_index, payload_bytes, align);
        }
        attempt.map_err(heap_error)
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
        let descriptor = self.heap(owner)?.arena_of(address).map_err(heap_error)?.1;
        let source_block = self.heap(owner)?.block_of(address).map_err(heap_error)?;
        let new_owner = if value == 0 {
            owner
        } else {
            self.owner_of(value)?
        };
        let new_in_nursery = value != 0 && self.heap(new_owner)?.in_nursery(value);
        let new_block = if value == 0 {
            None
        } else {
            self.heap(new_owner)?.block_of(value).ok()
        };
        self.heap_mut(owner)?
            .set_field(address, offset, value)
            .map_err(heap_error)?;
        let site = BarrierSite {
            arena_descriptor: descriptor,
            arena_generation: 0,
            offset,
            cycle_epoch: self.heap_cycle_epoch,
            old_present: old != 0,
            new_present: value != 0,
            new_in_nursery,
            owner_old: object.generation >= GENERATION_OLD,
            marking: false,
            stack_grey: true,
            new_block,
            source_block,
            new_owner,
            source_owner: owner,
        };
        self.perform_barrier(processor, site).map(|_| ())
    }

    /// pin 一个 managed 对象；nursery/aging 对象先提升并改写全部强引用。
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
        self.heap_cycle_epoch += 1;
        self.advance_barrier_epoch(owner, self.heap_cycle_epoch)?;
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

    /// 确保目标类别的 arena 具备一次分配所需的 block；nursery 需要整个 TLAB span 连续。
    fn ensure_heap_space(
        &mut self,
        owner: u32,
        kind: HeapArenaKind,
        blocks: u32,
        large: bool,
    ) -> Result<(), RawInvariant> {
        self.ensure_arena(owner, kind)?;
        let block_bytes = u64::from(self.heap_contract()?.block_bytes());
        let span = if kind == HeapArenaKind::Nursery && !large {
            self.heap(owner)?.tlab_span_blocks()
        } else {
            blocks.max(1)
        };
        let heap_index = self
            .heap(owner)?
            .arena_index(kind, 0)
            .ok_or_else(|| RawInvariant::new("LocalHeap arena 缺失"))?;
        let total = self.heap(owner)?.blocks_per_arena();
        for _ in 0..total {
            if self.heap(owner)?.has_allocatable(heap_index, span) {
                return Ok(());
            }
            let class = self.heap_block_class;
            let offset = self.commit_managed_block(owner, class)?;
            let block = u32::try_from(offset / block_bytes).expect("block 下标适配 u32");
            self.heap_mut(owner)?
                .commit_block(heap_index, block)
                .map_err(heap_error)?;
        }
        Err(RawInvariant::new(
            "LocalHeap arena 无法提供所需的连续 block",
        ))
    }

    /// 确保某类别的 arena 已登记。
    fn ensure_arena(&mut self, owner: u32, kind: HeapArenaKind) -> Result<usize, RawInvariant> {
        if let Some(index) = self.heap(owner)?.arena_index(kind, 0) {
            return Ok(index);
        }
        let contract = self
            .local_heap_contract
            .clone()
            .ok_or_else(|| RawInvariant::new("LocalHeap 未按契约配置"))?;
        let token = self.token(owner);
        let arena = self.open_arena(owner, token, MemoryDomainId::MANAGED_LOCAL)?;
        let base = self
            .extents
            .arena_base(arena)
            .ok_or_else(|| RawInvariant::new("LocalHeap arena 缺少基址"))?;
        let index = self.heap_mut(owner)?.attach_arena(kind, base, &contract);
        let descriptor = self
            .heap(owner)?
            .arena(index)
            .ok_or_else(|| RawInvariant::new("LocalHeap arena 缺失"))?
            .descriptor();
        let raw = u32::try_from(descriptor).map_err(|_| RawInvariant::new("arena 身份超出 u32"))?;
        self.register_managed_arena(owner, SlabDescriptorId::from_raw(raw), 0)?;
        Ok(index)
    }
}

/// 返回类别的最小对齐。
fn kind_align(kind: HeapArenaKind, contract: &LocalHeapRuntimeContract) -> u64 {
    match kind {
        HeapArenaKind::Large => u64::from(contract.page_bytes),
        _ => u64::from(contract.granule_bytes),
    }
}
