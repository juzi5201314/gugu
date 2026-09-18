//! 共享 payload 的世界侧 block registry。
//!
//! SharedHeap 只保存逻辑身份；世界侧登记项把 `SharedHandle` 映射成**世界级稳定 block 身份**
//! （`BlockRef`）与 payload owner，供 mark、candidate 与 edge 路由使用。descriptor 从
//! `SHARED_DESCRIPTOR_BASE` 以上的独立编号段分配，与 LocalHeap arena 的稠密 descriptor 空间
//! 严格不重叠：同一条 `descriptor * 64 + index` 编码因此能同时表达两种 block 而不会撞车。
//!
//! 表按 handle 的稠密 slot 直接索引（`Vec<Option<..>>` 而非哈希表：slot 编号就是下标，
//! 上界等于表的长度），generation 是唯一的 ABA 防线：旧 handle 即使命中下标也会在代际校验处
//! 失败，不会被解释成新对象的 block。

use super::super::gc_metadata_contract::GC_BLOCK_BYTES;
use super::super::local_heap::{BlockRef, ManagedBlockId};
use super::super::shared_heap_schema::{SharedHandle, SharedPayloadId};
use super::super::slab::RawInvariant;

/// 共享 block descriptor 的起始编号；LocalHeap arena descriptor 从 1 开始稠密分配，两者
/// 因此永远不会重合。
pub(crate) const SHARED_DESCRIPTOR_BASE: u32 = 1 << 24;

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
}

/// 一个正在填充的共享 block。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenBlock {
    block: BlockRef,
    owner: u32,
    used: u32,
}

/// 共享 block 登记表；按 handle slot 稠密索引。
#[derive(Debug)]
pub(crate) struct SharedRegistry {
    entries: Vec<Option<SharedPayloadBlock>>,
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
            current: None,
            next_descriptor: SHARED_DESCRIPTOR_BASE,
        }
    }

    /// 为一个新 payload 分配世界级 block 身份。
    ///
    /// 同一个 owner 的 payload 依次落在同一个共享 block 的不同偏移上；block 放不下下一个
    /// payload 或 owner 变化时推进 descriptor，因此 block 身份既能区分 owner，也不会与
    /// LocalHeap arena descriptor 撞车。
    fn allocate_block(&mut self, owner: u32, bytes: u32) -> Result<(BlockRef, u32), RawInvariant> {
        let needed = bytes.max(1);
        let fits = self.current.is_some_and(|open| {
            open.owner == owner
                && open
                    .used
                    .checked_add(needed)
                    .is_some_and(|end| end <= GC_BLOCK_BYTES)
        });
        if !fits {
            let descriptor = self.next_descriptor;
            self.next_descriptor = self
                .next_descriptor
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("共享 block descriptor 溢出"))?;
            let id = ManagedBlockId::new(descriptor, 0)
                .map_err(|_| RawInvariant::new("共享 block 身份越界"))?;
            self.current = Some(OpenBlock {
                block: BlockRef { id, generation: 1 },
                owner,
                used: 0,
            });
        }
        let open = self
            .current
            .as_mut()
            .ok_or_else(|| RawInvariant::new("共享 block 登记状态缺失"))?;
        let offset = open.used;
        open.used = open
            .used
            .checked_add(needed)
            .ok_or_else(|| RawInvariant::new("共享 payload 偏移溢出"))?;
        Ok((open.block, offset))
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
    ) -> Result<(BlockRef, u32), RawInvariant> {
        self.allocate_block(owner, bytes)
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
        };
        if slot >= self.entries.len() {
            self.entries.resize(slot + 1, None);
        }
        self.entries[slot] = Some(entry);
        Ok(entry)
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

    /// 返回登记项数量。
    pub(crate) fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// 返回已分配到的 descriptor 上界（不含）；测试用它断言与 LocalHeap 编号段不重叠。
    pub(crate) const fn descriptor_high_water(&self) -> u32 {
        self.next_descriptor
    }
}
