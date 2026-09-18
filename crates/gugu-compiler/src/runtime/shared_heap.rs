//! SharedHeap stable handle、access guard、forwarding grace 与共享字段写入的确定性参照实现。
//!
//! 参照模型只保存逻辑身份与字节向量：handle 是 `table/slot/generation` 三元组，payload 是
//! `generation|slot` 身份加上 block/offset 元数据，任何地方都不出现裸地址。slot 与 payload 的
//! 编号都是稠密整数，因此两张表用 `Vec` 直接索引，free 槽用稠密 free 栈复用（LIFO 使复用顺序
//! 确定），不用 `HashMap`：点查上界就是表的长度。generation 是唯一的 ABA 防线——slot 在每次
//! 重新发布时推进 handle generation，payload 槽在每次复用时推进 payload generation，因此旧
//! handle 与旧 payload identity 必然在索引命中后仍被代际校验拒绝。
//!
//! 搬迁只切换 slot 的 current payload：旧 payload 在 access guard、pin lease、mark ticket 与
//! forwarding lease 全部结清且 grace 步数走满之前一直留在表里，因此 guard 期间捕获的 token
//! 仍然读写它当时解析到的字节。

use super::model::RawModelError;
use super::shared_heap_schema::{
    SHARED_HANDLE_INITIAL_GENERATION, SHARED_HANDLE_SLOT_BYTES, SHARED_HANDLE_TABLE_LIMIT,
    SharedHandle, SharedHandleSlot, SharedHeapRuntimeContract, SharedPayloadId, SharedSlotState,
};

/// 参照实现失败；世界层把它映射到现有 `RawInvariant`（运行时失败走 E0058 路径）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedHeapError {
    message: String,
}

impl SharedHeapError {
    /// 用固定文本创建失败。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回失败文本。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for SharedHeapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<RawModelError> for SharedHeapError {
    fn from(value: RawModelError) -> Self {
        Self::new(value.message().to_owned())
    }
}

/// 一次 SharedHeap allocation 产生的 fresh payload 登记项。
///
/// 它不是地址，也不是 stable handle：`ResolveSharedHandle` 用它在 handle 表里发布一个新身份，
/// 同一个 payload identity 只能发布一次。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PayloadSource {
    /// fresh payload 的逻辑身份。
    pub(crate) id: SharedPayloadId,
    /// 拥有 payload 的 owner 编号。
    pub(crate) owner_id: u32,
    /// payload 所在共享 block 的全局身份。
    pub(crate) block_id: u32,
    /// payload 在 block 内的字节偏移。
    pub(crate) block_offset: u32,
    /// payload 的逻辑字节数。
    pub(crate) bytes: u32,
}

/// 参照模型里的一个 shared payload。
///
/// `content` 只存在于参照模型；外层 API 返回的是字节副本或写入记录，从不返回地址。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedPayload {
    /// payload 的逻辑身份。
    pub(crate) id: SharedPayloadId,
    /// 拥有 payload 的 owner 编号。
    pub(crate) owner_id: u32,
    /// payload 所在共享 block 的全局身份。
    pub(crate) block_id: u32,
    /// payload 在 block 内的字节偏移。
    pub(crate) block_offset: u32,
    /// payload 的逻辑字节数。
    pub(crate) bytes: u32,
    /// 发布该 payload 时的 handle generation；0 表示尚未发布。
    pub(crate) generation: u32,
    /// payload 的状态；与 slot 状态同域。
    pub(crate) state: SharedSlotState,
    /// 是否已经被 `resolve_payload` 发布过。
    pub(crate) resolved: bool,
    content: Vec<u8>,
}

impl SharedPayload {
    /// 返回 payload 的字节内容。
    pub(crate) fn content(&self) -> &[u8] {
        &self.content
    }
}

/// 一个已经解析、仍然 active 的 access guard。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedAccessToken {
    /// 建立 guard 时使用的稠密 token。
    pub(crate) token: u32,
    /// guard 打开时的 handle。
    pub(crate) handle: SharedHandle,
    /// 解析当时捕获的 payload identity；forward 不会改变它。
    pub(crate) payload: SharedPayloadId,
}

/// forward 被推迟的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForwardDeferred {
    /// 对象持有 pin lease：搬迁会移动被 pin 的 payload，因此必须推迟。
    Pinned,
}

/// 一次成功搬迁的转发记录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedForwardRecord {
    /// 被搬迁的 handle。
    pub(crate) handle: SharedHandle,
    /// 搬迁前的 payload identity。
    pub(crate) old_payload: SharedPayloadId,
    /// 搬迁后的 payload identity。
    pub(crate) new_payload: SharedPayloadId,
    /// 本次搬迁推进到的 forward generation。
    pub(crate) forward_generation: u32,
    /// 复制的字节数。
    pub(crate) bytes: u32,
}

/// `forward` 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SharedForward {
    /// payload 已切换，旧 payload 进入 grace。
    Forwarded(SharedForwardRecord),
    /// 搬迁被推迟；slot 的 current/forward generation 都没有改变。
    Deferred(ForwardDeferred),
}

/// 一次成功的 shared 字段写入；barrier 账本按它记录 card 与 edge。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedWrite {
    /// 目标 handle。
    pub(crate) handle: SharedHandle,
    /// 实际写入的 payload identity。
    pub(crate) payload: SharedPayloadId,
    /// payload 内偏移。
    pub(crate) offset: u32,
    /// 写入字节数。
    pub(crate) bytes: u32,
}

/// 一条已经建立但可能尚未解析的 access guard 记录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AccessRecord {
    handle: SharedHandle,
    payload: Option<SharedPayloadId>,
    ended: bool,
}

/// stable handle 表、payload 表与 forwarding grace 的参照实现。
///
/// 表按稠密 slot 直接索引：`slots` 的每一项就是一个 cache-line 大小的 `SharedHandleSlot`，
/// `payloads` 与 `payload_generations` 是 payload 槽的 SoA 表示，free 栈保存可复用编号。
/// `cycle_epoch` 与 `mark_epochs` 让「每个 cycle 每个对象至多一次 side mark」可判定：mark ticket
/// 只延迟旧 payload 回收，不改变 current payload。
#[derive(Debug)]
pub(crate) struct SharedHeap {
    table: u32,
    grace_steps: u32,
    slots: Vec<SharedHandleSlot>,
    free_slots: Vec<u32>,
    payloads: Vec<Option<SharedPayload>>,
    payload_generations: Vec<u32>,
    free_payloads: Vec<u32>,
    accesses: Vec<Option<AccessRecord>>,
    /// 每个 slot 当前在飞的 forwarding lease；0 表示没有。
    forward_leases: Vec<u32>,
    /// 每个 slot 已发出 mark ticket 的 cycle epoch；与当前 cycle 相同表示本 cycle 已标记。
    mark_epochs: Vec<u32>,
    /// 当前 mark cycle epoch；0 表示尚未开始。
    cycle_epoch: u32,
}

impl SharedHeap {
    /// 建立一个空的 SharedHeap；空需求仍然得到完整的契约状态。
    pub(crate) fn new(table: u32, contract: &SharedHeapRuntimeContract) -> Self {
        debug_assert!(table < SHARED_HANDLE_TABLE_LIMIT);
        debug_assert_eq!(
            contract.handle_slot_bytes(),
            SHARED_HANDLE_SLOT_BYTES,
            "handle slot 尺寸必须与契约一致"
        );
        Self {
            table,
            grace_steps: contract.forwarding_grace_steps(),
            slots: Vec::new(),
            free_slots: Vec::new(),
            payloads: Vec::new(),
            payload_generations: Vec::new(),
            free_payloads: Vec::new(),
            accesses: Vec::new(),
            forward_leases: Vec::new(),
            mark_epochs: Vec::new(),
            cycle_epoch: 0,
        }
    }

    /// 返回 SharedHeap 表身份。
    pub(crate) const fn table(&self) -> u32 {
        self.table
    }

    /// 返回当前登记（未释放）的 payload 数量。
    pub(crate) fn payload_count(&self) -> usize {
        self.payloads.iter().flatten().count()
    }

    /// 返回 slot 表长度。
    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// 登记一个新的 fresh payload；block/offset 由世界的共享 block 分配器提供。
    pub(crate) fn allocate_payload(
        &mut self,
        owner_id: u32,
        block_id: u32,
        block_offset: u32,
        bytes: u32,
    ) -> Result<PayloadSource, SharedHeapError> {
        let slot = match self.free_payloads.pop() {
            Some(slot) => slot,
            None => {
                let slot = u32::try_from(self.payloads.len())
                    .map_err(|_| SharedHeapError::new("shared payload 槽数量超过 u32"))?;
                self.payloads.push(None);
                self.payload_generations.push(0);
                slot
            }
        };
        let index = usize::try_from(slot).expect("payload 槽下标适配 usize");
        let generation = match self.payload_generations[index] {
            0 => SHARED_HANDLE_INITIAL_GENERATION,
            current => current
                .checked_add(1)
                .ok_or_else(|| SharedHeapError::new("shared payload generation 溢出"))?,
        };
        self.payload_generations[index] = generation;
        let id = SharedPayloadId::new(slot, generation);
        self.payloads[index] = Some(SharedPayload {
            id,
            owner_id,
            block_id,
            block_offset,
            bytes,
            generation: 0,
            state: SharedSlotState::Free,
            resolved: false,
            content: vec![0; usize::try_from(bytes).expect("payload 字节数适配 usize")],
        });
        Ok(PayloadSource {
            id,
            owner_id,
            block_id,
            block_offset,
            bytes,
        })
    }

    /// 把 fresh payload 发布成 stable handle；同一个 payload identity 只能发布一次。
    pub(crate) fn resolve_payload(
        &mut self,
        source: PayloadSource,
    ) -> Result<SharedHandle, SharedHeapError> {
        let payload_index = self.payload_index(source.id)?;
        let payload = self.payloads[payload_index]
            .as_ref()
            .ok_or_else(|| SharedHeapError::new("shared handle payload 未登记"))?;
        if payload.resolved {
            return Err(SharedHeapError::new("shared handle duplicate resolve"));
        }
        if payload.owner_id != source.owner_id
            || payload.block_id != source.block_id
            || payload.block_offset != source.block_offset
            || payload.bytes != source.bytes
        {
            return Err(SharedHeapError::new(
                "shared handle payload 登记项与解析来源不一致",
            ));
        }
        let slot = self.take_slot()?;
        let index = usize::try_from(slot).expect("slot 下标适配 usize");
        let generation = match self.slots[index].generation {
            0 => SHARED_HANDLE_INITIAL_GENERATION,
            current => current
                .checked_add(1)
                .ok_or_else(|| SharedHeapError::new("shared handle generation 溢出"))?,
        };
        let handle = SharedHandle::new(self.table, slot, generation)?;
        self.transition(index, SharedSlotState::Live)?;
        let record = &mut self.slots[index];
        record.generation = generation;
        record.current_payload = source.id.raw();
        record.old_payload = 0;
        record.forward_generation = 0;
        record.grace_epoch = 0;
        record.access_guards = 0;
        record.pin_leases = 0;
        record.mark_tickets = 0;
        record.forwarding_leases = 0;
        record.owner_id = source.owner_id;
        record.block_id = source.block_id;
        record.payload_bytes = source.bytes;
        self.forward_leases[index] = 0;
        self.mark_epochs[index] = 0;
        let payload = self.payloads[payload_index]
            .as_mut()
            .expect("payload 登记项已存在");
        payload.resolved = true;
        payload.generation = generation;
        payload.state = SharedSlotState::Live;
        Ok(handle)
    }

    /// 建立 access guard；只登记 token，不返回地址。
    pub(crate) fn begin_access(
        &mut self,
        token: u32,
        handle: SharedHandle,
    ) -> Result<(), SharedHeapError> {
        if token == 0 {
            return Err(SharedHeapError::new("shared access token 必须非零"));
        }
        let index = usize::try_from(token - 1).expect("token 下标适配 usize");
        if index >= self.accesses.len() {
            self.accesses.resize(index + 1, None);
        }
        if self.accesses[index].is_some() {
            return Err(SharedHeapError::new("shared access token 已在使用"));
        }
        self.access_slot(handle)?;
        self.accesses[index] = Some(AccessRecord {
            handle,
            payload: None,
            ended: false,
        });
        Ok(())
    }

    /// 解析 guard 捕获的 payload；同一 token 只能解析一次，并递增 access guard 计数。
    pub(crate) fn resolve_access(
        &mut self,
        token: u32,
    ) -> Result<SharedAccessToken, SharedHeapError> {
        let index = self.access_index(token)?;
        let record = self.accesses[index]
            .as_mut()
            .ok_or_else(|| SharedHeapError::new("shared access token 未建立"))?;
        if record.ended {
            return Err(SharedHeapError::new("shared access token 已结束"));
        }
        if record.payload.is_some() {
            return Err(SharedHeapError::new("shared access token 重复解析"));
        }
        let handle = record.handle;
        let slot = self.access_slot(handle)?;
        let payload = self.slots[slot].current_payload;
        if payload == 0 {
            return Err(SharedHeapError::new(
                "shared access handle 没有 current payload",
            ));
        }
        let record = self.accesses[index].as_mut().expect("access 记录已存在");
        record.payload = Some(SharedPayloadId::from_raw(payload));
        self.slots[slot].access_guards = self.slots[slot]
            .access_guards
            .checked_add(1)
            .ok_or_else(|| SharedHeapError::new("shared access guard 计数溢出"))?;
        Ok(SharedAccessToken {
            token,
            handle,
            payload: SharedPayloadId::from_raw(payload),
        })
    }

    /// 结束 access guard；未解析或重复结束都拒绝。
    pub(crate) fn end_access(&mut self, token: u32) -> Result<(), SharedHeapError> {
        let index = self.access_index(token)?;
        let record = self.accesses[index]
            .as_mut()
            .ok_or_else(|| SharedHeapError::new("shared access token 未建立"))?;
        if record.ended {
            return Err(SharedHeapError::new("shared access token 重复结束"));
        }
        if record.payload.is_none() {
            return Err(SharedHeapError::new("shared access token 尚未解析"));
        }
        let handle = record.handle;
        record.ended = true;
        let slot = self.access_slot(handle)?;
        self.slots[slot].access_guards = self.slots[slot]
            .access_guards
            .checked_sub(1)
            .ok_or_else(|| SharedHeapError::new("shared access guard 计数下溢"))?;
        Ok(())
    }

    /// 通过 active guard 读取 payload 字节。
    pub(crate) fn load(
        &self,
        access: SharedAccessToken,
        offset: u32,
        bytes: u32,
    ) -> Result<Vec<u8>, SharedHeapError> {
        let (payload, range) = self.access_range(access, offset, bytes)?;
        Ok(self.payloads[payload]
            .as_ref()
            .expect("payload 登记项已存在")
            .content[range]
            .to_vec())
    }

    /// 通过 active guard 写入 payload 字节；返回写入记录供 barrier 账本使用。
    pub(crate) fn store(
        &mut self,
        access: SharedAccessToken,
        offset: u32,
        value: &[u8],
    ) -> Result<SharedWrite, SharedHeapError> {
        let bytes = u32::try_from(value.len())
            .map_err(|_| SharedHeapError::new("shared 字段写入超过 u32 字节"))?;
        let (payload, range) = self.access_range(access, offset, bytes)?;
        let record = self.payloads[payload]
            .as_mut()
            .expect("payload 登记项已存在");
        record.content[range].copy_from_slice(value);
        Ok(SharedWrite {
            handle: access.handle,
            payload: access.payload,
            offset,
            bytes,
        })
    }

    /// 把一个 payload lease 提升为非移动 lease；它阻止 forwarding。
    pub(crate) fn pin(&mut self, handle: SharedHandle) -> Result<(), SharedHeapError> {
        let slot = self.access_slot(handle)?;
        self.slots[slot].pin_leases = self.slots[slot]
            .pin_leases
            .checked_add(1)
            .ok_or_else(|| SharedHeapError::new("shared pin lease 计数溢出"))?;
        Ok(())
    }

    /// 释放一个 pin lease；没有未结清 lease 时拒绝。
    pub(crate) fn unpin(&mut self, handle: SharedHandle) -> Result<(), SharedHeapError> {
        let slot = self.access_slot(handle)?;
        if self.slots[slot].pin_leases == 0 {
            return Err(SharedHeapError::new("shared pin lease 没有未结清租约"));
        }
        self.slots[slot].pin_leases -= 1;
        Ok(())
    }

    /// 推进 mark cycle；epoch 必须严格单调递增且非零。
    pub(crate) fn begin_mark_cycle(&mut self, epoch: u32) -> Result<(), SharedHeapError> {
        if epoch == 0 {
            return Err(SharedHeapError::new("shared mark cycle epoch 必须非零"));
        }
        if epoch <= self.cycle_epoch {
            return Err(SharedHeapError::new("shared mark cycle epoch 必须单调递增"));
        }
        self.cycle_epoch = epoch;
        Ok(())
    }

    /// 为对象发出一次 side mark；每个 cycle 至多一次，返回本次是否真的发出。
    pub(crate) fn mark_ticket(&mut self, handle: SharedHandle) -> Result<bool, SharedHeapError> {
        if self.cycle_epoch == 0 {
            return Err(SharedHeapError::new(
                "shared mark ticket 需要已开始的 mark cycle",
            ));
        }
        let slot = self.access_slot(handle)?;
        if self.mark_epochs[slot] == self.cycle_epoch {
            return Ok(false);
        }
        self.mark_epochs[slot] = self.cycle_epoch;
        self.slots[slot].mark_tickets = self.slots[slot]
            .mark_tickets
            .checked_add(1)
            .ok_or_else(|| SharedHeapError::new("shared mark ticket 计数溢出"))?;
        Ok(true)
    }

    /// 结清一次 side mark；没有未结清票据时拒绝。
    pub(crate) fn finish_mark_ticket(
        &mut self,
        handle: SharedHandle,
    ) -> Result<(), SharedHeapError> {
        let slot = self.access_slot(handle)?;
        if self.slots[slot].mark_tickets == 0 {
            return Err(SharedHeapError::new("shared mark ticket 没有未结清票据"));
        }
        self.slots[slot].mark_tickets -= 1;
        Ok(())
    }

    /// 把 payload 搬迁到调用者分配好的目标 payload；旧 payload 进入 forwarding grace。
    ///
    /// 目标 payload 由调用者（世界）先行分配，因此共享 block descriptor 始终来自世界的单调
    /// 分配器；本方法只负责身份切换、深复制与 grace 状态机。
    pub(crate) fn forward(
        &mut self,
        handle: SharedHandle,
        destination: SharedPayloadId,
        lease: u32,
        next_generation: u32,
    ) -> Result<SharedForward, SharedHeapError> {
        if lease == 0 {
            return Err(SharedHeapError::new("shared forwarding lease 必须非零"));
        }
        let slot = self.access_slot_raw(handle)?;
        if SharedSlotState::from_raw(self.slots[slot].state) != Some(SharedSlotState::Live) {
            return Err(SharedHeapError::new(
                "shared handle slot 状态不允许 forward",
            ));
        }
        if self.forward_leases[slot] != 0 {
            return Err(SharedHeapError::new("shared forwarding lease 尚未结清"));
        }
        if self.slots[slot].pin_leases != 0 {
            return Ok(SharedForward::Deferred(ForwardDeferred::Pinned));
        }
        let expected = self.slots[slot]
            .forward_generation
            .checked_add(1)
            .ok_or_else(|| SharedHeapError::new("shared forward generation 溢出"))?;
        if next_generation != expected {
            return Err(SharedHeapError::new(
                "shared forward generation 必须等于当前 forward generation 加一",
            ));
        }
        let old_payload = SharedPayloadId::from_raw(self.slots[slot].current_payload);
        let destination_index = self.payload_index(destination)?;
        let source_index = self.payload_index(old_payload)?;
        if destination == old_payload {
            return Err(SharedHeapError::new(
                "shared forward 目标 payload 不能与旧 payload 相同",
            ));
        }
        let bytes = self.payloads[source_index]
            .as_ref()
            .expect("payload 登记项已存在")
            .bytes;
        let content = self.payloads[source_index]
            .as_ref()
            .expect("payload 登记项已存在")
            .content
            .clone();
        {
            let destination_record = self.payloads[destination_index]
                .as_mut()
                .ok_or_else(|| SharedHeapError::new("shared forward 目标 payload 未登记"))?;
            if destination_record.bytes != bytes || destination_record.resolved {
                return Err(SharedHeapError::new(
                    "shared forward 目标 payload 的字节数或状态不匹配",
                ));
            }
            destination_record.content = content;
            destination_record.resolved = true;
            destination_record.generation = self.slots[slot].generation;
            // 目标 payload 在切换后就是 current payload，因此状态是 live；只有旧 payload 进入 grace。
            destination_record.state = SharedSlotState::Live;
        }
        // 先进入 forwarding：这一瞬间 new payload 已建立，current 仍指向旧 payload，因此任何
        // 观察者要么看到完整的旧对象，要么看到完整的新对象。
        self.transition(slot, SharedSlotState::Forwarding)?;
        let record = &mut self.slots[slot];
        record.old_payload = old_payload.raw();
        record.current_payload = destination.raw();
        record.forward_generation = next_generation;
        record.forwarding_leases = 1;
        self.forward_leases[slot] = lease;
        record.grace_epoch = 0;
        self.transition(slot, SharedSlotState::Grace)?;
        if let Some(source) = self.payloads[source_index].as_mut() {
            source.state = SharedSlotState::Grace;
        }
        Ok(SharedForward::Forwarded(SharedForwardRecord {
            handle,
            old_payload,
            new_payload: destination,
            forward_generation: next_generation,
            bytes,
        }))
    }

    /// 结清一次 forwarding lease；lease 不匹配时拒绝。
    pub(crate) fn end_forward_lease(
        &mut self,
        handle: SharedHandle,
        lease: u32,
    ) -> Result<(), SharedHeapError> {
        let slot = self.access_slot(handle)?;
        if lease == 0 || self.forward_leases[slot] != lease {
            return Err(SharedHeapError::new("shared forwarding lease 不匹配"));
        }
        self.forward_leases[slot] = 0;
        self.slots[slot].forwarding_leases = self.slots[slot]
            .forwarding_leases
            .checked_sub(1)
            .ok_or_else(|| SharedHeapError::new("shared forwarding lease 计数下溢"))?;
        Ok(())
    }

    /// 推进 forwarding grace；只有 grace 走满且全部 lease 结清才释放旧 payload。
    pub(crate) fn advance_grace(
        &mut self,
        handle: SharedHandle,
        steps: u32,
    ) -> Result<(), SharedHeapError> {
        if steps == 0 {
            return Err(SharedHeapError::new("shared grace 步数必须非零"));
        }
        let slot = self.access_slot(handle)?;
        match SharedSlotState::from_raw(self.slots[slot].state) {
            Some(SharedSlotState::Grace) => {
                let epoch = self.slots[slot]
                    .grace_epoch
                    .checked_add(steps)
                    .ok_or_else(|| SharedHeapError::new("shared grace epoch 溢出"))?;
                self.slots[slot].grace_epoch = epoch.min(self.grace_steps);
                if self.slots[slot].grace_epoch >= self.grace_steps {
                    self.transition(slot, SharedSlotState::Reclaimable)?;
                }
            }
            Some(SharedSlotState::Reclaimable) => {}
            _ => {
                return Err(SharedHeapError::new(
                    "shared handle slot 状态不在 forwarding grace 中",
                ));
            }
        }
        if SharedSlotState::from_raw(self.slots[slot].state) == Some(SharedSlotState::Reclaimable)
            && self.grace_settled(slot)
        {
            let old = SharedPayloadId::from_raw(self.slots[slot].old_payload);
            if old.is_valid() {
                self.free_payload(old)?;
            }
            let record = &mut self.slots[slot];
            record.old_payload = 0;
            record.grace_epoch = 0;
            self.transition(slot, SharedSlotState::Live)?;
        }
        Ok(())
    }

    /// 释放对象；只有没有 current/old pending 且全部 lease 结清时才允许。
    pub(crate) fn release(&mut self, handle: SharedHandle) -> Result<(), SharedHeapError> {
        let slot = self.access_slot(handle)?;
        if SharedSlotState::from_raw(self.slots[slot].state) != Some(SharedSlotState::Live) {
            return Err(SharedHeapError::new(
                "shared handle slot 状态不允许 release",
            ));
        }
        if self.slots[slot].old_payload != 0 {
            return Err(SharedHeapError::new(
                "shared handle 仍持有 forwarding grace 的旧 payload",
            ));
        }
        if !self.grace_settled(slot) {
            return Err(SharedHeapError::new(
                "shared handle 仍有未结清的 guard、lease 或票据",
            ));
        }
        let payload = SharedPayloadId::from_raw(self.slots[slot].current_payload);
        if payload.is_valid() {
            self.free_payload(payload)?;
        }
        let record = &mut self.slots[slot];
        record.current_payload = 0;
        record.payload_bytes = 0;
        record.owner_id = 0;
        record.block_id = 0;
        record.grace_epoch = 0;
        self.transition(slot, SharedSlotState::OwnedFree)?;
        let slot_id = u32::try_from(slot).expect("slot 下标适配 u32");
        self.free_slots.push(slot_id);
        Ok(())
    }

    /// 返回 slot 的状态；handle 过期时返回 `None`。
    pub(crate) fn slot_state(&self, handle: SharedHandle) -> Option<SharedSlotState> {
        let index = self.slot_index(handle).ok()?;
        SharedSlotState::from_raw(self.slots[index].state)
    }

    /// 返回 slot 的只读快照；handle 过期时返回 `None`。
    pub(crate) fn slot_record(&self, handle: SharedHandle) -> Option<SharedHandleSlot> {
        self.slot_index(handle).ok().map(|index| self.slots[index])
    }

    /// 返回当前 mark cycle epoch；尚未开始任何 cycle 时返回 `None`。
    pub(crate) const fn mark_cycle(&self) -> Option<u32> {
        if self.cycle_epoch == 0 {
            None
        } else {
            Some(self.cycle_epoch)
        }
    }

    /// 判断 handle 是否在本 mark cycle 被标记。
    ///
    /// 判定口径与 mark ticket 同源：`mark_epochs[slot]` 等于当前 cycle 即本 cycle 已标记。
    /// 尚未开始任何 cycle 时不存在标记结果，返回 `false`；handle 已释放、generation 过期或
    /// table 不匹配时失败，不按 slot 猜测对象。
    pub(crate) fn is_marked(&self, handle: SharedHandle) -> Result<bool, SharedHeapError> {
        let index = self.access_slot_raw(handle)?;
        Ok(self.cycle_epoch != 0 && self.mark_epochs[index] == self.cycle_epoch)
    }

    /// 判断某个 payload identity 是否仍然登记。
    pub(crate) fn payload_exists(&self, payload: SharedPayloadId) -> bool {
        self.payload_index(payload)
            .is_ok_and(|index| self.payloads[index].is_some())
    }

    /// 返回某个 payload 的字节内容。
    pub(crate) fn payload_content(&self, payload: SharedPayloadId) -> Option<&[u8]> {
        let index = self.payload_index(payload).ok()?;
        self.payloads[index].as_ref().map(SharedPayload::content)
    }

    /// 返回某个 payload 的登记记录。
    pub(crate) fn payload_record(&self, payload: SharedPayloadId) -> Option<&SharedPayload> {
        let index = self.payload_index(payload).ok()?;
        self.payloads[index].as_ref()
    }

    /// 返回 slot 的 current payload identity。
    pub(crate) fn current_payload(&self, handle: SharedHandle) -> Option<SharedPayloadId> {
        let index = self.slot_index(handle).ok()?;
        let payload = SharedPayloadId::from_raw(self.slots[index].current_payload);
        payload.is_valid().then_some(payload)
    }

    /// 返回 slot 正在 grace 的旧 payload identity。
    pub(crate) fn old_payload(&self, handle: SharedHandle) -> Option<SharedPayloadId> {
        let index = self.slot_index(handle).ok()?;
        let payload = SharedPayloadId::from_raw(self.slots[index].old_payload);
        payload.is_valid().then_some(payload)
    }

    /// 判断 grace 的全部 lease 是否已经结清（不含 grace 步数本身）。
    fn grace_settled(&self, slot: usize) -> bool {
        let record = &self.slots[slot];
        record.access_guards == 0
            && record.pin_leases == 0
            && record.mark_tickets == 0
            && record.forwarding_leases == 0
    }

    /// 校验 guard token 并解析出 payload 与访问范围。
    fn access_range(
        &self,
        access: SharedAccessToken,
        offset: u32,
        bytes: u32,
    ) -> Result<(usize, std::ops::Range<usize>), SharedHeapError> {
        let index = self.access_index(access.token)?;
        let record = self.accesses[index]
            .as_ref()
            .ok_or_else(|| SharedHeapError::new("shared access token 未建立"))?;
        if record.ended {
            return Err(SharedHeapError::new("shared access token 已结束"));
        }
        if record.handle != access.handle || record.payload != Some(access.payload) {
            return Err(SharedHeapError::new("shared access token 与 handle 不匹配"));
        }
        let payload_index = self.payload_index(access.payload)?;
        let payload = self.payloads[payload_index]
            .as_ref()
            .ok_or_else(|| SharedHeapError::new("shared access payload 已释放"))?;
        let end = offset
            .checked_add(bytes)
            .ok_or_else(|| SharedHeapError::new("shared payload 访问范围溢出"))?;
        if end > payload.bytes {
            return Err(SharedHeapError::new("shared payload 访问越界"));
        }
        let start = usize::try_from(offset).expect("payload 偏移适配 usize");
        let end = usize::try_from(end).expect("payload 偏移适配 usize");
        Ok((payload_index, start..end))
    }

    /// 取一个可发布的 slot；已释放的槽在下一次发布时推进 handle generation。
    fn take_slot(&mut self) -> Result<u32, SharedHeapError> {
        if let Some(slot) = self.free_slots.pop() {
            return Ok(slot);
        }
        let slot = u32::try_from(self.slots.len())
            .map_err(|_| SharedHeapError::new("shared handle slot 数量超过 u32"))?;
        self.slots.push(SharedHandleSlot::default());
        self.forward_leases.push(0);
        self.mark_epochs.push(0);
        Ok(slot)
    }

    /// 释放一个 payload 登记项并把槽放回 free 栈。
    fn free_payload(&mut self, payload: SharedPayloadId) -> Result<(), SharedHeapError> {
        let index = self.payload_index(payload)?;
        if self.payloads[index].is_none() {
            return Err(SharedHeapError::new("shared payload 已经释放"));
        }
        self.payloads[index] = None;
        let slot = u32::try_from(index).expect("payload 槽下标适配 u32");
        self.free_payloads.push(slot);
        Ok(())
    }

    /// 执行一次唯一允许的状态迁移。
    fn transition(&mut self, slot: usize, next: SharedSlotState) -> Result<(), SharedHeapError> {
        let current = SharedSlotState::from_raw(self.slots[slot].state)
            .ok_or_else(|| SharedHeapError::new("shared handle slot 状态判别值未登记"))?;
        if !SharedHeapRuntimeContract::allows_transition(current, next) {
            return Err(SharedHeapError::new(format!(
                "shared handle slot 不允许从 {} 迁移到 {}",
                current.name(),
                next.name()
            )));
        }
        self.slots[slot].state = next.raw();
        Ok(())
    }

    /// 校验 table 与 handle generation，返回 slot 下标。
    fn access_slot(&self, handle: SharedHandle) -> Result<usize, SharedHeapError> {
        let index = self.access_slot_raw(handle)?;
        match SharedSlotState::from_raw(self.slots[index].state) {
            Some(
                SharedSlotState::Live
                | SharedSlotState::Forwarding
                | SharedSlotState::Grace
                | SharedSlotState::Reclaimable,
            ) => Ok(index),
            _ => Err(SharedHeapError::new("shared handle slot 状态不允许访问")),
        }
    }

    /// 只校验 table 与 handle generation，不看 slot 状态。
    fn access_slot_raw(&self, handle: SharedHandle) -> Result<usize, SharedHeapError> {
        let index = self.slot_index(handle)?;
        match SharedSlotState::from_raw(self.slots[index].state) {
            Some(SharedSlotState::Free | SharedSlotState::OwnedFree) => {
                Err(SharedHeapError::new("shared handle slot 已释放"))
            }
            Some(_) => Ok(index),
            None => Err(SharedHeapError::new("shared handle slot 状态判别值未登记")),
        }
    }

    /// 校验 table 与 generation 并返回 slot 下标。
    fn slot_index(&self, handle: SharedHandle) -> Result<usize, SharedHeapError> {
        if handle.table() != self.table {
            return Err(SharedHeapError::new("shared handle table mismatch"));
        }
        let index = usize::try_from(handle.slot()).expect("slot 下标适配 usize");
        let record = self
            .slots
            .get(index)
            .ok_or_else(|| SharedHeapError::new("shared handle generation mismatch"))?;
        if record.generation != handle.generation() || handle.generation() == 0 {
            return Err(SharedHeapError::new("shared handle generation mismatch"));
        }
        Ok(index)
    }

    /// 校验 payload generation 并返回 payload 下标。
    fn payload_index(&self, payload: SharedPayloadId) -> Result<usize, SharedHeapError> {
        if !payload.is_valid() {
            return Err(SharedHeapError::new(
                "shared handle payload generation mismatch",
            ));
        }
        let index = usize::try_from(payload.slot()).expect("payload 槽下标适配 usize");
        let generation = self
            .payload_generations
            .get(index)
            .ok_or_else(|| SharedHeapError::new("shared handle payload generation mismatch"))?;
        if *generation != payload.generation() {
            return Err(SharedHeapError::new(
                "shared handle payload generation mismatch",
            ));
        }
        Ok(index)
    }

    /// 校验 access token 并返回下标。
    fn access_index(&self, token: u32) -> Result<usize, SharedHeapError> {
        if token == 0 {
            return Err(SharedHeapError::new("shared access token 必须非零"));
        }
        let index = usize::try_from(token - 1).expect("token 下标适配 usize");
        if index >= self.accesses.len() {
            return Err(SharedHeapError::new("shared access token 未建立"));
        }
        Ok(index)
    }
}
