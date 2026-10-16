//! 共享 payload 的世界侧 block registry。
//!
//! SharedHeap 只保存逻辑身份；世界侧登记项把 `SharedHandle` 映射成**世界级稳定 block 身份**
//! （`BlockRef`）与 payload owner，供 mark、candidate 与 edge 路由使用。descriptor 从
//! `SHARED_DESCRIPTOR_BASE` 以上的独立编号段分配，与 LocalHeap arena 的稠密 descriptor 空间
//! 严格不重叠：同一条 `descriptor * 64 + index` 编码因此能同时表达两种 block 而不会撞车。
//!
//! 表按 handle 的稠密 slot 直接索引（`Vec<Option<..>>` 而非哈希表：slot 编号就是下标，
//! 上界等于表的长度），generation 是唯一的 ABA 防线：旧 handle 即使命中下标也会在代际校验处
//! 失败，不会被解释成新对象的 block。block 记录同样按 descriptor 稠密索引（下标就是
//! `descriptor - SHARED_DESCRIPTOR_BASE`，与分配段一一对应），它同时是搬迁判据与归还判据的
//! 唯一账本：`live_bytes` 含仍在 forwarding grace 里的旧 payload，`dead_bytes` 只记已经真正
//! 释放的字节，因此「搬迁字节不多于已释放字节」可以直接比较两个计数器。

use super::super::extent::ExtentId;
use super::super::gc_metadata_contract::GC_BLOCK_BYTES;
use super::super::local_heap::{BlockRef, ManagedBlockId};
use super::super::shared_heap_schema::{SharedHandle, SharedPayloadId};
use super::super::slab::{OwnerToken, RawInvariant};
use super::RawWorld;

/// 共享 block descriptor 的起始编号；LocalHeap arena descriptor 从 1 开始稠密分配，两者
/// 因此永远不会重合。
pub(crate) const SHARED_DESCRIPTOR_BASE: u32 = 1 << 24;

/// 判断一个 block descriptor 是否来自共享编号段。
///
/// block 身份解析必须先看编号段：共享 block 不在 `managed_arenas` 里（它的 arena 定位、物理归还
/// 与 extent 阶梯属于后续的 block return 路径），因此凡是按 descriptor 反查 owner/manager 的
/// 地方都必须在这里分流，否则共享 block 会被当成「未登记的 LocalHeap arena」。
pub(crate) const fn is_shared_descriptor(descriptor: u32) -> bool {
    descriptor >= SHARED_DESCRIPTOR_BASE
}

/// 一次已经发布但尚未在目标 owner 结清的 payload 搬迁。
///
/// 记录在源登记项上：搬迁已经把 current payload 切到新位置，但旧 payload 仍受 guard、pin、
/// 票据与 forwarding lease 保护，必须等到目标 owner 消费 `HandleForward` 并走满 grace 之后
/// 才能回收。`lease` 是目标 owner 结清时唯一接受的凭据，`forward_generation` 使重复或跳号的
/// 通知在写任何状态之前被拒绝。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedPendingForward {
    /// 进入 forwarding grace 的旧 payload identity。
    pub(crate) old_payload: SharedPayloadId,
    /// 旧 payload 所在的世界级 block。
    pub(crate) old_block: BlockRef,
    /// 旧 payload 在旧 block 内的字节偏移。
    pub(crate) old_block_offset: u32,
    /// 旧 payload 的逻辑字节数。
    pub(crate) bytes: u32,
    /// 本次搬迁的 forwarding lease；只有它能把 lease 结清。
    pub(crate) lease: u32,
    /// 本次搬迁推进到的 forward generation。
    pub(crate) forward_generation: u32,
}

/// 一个共享 payload 的世界侧登记项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedPayloadBlock {
    /// 该 payload 的稳定 handle；table 与 generation 一起构成索引键。
    pub(crate) handle: SharedHandle,
    /// payload 的逻辑身份。
    pub(crate) payload: SharedPayloadId,
    /// 拥有 payload 的 owner 编号。
    pub(crate) owner: u32,
    /// 世界级稳定 block 身份。
    pub(crate) block: BlockRef,
    /// payload 在 block 内的字节偏移。
    pub(crate) block_offset: u32,
    /// payload 的逻辑字节数。
    pub(crate) payload_bytes: u32,
    /// 该 payload 是否已经交还给候选回收；交还后不再接受新的 mark/edge 工作。
    pub(crate) returned: bool,
    /// 在飞搬迁；`Some` 表示旧 payload 仍在 grace 且 lease 尚未在目标 owner 结清。
    pub(crate) pending_forward: Option<SharedPendingForward>,
}

/// 一个共享 block 的世界级账本记录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedBlockRecord {
    /// 世界级稳定 block 身份。
    pub(crate) block: BlockRef,
    /// 拥有该 block 的 owner 编号。
    pub(crate) owner: u32,
    /// bump 游标：下一个 payload 的起始偏移。
    pub(crate) used_bytes: u32,
    /// 仍然在用的 payload 字节数；含仍在 forwarding grace 里的旧 payload。
    pub(crate) live_bytes: u32,
    /// 已经真正释放的 payload 字节数。
    pub(crate) dead_bytes: u32,
    /// 仍然登记的 payload 数。
    pub(crate) live_payloads: u32,
    /// 该 block 是否已经不再是正在填充的 block。
    pub(crate) sealed: bool,
    /// 该 block 对应的 MANAGED_SHARED extent。
    pub(crate) extent: Option<ExtentId>,
    /// extent 在 arena 内的字节偏移。
    pub(crate) extent_offset: u64,
}

impl SharedBlockRecord {
    /// 判断 block 是否可以归还：已封口且不再有任何登记的 payload。
    pub(crate) fn is_empty(&self) -> bool {
        self.sealed && self.live_payloads == 0
    }
}

/// 一个正在填充的共享 block。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenBlock {
    block: BlockRef,
    owner: u32,
}

/// 一次 block 预留的撤销凭据；只覆盖 `reserve` 自身改动的表状态。
///
/// 线性化点之前失败（payload 未发布）时用它把 block 表恢复到预留之前：`used_bytes` 回写、
/// 新开的 block 弹出、上一个活动块的封口位与 `current` 复位。
pub(crate) struct BlockReservation {
    descriptor: u32,
    /// 预留前的 `used_bytes`。
    offset: u32,
    /// 本次预留是否新开了一个 block。
    opened: bool,
    /// 新开之前的活动块；`opened` 为 true 时撤销要恢复它的封口位与 `current`。
    previous: Option<OpenBlock>,
}

/// 共享 block 登记表；按 handle slot 与 descriptor 稠密索引。
#[derive(Debug)]
pub(crate) struct SharedRegistry {
    entries: Vec<Option<SharedPayloadBlock>>,
    /// 下标就是 `descriptor - SHARED_DESCRIPTOR_BASE`：descriptor 由本表单调分配，因此编号段
    /// 与下标段一一对应，不需要哈希表，也不会出现空洞以外的分配。
    blocks: Vec<SharedBlockRecord>,
    current: Option<OpenBlock>,
    next_descriptor: u32,
}

impl Default for SharedRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedRegistry {
    /// 建立空登记表；descriptor 从独立编号段开始。
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            blocks: Vec::new(),
            current: None,
            next_descriptor: SHARED_DESCRIPTOR_BASE,
        }
    }

    /// 把 descriptor 映射到 block 记录下标。
    fn block_index(descriptor: u32) -> Result<usize, RawInvariant> {
        let offset = descriptor
            .checked_sub(SHARED_DESCRIPTOR_BASE)
            .ok_or_else(|| RawInvariant::new("共享 block descriptor 落在独立编号段之外"))?;
        Ok(usize::try_from(offset).expect("descriptor 偏移适配 usize"))
    }

    /// 为一个新 payload 分配世界级 block 身份，并返回撤销凭据。
    ///
    /// 同一个 owner 的 payload 依次落在同一个共享 block 的不同偏移上；block 放不下下一个
    /// payload 或 owner 变化时推进 descriptor，因此 block 身份既能区分 owner，也不会与
    /// LocalHeap arena descriptor 撞车。切换 block 时旧 block 就地封口：封口是搬迁判据的一部分，
    /// 正在填充的 block 永远不会因为「暂时没有存活 payload」被判定为空。
    ///
    /// 预留会改动表状态（封口、推进 descriptor、写 `used_bytes`），因此返回的凭据必须由调用者
    /// 在线性化点之前失败时交回 `rollback_reserve`。
    fn allocate_block(
        &mut self,
        owner: u32,
        bytes: u32,
    ) -> Result<(BlockReservation, BlockRef, u32), RawInvariant> {
        let needed = bytes.max(1);
        let fits = match self.current {
            Some(open) if open.owner == owner => {
                let index = Self::block_index(open.block.id.arena())?;
                self.blocks[index]
                    .used_bytes
                    .checked_add(needed)
                    .is_some_and(|end| end <= GC_BLOCK_BYTES)
            }
            _ => false,
        };
        let mut opened = false;
        let mut previous = None;
        if !fits {
            previous = self.current;
            if let Some(open) = previous {
                let index = Self::block_index(open.block.id.arena())?;
                self.blocks[index].sealed = true;
            }
            let descriptor = self.next_descriptor;
            self.next_descriptor = self
                .next_descriptor
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("共享 block descriptor 溢出"))?;
            let id = ManagedBlockId::new(descriptor, 0)
                .map_err(|_| RawInvariant::new("共享 block 身份越界"))?;
            let block = BlockRef { id, generation: 1 };
            debug_assert_eq!(
                Self::block_index(descriptor).expect("descriptor 在编号段内"),
                self.blocks.len(),
                "descriptor 必须按编号段稠密追加"
            );
            self.blocks.push(SharedBlockRecord {
                block,
                owner,
                used_bytes: 0,
                live_bytes: 0,
                dead_bytes: 0,
                live_payloads: 0,
                sealed: false,
                extent: None,
                extent_offset: 0,
            });
            self.current = Some(OpenBlock { block, owner });
            opened = true;
        }
        let open = self
            .current
            .ok_or_else(|| RawInvariant::new("共享 block 登记状态缺失"))?;
        let index = Self::block_index(open.block.id.arena())?;
        let offset = self.blocks[index].used_bytes;
        self.blocks[index].used_bytes = offset
            .checked_add(needed)
            .ok_or_else(|| RawInvariant::new("共享 payload 偏移溢出"))?;
        let reservation = BlockReservation {
            descriptor: open.block.id.arena(),
            offset,
            opened,
            previous,
        };
        Ok((reservation, open.block, offset))
    }

    /// 为一个新 payload 预留世界级 block 位置。
    ///
    /// 同一个 owner 的 payload 依次落在同一个共享 block 的不同偏移上；block 放不下下一个
    /// payload 或 owner 变化时推进 descriptor，因此 block 身份既能区分 owner，也不会与
    /// LocalHeap arena descriptor 撞车。预留与实际登记分成两步：调用者先用这个位置在
    /// SharedHeap 上建立 payload 记录，再把发布后的 handle 交给 `insert`。
    pub(crate) fn reserve(
        &mut self,
        owner: u32,
        bytes: u32,
    ) -> Result<(BlockReservation, BlockRef, u32), RawInvariant> {
        self.allocate_block(owner, bytes)
    }

    /// 撤销一次未生效的 `reserve`：把 block 表恢复到预留之前（含解封上一个活动块）。
    ///
    /// 只回滚 `reserve` 自身改动的表状态；已经提交的 extent 必须由调用者先经
    /// `discard_shared_block_pages` 结清，否则回滚会丢掉唯一引用它的 block 记录。
    pub(crate) fn rollback_reserve(
        &mut self,
        reservation: &BlockReservation,
    ) -> Result<(), RawInvariant> {
        let index = Self::block_index(reservation.descriptor)?;
        let record = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("共享 block 记录未登记"))?;
        record.used_bytes = reservation.offset;
        if !reservation.opened {
            return Ok(());
        }
        // 新开的 block 必然在表尾：descriptor 与下标一一对应。
        if index + 1 != self.blocks.len() {
            return Err(RawInvariant::new("共享 block 撤销引用非表尾 block"));
        }
        self.blocks.pop();
        self.next_descriptor = self
            .next_descriptor
            .checked_sub(1)
            .ok_or_else(|| RawInvariant::new("共享 block descriptor 下溢"))?;
        match reservation.previous {
            Some(previous) => {
                let previous_index = Self::block_index(previous.block.id.arena())?;
                let record = self
                    .blocks
                    .get_mut(previous_index)
                    .ok_or_else(|| RawInvariant::new("共享 block 撤销丢失上一个活动块"))?;
                record.sealed = false;
                self.current = Some(previous);
            }
            None => self.current = None,
        }
        Ok(())
    }

    /// 记入一个已经预留在 block 内的 payload 字节。
    ///
    /// 首次分配与搬迁共用这一步：搬迁的新 payload 同样占用目标 block 的字节，旧 payload 在
    /// grace 结束前仍计 live，因此这里不释放任何字节，只把新 payload 累加进 `live_bytes`。
    pub(crate) fn note_payload_added(
        &mut self,
        descriptor: u32,
        bytes: u32,
    ) -> Result<(), RawInvariant> {
        let index = Self::block_index(descriptor)?;
        let record = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("共享 block 记录未登记"))?;
        record.live_bytes = record
            .live_bytes
            .checked_add(bytes)
            .ok_or_else(|| RawInvariant::new("共享 block live 字节溢出"))?;
        record.live_payloads = record
            .live_payloads
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("共享 block payload 计数溢出"))?;
        Ok(())
    }

    /// 在一个已预留的位置登记已经发布的 payload。
    pub(crate) fn insert(
        &mut self,
        handle: SharedHandle,
        payload: SharedPayloadId,
        owner: u32,
        bytes: u32,
        block: BlockRef,
        block_offset: u32,
    ) -> Result<SharedPayloadBlock, RawInvariant> {
        let slot = usize::try_from(handle.slot()).expect("handle slot 适配下标");
        if self.entries.get(slot).is_some_and(|entry| entry.is_some()) {
            return Err(RawInvariant::new("共享 handle slot 已经登记"));
        }
        let entry = SharedPayloadBlock {
            handle,
            payload,
            owner,
            block,
            block_offset,
            payload_bytes: bytes,
            returned: false,
            pending_forward: None,
        };
        if slot >= self.entries.len() {
            self.entries.resize(slot + 1, None);
        }
        self.entries[slot] = Some(entry);
        self.note_payload_added(block.id.arena(), bytes)?;
        Ok(entry)
    }

    /// 把一次搬迁后的新位置写回登记项；旧位置由 pending forward 记录。
    pub(crate) fn relocate(
        &mut self,
        handle: SharedHandle,
        payload: SharedPayloadId,
        block: BlockRef,
        block_offset: u32,
    ) -> Result<(), RawInvariant> {
        let slot = usize::try_from(handle.slot()).expect("handle slot 适配下标");
        let entry = self
            .entries
            .get_mut(slot)
            .and_then(|entry| entry.as_mut())
            .ok_or_else(|| RawInvariant::new("共享 payload 未登记"))?;
        if entry.handle.generation() != handle.generation() {
            return Err(RawInvariant::new("共享 payload 身份与登记项不一致"));
        }
        entry.payload = payload;
        entry.block = block;
        entry.block_offset = block_offset;
        Ok(())
    }

    /// 按 handle 取登记项；table、generation 不匹配或槽未登记都返回 `None`。
    pub(crate) fn get(&self, handle: SharedHandle) -> Option<&SharedPayloadBlock> {
        let slot = usize::try_from(handle.slot()).ok()?;
        let entry = self.entries.get(slot)?.as_ref()?;
        (entry.handle.generation() == handle.generation() && entry.handle.table() == handle.table())
            .then_some(entry)
    }

    /// 把一个 payload 登记为已交还；重复交还或身份不匹配返回错误。
    pub(crate) fn mark_returned(&mut self, handle: SharedHandle) -> Result<(), RawInvariant> {
        let slot = usize::try_from(handle.slot()).expect("handle slot 适配下标");
        let entry = self
            .entries
            .get_mut(slot)
            .and_then(|entry| entry.as_mut())
            .ok_or_else(|| RawInvariant::new("共享 payload 未登记"))?;
        if entry.handle.generation() != handle.generation() || entry.returned {
            return Err(RawInvariant::new("共享 payload 身份与登记项不一致"));
        }
        entry.returned = true;
        Ok(())
    }

    /// 写入待结清的搬迁记录；已有记录时拒绝（同一个 handle 同时只能有一个在飞 forward）。
    pub(crate) fn set_pending_forward(
        &mut self,
        handle: SharedHandle,
        pending: SharedPendingForward,
    ) -> Result<(), RawInvariant> {
        let slot = usize::try_from(handle.slot()).expect("handle slot 适配下标");
        let entry = self
            .entries
            .get_mut(slot)
            .and_then(|entry| entry.as_mut())
            .ok_or_else(|| RawInvariant::new("共享 payload 未登记"))?;
        if entry.handle.generation() != handle.generation() {
            return Err(RawInvariant::new("共享 payload 身份与登记项不一致"));
        }
        if entry.pending_forward.is_some() {
            return Err(RawInvariant::new("共享 payload 已有在飞搬迁"));
        }
        entry.pending_forward = Some(pending);
        Ok(())
    }

    /// 按值复制待结清的搬迁记录；供消费与结清判定使用。
    pub(crate) fn pending_forward(&self, handle: SharedHandle) -> Option<SharedPendingForward> {
        self.get(handle)?.pending_forward
    }

    /// 清掉待结清的搬迁记录；只有旧 payload 真正释放之后才允许调用。
    pub(crate) fn clear_pending_forward(
        &mut self,
        handle: SharedHandle,
    ) -> Result<(), RawInvariant> {
        let slot = usize::try_from(handle.slot()).expect("handle slot 适配下标");
        let entry = self
            .entries
            .get_mut(slot)
            .and_then(|entry| entry.as_mut())
            .ok_or_else(|| RawInvariant::new("共享 payload 未登记"))?;
        if entry.handle.generation() != handle.generation() {
            return Err(RawInvariant::new("共享 payload 身份与登记项不一致"));
        }
        if entry.pending_forward.take().is_none() {
            return Err(RawInvariant::new("共享 payload 没有在飞搬迁"));
        }
        Ok(())
    }

    /// 记入一个真正释放的 payload；下溢即不变量失败。
    pub(crate) fn note_payload_freed(
        &mut self,
        descriptor: u32,
        bytes: u32,
    ) -> Result<(), RawInvariant> {
        let index = Self::block_index(descriptor)?;
        let record = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("共享 block 记录未登记"))?;
        record.live_bytes = record
            .live_bytes
            .checked_sub(bytes)
            .ok_or_else(|| RawInvariant::new("共享 block live 字节下溢"))?;
        record.dead_bytes = record
            .dead_bytes
            .checked_add(bytes)
            .ok_or_else(|| RawInvariant::new("共享 block dead 字节溢出"))?;
        record.live_payloads = record
            .live_payloads
            .checked_sub(1)
            .ok_or_else(|| RawInvariant::new("共享 block payload 计数下溢"))?;
        Ok(())
    }

    /// 对象释放完成后删除登记项。
    ///
    /// slot 已经被 SharedHeap 的 free 栈收回，条目必须让位给同 slot 的下一次发布；旧 handle 由
    /// generation 校验拒绝，因此这里不保留墓碑。
    pub(crate) fn release_entry(&mut self, handle: SharedHandle) -> Result<(), RawInvariant> {
        let slot = usize::try_from(handle.slot()).expect("handle slot 适配下标");
        let entry = self
            .entries
            .get_mut(slot)
            .and_then(|entry| entry.as_mut())
            .ok_or_else(|| RawInvariant::new("共享 payload 未登记"))?;
        if entry.handle.generation() != handle.generation() {
            return Err(RawInvariant::new("共享 payload 身份与登记项不一致"));
        }
        self.entries[slot] = None;
        Ok(())
    }

    /// 按 handle slot 升序迭代某个 owner 的全部登记项。
    pub(crate) fn entries_for_owner(
        &self,
        owner: u32,
    ) -> impl Iterator<Item = (SharedHandle, &SharedPayloadBlock)> {
        self.entries
            .iter()
            .flatten()
            .filter(move |entry| entry.owner == owner)
            .map(|entry| (entry.handle, entry))
    }

    /// 按 descriptor 升序迭代某个 owner 名下**仍绑定 extent**的全部 block 记录；元素是
    /// `(descriptor, 记录)`。
    ///
    /// 归还完成的 tombstone（`extent` 已清空）不再伪装成活动 block。
    pub(crate) fn blocks_for_owner(
        &self,
        owner: u32,
    ) -> impl Iterator<Item = (u32, &SharedBlockRecord)> {
        self.blocks
            .iter()
            .enumerate()
            .filter(move |(_, record)| record.owner == owner && record.extent.is_some())
            .map(|(index, record)| {
                (
                    SHARED_DESCRIPTOR_BASE + u32::try_from(index).expect("block 下标适配 u32"),
                    record,
                )
            })
    }

    /// 迭代全部 block 记录；元素是 `(descriptor, 记录)`。
    ///
    /// 与 `blocks_for_owner` 不同，这里不过滤 owner 与 extent：账本归属要按 extent 的物理
    /// owner 解析，因此必须能看到全部记录。
    pub(crate) fn block_records(&self) -> impl Iterator<Item = (u32, &SharedBlockRecord)> {
        self.blocks.iter().enumerate().map(|(index, record)| {
            (
                SHARED_DESCRIPTOR_BASE + u32::try_from(index).expect("block 下标适配 u32"),
                record,
            )
        })
    }

    /// 迭代全部在飞搬迁；元素是 `(handle, 记录)`，按 handle slot 升序。
    pub(crate) fn pending_forwards(
        &self,
    ) -> impl Iterator<Item = (SharedHandle, SharedPendingForward)> + '_ {
        self.entries
            .iter()
            .flatten()
            .filter_map(|entry| entry.pending_forward.map(|pending| (entry.handle, pending)))
    }

    /// 按 descriptor 取 block 记录。
    pub(crate) fn block_record(&self, descriptor: u32) -> Option<&SharedBlockRecord> {
        let index = Self::block_index(descriptor).ok()?;
        self.blocks.get(index)
    }

    /// 把一个共享 block 绑定到它的 MANAGED_SHARED extent。
    pub(crate) fn attach_extent(
        &mut self,
        descriptor: u32,
        extent: ExtentId,
        offset: u64,
    ) -> Result<(), RawInvariant> {
        let index = Self::block_index(descriptor)?;
        let record = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("共享 block 记录未登记"))?;
        record.extent = Some(extent);
        record.extent_offset = offset;
        Ok(())
    }

    /// 解绑一个共享 block 的 extent，清除记录上的物理绑定。
    pub(crate) fn clear_extent(&mut self, descriptor: u32) -> Result<(), RawInvariant> {
        let index = Self::block_index(descriptor)?;
        let record = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("共享 block 记录未登记"))?;
        record.extent = None;
        record.extent_offset = 0;
        Ok(())
    }

    /// 从 registry 摔掉一个已归还的共享 block 记录。
    pub(crate) fn drop_block(&mut self, descriptor: u32) -> Result<(), RawInvariant> {
        let index = Self::block_index(descriptor)?;
        let record = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("共享 block 记录未登记"))?;
        if !record.is_empty() {
            return Err(RawInvariant::new("摔掉共享 block 时仍有存活 payload"));
        }
        record.extent = None;
        record.extent_offset = 0;
        Ok(())
    }

    /// 返回已封口、不再有存活 payload 且仍绑定 extent 的 block 数。
    pub(crate) fn empty_block_count(&self) -> u64 {
        u64::try_from(
            self.blocks
                .iter()
                .filter(|record| record.is_empty() && record.extent.is_some())
                .count(),
        )
        .expect("block 数适配 u64")
    }

    /// 把 `from` 名下的全部登记项与 block 记录改写到 `to`；返回移动的 block 数。
    ///
    /// payload 仍留在原 block，只有管理权与投递目标随之转移：搬迁判据、mark ticket 路由与
    /// card batch 目标读的都是这里的 owner，因此三者必须在同一个线性化点一起改。
    pub(crate) fn handover_owner(&mut self, from: u32, to: u32) -> u64 {
        let mut moved = 0_u64;
        for entry in self.entries.iter_mut().flatten() {
            if entry.owner == from {
                entry.owner = to;
            }
        }
        for record in &mut self.blocks {
            if record.owner == from {
                record.owner = to;
                moved += 1;
            }
        }
        if let Some(open) = self.current.as_mut()
            && open.owner == from
        {
            open.owner = to;
        }
        moved
    }

    /// 返回登记项数量。
    pub(crate) fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// 返回已分配到的 descriptor 上界（不含）；测试用它断言与 LocalHeap 编号段不重叠。
    pub(crate) const fn descriptor_high_water(&self) -> u32 {
        self.next_descriptor
    }
}

impl RawWorld {
    /// 返回一个共享 block 身份当前的 manager token（payload owner）。
    pub(crate) fn shared_block_manager(&self, block: BlockRef) -> Result<OwnerToken, RawInvariant> {
        let owner = self.shared_block_owner(block)?;
        Ok(self.token(owner))
    }

    /// 返回一个共享 block 身份当前的 payload owner。
    ///
    /// generation 必须与登记项逐位相同：引用已复用的 block 是真正的不变量失败，不能按 descriptor
    /// 落回「某个还活着的块」。
    pub(crate) fn shared_block_owner(&self, block: BlockRef) -> Result<u32, RawInvariant> {
        let record = self
            .shared_registry
            .block_record(block.id.arena())
            .ok_or_else(|| RawInvariant::new("共享 block 未在世界 registry 登记"))?;
        if record.block != block {
            return Err(RawInvariant::new("共享 block 身份引用已复用的 generation"));
        }
        Ok(record.owner)
    }
}
