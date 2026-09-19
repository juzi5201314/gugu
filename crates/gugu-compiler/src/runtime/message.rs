//! remote return message、link 编码、producer staging 与 consumer-side 聚合。
//!
//! 消息只携带稳定的 descriptor index、slot/block/line/extent index、generation、epoch、
//! bytes 与 integrity；任何 managed object 裸地址、未 pin 的 interior pointer 或可能在
//! evacuation 中更新的 field 地址都不允许进入消息。链上的 `next` 与 free-list link 都是
//! per-domain secret 编码的整数，解码后同时校验 canonical offset、alignment、所属 slab、
//! generation 与 class。

use std::sync::atomic::{AtomicU64, Ordering};

use super::barrier_schema::MessageFamilyTag;
use super::inbox::{OwnerInbox, ShardIndex};
use super::local_heap::{BlockRef, ManagedBlockId};
use super::mark_schema::GcCreditId;
use super::shared_heap_schema::SharedPayloadId;
use super::size_class::RuntimeSizeClassId;
use super::slab::{
    Epoch, MemoryDomainId, OwnerGeneration, OwnerId, OwnerToken, RawInvariant, RouteKey,
    SlabDescriptor, SlabDescriptorId, SlabGeneration,
};

/// 空链指针；任何编码后的 link 都不会等于它。
pub(crate) const NULL_LINK: u64 = u64::MAX;

/// link 编码失败的稳定分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkError {
    /// 空链被当作有效 link 使用。
    Null,
    /// 校验位不匹配，link 字被破坏。
    Checksum,
    /// 偏移越过 span 范围。
    OutOfRange { offset: u64, extent: u64 },
    /// 偏移不满足 alignment 或 stride 的整数倍。
    Alignment { offset: u64 },
    /// link 指向了过期 generation 的 slot。
    Generation { expected: u64, actual: u64 },
    /// link 来自其它 slab 或其它 class。
    Foreign { descriptor: u32 },
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Null => formatter.write_str("空 chain 指针"),
            Self::Checksum => formatter.write_str("link 校验位不匹配"),
            Self::OutOfRange { offset, extent } => {
                write!(formatter, "link 偏移 {offset} 越过 span 长度 {extent}")
            }
            Self::Alignment { offset } => write!(formatter, "link 偏移 {offset} 不满足对齐"),
            Self::Generation { expected, actual } => write!(
                formatter,
                "link 指向过期 generation：期望 {expected}，实际 {actual}"
            ),
            Self::Foreign { descriptor } => {
                write!(formatter, "link 不属于 slab {descriptor}")
            }
        }
    }
}

/// free 链完整性走查的失败分类；link 层错误保持原样，跨 slot 与长度问题单独登记。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChainWalkError {
    /// 某一跳的 encoded link 解码失败。
    Link(LinkError),
    /// 链跳转落在本 slab 的 slot 范围之外。
    OutsideSpan,
    /// 走查长度超过 span 容量，链上存在环。
    Overflow,
}

impl std::fmt::Display for ChainWalkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link(error) => std::fmt::Display::fmt(error, formatter),
            Self::OutsideSpan => formatter.write_str("free 链跳转到本 slab 之外的 slot"),
            Self::Overflow => formatter.write_str("free 链长度超过 span 容量"),
        }
    }
}

/// per-domain link 编码器：用 secret 与 slot 偏移派生 mask。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LinkCodec {
    secret: [u8; 32],
}

impl LinkCodec {
    /// 空 link 字。
    pub(crate) const NULL: u64 = NULL_LINK;

    /// 用 per-domain secret 创建编码器；secret 必须来自 runtime entropy 或显式种子。
    pub(crate) fn new(secret: [u8; 32]) -> Self {
        Self { secret }
    }

    /// 把 NULL 归一为 `None`。
    pub(crate) const fn normalize(&self, word: u64) -> Option<u64> {
        if word == NULL_LINK { None } else { Some(word) }
    }

    /// 编码一个 slot 偏移；编码结果绝不等于 NULL。
    pub(crate) fn encode(
        &self,
        descriptor: &SlabDescriptor,
        offset: u64,
    ) -> Result<u64, RawInvariant> {
        if offset >= descriptor.span_extent {
            return Err(RawInvariant::new("link 编码的偏移越过 span"));
        }
        if !descriptor.contains_offset(offset, u64::from(descriptor.alignment)) {
            return Err(RawInvariant::new("link 编码的偏移不满足对齐"));
        }
        let offset = u32::try_from(offset).map_err(|_| RawInvariant::new("偏移超出编码宽度"))?;
        let tag = self.tag(descriptor, u64::from(offset));
        let checksum = self.checksum(u64::from(offset), tag);
        let word = u64::from(offset) | (u64::from(tag) << 32) | (u64::from(checksum) << 48);
        if word == NULL_LINK {
            return Err(RawInvariant::new("link 编码与空指针冲突"));
        }
        Ok(word)
    }

    /// 编码一个「校验位合法但 tag 被替换」的伪造 link；只供链完整性测试构造攻击字。
    #[cfg(test)]
    pub(crate) fn forge_word(&self, offset: u64, tag: u16) -> Result<u64, RawInvariant> {
        let offset = u32::try_from(offset).map_err(|_| RawInvariant::new("偏移超出编码宽度"))?;
        let checksum = self.checksum(u64::from(offset), tag);
        let word = u64::from(offset) | (u64::from(tag) << 32) | (u64::from(checksum) << 48);
        if word == NULL_LINK {
            return Err(RawInvariant::new("link 编码与空指针冲突"));
        }
        Ok(word)
    }

    /// 解码一个 slot 偏移并校验归属、generation、class 与对齐。
    pub(crate) fn decode(&self, descriptor: &SlabDescriptor, word: u64) -> Result<u32, LinkError> {
        if word == NULL_LINK {
            return Err(LinkError::Null);
        }
        let offset = word & 0xFFFF_FFFF;
        let tag = ((word >> 32) & 0xFFFF) as u16;
        let checksum = (word >> 48) as u16;
        if checksum != self.checksum(offset, tag) {
            return Err(LinkError::Checksum);
        }
        if offset >= descriptor.span_extent {
            return Err(LinkError::OutOfRange {
                offset,
                extent: descriptor.span_extent,
            });
        }
        let index = descriptor.index_of(offset);
        if u64::from(index) * u64::from(descriptor.slot_stride) != offset
            || offset & (u64::from(descriptor.alignment) - 1) != 0
        {
            return Err(LinkError::Alignment { offset });
        }
        if tag != self.tag(descriptor, offset) {
            if descriptor.generation.raw() > 0
                && tag
                    == self.tag_with_generation(descriptor, offset, descriptor.generation.raw() - 1)
            {
                return Err(LinkError::Generation {
                    expected: descriptor.generation.raw(),
                    actual: descriptor.generation.raw() - 1,
                });
            }
            return Err(LinkError::Foreign {
                descriptor: descriptor.extent.raw(),
            });
        }
        Ok(index)
    }

    fn tag(&self, descriptor: &SlabDescriptor, offset: u64) -> u16 {
        self.tag_with_generation(descriptor, offset, descriptor.generation.raw())
    }

    fn tag_with_generation(
        &self,
        descriptor: &SlabDescriptor,
        offset: u64,
        generation: u64,
    ) -> u16 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-raw-link-tag-v1");
        hasher.update(&self.secret);
        hasher.update(&descriptor.class.raw().to_le_bytes());
        hasher.update(&descriptor.slot_stride.to_le_bytes());
        hasher.update(&generation.to_le_bytes());
        hasher.update(&offset.to_le_bytes());
        let digest = hasher.finalize();
        u16::from_le_bytes([digest.as_bytes()[0], digest.as_bytes()[1]])
    }

    fn checksum(&self, offset: u64, tag: u16) -> u16 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-raw-link-sum-v1");
        hasher.update(&self.secret);
        hasher.update(&offset.to_le_bytes());
        hasher.update(&tag.to_le_bytes());
        let digest = hasher.finalize();
        u16::from_le_bytes([digest.as_bytes()[2], digest.as_bytes()[3]])
    }
}

/// return unit 的种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReturnKind {
    /// raw slab 的单个 slot。
    RawSlot,
    /// stack arena 的 span。
    StackSpan,
    /// `ResourceCell` 的 release。
    ResourceRelease,
    /// 完整空 block。
    HeapBlock,
    /// block 内连续 free line-run。
    HeapLineRun,
    /// 独立 mapping 的 extent。
    Extent,
    /// wait-node 的跨 owner 归还。
    WaitNode,
    /// 整个 managed arena。
    HeapArena,
    /// 跨连续 block 的 large-object mapping。
    LargeMapping,
}

impl ReturnKind {
    /// 返回种类名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::RawSlot => "RawSlot",
            Self::StackSpan => "StackSpan",
            Self::ResourceRelease => "ResourceRelease",
            Self::HeapBlock => "HeapBlock",
            Self::HeapLineRun => "HeapLineRun",
            Self::Extent => "Extent",
            Self::WaitNode => "WaitNode",
            Self::HeapArena => "HeapArena",
            Self::LargeMapping => "LargeMapping",
        }
    }

    fn raw(self) -> u8 {
        match self {
            Self::RawSlot => 0,
            Self::StackSpan => 1,
            Self::ResourceRelease => 2,
            Self::HeapBlock => 3,
            Self::HeapLineRun => 4,
            Self::Extent => 5,
            Self::WaitNode => 6,
            Self::HeapArena => 7,
            Self::LargeMapping => 8,
        }
    }

    fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => Self::RawSlot,
            1 => Self::StackSpan,
            2 => Self::ResourceRelease,
            3 => Self::HeapBlock,
            4 => Self::HeapLineRun,
            5 => Self::Extent,
            6 => Self::WaitNode,
            7 => Self::HeapArena,
            8 => Self::LargeMapping,
            _ => return None,
        })
    }
}

/// 消息在生命周期内的状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageState {
    /// 已进入 producer staging，尚未发布。
    Staged,
    /// 已进入 owner inbox。
    Published,
    /// 因 generation/topology 变化被转发。
    Forwarded,
    /// 已被目标 owner 消费。
    Consumed,
}

impl MessageState {
    /// 返回状态名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Staged => "Staged",
            Self::Published => "Published",
            Self::Forwarded => "Forwarded",
            Self::Consumed => "Consumed",
        }
    }
}

/// 逻辑消息记录；所有身份字段都是整数序号，不含任何地址。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReturnMessage {
    /// raw intrusive link；只存在于 non-moving message/slab storage。
    pub(crate) next: Option<u32>,
    pub(crate) target: OwnerToken,
    pub(crate) kind: ReturnKind,
    pub(crate) descriptor: SlabDescriptorId,
    pub(crate) unit: u32,
    pub(crate) bytes: u32,
    pub(crate) source_epoch: Epoch,
    pub(crate) state: MessageState,
    /// generation、class、owner 与 link 的校验信息。
    pub(crate) integrity: IntegrityTag,
}

/// 一条 remembered-set card batch；GC 工作消息族，与 return 消息共用传输与 grace。
///
/// 只携带稳定 arena descriptor、generation、card 区间、cycle epoch 与 bytes：card table
/// 只能由 arena allocation owner 写入，因此 batch 既不携带 field 地址，也不携带 managed
/// pointer。`card_count == 0` 表示该 batch 只宣告 epoch 前进，不置位任何 card。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CardMarkBatch {
    /// raw intrusive link；只存在于 non-moving message storage。
    pub(crate) next: Option<u32>,
    /// arena allocation owner 的稳定身份。
    pub(crate) target: OwnerToken,
    /// arena descriptor 的稠密编号。
    pub(crate) arena: SlabDescriptorId,
    /// arena 的 generation；回收后旧 batch 必须被拒绝。
    pub(crate) arena_generation: u32,
    /// batch 覆盖的起始 card 序号。
    pub(crate) card_start: u32,
    /// batch 覆盖的 card 数量；连续区间。
    pub(crate) card_count: u32,
    /// 产生这些键的 GC cycle epoch。
    pub(crate) cycle_epoch: u64,
    /// 本次 batch 触及的 distinct card 字节数。
    pub(crate) bytes: u32,
    pub(crate) state: MessageState,
    /// arena generation、owner 与 cycle epoch 的校验信息。
    pub(crate) integrity: IntegrityTag,
}

/// 一条 `RegionTransfer` 消息：把私有 region 的整体所有权移交给目标 owner。
///
/// 与 `CardMarkBatch` 一样，消息只携带逻辑身份（region 序号、generation、type summary 编号、
/// bytes、export summary 与 cycle epoch），不携带任何地址；目标 owner 通过自己的 registry
/// 采纳这条 region，物理地址仍然只在 owner-local 状态里出现。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionTransferBatch {
    /// raw intrusive link；只存在于 non-moving message storage。
    pub(crate) next: Option<u32>,
    /// 接收 region 的 owner 稳定身份。
    pub(crate) target: OwnerToken,
    /// 发出 region 的 owner 身份；接收方据此回执并移交账本。
    pub(crate) source: OwnerId,
    /// 被移交的 region 序号；只在发送者 registry 内稠密。
    pub(crate) region: u32,
    /// 发送者 region 的 generation；回收后旧消息必须被拒绝。
    pub(crate) region_generation: u32,
    /// region 内对象的 stable type summary 编号。
    pub(crate) type_summary: SlabDescriptorId,
    /// region 的 payload 字节数。
    pub(crate) bytes: u32,
    /// 容量 class 下标；接收方按同一档阶梯建立 descriptor。
    pub(crate) capacity_class: u32,
    /// 编译器声明的 export summary 位。
    pub(crate) export_state: u8,
    /// 发送时 runtime 观察到的 export summary 位。
    pub(crate) observed: u8,
    /// 产生这次移交的 transfer epoch。
    pub(crate) cycle_epoch: u64,
    pub(crate) state: MessageState,
    /// region 身份、owner 与 cycle epoch 的校验信息。
    pub(crate) integrity: IntegrityTag,
}

impl RegionTransferBatch {
    /// export summary 的并集；门禁检查使用它。
    pub(crate) const fn export(&self) -> u8 {
        self.export_state | self.observed
    }
}

/// 一条 mark ticket 的目标身份。
///
/// 两种形式都不含 managed 地址：owner-local 目标用 arena descriptor + header 偏移，跨 owner
/// 共享目标用 stable handle 的 `table/slot/generation`。目标种类的判别值参与 integrity，
/// 因此不能靠猜测字段来源来解释一条 ticket。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkTarget {
    /// 目标对象在目标 owner 的本地 arena 内。
    Local {
        /// 目标对象所在 arena 的 descriptor 稠密编号。
        arena: SlabDescriptorId,
        /// 目标对象 header 在 arena 内的字节偏移；arena 不超过 2 MiB，适配 u32。
        offset: u32,
        /// 目标对象所属 block 的 generation；目标 owner 解析 block 后必须校验它。
        block_generation: u32,
    },
    /// 目标对象是跨 owner 发布的 shared payload。
    Shared {
        /// stable handle 的逻辑表身份。
        handle_table: u32,
        /// stable handle 的 slot 编号。
        handle_slot: u32,
        /// stable handle 的 generation；重放旧 handle 必然失败。
        handle_generation: u32,
    },
}

impl MarkTarget {
    /// 本地偏移目标的种类判别值。
    pub(crate) const LOCAL_KIND: u32 = 1;
    /// 共享 handle 目标的种类判别值。
    pub(crate) const SHARED_KIND: u32 = 2;

    /// 返回目标种类判别值。
    pub(crate) const fn kind(self) -> u32 {
        match self {
            Self::Local { .. } => Self::LOCAL_KIND,
            Self::Shared { .. } => Self::SHARED_KIND,
        }
    }

    /// 返回 lane 里的身份低位：local 为 arena descriptor，shared 为 handle table。
    pub(crate) const fn identity_low(self) -> u32 {
        match self {
            Self::Local { arena, .. } => arena.raw(),
            Self::Shared { handle_table, .. } => handle_table,
        }
    }

    /// 返回 lane 里的身份高位：local 为 header 偏移，shared 为 handle slot。
    pub(crate) const fn identity_high(self) -> u32 {
        match self {
            Self::Local { offset, .. } => offset,
            Self::Shared { handle_slot, .. } => handle_slot,
        }
    }

    /// 返回 lane 里的 generation：local 为 block generation，shared 为 handle generation。
    pub(crate) const fn generation(self) -> u32 {
        match self {
            Self::Local {
                block_generation, ..
            } => block_generation,
            Self::Shared {
                handle_generation, ..
            } => handle_generation,
        }
    }

    /// 由判别值与身份车道还原目标；未知判别值或非法身份直接拒绝。
    pub(crate) fn decode(kind: u32, low: u32, high: u32, generation: u32) -> Option<Self> {
        match kind {
            Self::LOCAL_KIND => Some(Self::Local {
                arena: SlabDescriptorId::from_raw(low),
                offset: high,
                block_generation: generation,
            }),
            Self::SHARED_KIND => Some(Self::Shared {
                handle_table: low,
                handle_slot: high,
                handle_generation: generation,
            }),
            _ => None,
        }
    }
}

/// 一条跨 owner 的 GC 工作消息：把一个待标记对象交给它的 arena owner。
///
/// 与 `CardMarkBatch`/`RegionTransferBatch` 共用同一条传输、staging 与 grace；区别只在
/// 车道解释与 integrity 派生键。消息只携带稳定身份——目标 owner token、`MarkTarget`、
/// 产生引用的 source block、cycle/topology epoch、owner credit 与 bytes——不含任何 managed
/// 地址：目标 owner 用自己的 arena 或 SharedHeap 表反查目标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkTicket {
    /// raw intrusive link；只存在于 non-moving message storage。
    pub(crate) next: Option<u32>,
    /// 目标 arena owner 的稳定身份。
    pub(crate) target: OwnerToken,
    /// 目标对象身份。
    pub(crate) mark: MarkTarget,
    /// 产生这条 mark 工作的 source block：全局块身份（`descriptor * 64 + block`）。
    ///
    /// 消费端据此解析来源 owner 并确认来源块仍然存在；只带 arena 内下标的编码会被拒绝。
    pub(crate) source_block: u32,
    /// 产生这条 ticket 的 GC cycle epoch。
    pub(crate) cycle_epoch: u64,
    /// producer topology epoch；拓扑变化后旧 ticket 必须被拒绝。
    pub(crate) topology_epoch: u32,
    /// 该 ticket 占用的共享 credit 身份；consume 后由还款收口。
    pub(crate) credit: GcCreditId,
    /// 目标对象占用的字节数。
    pub(crate) bytes: u32,
    pub(crate) state: MessageState,
    /// 目标身份、cycle/topology epoch 与 credit 的校验信息。
    pub(crate) integrity: IntegrityTag,
}

/// 一条跨 owner 的 GC 工作消息：把一个 shared handle 的搬迁结果交给目标 owner。
///
/// 与其它 GC 消息共用同一条传输、staging 与 grace；只携带稳定 handle 身份、两条 payload
/// identity、forward generation、cycle/topology epoch 与 bytes。`old_payload` 让目标 owner
/// 知道哪个 payload 进入 forwarding grace，`new_payload` 是搬迁后必须解析到的 payload；
/// 两者都是逻辑身份，不是地址。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleForward {
    /// raw intrusive link；只存在于 non-moving message storage。
    pub(crate) next: Option<u32>,
    /// 目标 owner 的稳定身份（payload 与 block 的 owner，也是该 block card table 的 manager）。
    ///
    /// 搬迁通知的投递目标与共享 block 的另外两条通道（mark ticket、card batch）同源：三者都指向
    /// block 的 payload owner，因为只有它能在自己的消费上下文里结清 lease、推进 grace 并回收旧
    /// payload。契约里没有第二份「handle 表 owner」身份，未来按 owner 分表时表 owner 就是 payload
    /// owner，两种读法收敛。
    pub(crate) target: OwnerToken,
    /// stable handle 的逻辑表身份。
    pub(crate) handle_table: u32,
    /// stable handle 的 slot 编号。
    pub(crate) handle_slot: u32,
    /// stable handle 的 generation；目标 owner 必须按它校验 handle 仍然有效。
    pub(crate) handle_generation: u32,
    /// 本次搬迁推进到的 forward generation；跳号或回退都必须被拒绝。
    pub(crate) forward_generation: u32,
    /// 搬迁前的 payload identity；它进入 forwarding grace。
    pub(crate) old_payload: SharedPayloadId,
    /// 搬迁后的 payload identity；linearize 之后必须解析到它。
    pub(crate) new_payload: SharedPayloadId,
    /// 搬迁发生的 GC cycle epoch。
    pub(crate) cycle_epoch: u64,
    /// producer topology epoch；拓扑变化后旧消息必须被拒绝。
    pub(crate) topology_epoch: u32,
    /// payload 的逻辑字节数。
    pub(crate) bytes: u32,
    pub(crate) state: MessageState,
    /// handle 身份、两条 payload identity 与 epoch 的校验信息。
    pub(crate) integrity: IntegrityTag,
}

/// 一条跨 owner 的 GC 工作消息：把一个 block 对的边增删交给 target 的 manager。
///
/// 与其它 GC 消息共用同一条传输、staging 与 grace；只携带稳定 `BlockRef` 身份、block 对内
/// 的发布序号、signed 差量与共享 credit。target 侧按序号顺序应用，未来序号必须保留原 node 与
/// credit 等缺口补齐，因此旧/重复序号是真正的不变量失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdgeDelta {
    /// raw intrusive link；只存在于 non-moving message storage。
    pub(crate) next: Option<u32>,
    /// destination block 当前 manager 的稳定身份。
    pub(crate) target: OwnerToken,
    /// 产生这条边的 source block。
    pub(crate) source: BlockRef,
    /// 收到这条边的 destination block。
    pub(crate) destination: BlockRef,
    /// 聚合到该记录的 cycle epoch。
    pub(crate) cycle_epoch: u64,
    /// 发布时的 producer topology epoch。
    pub(crate) topology_epoch: u32,
    /// 该 block 对内的发布序号；target 必须按它顺序应用。
    pub(crate) sequence: u64,
    /// signed 净差量。
    pub(crate) delta: i64,
    /// 该记录占用的共享 credit 身份。
    pub(crate) credit: GcCreditId,
    /// 本记录自身占用的字节数。
    pub(crate) bytes: u32,
    pub(crate) state: MessageState,
    /// 全部身份、序号、差量与 credit 的校验信息。
    pub(crate) integrity: IntegrityTag,
}

/// 消息 integrity 校验信息。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntegrityTag {
    pub(crate) generation: SlabGeneration,
    pub(crate) class: RuntimeSizeClassId,
    pub(crate) owner_id: OwnerId,
    pub(crate) route_key: RouteKey,
    pub(crate) checksum: u32,
}

impl IntegrityTag {
    /// 用 per-domain secret 与全部身份字段计算校验值。
    pub(crate) fn compute(secret: &[u8; 32], message: &ReturnMessage) -> u32 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-return-integrity-v1");
        hasher.update(secret);
        hasher.update(&message.target.domain.raw().to_le_bytes());
        hasher.update(&message.target.owner_id.raw().to_le_bytes());
        hasher.update(&message.target.generation.raw().to_le_bytes());
        hasher.update(&message.target.route_key.raw().to_le_bytes());
        hasher.update(&message.kind.raw().to_le_bytes());
        hasher.update(&message.descriptor.raw().to_le_bytes());
        hasher.update(&message.unit.to_le_bytes());
        hasher.update(&message.bytes.to_le_bytes());
        hasher.update(&message.source_epoch.raw().to_le_bytes());
        hasher.update(&message.integrity.generation.raw().to_le_bytes());
        hasher.update(&message.integrity.class.raw().to_le_bytes());
        let digest = hasher.finalize();
        u32::from_le_bytes([
            digest.as_bytes()[0],
            digest.as_bytes()[1],
            digest.as_bytes()[2],
            digest.as_bytes()[3],
        ])
    }
    /// 用 per-domain secret 与全部身份字段计算 card batch 的校验值。
    pub(crate) fn compute_card_mark(secret: &[u8; 32], batch: &CardMarkBatch) -> u32 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-card-mark-integrity-v1");
        hasher.update(secret);
        hasher.update(&batch.target.domain.raw().to_le_bytes());
        hasher.update(&batch.target.owner_id.raw().to_le_bytes());
        hasher.update(&batch.target.generation.raw().to_le_bytes());
        hasher.update(&batch.target.route_key.raw().to_le_bytes());
        hasher.update(&MessageFamilyTag::CardMark.raw().to_le_bytes());
        hasher.update(&batch.arena.raw().to_le_bytes());
        hasher.update(&batch.arena_generation.to_le_bytes());
        hasher.update(&batch.card_start.to_le_bytes());
        hasher.update(&batch.card_count.to_le_bytes());
        hasher.update(&batch.cycle_epoch.to_le_bytes());
        hasher.update(&batch.bytes.to_le_bytes());
        // 只有 card batch 自己的身份进入摘要：`integrity.generation`/`class` 是 return 消息
        // 的载入键，对 card 族没有语义，因此不参与校验，避免载入键与记录内容互相绑定。
        let digest = hasher.finalize();
        u32::from_le_bytes([
            digest.as_bytes()[0],
            digest.as_bytes()[1],
            digest.as_bytes()[2],
            digest.as_bytes()[3],
        ])
    }

    /// 用 per-domain secret 与 `RegionTransfer` 的全部身份字段计算校验值。
    pub(crate) fn compute_region_transfer(secret: &[u8; 32], batch: &RegionTransferBatch) -> u32 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-region-transfer-integrity-v1");
        hasher.update(secret);
        hasher.update(&batch.target.domain.raw().to_le_bytes());
        hasher.update(&batch.target.owner_id.raw().to_le_bytes());
        hasher.update(&batch.target.generation.raw().to_le_bytes());
        hasher.update(&batch.target.route_key.raw().to_le_bytes());
        hasher.update(&MessageFamilyTag::RegionTransfer.raw().to_le_bytes());
        hasher.update(&batch.source.raw().to_le_bytes());
        hasher.update(&batch.region.to_le_bytes());
        hasher.update(&batch.region_generation.to_le_bytes());
        hasher.update(&batch.capacity_class.to_le_bytes());
        hasher.update(&batch.type_summary.raw().to_le_bytes());
        hasher.update(&batch.bytes.to_le_bytes());
        hasher.update(&[batch.export_state, batch.observed]);
        hasher.update(&batch.cycle_epoch.to_le_bytes());
        let digest = hasher.finalize();
        u32::from_le_bytes([
            digest.as_bytes()[4],
            digest.as_bytes()[5],
            digest.as_bytes()[6],
            digest.as_bytes()[7],
        ])
    }

    /// 用 per-domain secret 与 `MarkTicket` 的全部身份字段计算校验值。
    pub(crate) fn compute_mark_ticket(secret: &[u8; 32], ticket: &MarkTicket) -> u32 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-mark-ticket-integrity-v1");
        hasher.update(secret);
        hasher.update(&ticket.target.domain.raw().to_le_bytes());
        hasher.update(&ticket.target.owner_id.raw().to_le_bytes());
        hasher.update(&ticket.target.generation.raw().to_le_bytes());
        hasher.update(&ticket.target.route_key.raw().to_le_bytes());
        hasher.update(&MessageFamilyTag::MarkTicket.raw().to_le_bytes());
        hasher.update(&ticket.mark.kind().to_le_bytes());
        hasher.update(&ticket.mark.identity_low().to_le_bytes());
        hasher.update(&ticket.mark.identity_high().to_le_bytes());
        hasher.update(&ticket.mark.generation().to_le_bytes());
        hasher.update(&ticket.source_block.to_le_bytes());
        hasher.update(&ticket.cycle_epoch.to_le_bytes());
        hasher.update(&ticket.topology_epoch.to_le_bytes());
        hasher.update(&ticket.credit.raw().to_le_bytes());
        hasher.update(&ticket.bytes.to_le_bytes());
        // 取字节 8..12：card 族用 0..4、region 族用 4..8，三族不共用摘要前缀。
        let digest = hasher.finalize();
        u32::from_le_bytes([
            digest.as_bytes()[8],
            digest.as_bytes()[9],
            digest.as_bytes()[10],
            digest.as_bytes()[11],
        ])
    }

    /// 用 per-domain secret 与 `EdgeDelta` 的全部身份字段计算校验值。
    pub(crate) fn compute_edge_delta(secret: &[u8; 32], delta: &EdgeDelta) -> u32 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-edge-delta-integrity-v1");
        hasher.update(secret);
        hasher.update(&delta.target.domain.raw().to_le_bytes());
        hasher.update(&delta.target.owner_id.raw().to_le_bytes());
        hasher.update(&delta.target.generation.raw().to_le_bytes());
        hasher.update(&delta.target.route_key.raw().to_le_bytes());
        hasher.update(&MessageFamilyTag::EdgeDelta.raw().to_le_bytes());
        hasher.update(&delta.source.id.0.to_le_bytes());
        hasher.update(&delta.source.generation.to_le_bytes());
        hasher.update(&delta.destination.id.0.to_le_bytes());
        hasher.update(&delta.destination.generation.to_le_bytes());
        hasher.update(&delta.cycle_epoch.to_le_bytes());
        hasher.update(&delta.topology_epoch.to_le_bytes());
        hasher.update(&delta.sequence.to_le_bytes());
        hasher.update(&delta.delta.to_le_bytes());
        hasher.update(&delta.credit.raw().to_le_bytes());
        hasher.update(&delta.bytes.to_le_bytes());
        // 取字节 12..16：card 族用 0..4、region 族用 4..8、mark 族用 8..12，四族不共用前缀。
        let digest = hasher.finalize();
        u32::from_le_bytes([
            digest.as_bytes()[12],
            digest.as_bytes()[13],
            digest.as_bytes()[14],
            digest.as_bytes()[15],
        ])
    }

    /// 校验 edge delta 的 checksum 与本记录的其他身份字段一致。
    pub(crate) fn verify_edge_delta(
        &self,
        secret: &[u8; 32],
        delta: &EdgeDelta,
    ) -> Result<(), RawInvariant> {
        if self.checksum != Self::compute_edge_delta(secret, delta) {
            return Err(RawInvariant::new("edge delta integrity 校验失败"));
        }
        Ok(())
    }

    /// 校验 mark ticket 的 checksum 与本记录的其他身份字段一致。
    pub(crate) fn verify_mark_ticket(
        &self,
        secret: &[u8; 32],
        ticket: &MarkTicket,
    ) -> Result<(), RawInvariant> {
        if self.checksum != Self::compute_mark_ticket(secret, ticket) {
            return Err(RawInvariant::new("mark ticket integrity 校验失败"));
        }
        Ok(())
    }

    /// 用 per-domain secret 与 `HandleForward` 的全部身份字段计算校验值。
    ///
    /// 域分离键与 mark/edge 族不同，因此同一条消息不能被解释成另一族而通过校验。
    pub(crate) fn compute_handle_forward(secret: &[u8; 32], message: &HandleForward) -> u32 {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-handle-forward-integrity-v1");
        hasher.update(secret);
        hasher.update(&message.target.domain.raw().to_le_bytes());
        hasher.update(&message.target.owner_id.raw().to_le_bytes());
        hasher.update(&message.target.generation.raw().to_le_bytes());
        hasher.update(&message.target.route_key.raw().to_le_bytes());
        hasher.update(&MessageFamilyTag::HandleForward.raw().to_le_bytes());
        hasher.update(&message.handle_table.to_le_bytes());
        hasher.update(&message.handle_slot.to_le_bytes());
        hasher.update(&message.handle_generation.to_le_bytes());
        hasher.update(&message.forward_generation.to_le_bytes());
        hasher.update(&message.old_payload.raw().to_le_bytes());
        hasher.update(&message.new_payload.raw().to_le_bytes());
        hasher.update(&message.cycle_epoch.to_le_bytes());
        hasher.update(&message.topology_epoch.to_le_bytes());
        hasher.update(&message.bytes.to_le_bytes());
        // 取字节 12..16：card 族用 0..4、region 族用 4..8、mark 族用 8..12，四族不共用前缀。
        let digest = hasher.finalize();
        u32::from_le_bytes([
            digest.as_bytes()[12],
            digest.as_bytes()[13],
            digest.as_bytes()[14],
            digest.as_bytes()[15],
        ])
    }

    /// 校验 handle forward 的 checksum 与本记录的其他身份字段一致。
    pub(crate) fn verify_handle_forward(
        &self,
        secret: &[u8; 32],
        message: &HandleForward,
    ) -> Result<(), RawInvariant> {
        if self.checksum != Self::compute_handle_forward(secret, message) {
            return Err(RawInvariant::new("handle forward integrity 校验失败"));
        }
        Ok(())
    }

    /// 校验 card batch 的 checksum 与本记录的其他身份字段一致。
    pub(crate) fn verify_card_mark(
        &self,
        secret: &[u8; 32],
        batch: &CardMarkBatch,
    ) -> Result<(), RawInvariant> {
        if self.checksum != Self::compute_card_mark(secret, batch) {
            return Err(RawInvariant::new("card mark batch integrity 校验失败"));
        }
        Ok(())
    }

    /// 校验 checksum 与本记录的其他身份字段一致。
    pub(crate) fn verify(
        &self,
        secret: &[u8; 32],
        message: &ReturnMessage,
    ) -> Result<(), RawInvariant> {
        if self.checksum != Self::compute(secret, message) {
            return Err(RawInvariant::new("return message integrity 校验失败"));
        }
        Ok(())
    }
}

/// 一个 return node 的稠密编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ReturnNodeId(u32);

impl ReturnNodeId {
    /// 由编号原值构造；`ReturnNodePool` 保证编号不越界，测试与诊断复用同一解释。
    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 返回作为下标的编号。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// 一个 non-moving message node 的物理车道。
///
/// 每个车道都是 64-bit 整数，承载逻辑序号、generation、epoch、bytes 或校验值；不存在的
/// 地址字段与 managed pointer 字段由模型 verifier 在 schema 层拒绝。
///
/// 车道分配：`next` 为 encoded node link 或 NULL；`owner_id`、`generation`、`route_key`
/// 保存目标 owner 身份；`descriptor_unit` 为 `descriptor(32) | unit(32)`；
/// `bytes_epoch` 为 `bytes(32) | source_epoch(32)`；`state_kind` 为
/// `state_kind` 为 `state(8) | kind(8) | domain(8) | family(8) | 保留`；
/// `payload_low`/`payload_high` 为消息族专属车道（card batch 用
/// `arena(32) | card_start(32)` 与 `arena_generation(32) | card_count(32)`；mark ticket 用
/// `cycle_epoch(64)` 与 `target_block_generation(32) | source_block(32)`，并把
/// `descriptor_unit` 解释成 `target_arena(32) | target_offset(32)`、`bytes_epoch` 解释成
/// `bytes(32) | topology_epoch(32)`、`credit` 车道解释成 `GcCreditId`；edge delta 用
/// `source(32) | destination(32)`、`source_generation(32) | destination_generation(32)`
/// 与 `cycle_epoch`，并把 `sequence`/`delta`/`credit` 三条车道填满）；
/// `integrity` 保存 integrity checksum；`reuse` 保存
/// `Free`/`InUse` 复用标记。车道不足一个 cache line 时补齐，node stride 因此是
/// `RETURN_NODE_BYTES`。
#[repr(align(64))]
#[derive(Debug, Default)]
pub(crate) struct ReturnNode {
    next: AtomicU64,
    owner_id: AtomicU64,
    generation: AtomicU64,
    route_key: AtomicU64,
    descriptor_unit: AtomicU64,
    bytes_epoch: AtomicU64,
    state_kind: AtomicU64,
    payload_low: AtomicU64,
    payload_high: AtomicU64,
    integrity: AtomicU64,
    reuse: AtomicU64,
    /// block 对内的发布序号；只有 edge delta 族使用。
    sequence: AtomicU64,
    /// signed 净差量；只有 edge delta 族使用。
    delta: AtomicU64,
    /// credit 身份 `generation(32) | slot(32)`；mark ticket 与 edge delta 共用。
    credit: AtomicU64,
    /// free stack 专用 link；与 message chain 的 `next` 分离，避免两种语义互相覆盖。
    free_next: AtomicU64,
}

/// node 的字节大小与对齐；必须能容纳全部车道。
pub(crate) const RETURN_NODE_BYTES: u32 = 128;

/// node 的对齐要求。
pub(crate) const RETURN_NODE_ALIGN: u32 = 64;

/// node 的复用标记取值。
const NODE_FREE: u64 = 0;
const NODE_IN_USE: u64 = 1;

impl ReturnNode {
    fn store(&self, message: &ReturnMessage, integrity: u32) {
        self.owner_id
            .store(message.target.owner_id.raw(), Ordering::Relaxed);
        self.generation
            .store(message.target.generation.raw(), Ordering::Relaxed);
        self.route_key
            .store(message.target.route_key.raw(), Ordering::Relaxed);
        self.descriptor_unit.store(
            u64::from(message.descriptor.raw()) | (u64::from(message.unit) << 32),
            Ordering::Relaxed,
        );
        self.bytes_epoch.store(
            u64::from(message.bytes) | (u64::from(message.source_epoch.raw()) << 32),
            Ordering::Relaxed,
        );
        self.state_kind.store(
            message.state.code()
                | (u64::from(message.kind.raw()) << 8)
                | (u64::from(message.target.domain.raw()) << 16)
                | (u64::from(MessageFamilyTag::Return.raw()) << 24),
            Ordering::Relaxed,
        );
        self.payload_low.store(0, Ordering::Relaxed);
        self.payload_high.store(0, Ordering::Relaxed);
        self.integrity
            .store(u64::from(integrity), Ordering::Relaxed);
    }

    fn store_card_mark(&self, batch: &CardMarkBatch, integrity: u32) {
        self.owner_id
            .store(batch.target.owner_id.raw(), Ordering::Relaxed);
        self.generation
            .store(batch.target.generation.raw(), Ordering::Relaxed);
        self.route_key
            .store(batch.target.route_key.raw(), Ordering::Relaxed);
        self.descriptor_unit.store(
            u64::from(batch.arena.raw()) | (u64::from(batch.card_start) << 32),
            Ordering::Relaxed,
        );
        self.bytes_epoch.store(
            u64::from(batch.bytes) | ((batch.cycle_epoch & 0xFFFF_FFFF) << 32),
            Ordering::Relaxed,
        );
        self.state_kind.store(
            batch.state.code()
                | (u64::from(MessageFamilyTag::CardMark.raw()) << 8)
                | (u64::from(batch.target.domain.raw()) << 16)
                | (u64::from(MessageFamilyTag::CardMark.raw()) << 24),
            Ordering::Relaxed,
        );
        self.payload_low
            .store(batch.cycle_epoch >> 32, Ordering::Relaxed);
        // card 区间与 arena generation 共用一个车道：低 32 位是 card 数量，高 32 位是
        // generation，两者都是 32-bit 身份字段，因此无需第三车道。
        self.payload_high.store(
            u64::from(batch.card_count) | (u64::from(batch.arena_generation) << 32),
            Ordering::Relaxed,
        );
        self.integrity
            .store(u64::from(integrity), Ordering::Relaxed);
    }
    fn store_region_transfer(&self, batch: &RegionTransferBatch, integrity: u32) {
        self.owner_id
            .store(batch.target.owner_id.raw(), Ordering::Relaxed);
        self.generation
            .store(batch.target.generation.raw(), Ordering::Relaxed);
        self.route_key
            .store(batch.target.route_key.raw(), Ordering::Relaxed);
        self.descriptor_unit.store(
            u64::from(batch.type_summary.raw()) | (u64::from(batch.region) << 32),
            Ordering::Relaxed,
        );
        self.bytes_epoch.store(
            u64::from(batch.bytes) | ((batch.cycle_epoch & 0xFFFF_FFFF) << 32),
            Ordering::Relaxed,
        );
        self.state_kind.store(
            batch.state.code()
                | (u64::from(MessageFamilyTag::RegionTransfer.raw()) << 8)
                | (u64::from(batch.target.domain.raw()) << 16)
                | (u64::from(MessageFamilyTag::RegionTransfer.raw()) << 24),
            Ordering::Relaxed,
        );
        self.payload_low.store(
            u64::from(batch.region_generation) | ((batch.cycle_epoch >> 32) << 32),
            Ordering::Relaxed,
        );
        // export summary 的两个来源、容量 class 与来源 owner 共用一个车道：位 0..8 是编译器
        // 声明，8..16 是 runtime 观察结果，16..24 是容量 class 下标，24..56 是来源 owner id；
        // 这些都是稳定的小整数身份，不需要独立车道。
        self.payload_high.store(
            u64::from(batch.export_state)
                | (u64::from(batch.observed) << 8)
                | (u64::from(batch.capacity_class) << 16)
                | (u64::from(batch.source.raw()) << 24),
            Ordering::Relaxed,
        );
        self.integrity
            .store(u64::from(integrity), Ordering::Relaxed);
    }

    /// 从车道重建 region transfer；只有该消息族才会调用。
    fn load_region_transfer(&self) -> RegionTransferBatch {
        let owner_id = self.owner_id.load(Ordering::Relaxed);
        let target_generation = self.generation.load(Ordering::Relaxed);
        let route_key = self.route_key.load(Ordering::Relaxed);
        let descriptor_unit = self.descriptor_unit.load(Ordering::Relaxed);
        let bytes_epoch = self.bytes_epoch.load(Ordering::Relaxed);
        let state_kind = self.state_kind.load(Ordering::Relaxed);
        let payload_low = self.payload_low.load(Ordering::Relaxed);
        let payload_high = self.payload_high.load(Ordering::Relaxed);
        let integrity = self.integrity.load(Ordering::Relaxed);
        let domain = MemoryDomainId::from_raw(((state_kind >> 16) & 0xFF) as u8)
            .unwrap_or(MemoryDomainId::RUNTIME_RAW);
        RegionTransferBatch {
            next: None,
            target: OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(owner_id),
                generation: OwnerGeneration::from_raw(target_generation),
                route_key: RouteKey::from_raw(route_key),
            },
            source: OwnerId::from_raw((payload_high >> 24) & 0xFFFF_FFFF),
            region: (descriptor_unit >> 32) as u32,
            region_generation: (payload_low & 0xFFFF_FFFF) as u32,
            type_summary: SlabDescriptorId::from_raw((descriptor_unit & 0xFFFF_FFFF) as u32),
            bytes: (bytes_epoch & 0xFFFF_FFFF) as u32,
            capacity_class: ((payload_high >> 16) & 0xFF) as u32,
            export_state: (payload_high & 0xFF) as u8,
            observed: ((payload_high >> 8) & 0xFF) as u8,
            cycle_epoch: ((payload_low >> 32) << 32) | ((bytes_epoch >> 32) & 0xFFFF_FFFF),
            state: MessageState::from_code((state_kind & 0xFF) as u8),
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(payload_low & 0xFFFF_FFFF),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: OwnerId::from_raw(owner_id),
                route_key: RouteKey::from_raw(route_key),
                checksum: integrity as u32,
            },
        }
    }

    fn store_mark_ticket(&self, ticket: &MarkTicket, integrity: u32) {
        self.owner_id
            .store(ticket.target.owner_id.raw(), Ordering::Relaxed);
        self.generation
            .store(ticket.target.generation.raw(), Ordering::Relaxed);
        self.route_key
            .store(ticket.target.route_key.raw(), Ordering::Relaxed);
        // 身份车道按目标种类解释：低半是 arena descriptor 或 handle table，高半是对象偏移
        // 或 handle slot；generation 车道同理承载 block generation 或 handle generation。
        self.descriptor_unit.store(
            u64::from(ticket.mark.identity_low()) | (u64::from(ticket.mark.identity_high()) << 32),
            Ordering::Relaxed,
        );
        self.bytes_epoch.store(
            u64::from(ticket.bytes) | (u64::from(ticket.topology_epoch) << 32),
            Ordering::Relaxed,
        );
        self.state_kind.store(
            ticket.state.code()
                | (u64::from(MessageFamilyTag::MarkTicket.raw()) << 8)
                | (u64::from(ticket.target.domain.raw()) << 16)
                | (u64::from(MessageFamilyTag::MarkTicket.raw()) << 24),
            Ordering::Relaxed,
        );
        self.payload_low
            .store(ticket.cycle_epoch, Ordering::Relaxed);
        self.payload_high.store(
            u64::from(ticket.mark.generation()) | (u64::from(ticket.source_block) << 32),
            Ordering::Relaxed,
        );
        // mark 族不使用 sequence/delta 的序列语义：两条车道承载目标种类与 handle generation。
        self.sequence
            .store(u64::from(ticket.mark.kind()), Ordering::Relaxed);
        self.delta.store(0, Ordering::Relaxed);
        self.credit.store(ticket.credit.raw(), Ordering::Relaxed);
        self.integrity
            .store(u64::from(integrity), Ordering::Relaxed);
    }

    /// 从车道重建 mark ticket；只有 mark 族才会调用。
    fn load_mark_ticket(&self) -> MarkTicket {
        let owner_id = self.owner_id.load(Ordering::Relaxed);
        let target_generation = self.generation.load(Ordering::Relaxed);
        let route_key = self.route_key.load(Ordering::Relaxed);
        let descriptor_unit = self.descriptor_unit.load(Ordering::Relaxed);
        let bytes_epoch = self.bytes_epoch.load(Ordering::Relaxed);
        let state_kind = self.state_kind.load(Ordering::Relaxed);
        let payload_low = self.payload_low.load(Ordering::Relaxed);
        let payload_high = self.payload_high.load(Ordering::Relaxed);
        let sequence = self.sequence.load(Ordering::Relaxed);
        let credit = self.credit.load(Ordering::Relaxed);
        let integrity = self.integrity.load(Ordering::Relaxed);
        let domain = MemoryDomainId::from_raw(((state_kind >> 16) & 0xFF) as u8)
            .unwrap_or(MemoryDomainId::RUNTIME_RAW);
        // 车道按固定位宽掩码后截断到目标字段宽度：每个身份字段只占 32-bit，掩码已保证无溢出。
        // 未知目标种类是真正的编码失败，按本地偏移解释会静默标记错误对象。
        let mark = MarkTarget::decode(
            (sequence & 0xFFFF_FFFF) as u32,
            (descriptor_unit & 0xFFFF_FFFF) as u32,
            (descriptor_unit >> 32) as u32,
            (payload_high & 0xFFFF_FFFF) as u32,
        )
        .unwrap_or(MarkTarget::Local {
            arena: SlabDescriptorId::from_raw((descriptor_unit & 0xFFFF_FFFF) as u32),
            offset: (descriptor_unit >> 32) as u32,
            block_generation: (payload_high & 0xFFFF_FFFF) as u32,
        });
        MarkTicket {
            next: None,
            target: OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(owner_id),
                generation: OwnerGeneration::from_raw(target_generation),
                route_key: RouteKey::from_raw(route_key),
            },
            mark,
            source_block: (payload_high >> 32) as u32,
            cycle_epoch: payload_low,
            topology_epoch: (bytes_epoch >> 32) as u32,
            credit: GcCreditId::from_raw(credit),
            bytes: (bytes_epoch & 0xFFFF_FFFF) as u32,
            state: MessageState::from_code((state_kind & 0xFF) as u8),
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(target_generation),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: OwnerId::from_raw(owner_id),
                route_key: RouteKey::from_raw(route_key),
                checksum: integrity as u32,
            },
        }
    }

    fn store_handle_forward(&self, message: &HandleForward, integrity: u32) {
        self.owner_id
            .store(message.target.owner_id.raw(), Ordering::Relaxed);
        self.generation
            .store(message.target.generation.raw(), Ordering::Relaxed);
        self.route_key
            .store(message.target.route_key.raw(), Ordering::Relaxed);
        // handle 身份与 forward generation 共用一个车道对：低半是 table，高半是 slot；
        // generation 车道承载 handle generation 与 forward generation。
        self.descriptor_unit.store(
            u64::from(message.handle_table) | (u64::from(message.handle_slot) << 32),
            Ordering::Relaxed,
        );
        self.bytes_epoch.store(
            u64::from(message.bytes) | (u64::from(message.topology_epoch) << 32),
            Ordering::Relaxed,
        );
        self.state_kind.store(
            message.state.code()
                | (u64::from(MessageFamilyTag::HandleForward.raw()) << 8)
                | (u64::from(message.target.domain.raw()) << 16)
                | (u64::from(MessageFamilyTag::HandleForward.raw()) << 24),
            Ordering::Relaxed,
        );
        self.payload_low
            .store(message.cycle_epoch, Ordering::Relaxed);
        self.payload_high.store(
            u64::from(message.handle_generation) | (u64::from(message.forward_generation) << 32),
            Ordering::Relaxed,
        );
        // 两条 payload identity 各占一条 64-bit 车道：它们是逻辑身份而不是地址，因此可以
        // 原样编码，不需要掩码或拆分。
        self.sequence
            .store(message.old_payload.raw(), Ordering::Relaxed);
        self.delta
            .store(message.new_payload.raw(), Ordering::Relaxed);
        self.credit.store(0, Ordering::Relaxed);
        self.integrity
            .store(u64::from(integrity), Ordering::Relaxed);
    }

    /// 从车道重建 handle forward；只有该消息族才会调用。
    fn load_handle_forward(&self) -> HandleForward {
        let owner_id = self.owner_id.load(Ordering::Relaxed);
        let target_generation = self.generation.load(Ordering::Relaxed);
        let route_key = self.route_key.load(Ordering::Relaxed);
        let descriptor_unit = self.descriptor_unit.load(Ordering::Relaxed);
        let bytes_epoch = self.bytes_epoch.load(Ordering::Relaxed);
        let state_kind = self.state_kind.load(Ordering::Relaxed);
        let payload_low = self.payload_low.load(Ordering::Relaxed);
        let payload_high = self.payload_high.load(Ordering::Relaxed);
        let old_payload = self.sequence.load(Ordering::Relaxed);
        let new_payload = self.delta.load(Ordering::Relaxed);
        let integrity = self.integrity.load(Ordering::Relaxed);
        let domain = MemoryDomainId::from_raw(((state_kind >> 16) & 0xFF) as u8)
            .unwrap_or(MemoryDomainId::RUNTIME_RAW);
        // 车道按固定位宽掩码后截断到目标字段宽度：每个身份字段只占 32-bit，掩码已保证无溢出。
        HandleForward {
            next: None,
            target: OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(owner_id),
                generation: OwnerGeneration::from_raw(target_generation),
                route_key: RouteKey::from_raw(route_key),
            },
            handle_table: (descriptor_unit & 0xFFFF_FFFF) as u32,
            handle_slot: (descriptor_unit >> 32) as u32,
            handle_generation: (payload_high & 0xFFFF_FFFF) as u32,
            forward_generation: (payload_high >> 32) as u32,
            old_payload: SharedPayloadId::from_raw(old_payload),
            new_payload: SharedPayloadId::from_raw(new_payload),
            cycle_epoch: payload_low,
            topology_epoch: (bytes_epoch >> 32) as u32,
            bytes: (bytes_epoch & 0xFFFF_FFFF) as u32,
            state: MessageState::from_code((state_kind & 0xFF) as u8),
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(target_generation),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: OwnerId::from_raw(owner_id),
                route_key: RouteKey::from_raw(route_key),
                checksum: integrity as u32,
            },
        }
    }

    fn store_edge_delta(&self, delta: &EdgeDelta, integrity: u32) {
        self.owner_id
            .store(delta.target.owner_id.raw(), Ordering::Relaxed);
        self.generation
            .store(delta.target.generation.raw(), Ordering::Relaxed);
        self.route_key
            .store(delta.target.route_key.raw(), Ordering::Relaxed);
        self.descriptor_unit.store(
            u64::from(delta.source.id.0) | (u64::from(delta.destination.id.0) << 32),
            Ordering::Relaxed,
        );
        self.bytes_epoch.store(
            u64::from(delta.bytes) | (u64::from(delta.topology_epoch) << 32),
            Ordering::Relaxed,
        );
        self.state_kind.store(
            delta.state.code()
                | (u64::from(MessageFamilyTag::EdgeDelta.raw()) << 8)
                | (u64::from(delta.target.domain.raw()) << 16)
                | (u64::from(MessageFamilyTag::EdgeDelta.raw()) << 24),
            Ordering::Relaxed,
        );
        self.payload_low.store(delta.cycle_epoch, Ordering::Relaxed);
        self.payload_high.store(
            u64::from(delta.source.generation) | (u64::from(delta.destination.generation) << 32),
            Ordering::Relaxed,
        );
        self.sequence.store(delta.sequence, Ordering::Relaxed);
        self.delta.store(delta.delta as u64, Ordering::Relaxed);
        self.credit.store(delta.credit.raw(), Ordering::Relaxed);
        self.integrity
            .store(u64::from(integrity), Ordering::Relaxed);
    }

    /// 从车道重建 edge delta；只有 edge delta 族才会调用。
    fn load_edge_delta(&self) -> EdgeDelta {
        let owner_id = self.owner_id.load(Ordering::Relaxed);
        let target_generation = self.generation.load(Ordering::Relaxed);
        let route_key = self.route_key.load(Ordering::Relaxed);
        let descriptor_unit = self.descriptor_unit.load(Ordering::Relaxed);
        let bytes_epoch = self.bytes_epoch.load(Ordering::Relaxed);
        let state_kind = self.state_kind.load(Ordering::Relaxed);
        let payload_low = self.payload_low.load(Ordering::Relaxed);
        let payload_high = self.payload_high.load(Ordering::Relaxed);
        let sequence = self.sequence.load(Ordering::Relaxed);
        // 车道按固定位宽掩码后截断；掩码已保证目标字段无溢出。
        let delta = self.delta.load(Ordering::Relaxed) as i64;
        let credit = self.credit.load(Ordering::Relaxed);
        let integrity = self.integrity.load(Ordering::Relaxed);
        let domain = MemoryDomainId::from_raw(((state_kind >> 16) & 0xFF) as u8)
            .unwrap_or(MemoryDomainId::RUNTIME_RAW);
        EdgeDelta {
            next: None,
            target: OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(owner_id),
                generation: OwnerGeneration::from_raw(target_generation),
                route_key: RouteKey::from_raw(route_key),
            },
            source: BlockRef {
                id: ManagedBlockId((descriptor_unit & 0xFFFF_FFFF) as u32),
                generation: (payload_high & 0xFFFF_FFFF) as u32,
            },
            destination: BlockRef {
                id: ManagedBlockId((descriptor_unit >> 32) as u32),
                generation: (payload_high >> 32) as u32,
            },
            cycle_epoch: payload_low,
            topology_epoch: (bytes_epoch >> 32) as u32,
            sequence,
            delta,
            credit: GcCreditId::from_raw(credit),
            bytes: (bytes_epoch & 0xFFFF_FFFF) as u32,
            state: MessageState::from_code((state_kind & 0xFF) as u8),
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(target_generation),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: OwnerId::from_raw(owner_id),
                route_key: RouteKey::from_raw(route_key),
                checksum: integrity as u32,
            },
        }
    }

    fn load(&self, class: RuntimeSizeClassId, generation: SlabGeneration) -> ReturnMessage {
        let owner_id = self.owner_id.load(Ordering::Relaxed);
        let target_generation = self.generation.load(Ordering::Relaxed);
        let route_key = self.route_key.load(Ordering::Relaxed);
        let descriptor_unit = self.descriptor_unit.load(Ordering::Relaxed);
        let bytes_epoch = self.bytes_epoch.load(Ordering::Relaxed);
        let state_kind = self.state_kind.load(Ordering::Relaxed);
        let integrity = self.integrity.load(Ordering::Relaxed);
        let domain = MemoryDomainId::from_raw(((state_kind >> 16) & 0xFF) as u8)
            .unwrap_or(MemoryDomainId::RUNTIME_RAW);
        ReturnMessage {
            next: None,
            target: OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(owner_id),
                generation: OwnerGeneration::from_raw(target_generation),
                route_key: RouteKey::from_raw(route_key),
            },
            kind: ReturnKind::from_raw(((state_kind >> 8) & 0xFF) as u8)
                .unwrap_or(ReturnKind::RawSlot),
            descriptor: SlabDescriptorId::from_raw((descriptor_unit & 0xFFFF_FFFF) as u32),
            unit: (descriptor_unit >> 32) as u32,
            bytes: (bytes_epoch & 0xFFFF_FFFF) as u32,
            source_epoch: Epoch::from_raw((bytes_epoch >> 32) as u32),
            state: MessageState::from_code((state_kind & 0xFF) as u8),
            integrity: IntegrityTag {
                generation,
                class,
                owner_id: OwnerId::from_raw(owner_id),
                route_key: RouteKey::from_raw(route_key),
                checksum: integrity as u32,
            },
        }
    }

    /// 从车道重建 card batch；只有 card 族才会调用。
    fn load_card_mark(&self) -> CardMarkBatch {
        let owner_id = self.owner_id.load(Ordering::Relaxed);
        let target_generation = self.generation.load(Ordering::Relaxed);
        let route_key = self.route_key.load(Ordering::Relaxed);
        let descriptor_unit = self.descriptor_unit.load(Ordering::Relaxed);
        let bytes_epoch = self.bytes_epoch.load(Ordering::Relaxed);
        let state_kind = self.state_kind.load(Ordering::Relaxed);
        let payload_low = self.payload_low.load(Ordering::Relaxed);
        let payload_high = self.payload_high.load(Ordering::Relaxed);
        let integrity = self.integrity.load(Ordering::Relaxed);
        let arena_generation = (payload_high >> 32) as u32;
        let domain = MemoryDomainId::from_raw(((state_kind >> 16) & 0xFF) as u8)
            .unwrap_or(MemoryDomainId::RUNTIME_RAW);
        CardMarkBatch {
            next: None,
            target: OwnerToken {
                domain,
                owner_id: OwnerId::from_raw(owner_id),
                generation: OwnerGeneration::from_raw(target_generation),
                route_key: RouteKey::from_raw(route_key),
            },
            arena: SlabDescriptorId::from_raw((descriptor_unit & 0xFFFF_FFFF) as u32),
            card_start: (descriptor_unit >> 32) as u32,
            arena_generation,
            cycle_epoch: (payload_low << 32) | (bytes_epoch >> 32),
            card_count: (payload_high & 0xFFFF_FFFF) as u32,
            bytes: (bytes_epoch & 0xFFFF_FFFF) as u32,
            state: MessageState::from_code((state_kind & 0xFF) as u8),
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(u64::from(arena_generation)),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: OwnerId::from_raw(owner_id),
                route_key: RouteKey::from_raw(route_key),
                checksum: integrity as u32,
            },
        }
    }

    fn load_word(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    fn store_word(&self, word: u64, ordering: Ordering) {
        self.next.store(word, ordering);
    }

    fn load_free(&self) -> u64 {
        self.free_next.load(Ordering::Acquire)
    }

    fn store_free(&self, word: u64, ordering: Ordering) {
        self.free_next.store(word, ordering);
    }

    fn flag(&self) -> u64 {
        self.reuse.load(Ordering::Acquire)
    }

    fn set_flag(&self, value: u64) {
        self.reuse.store(value, Ordering::Release);
    }

    fn bytes(&self) -> u64 {
        self.bytes_epoch.load(Ordering::Acquire) & 0xFFFF_FFFF
    }

    fn descriptor(&self) -> SlabDescriptorId {
        SlabDescriptorId::from_raw(
            (self.descriptor_unit.load(Ordering::Acquire) & 0xFFFF_FFFF) as u32,
        )
    }

    fn unit(&self) -> u32 {
        (self.descriptor_unit.load(Ordering::Acquire) >> 32) as u32
    }

    fn kind(&self) -> ReturnKind {
        ReturnKind::from_raw(((self.state_kind.load(Ordering::Acquire) >> 8) & 0xFF) as u8)
            .unwrap_or(ReturnKind::RawSlot)
    }

    /// 读取消息族判别值；未知取值按 `Return` 回退，由 integrity 与 schema 继续拒绝。
    fn family(&self) -> MessageFamilyTag {
        match (self.state_kind.load(Ordering::Acquire) >> 24) & 0xFF {
            1 => MessageFamilyTag::CardMark,
            2 => MessageFamilyTag::RegionTransfer,
            3 => MessageFamilyTag::MarkTicket,
            4 => MessageFamilyTag::EdgeDelta,
            5 => MessageFamilyTag::HandleForward,
            _ => MessageFamilyTag::Return,
        }
    }

    fn owner_id(&self) -> OwnerId {
        OwnerId::from_raw(self.owner_id.load(Ordering::Acquire))
    }
}

impl MessageState {
    fn code(self) -> u64 {
        match self {
            Self::Staged => 0,
            Self::Published => 1,
            Self::Forwarded => 2,
            Self::Consumed => 3,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Published,
            2 => Self::Forwarded,
            3 => Self::Consumed,
            _ => Self::Staged,
        }
    }
}

/// non-moving message node pool；refill 走 typed 冷路径，不为每次 return 调用普通分配器。
///
/// free stack 是带 tag 的 Treiber 栈：head 由 `(tag, index)` 组成，node 的 `free_next` 车道
/// 保存完整 head 字。tag 让 CAS 能识别 ABA（同一 node 被弹出又压回），因此栈上不会出现已经
/// 被其它 producer 取走的 node。message chain 使用独立的 `next` 车道，两者互不覆盖。
#[derive(Debug)]
pub(crate) struct ReturnNodePool {
    nodes: Vec<ReturnNode>,
    capacity: u32,
    free_head: AtomicU64,
}

impl ReturnNodePool {
    /// 创建一个固定容量的 node pool，全部 node 初始可复用。
    pub(crate) fn new(capacity: u32) -> Self {
        let pool = Self {
            nodes: (0..capacity).map(|_| ReturnNode::default()).collect(),
            capacity,
            free_head: AtomicU64::new(NULL_LINK),
        };
        for index in (0..capacity).rev() {
            let next = if index + 1 < capacity {
                encode_free(index + 1, 0)
            } else {
                NULL_LINK
            };
            pool.nodes[index as usize].store_free(next, Ordering::Relaxed);
            pool.nodes[index as usize].set_flag(NODE_FREE);
        }
        pool.free_head.store(
            if capacity == 0 {
                NULL_LINK
            } else {
                encode_free(0, 0)
            },
            Ordering::Relaxed,
        );
        pool
    }

    /// 返回容量。
    pub(crate) const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// 弹出一个空闲 node；free stack 为空时返回错误。
    ///
    /// head 的读取与 CAS 之间存在窗口：其它 producer 可能已经推进 head，因此 node 的复用
    /// 标记与 head 不一致时只重新读取 head。只有 CAS 成功才表示该 node 仍属于本线程，此时
    /// 它的标记必然是 `Free`。
    pub(crate) fn allocate(&self) -> Result<ReturnNodeId, RawInvariant> {
        loop {
            let head = self.free_head.load(Ordering::Acquire);
            if head == NULL_LINK {
                return Err(RawInvariant::new("return node pool 已耗尽"));
            }
            let (_, index) = decode_free(head);
            if index >= self.capacity {
                return Err(RawInvariant::new("free 链编号越过 pool 容量"));
            }
            let node = &self.nodes[index as usize];
            if node.flag() != NODE_FREE {
                continue;
            }
            let next = node.load_free();
            match self.free_head.compare_exchange_weak(
                head,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    node.set_flag(NODE_IN_USE);
                    return Ok(ReturnNodeId(index));
                }
                Err(_) => continue,
            }
        }
    }

    /// 把 node 还回 pool；重复释放报不变量失败。
    pub(crate) fn release(&self, id: ReturnNodeId) -> Result<(), RawInvariant> {
        let node = self
            .nodes
            .get(id.index())
            .ok_or_else(|| RawInvariant::new("release 引用越界 node"))?;
        if node.flag() != NODE_IN_USE {
            return Err(RawInvariant::new("return node 被重复释放"));
        }
        node.set_flag(NODE_FREE);
        let mut head = self.free_head.load(Ordering::Acquire);
        loop {
            let (tag, _) = decode_free(head);
            let pushed = encode_free(id.raw(), tag.wrapping_add(1));
            node.store_free(head, Ordering::Release);
            match self.free_head.compare_exchange_weak(
                head,
                pushed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => head = actual,
            }
        }
    }

    /// 写入一个 node 的 return payload；`next` 由调用者随后发布。
    pub(crate) fn store(&self, id: ReturnNodeId, message: &ReturnMessage, integrity: u32) {
        self.nodes[id.index()].store(message, integrity);
    }

    /// 写入一个 node 的 card batch payload；`next` 由调用者随后发布。
    pub(crate) fn store_card_mark(&self, id: ReturnNodeId, batch: &CardMarkBatch, integrity: u32) {
        self.nodes[id.index()].store_card_mark(batch, integrity);
    }

    /// 读取一个 node 的 card batch payload。
    ///
    /// card 族没有 class 语义，arena generation 来自 batch 自身的 arena 身份，因此载入不
    /// 需要调用方提供载入键；integrity 仍按 batch 自身的身份字段校验。
    pub(crate) fn load_card_mark(&self, id: ReturnNodeId) -> CardMarkBatch {
        self.nodes[id.index()].load_card_mark()
    }

    /// 写入一个 node 的 region transfer payload。
    pub(crate) fn store_region_transfer(
        &self,
        id: ReturnNodeId,
        batch: &RegionTransferBatch,
        integrity: u32,
    ) {
        self.nodes[id.index()].store_region_transfer(batch, integrity);
    }

    /// 读取一个 node 的 region transfer payload。
    ///
    /// 与 card 族一样，载入不需要 class/generation 键：integrity 按 batch 自身的身份字段校验。
    pub(crate) fn load_region_transfer(&self, id: ReturnNodeId) -> RegionTransferBatch {
        self.nodes[id.index()].load_region_transfer()
    }

    /// 写入一个 node 的 mark ticket payload。
    pub(crate) fn store_mark_ticket(&self, id: ReturnNodeId, ticket: &MarkTicket, integrity: u32) {
        self.nodes[id.index()].store_mark_ticket(ticket, integrity);
    }

    /// 读取一个 node 的 mark ticket payload。
    ///
    /// 与 card/region 族一样，载入不需要 class/generation 键：integrity 按 ticket 自身的身份
    /// 字段校验。
    pub(crate) fn load_mark_ticket(&self, id: ReturnNodeId) -> MarkTicket {
        self.nodes[id.index()].load_mark_ticket()
    }

    /// 写入一个 node 的 edge delta payload。
    pub(crate) fn store_edge_delta(&self, id: ReturnNodeId, delta: &EdgeDelta, integrity: u32) {
        self.nodes[id.index()].store_edge_delta(delta, integrity);
    }

    /// 读取一个 node 的 edge delta payload。
    ///
    /// 与其它 GC 族一样，载入不需要 class/generation 键：integrity 按记录自身的身份字段校验。
    pub(crate) fn load_edge_delta(&self, id: ReturnNodeId) -> EdgeDelta {
        self.nodes[id.index()].load_edge_delta()
    }

    /// 写入一个 node 的 handle forward payload。
    pub(crate) fn store_handle_forward(
        &self,
        id: ReturnNodeId,
        message: &HandleForward,
        integrity: u32,
    ) {
        self.nodes[id.index()].store_handle_forward(message, integrity);
    }

    /// 读取一个 node 的 handle forward payload。
    ///
    /// 与其它 GC 族一样，载入不需要 class/generation 键：integrity 按记录自身的身份字段校验，
    /// 两条 payload identity 原样解码后由目标 owner 在自己的 SharedHeap 表里校验。
    pub(crate) fn load_handle_forward(&self, id: ReturnNodeId) -> HandleForward {
        self.nodes[id.index()].load_handle_forward()
    }

    /// 按给定 class 与 generation 读取一个 return payload。
    pub(crate) fn load(
        &self,
        id: ReturnNodeId,
        class: RuntimeSizeClassId,
        generation: SlabGeneration,
    ) -> ReturnMessage {
        self.nodes[id.index()].load(class, generation)
    }

    /// 只读取 node 的消息族判别值。
    pub(crate) fn family_of(&self, id: ReturnNodeId) -> MessageFamilyTag {
        self.nodes[id.index()].family()
    }

    /// 以 Release 发布 node 的 next link。
    pub(crate) fn link(&self, id: ReturnNodeId, next: Option<ReturnNodeId>) {
        let word = next.map_or(NULL_LINK, encode_node);
        self.nodes[id.index()].store_word(word, Ordering::Release);
    }

    /// 以 Acquire 读取 node 的 next link。
    pub(crate) fn next(&self, id: ReturnNodeId) -> Option<ReturnNodeId> {
        decode_node(self.nodes[id.index()].load_word())
    }

    /// 判断 node 是否正在使用。
    pub(crate) fn in_use(&self, id: ReturnNodeId) -> bool {
        self.nodes[id.index()].flag() == NODE_IN_USE
    }

    /// 读取 node 记录的待处理字节数。
    pub(crate) fn node_bytes(&self, id: ReturnNodeId) -> u64 {
        self.nodes[id.index()].bytes()
    }

    /// 只读取 node 的 descriptor 编号；consumer 用它定位 slab 后再取完整 payload。
    pub(crate) fn descriptor_of(&self, id: ReturnNodeId) -> SlabDescriptorId {
        self.nodes[id.index()].descriptor()
    }

    /// 只读取 node 的 return 类别；consumer 用它决定用 slab 还是 extent 的载入键。
    pub(crate) fn kind_of(&self, id: ReturnNodeId) -> ReturnKind {
        self.nodes[id.index()].kind()
    }

    /// 只读取 node 的 unit 编号。
    pub(crate) fn unit_of(&self, id: ReturnNodeId) -> u32 {
        self.nodes[id.index()].unit()
    }

    /// 只读取 node 的目标 owner 编号。
    pub(crate) fn owner_id_of(&self, id: ReturnNodeId) -> OwnerId {
        self.nodes[id.index()].owner_id()
    }
}

/// free stack 的 head 字：`tag(32) | index(32)`。
fn encode_free(index: u32, tag: u32) -> u64 {
    (u64::from(tag) << 32) | u64::from(index)
}

/// 解码 free stack 的 head 字。
fn decode_free(word: u64) -> (u32, u32) {
    ((word >> 32) as u32, (word & 0xFFFF_FFFF) as u32)
}

/// node link 的编码：编号与 generation 无关，但必须与 NULL 区分。
pub(crate) fn encode_node(id: ReturnNodeId) -> u64 {
    u64::from(id.raw())
}

/// 解码 node link。
pub(crate) fn decode_node(word: u64) -> Option<ReturnNodeId> {
    if word == NULL_LINK {
        None
    } else {
        u32::try_from(word).ok().map(ReturnNodeId)
    }
}

/// batch 的双上限；item 上限与 byte 上限必须同时保存。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BatchLimits {
    pub(crate) items: u32,
    pub(crate) batch_soft_bytes: u64,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            items: super::BATCH_MAX,
            batch_soft_bytes: DEFAULT_BATCH_SOFT_BYTES,
        }
    }
}

/// byte 上限的基线候选值；真实值由 target workload 的 benchmark 校准。
pub(crate) const DEFAULT_BATCH_SOFT_BYTES: u64 = 1 << 20;

/// 触发 batch 发布的条件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FlushTrigger {
    /// item 数达到上限。
    ItemLimit,
    /// byte 数达到 soft limit。
    ByteLimit,
    /// target、shard、domain 或 route mode 改变。
    TargetChanged,
    /// producer 即将 park、进入 ForeignBridge 或 stop gate。
    ProducerStopping,
    /// owner retire、GC handoff 或 memory pressure 要求排空。
    OwnerPressure,
    /// maintenance service 到期。
    Maintenance,
    /// candidate / shared plane 在 GC cycle 内交出 empty block。
    GcHandoff,
}

impl FlushTrigger {
    /// 返回触发名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ItemLimit => "item-limit",
            Self::ByteLimit => "byte-limit",
            Self::TargetChanged => "target-changed",
            Self::ProducerStopping => "producer-stopping",
            Self::OwnerPressure => "owner-pressure",
            Self::Maintenance => "maintenance",
            Self::GcHandoff => "gc-handoff",
        }
    }
}

/// producer staging：固定数量的 staging slot 加一个当前目标链，不为每个 owner 预分配队列。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerStaging {
    limits: BatchLimits,
    target: Option<OwnerToken>,
    shard: Option<ShardIndex>,
    first: Option<ReturnNodeId>,
    last: Option<ReturnNodeId>,
    count: u32,
    bytes: u64,
    publish_active: bool,
}

impl ProducerStaging {
    /// 创建空 staging。
    pub(crate) fn new(limits: BatchLimits) -> Self {
        Self {
            limits,
            target: None,
            shard: None,
            first: None,
            last: None,
            count: 0,
            bytes: 0,
            publish_active: false,
        }
    }

    /// 返回当前 target。
    pub(crate) const fn target(&self) -> Option<OwnerToken> {
        self.target
    }

    /// 返回当前 shard。
    pub(crate) const fn shard(&self) -> Option<ShardIndex> {
        self.shard
    }

    /// 返回暂存的 item 数。
    pub(crate) const fn count(&self) -> u32 {
        self.count
    }

    /// 返回暂存的字节数。
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 返回 chain 头。
    pub(crate) const fn first(&self) -> Option<ReturnNodeId> {
        self.first
    }

    /// 返回 chain 尾；producer 追加新 node 时在它上面发布链。
    pub(crate) const fn last(&self) -> Option<ReturnNodeId> {
        self.last
    }

    /// 返回 producer 是否处于 publish 区间。
    pub(crate) const fn publish_active(&self) -> bool {
        self.publish_active
    }

    /// 返回 batch 上限。
    pub(crate) const fn limits(&self) -> BatchLimits {
        self.limits
    }

    /// 进入 publish 区间：设置 `publish_active` 后必须重新读取 queue control epoch。
    pub(crate) fn begin_publish(&mut self) {
        self.publish_active = true;
    }

    /// 离开 publish 区间。
    pub(crate) fn end_publish(&mut self) {
        self.publish_active = false;
    }

    /// 暂存一个 node；target 或 shard 改变时先由调用者刷新旧链。
    ///
    /// 载荷无关：return 与 card batch 共用同一 staging，族判别值已在 node 车道上。
    pub(crate) fn stage(
        &mut self,
        node: ReturnNodeId,
        target: OwnerToken,
        bytes: u32,
        shard: ShardIndex,
    ) -> Result<(), RawInvariant> {
        if let Some(current) = self.target
            && current != target
        {
            return Err(RawInvariant::new("staging 一次只允许一个 target"));
        }
        if let Some(current) = self.shard
            && current != shard
        {
            return Err(RawInvariant::new("staging 一次只允许一个 shard"));
        }
        self.target = Some(target);
        self.shard = Some(shard);
        match self.last {
            Some(_) => self.last = Some(node),
            None => {
                self.first = Some(node);
                self.last = Some(node);
            }
        }
        self.count += 1;
        self.bytes += u64::from(bytes);
        Ok(())
    }
    /// 返回当前暂存链的发布触发条件；条件不满足时返回 `None`。
    pub(crate) fn flush_trigger(&self) -> Option<FlushTrigger> {
        if self.count >= self.limits.items {
            return Some(FlushTrigger::ItemLimit);
        }
        if self.bytes >= self.limits.batch_soft_bytes {
            return Some(FlushTrigger::ByteLimit);
        }
        None
    }

    /// 排空 staging 并返回 chain 边界。
    pub(crate) fn drain(&mut self) -> Option<StagedChain> {
        let first = self.first.take()?;
        let chain = StagedChain {
            first,
            last: self.last.take().expect("非空 chain 必有尾节点"),
            count: self.count,
            bytes: self.bytes,
            target: self.target,
            shard: self.shard,
        };
        self.count = 0;
        self.bytes = 0;
        self.target = None;
        self.shard = None;
        Some(chain)
    }
}

/// 一次 staging 发布的 chain 边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StagedChain {
    pub(crate) first: ReturnNodeId,
    pub(crate) last: ReturnNodeId,
    pub(crate) count: u32,
    pub(crate) bytes: u64,
    pub(crate) target: Option<OwnerToken>,
    pub(crate) shard: Option<ShardIndex>,
}

/// ring 关闭的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RingCloseReason {
    /// victim 被新 key 挤出。
    Victim,
    /// maintenance service 到期。
    Maintenance,
    /// owner retire。
    OwnerRetire,
    /// GC handoff。
    GcHandoff,
    /// memory pressure 要求排空。
    PressureDrain,
    /// producer/consumer deregister。
    Deregister,
}

impl RingCloseReason {
    /// 返回原因名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Victim => "victim",
            Self::Maintenance => "maintenance",
            Self::OwnerRetire => "owner-retire",
            Self::GcHandoff => "gc-handoff",
            Self::PressureDrain => "pressure-drain",
            Self::Deregister => "deregister",
        }
    }
}

/// 一个 source slab 的聚合 ring。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RingWay {
    key: Option<(SlabDescriptorId, SlabGeneration)>,
    slots: Vec<u32>,
    bytes: u64,
}

impl RingWay {
    /// 返回 ring 的 key。
    pub(crate) const fn key(&self) -> Option<(SlabDescriptorId, SlabGeneration)> {
        self.key
    }

    /// 返回积累的字节数。
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 返回积累的 slot 数量。
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

/// ring 关闭后产生的 batch。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClosedRing {
    pub(crate) key: (SlabDescriptorId, SlabGeneration),
    pub(crate) slots: Vec<u32>,
    pub(crate) bytes: u64,
    pub(crate) reason: RingCloseReason,
}

/// consumer-side same-slab 聚合：固定 8 set、每 set 2 way 的关联 ring。
///
/// key 是 stable `SlabDescriptor` 与 generation，不是 managed 地址；cache 只保存最近的
/// source slab，不建立 owner 数量大小的 hash table。hit 把 slot 追加到当前 ring，miss
/// 先关闭 victim ring（选择积累量最大的 way）再打开新 ring；关闭后的 ring 转成一个
/// `ReturnMessage` batch，再按 owner token 进入 staging。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReturnSlabCache {
    sets: Vec<[RingWay; 2]>,
    hits: u32,
    misses: u32,
}

impl ReturnSlabCache {
    /// 创建默认关联度（8 set × 2 way）的聚合 cache。
    pub(crate) fn new() -> Self {
        Self {
            sets: vec![
                [RingWay::default(), RingWay::default()];
                super::RETURN_SLAB_CACHE_SETS as usize
            ],
            hits: 0,
            misses: 0,
        }
    }

    /// 返回命中次数。
    pub(crate) const fn hits(&self) -> u32 {
        self.hits
    }

    /// 返回未命中次数。
    pub(crate) const fn misses(&self) -> u32 {
        self.misses
    }

    /// 返回全部 open ring 的待处理字节。
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.sets.iter().flatten().map(|way| way.bytes).sum::<u64>()
    }

    /// 返回全部 open ring 的 slot 总数。
    pub(crate) fn pending_slots(&self) -> usize {
        self.sets.iter().flatten().map(RingWay::len).sum()
    }

    fn set_of(key: (SlabDescriptorId, SlabGeneration)) -> usize {
        ((key.0.raw() as usize) ^ (key.1.raw() as usize).rotate_left(7))
            % super::RETURN_SLAB_CACHE_SETS as usize
    }

    /// 追加一个 slot；必要时关闭 victim ring 并返回其 batch。
    pub(crate) fn insert(
        &mut self,
        key: (SlabDescriptorId, SlabGeneration),
        slot: u32,
        bytes: u64,
    ) -> Option<ClosedRing> {
        let set = Self::set_of(key);
        if let Some(way) = self.sets[set].iter_mut().find(|way| way.key == Some(key)) {
            way.slots.push(slot);
            way.bytes += bytes;
            self.hits += 1;
            return None;
        }
        self.misses += 1;
        let closable = self.sets[set]
            .iter()
            .position(|way| way.key.is_none())
            .unwrap_or_else(|| {
                let mut victim = 0;
                for index in 1..self.sets[set].len() {
                    if self.sets[set][index].bytes > self.sets[set][victim].bytes {
                        victim = index;
                    }
                }
                victim
            });
        let closed = self.close_way(set, closable, RingCloseReason::Victim);
        self.sets[set][closable] = RingWay {
            key: Some(key),
            slots: vec![slot],
            bytes,
        };
        closed
    }

    /// 关闭全部 open ring；maintenance、owner retire、GC handoff、pressure drain 与
    /// producer/consumer deregister 都必须调用。
    pub(crate) fn close_all(&mut self, reason: RingCloseReason) -> Vec<ClosedRing> {
        let mut closed = Vec::new();
        for set in 0..self.sets.len() {
            for way in 0..self.sets[set].len() {
                if let Some(ring) = self.close_way(set, way, reason) {
                    closed.push(ring);
                }
            }
        }
        closed
    }

    fn close_way(&mut self, set: usize, way: usize, reason: RingCloseReason) -> Option<ClosedRing> {
        let entry = &mut self.sets[set][way];
        let key = entry.key.take()?;
        let slots = std::mem::take(&mut entry.slots);
        let bytes = std::mem::take(&mut entry.bytes);
        Some(ClosedRing {
            key,
            slots,
            bytes,
            reason,
        })
    }
}

/// producer 一次 flush 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PublishOutcome {
    /// 发布的 item 数。
    pub(crate) items: u32,
    /// 发布的字节数。
    pub(crate) bytes: u64,
    /// 触发的条件。
    pub(crate) trigger: FlushTrigger,
}

/// producer 侧：写入 node、链入 staging，并在触发条件满足时发布。
///
/// 目标是唯一既有 producer 语义的发布入口：单线程测试与真实并发压测走同一份代码。
pub(crate) fn stage_message(
    pool: &ReturnNodePool,
    inbox: Option<&OwnerInbox>,
    staging: &mut ProducerStaging,
    message: &ReturnMessage,
    shard: ShardIndex,
    forced: Option<FlushTrigger>,
) -> Result<Option<PublishOutcome>, RawInvariant> {
    let mut outcome = None;
    if staging
        .target()
        .is_some_and(|target| target != message.target)
    {
        return Err(RawInvariant::new("staging 目标改变前必须先发布旧 chain"));
    }
    let node = pool.allocate()?;
    pool.store(node, message, message.integrity.checksum);
    pool.link(node, None);
    if let Some(last) = staging.last() {
        pool.link(last, Some(node));
    }
    staging.stage(node, message.target, message.bytes, shard)?;
    if let Some(trigger) = forced.or_else(|| staging.flush_trigger())
        && let Some(inbox) = inbox
    {
        outcome = Some(flush_staging(pool, inbox, staging, trigger)?);
    }
    Ok(outcome)
}

/// producer 侧：写入 card batch node、链入 staging，并在触发条件满足时发布。
///
/// 与 `stage_message` 共用 node pool、staging 与 grace；区别只在车道解释与 integrity
/// 派生键，因此两个族不可能互相冒充。
pub(crate) fn stage_card_mark(
    pool: &ReturnNodePool,
    inbox: Option<&OwnerInbox>,
    staging: &mut ProducerStaging,
    batch: &CardMarkBatch,
    shard: ShardIndex,
    forced: Option<FlushTrigger>,
) -> Result<Option<PublishOutcome>, RawInvariant> {
    let mut outcome = None;
    if staging
        .target()
        .is_some_and(|target| target != batch.target)
    {
        return Err(RawInvariant::new("staging 目标改变前必须先发布旧 chain"));
    }
    let node = pool.allocate()?;
    pool.store_card_mark(node, batch, batch.integrity.checksum);
    pool.link(node, None);
    if let Some(last) = staging.last() {
        pool.link(last, Some(node));
    }
    staging.stage(node, batch.target, batch.bytes, shard)?;
    if let Some(trigger) = forced.or_else(|| staging.flush_trigger())
        && let Some(inbox) = inbox
    {
        outcome = Some(flush_staging(pool, inbox, staging, trigger)?);
    }
    Ok(outcome)
}

/// 把一个 region transfer 消息写入 staging chain 并发布到目标 owner 的 inbox。
///
/// 复用 return/card 的 producer 路径：node 从同一个 pool 取，chain 由同一套 staging 与
/// flush 触发器管理，因此 region 移交不会绕过 producer gate 与 queue-page grace。
pub(crate) fn stage_region_transfer(
    pool: &ReturnNodePool,
    inbox: Option<&OwnerInbox>,
    staging: &mut ProducerStaging,
    batch: &RegionTransferBatch,
    shard: ShardIndex,
    forced: Option<FlushTrigger>,
) -> Result<Option<PublishOutcome>, RawInvariant> {
    if staging
        .target()
        .is_some_and(|target| target != batch.target)
    {
        return Err(RawInvariant::new("staging 目标改变前必须先发布旧 chain"));
    }
    let node = pool.allocate()?;
    pool.store_region_transfer(node, batch, batch.integrity.checksum);
    pool.link(node, None);
    if let Some(last) = staging.last() {
        pool.link(last, Some(node));
    }
    staging.stage(node, batch.target, batch.bytes, shard)?;
    let mut outcome = None;
    if let Some(trigger) = forced.or_else(|| staging.flush_trigger())
        && let Some(inbox) = inbox
    {
        outcome = Some(flush_staging(pool, inbox, staging, trigger)?);
    }
    Ok(outcome)
}

/// 把一个 mark ticket 写入 staging chain 并发布到目标 owner 的 inbox。
///
/// 复用 return/card/region 的 producer 路径：node 从同一个 pool 取，chain 由同一套 staging
/// 与 flush 触发器管理，因此跨 owner 标记不会绕过 producer gate 与 queue-page grace。
pub(crate) fn stage_mark_ticket(
    pool: &ReturnNodePool,
    inbox: Option<&OwnerInbox>,
    staging: &mut ProducerStaging,
    ticket: &MarkTicket,
    shard: ShardIndex,
    forced: Option<FlushTrigger>,
) -> Result<Option<PublishOutcome>, RawInvariant> {
    let mut outcome = None;
    if staging
        .target()
        .is_some_and(|target| target != ticket.target)
    {
        return Err(RawInvariant::new("staging 目标改变前必须先发布旧 chain"));
    }
    let node = pool.allocate()?;
    pool.store_mark_ticket(node, ticket, ticket.integrity.checksum);
    pool.link(node, None);
    if let Some(last) = staging.last() {
        pool.link(last, Some(node));
    }
    staging.stage(node, ticket.target, ticket.bytes, shard)?;
    if let Some(trigger) = forced.or_else(|| staging.flush_trigger())
        && let Some(inbox) = inbox
    {
        outcome = Some(flush_staging(pool, inbox, staging, trigger)?);
    }
    Ok(outcome)
}

/// 把一个 handle forward 写入 staging chain 并发布到目标 owner 的 inbox。
///
/// 复用 return/card/region/mark/edge 的 producer 路径：node 从同一个 pool 取，chain 由同一套
/// staging 与 flush 触发器管理，因此 handle 搬迁通知不会绕过 producer gate 与 queue-page grace。
pub(crate) fn stage_handle_forward(
    pool: &ReturnNodePool,
    inbox: Option<&OwnerInbox>,
    staging: &mut ProducerStaging,
    message: &HandleForward,
    shard: ShardIndex,
    forced: Option<FlushTrigger>,
) -> Result<Option<PublishOutcome>, RawInvariant> {
    let mut outcome = None;
    if staging
        .target()
        .is_some_and(|target| target != message.target)
    {
        return Err(RawInvariant::new("staging 目标改变前必须先发布旧 chain"));
    }
    let node = pool.allocate()?;
    pool.store_handle_forward(node, message, message.integrity.checksum);
    pool.link(node, None);
    if let Some(last) = staging.last() {
        pool.link(last, Some(node));
    }
    staging.stage(node, message.target, message.bytes, shard)?;
    if let Some(trigger) = forced.or_else(|| staging.flush_trigger())
        && let Some(inbox) = inbox
    {
        outcome = Some(flush_staging(pool, inbox, staging, trigger)?);
    }
    Ok(outcome)
}

/// 把一个 edge delta 写入 staging chain 并发布到目标 owner 的 inbox。
///
/// 复用 return/card/region/mark 的 producer 路径：node 从同一个 pool 取，chain 由同一套
/// staging 与 flush 触发器管理，因此跨 block 边差量不会绕过 producer gate 与 queue-page grace。
pub(crate) fn stage_edge_delta(
    pool: &ReturnNodePool,
    inbox: Option<&OwnerInbox>,
    staging: &mut ProducerStaging,
    delta: &EdgeDelta,
    shard: ShardIndex,
    forced: Option<FlushTrigger>,
) -> Result<Option<PublishOutcome>, RawInvariant> {
    let mut outcome = None;
    if staging
        .target()
        .is_some_and(|target| target != delta.target)
    {
        return Err(RawInvariant::new("staging 目标改变前必须先发布旧 chain"));
    }
    let node = pool.allocate()?;
    pool.store_edge_delta(node, delta, delta.integrity.checksum);
    pool.link(node, None);
    if let Some(last) = staging.last() {
        pool.link(last, Some(node));
    }
    staging.stage(node, delta.target, delta.bytes, shard)?;
    if let Some(trigger) = forced.or_else(|| staging.flush_trigger())
        && let Some(inbox) = inbox
    {
        outcome = Some(flush_staging(pool, inbox, staging, trigger)?);
    }
    Ok(outcome)
}

/// 发布当前 staging 的全部 item。
pub(crate) fn flush_staging(
    pool: &ReturnNodePool,
    inbox: &OwnerInbox,
    staging: &mut ProducerStaging,
    trigger: FlushTrigger,
) -> Result<PublishOutcome, RawInvariant> {
    let Some(chain) = staging.drain() else {
        return Ok(PublishOutcome {
            items: 0,
            bytes: 0,
            trigger,
        });
    };
    let target = chain
        .target
        .ok_or_else(|| RawInvariant::new("发布 chain 缺少目标 owner"))?;
    let _ = target;
    inbox.publish_batch(&chain, pool)?;
    Ok(PublishOutcome {
        items: chain.count,
        bytes: chain.bytes,
        trigger,
    })
}
