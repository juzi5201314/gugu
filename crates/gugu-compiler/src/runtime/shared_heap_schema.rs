//! SharedHeap stable handle、access guard、forwarding grace 与共享字段屏障的契约段。
//!
//! 本段把「跨 owner managed 对象的稳定身份编码」「handle slot 与 payload record 的固定布局」
//! 「slot 状态机与唯一允许迁移」「forwarding grace 步数」「`HandleForward` 消息字段集合」与
//! 「由优化后 LIR 推导的 SharedHeap 需求」固定成带版本的对象，与 `local_heap_schema`/
//! `mark_schema`/`barrier_schema` 共用 `build`/`verify`/`canonical_bytes`/`fingerprint`/
//! `dump` 闭环。
//!
//! 契约只登记身份位宽、容量、状态名、迁移与字段偏移，不登记宿主地址、payload 内容或线程编号：
//! 跨 owner 发布的身份是 `table/slot/generation` 三元组，payload 搬迁只切换 slot 的 current
//! payload，旧 payload 在 access guard、pin、mark ticket 与 forwarding grace 全部结清后才回收。
//! 共享访问的额外状态全部留在本段与 `shared_heap`，LocalHeap 的 direct pointer 热路径不登记
//! 任何 guard、handle 或 forwarding 字段。

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::mem::{align_of, offset_of, size_of};

use super::model::{FieldKind, MessageFieldSchema, MessageSchemaV1, RawModelError};

/// SharedHeap 契约段 schema。
pub(crate) const SHARED_HEAP_SCHEMA: u32 = 1;

/// 内建 SharedHeap profile 名。
pub(crate) const SHARED_HEAP_PROFILE_NAME: &str = "mosaic-shared-handle";
/// SharedHeap profile 的 revision；位宽、状态机或 grace 变化都必须递增。
pub(crate) const SHARED_HEAP_PROFILE_REVISION: u32 = 1;

/// stable handle 的身份 tag；高 4 位，使 handle 与任何 direct pointer 或压缩引用不可能混淆。
pub(crate) const SHARED_HANDLE_TAG: u8 = 0xA;
/// tag 字段位宽。
pub(crate) const SHARED_HANDLE_TAG_BITS: u32 = 4;
/// SharedHeap 表身份位宽；table 是逻辑表编号，不是宿主地址。
pub(crate) const SHARED_HANDLE_TABLE_BITS: u32 = 12;
/// slot 字段位宽。
pub(crate) const SHARED_HANDLE_SLOT_BITS: u32 = 24;
/// handle generation 字段位宽。
pub(crate) const SHARED_HANDLE_GENERATION_BITS: u32 = 24;
/// 一个 handle slot 的规范字节数。
pub(crate) const SHARED_HANDLE_SLOT_BYTES: u32 = 64;
/// 一条 shared payload record 的规范字节数。
pub(crate) const SHARED_PAYLOAD_RECORD_BYTES: u32 = 32;
/// forwarding 从切换 payload 到允许回收旧 payload 之间必须推进的 grace 步数。
pub(crate) const SHARED_FORWARDING_GRACE_STEPS: u32 = 4;

/// 表编号的合法上界（不含）；超出掩码必须拒绝，不静默截断。
pub(crate) const SHARED_HANDLE_TABLE_LIMIT: u32 = 1 << SHARED_HANDLE_TABLE_BITS;
/// slot 编号的合法上界（不含）。
pub(crate) const SHARED_HANDLE_SLOT_LIMIT: u32 = 1 << SHARED_HANDLE_SLOT_BITS;
/// generation 的合法上界（不含）；0 保留给「无效身份」。
pub(crate) const SHARED_HANDLE_GENERATION_LIMIT: u32 = 1 << SHARED_HANDLE_GENERATION_BITS;
/// slot 与 payload 的初始 generation；0 表示尚未发布。
pub(crate) const SHARED_HANDLE_INITIAL_GENERATION: u32 = 1;

const _: () = assert!(
    SHARED_HANDLE_TAG_BITS
        + SHARED_HANDLE_TABLE_BITS
        + SHARED_HANDLE_SLOT_BITS
        + SHARED_HANDLE_GENERATION_BITS
        == 64
);
const _: () = assert!(size_of::<SharedHandleSlot>() == SHARED_HANDLE_SLOT_BYTES as usize);
const _: () = assert!(size_of::<SharedPayloadRecord>() == SHARED_PAYLOAD_RECORD_BYTES as usize);

/// payload identity 的 slot 字段位宽；generation 占高 32 位。
pub(crate) const SHARED_PAYLOAD_SLOT_BITS: u32 = 32;

/// slot 状态名；顺序即判别值，也是迁移表的取值域。
pub(crate) const SHARED_SLOT_STATE_NAMES: [&str; 6] = [
    "free",
    "live",
    "forwarding",
    "grace",
    "reclaimable",
    "owned-free",
];

/// 唯一允许的 slot 状态迁移；任何其它迁移都是 `RawInvariant`。
///
/// `free -> live` 是首次发布、`owned-free -> live` 是已释放槽的下一次发布（发布时推进 handle
/// generation，因此复用不会让旧 handle 重新生效）；`live -> forwarding`/`forwarding -> grace`
/// 是搬迁、`grace -> live` 是旧 payload 回收完成、`live -> owned-free` 是释放、
/// `grace -> reclaimable -> live` 是 grace 结束后先冻结旧 payload 再回到可复用状态。
pub(crate) const SHARED_SLOT_TRANSITIONS: [(&str, &str); 8] = [
    ("free", "live"),
    ("live", "forwarding"),
    ("forwarding", "grace"),
    ("grace", "live"),
    ("live", "owned-free"),
    ("grace", "reclaimable"),
    ("reclaimable", "live"),
    ("owned-free", "live"),
];

/// 一条 SharedHeap 记录的字段布局；offset 由 `offset_of!` 派生，不可能与 `#[repr(C)]` 漂移。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SharedHeapRecordField {
    /// 字段名。
    pub name: String,
    /// 字段字节偏移。
    pub offset: u32,
}

/// 一条 SharedHeap 记录的完整布局；由内建 `std/runtime/heap.gg` 逐字段交叉校验。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SharedHeapRecordLayout {
    /// record 名。
    pub name: String,
    /// record 字节数。
    pub bytes: u32,
    /// record 对齐。
    pub alignment: u32,
    /// 声明顺序的字段偏移。
    pub fields: Vec<SharedHeapRecordField>,
}

/// 一条状态迁移登记项。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SharedSlotTransition {
    /// 起始状态名。
    pub from: String,
    /// 目标状态名。
    pub to: String,
}

/// slot 状态判别值；与 `SHARED_SLOT_STATE_NAMES` 同序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SharedSlotState {
    /// 空槽；handle generation 在复用时才推进。
    Free,
    /// 有 current payload 的活跃槽。
    Live,
    /// 正在切换 payload：new payload 已建立，current 尚未切换。
    Forwarding,
    /// current 已切换，旧 payload 处于 forwarding grace。
    Grace,
    /// grace 结束但旧 payload 尚未释放。
    Reclaimable,
    /// 已释放给 owner，等待下一次发布。
    OwnedFree,
}

impl SharedSlotState {
    /// 返回判别值。
    pub(crate) const fn raw(self) -> u32 {
        match self {
            Self::Free => 0,
            Self::Live => 1,
            Self::Forwarding => 2,
            Self::Grace => 3,
            Self::Reclaimable => 4,
            Self::OwnedFree => 5,
        }
    }

    /// 由判别值还原；未知判别值是契约违约。
    pub(crate) const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Live),
            2 => Some(Self::Forwarding),
            3 => Some(Self::Grace),
            4 => Some(Self::Reclaimable),
            5 => Some(Self::OwnedFree),
            _ => None,
        }
    }

    /// 返回状态名；错误信息与 dump 按它点名状态。
    pub(crate) fn name(self) -> &'static str {
        SHARED_SLOT_STATE_NAMES[usize::try_from(self.raw()).expect("判别值适配下标")]
    }
}

/// 跨 owner managed 对象的稳定身份：`table/slot/generation`。
///
/// table 是 SharedHeap 表身份，不是宿主地址；`raw()` 只做数值编码，高 4 位 tag 保证它与任何
/// direct pointer、interior pointer 或压缩引用都不可混淆。generation 为 0 不是合法身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SharedHandle {
    table: u32,
    slot: u32,
    generation: u32,
}

impl SharedHandle {
    /// 由三个字段构造身份；越界或 generation 为 0 直接拒绝，不静默截断。
    pub(crate) fn new(table: u32, slot: u32, generation: u32) -> Result<Self, RawModelError> {
        if table >= SHARED_HANDLE_TABLE_LIMIT {
            return Err(RawModelError::new("shared handle table 超出登记位宽"));
        }
        if slot >= SHARED_HANDLE_SLOT_LIMIT {
            return Err(RawModelError::new("shared handle slot 超出登记位宽"));
        }
        if generation == 0 || generation >= SHARED_HANDLE_GENERATION_LIMIT {
            return Err(RawModelError::new("shared handle generation 非法"));
        }
        Ok(Self {
            table,
            slot,
            generation,
        })
    }

    /// 返回 SharedHeap 表身份。
    pub(crate) const fn table(self) -> u32 {
        self.table
    }

    /// 返回 slot 编号。
    pub(crate) const fn slot(self) -> u32 {
        self.slot
    }

    /// 返回 handle generation。
    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }

    /// 返回 `tag | table | generation | slot` 的数值编码。
    pub(crate) fn raw(self) -> u64 {
        (u64::from(SHARED_HANDLE_TAG) << (64 - SHARED_HANDLE_TAG_BITS))
            | (u64::from(self.table) << (SHARED_HANDLE_SLOT_BITS + SHARED_HANDLE_GENERATION_BITS))
            | (u64::from(self.generation) << SHARED_HANDLE_SLOT_BITS)
            | u64::from(self.slot)
    }

    /// 由数值编码还原身份；tag 不匹配与 generation 为 0 都拒绝，不按 slot 猜测对象。
    pub(crate) fn from_raw(raw: u64) -> Result<Self, RawModelError> {
        let tag = u8::try_from((raw >> (64 - SHARED_HANDLE_TAG_BITS)) & 0x0f)
            .map_err(|_| RawModelError::new("shared handle tag 字段超出 u8"))?;
        if tag != SHARED_HANDLE_TAG {
            return Err(RawModelError::new("shared handle tag 不匹配"));
        }
        let table = u32::try_from(
            (raw >> (SHARED_HANDLE_SLOT_BITS + SHARED_HANDLE_GENERATION_BITS))
                & u64::from(SHARED_HANDLE_TABLE_LIMIT - 1),
        )
        .map_err(|_| RawModelError::new("shared handle table 超出 u32"))?;
        let generation = u32::try_from(
            (raw >> SHARED_HANDLE_SLOT_BITS) & u64::from(SHARED_HANDLE_GENERATION_LIMIT - 1),
        )
        .map_err(|_| RawModelError::new("shared handle generation 超出 u32"))?;
        let slot = u32::try_from(raw & u64::from(SHARED_HANDLE_SLOT_LIMIT - 1))
            .map_err(|_| RawModelError::new("shared handle slot 超出 u32"))?;
        if generation == 0 {
            return Err(RawModelError::new("shared handle generation 为 0"));
        }
        Self::new(table, slot, generation)
    }
}

/// 一个 shared payload 的逻辑身份：`generation(32) | slot(32)`。
///
/// slot 在 payload 表内稠密且可复用；每次复用推进 generation，因此重放已经释放的旧 identity
/// 必然因为 generation 不匹配被拒绝。payload identity 只出现在逻辑记录与消息车道里，不是地址。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SharedPayloadId(u64);

impl SharedPayloadId {
    /// 由 slot 与 generation 组装身份。
    pub(crate) fn new(slot: u32, generation: u32) -> Self {
        Self((u64::from(generation) << SHARED_PAYLOAD_SLOT_BITS) | u64::from(slot))
    }

    /// 由原始 64-bit 编码还原。
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// 返回原始 64-bit 编码。
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// 返回 slot 编号；低 32 位由编码定义。
    pub(crate) const fn slot(self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32 // 掩码后必然落在 u32
    }

    /// 返回 generation；高 32 位由编码定义。
    pub(crate) const fn generation(self) -> u32 {
        (self.0 >> SHARED_PAYLOAD_SLOT_BITS) as u32 // 掩码后必然落在 u32
    }

    /// 判断身份是否有效：generation 0 不是任何 payload 的合法身份。
    pub(crate) const fn is_valid(self) -> bool {
        self.generation() != 0
    }
}

/// 一个 handle slot 的固定布局。
///
/// `current_payload`/`old_payload` 是 payload identity，不是地址：搬迁只切换 current，旧 payload
/// 在 access guard、pin、mark ticket 与 forwarding lease 全部结清后才释放。计数与 owner/block
/// 字段使 grace 判定与路由不需要任何旁路表。
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SharedHandleSlot {
    /// slot 的 handle generation；复用时推进，旧 handle 因此必然失败。
    pub generation: u32,
    /// slot 状态判别值；取值域由 `SharedSlotState` 固定。
    pub state: u32,
    /// 当前 payload identity。
    pub current_payload: u64,
    /// 处于 forwarding grace 的旧 payload identity。
    pub old_payload: u64,
    /// 最近一次成功 forward 的 forward generation。
    pub forward_generation: u32,
    /// 已经推进的 grace 步数。
    pub grace_epoch: u32,
    /// 仍然 active 的 access guard 数量。
    pub access_guards: u32,
    /// 仍然生效的 pin lease 数量。
    pub pin_leases: u32,
    /// 已经发出但尚未结清的 mark ticket 数量。
    pub mark_tickets: u32,
    /// 仍然在飞、引用旧 payload 的 forwarding lease 数量。
    pub forwarding_leases: u32,
    /// 拥有 payload 的 owner 编号。
    pub owner_id: u32,
    /// payload 所在共享 block 的全局身份。
    pub block_id: u32,
    /// payload 的逻辑字节数。
    pub payload_bytes: u32,
    /// 保留位，必须为 0。
    pub reserved: u32,
}

/// 一条 shared payload 的逻辑记录。
///
/// 只保存 payload identity、block/offset、generation、owner、bytes 与状态；不含 managed 或 raw
/// 地址，因为 payload 的物理位置由 owner 的 block registry 解析。
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SharedPayloadRecord {
    /// payload 的 generation。
    pub payload_generation: u32,
    /// payload 在 payload 表内的 slot。
    pub payload_slot: u32,
    /// payload 所在共享 block 的全局身份。
    pub block_id: u32,
    /// payload 在 block 内的字节偏移。
    pub block_offset: u32,
    /// 建立该 record 时的 slot generation。
    pub generation: u32,
    /// 拥有 payload 的 owner 编号。
    pub owner_id: u32,
    /// payload 的逻辑字节数。
    pub bytes: u32,
    /// payload record 的状态判别值；与 slot 状态同域。
    pub state: u32,
}

/// 从优化后 LIR 推导的 SharedHeap 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SharedHeapDemand {
    /// SharedHeap placement 的分配站点数。
    pub alloc_sites: u32,
    /// `ResolveSharedHandle` 站点数；每个 fresh payload 恰解析一次。
    pub resolve_sites: u32,
    /// `SharedAccessBegin` 站点数。
    pub access_begin_sites: u32,
    /// `SharedAccessEnd` 站点数。
    pub access_end_sites: u32,
    /// `ForwardSharedHandle` 站点数。
    pub forward_sites: u32,
    /// shared pin 站点数。
    pub pin_sites: u32,
    /// 需要跨 owner mark ticket 的 shared 站点数。
    pub mark_sites: u32,
    /// `SharedFieldBarrier` 站点数。
    pub barrier_sites: u32,
    /// 搬迁时复制 payload 字节的站点数；与 forward 站点同源。
    pub payload_copy_sites: u32,
    /// 同时存在的 handle slot 上界；等于分配站点数。
    pub handle_slots: u32,
    /// 单个 shared payload 的最大逻辑字节数。
    pub max_payload_bytes: u64,
}

impl SharedHeapDemand {
    /// 返回需求视图的稳定指纹；字段顺序即编码顺序。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(48);
        bytes.extend_from_slice(&self.alloc_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resolve_sites.to_le_bytes());
        bytes.extend_from_slice(&self.access_begin_sites.to_le_bytes());
        bytes.extend_from_slice(&self.access_end_sites.to_le_bytes());
        bytes.extend_from_slice(&self.forward_sites.to_le_bytes());
        bytes.extend_from_slice(&self.pin_sites.to_le_bytes());
        bytes.extend_from_slice(&self.mark_sites.to_le_bytes());
        bytes.extend_from_slice(&self.barrier_sites.to_le_bytes());
        bytes.extend_from_slice(&self.payload_copy_sites.to_le_bytes());
        bytes.extend_from_slice(&self.handle_slots.to_le_bytes());
        bytes.extend_from_slice(&self.max_payload_bytes.to_le_bytes());
        crate::frontend::mono::keys::hash_domain("gugu-shared-heap-demand-v1", &bytes)
    }

    /// 校验需求自身的可证明关系；跨段关系由 `RuntimeRawContractV1::verify` 检查。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.alloc_sites != self.resolve_sites || self.alloc_sites != self.handle_slots {
            return Err(RawModelError::new(
                "SharedHeap 分配站点、解析站点与 handle slot 上界必须相等",
            ));
        }
        if self.access_begin_sites != self.access_end_sites {
            return Err(RawModelError::new(
                "SharedHeap access guard 的 begin 与 end 站点必须相等",
            ));
        }
        if self.payload_copy_sites != self.forward_sites {
            return Err(RawModelError::new(
                "SharedHeap payload 复制站点必须与 forward 站点相等",
            ));
        }
        let mut total = 0u32;
        for count in [
            self.alloc_sites,
            self.access_begin_sites,
            self.forward_sites,
            self.pin_sites,
            self.mark_sites,
            self.barrier_sites,
        ] {
            total = total
                .checked_add(count)
                .ok_or_else(|| RawModelError::new("SharedHeap 站点计数溢出"))?;
        }
        if self.forward_sites > self.alloc_sites {
            return Err(RawModelError::new(
                "SharedHeap forward 站点不得超过分配站点",
            ));
        }
        if self.barrier_sites > self.access_begin_sites {
            return Err(RawModelError::new(
                "SharedHeap 共享字段屏障站点不得超过 access guard 站点",
            ));
        }
        Ok(())
    }

    /// 返回需求是否为空：空需求仍建立完整契约段。
    pub(crate) const fn is_empty(&self) -> bool {
        self.alloc_sites == 0
            && self.access_begin_sites == 0
            && self.forward_sites == 0
            && self.pin_sites == 0
            && self.mark_sites == 0
            && self.barrier_sites == 0
    }
}

/// 已验证的 SharedHeap runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SharedHeapRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// handle 身份 tag。
    pub handle_tag: u32,
    /// tag 字段位宽。
    pub handle_tag_bits: u32,
    /// table 字段位宽。
    pub handle_table_bits: u32,
    /// slot 字段位宽。
    pub handle_slot_bits: u32,
    /// generation 字段位宽。
    pub handle_generation_bits: u32,
    /// 一个 handle slot 的字节数。
    pub handle_slot_bytes: u32,
    /// 一条 payload record 的字节数。
    pub payload_record_bytes: u32,
    /// forwarding grace 步数。
    pub forwarding_grace_steps: u32,
    /// slot 状态目录；顺序即判别值。
    pub states: Vec<String>,
    /// 允许的状态迁移目录。
    pub transitions: Vec<SharedSlotTransition>,
    /// 固定 record 目录。
    pub records: Vec<SharedHeapRecordLayout>,
    /// `HandleForward` 的字段集合；禁止携带地址。
    pub(crate) handle_forward: MessageSchemaV1,
    /// 上游需求视图。
    pub demand: SharedHeapDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl SharedHeapRuntimeContract {
    /// 由需求视图构建契约；空需求仍建立完整契约状态。
    pub(crate) fn build(demand: SharedHeapDemand) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: SHARED_HEAP_SCHEMA,
            profile: SHARED_HEAP_PROFILE_NAME.to_owned(),
            profile_revision: SHARED_HEAP_PROFILE_REVISION,
            handle_tag: u32::from(SHARED_HANDLE_TAG),
            handle_tag_bits: SHARED_HANDLE_TAG_BITS,
            handle_table_bits: SHARED_HANDLE_TABLE_BITS,
            handle_slot_bits: SHARED_HANDLE_SLOT_BITS,
            handle_generation_bits: SHARED_HANDLE_GENERATION_BITS,
            handle_slot_bytes: SHARED_HANDLE_SLOT_BYTES,
            payload_record_bytes: SHARED_PAYLOAD_RECORD_BYTES,
            forwarding_grace_steps: SHARED_FORWARDING_GRACE_STEPS,
            states: SHARED_SLOT_STATE_NAMES
                .iter()
                .map(|state| (*state).to_owned())
                .collect(),
            transitions: SHARED_SLOT_TRANSITIONS
                .iter()
                .map(|(from, to)| SharedSlotTransition {
                    from: (*from).to_owned(),
                    to: (*to).to_owned(),
                })
                .collect(),
            records: fixed_layouts(),
            handle_forward: MessageSchemaV1::handle_forward(),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回内部 schema 版本。
    pub const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 profile 名。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回 handle tag。
    pub const fn handle_tag(&self) -> u32 {
        self.handle_tag
    }

    /// 返回一个 handle slot 的字节数。
    pub const fn handle_slot_bytes(&self) -> u32 {
        self.handle_slot_bytes
    }

    /// 返回一条 payload record 的字节数。
    pub const fn payload_record_bytes(&self) -> u32 {
        self.payload_record_bytes
    }

    /// 返回 forwarding grace 步数。
    pub const fn forwarding_grace_steps(&self) -> u32 {
        self.forwarding_grace_steps
    }

    /// 返回 slot 状态数量。
    pub fn state_count(&self) -> u32 {
        u32::try_from(self.states.len()).expect("状态数量适配 u32")
    }

    /// 返回状态迁移数量。
    pub fn transition_count(&self) -> u32 {
        u32::try_from(self.transitions.len()).expect("迁移数量适配 u32")
    }

    /// 返回 record 数量。
    pub fn record_count(&self) -> u32 {
        u32::try_from(self.records.len()).expect("record 数量适配 u32")
    }

    /// 返回 `HandleForward` 字段数。
    pub fn handle_forward_field_count(&self) -> u32 {
        u32::try_from(self.handle_forward.fields.len()).expect("字段数量适配 u32")
    }

    /// 返回 `HandleForward` 字段集合。
    pub(crate) const fn handle_forward_fields(&self) -> &MessageSchemaV1 {
        &self.handle_forward
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> SharedHeapDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 判断一次状态迁移是否被登记表允许。
    pub(crate) fn allows_transition(from: SharedSlotState, to: SharedSlotState) -> bool {
        SHARED_SLOT_TRANSITIONS
            .iter()
            .any(|(start, end)| *start == from.name() && *end == to.name())
    }

    /// 校验契约：常量、状态机目录、record 布局、消息字段与需求关系。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        self.verify_constants()?;
        self.verify_catalogs()?;
        self.verify_layouts()?;
        self.handle_forward
            .verify_family(super::barrier_schema::MessageFamilyTag::HandleForward)
            .map_err(|error| RawModelError::new(error.message().to_owned()))?;
        self.demand.verify()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("SharedHeap 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 校验固定参数与 profile 身份。
    fn verify_constants(&self) -> Result<(), RawModelError> {
        if self.schema != SHARED_HEAP_SCHEMA
            || self.profile != SHARED_HEAP_PROFILE_NAME
            || self.profile_revision != SHARED_HEAP_PROFILE_REVISION
            || self.handle_tag != u32::from(SHARED_HANDLE_TAG)
            || self.handle_tag_bits != SHARED_HANDLE_TAG_BITS
            || self.handle_table_bits != SHARED_HANDLE_TABLE_BITS
            || self.handle_slot_bits != SHARED_HANDLE_SLOT_BITS
            || self.handle_generation_bits != SHARED_HANDLE_GENERATION_BITS
            || self.handle_slot_bytes != SHARED_HANDLE_SLOT_BYTES
            || self.payload_record_bytes != SHARED_PAYLOAD_RECORD_BYTES
            || self.forwarding_grace_steps != SHARED_FORWARDING_GRACE_STEPS
        {
            return Err(RawModelError::new(
                "SharedHeap profile 参数与登记值不一致，或参数未随 revision 变化",
            ));
        }
        if self.handle_tag_bits
            + self.handle_table_bits
            + self.handle_slot_bits
            + self.handle_generation_bits
            != 64
        {
            return Err(RawModelError::new(
                "SharedHeap handle 身份的四段位宽必须合计 64",
            ));
        }
        if self.handle_slot_bytes
            != u32::try_from(size_of::<SharedHandleSlot>()).expect("slot 适配")
            || self.payload_record_bytes
                != u32::try_from(size_of::<SharedPayloadRecord>()).expect("record 适配")
        {
            return Err(RawModelError::new("SharedHeap 记录尺寸与运行时类型不一致"));
        }
        Ok(())
    }

    /// 校验状态目录与迁移表：每个迁移端点必须是登记状态，且不允许自环。
    fn verify_catalogs(&self) -> Result<(), RawModelError> {
        if self.states.len() != SHARED_SLOT_STATE_NAMES.len()
            || self
                .states
                .iter()
                .zip(SHARED_SLOT_STATE_NAMES.iter())
                .any(|(name, expected)| name != expected)
        {
            return Err(RawModelError::new("SharedHeap slot 状态目录与登记表不一致"));
        }
        if self.transitions.len() != SHARED_SLOT_TRANSITIONS.len() {
            return Err(RawModelError::new("SharedHeap 状态迁移数量与登记表不一致"));
        }
        for (transition, (from, to)) in self.transitions.iter().zip(SHARED_SLOT_TRANSITIONS.iter())
        {
            if transition.from != *from || transition.to != *to {
                return Err(RawModelError::new("SharedHeap 状态迁移与登记表不一致"));
            }
            for endpoint in [&transition.from, &transition.to] {
                if !self.states.iter().any(|state| state == endpoint) {
                    return Err(RawModelError::new("SharedHeap 状态迁移端点未登记"));
                }
            }
        }
        if self
            .transitions
            .iter()
            .any(|transition| transition.from == transition.to)
        {
            return Err(RawModelError::new("SharedHeap 状态迁移不允许自环"));
        }
        Ok(())
    }

    /// 校验 record 布局与登记表逐项一致，并确认尺寸/对齐自洽。
    fn verify_layouts(&self) -> Result<(), RawModelError> {
        if self.records != fixed_layouts() {
            return Err(RawModelError::new("SharedHeap 记录布局与登记表不一致"));
        }
        for record in &self.records {
            if record.alignment == 0
                || !record.alignment.is_power_of_two()
                || !record.bytes.is_multiple_of(record.alignment)
                || record.fields.is_empty()
            {
                return Err(RawModelError::new(
                    "SharedHeap 记录尺寸、对齐或字段集合非法",
                ));
            }
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.profile.len() as u32).to_le_bytes());
        bytes.extend_from_slice(self.profile.as_bytes());
        bytes.extend_from_slice(&self.profile_revision.to_le_bytes());
        bytes.extend_from_slice(&self.handle_tag.to_le_bytes());
        bytes.extend_from_slice(&self.handle_tag_bits.to_le_bytes());
        bytes.extend_from_slice(&self.handle_table_bits.to_le_bytes());
        bytes.extend_from_slice(&self.handle_slot_bits.to_le_bytes());
        bytes.extend_from_slice(&self.handle_generation_bits.to_le_bytes());
        bytes.extend_from_slice(&self.handle_slot_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.payload_record_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.forwarding_grace_steps.to_le_bytes());
        for state in &self.states {
            bytes.extend_from_slice(&(state.len() as u32).to_le_bytes());
            bytes.extend_from_slice(state.as_bytes());
        }
        for transition in &self.transitions {
            for endpoint in [&transition.from, &transition.to] {
                bytes.extend_from_slice(&(endpoint.len() as u32).to_le_bytes());
                bytes.extend_from_slice(endpoint.as_bytes());
            }
        }
        for record in &self.records {
            bytes.extend_from_slice(&(record.name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(record.name.as_bytes());
            bytes.extend_from_slice(&record.bytes.to_le_bytes());
            bytes.extend_from_slice(&record.alignment.to_le_bytes());
            bytes.extend_from_slice(&(record.fields.len() as u32).to_le_bytes());
            for field in &record.fields {
                bytes.extend_from_slice(&(field.name.len() as u32).to_le_bytes());
                bytes.extend_from_slice(field.name.as_bytes());
                bytes.extend_from_slice(&field.offset.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.handle_forward.canonical_bytes());
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes
    }

    /// 计算契约指纹。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        crate::frontend::mono::keys::hash_domain(
            "gugu-shared-heap-contract-v1",
            &self.canonical_bytes(),
        )
    }

    /// 返回人类可读的契约 dump；不含地址与 payload 内容。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "shared-heap schema={} profile={} revision={} tag={} tag-bits={} table-bits={} slot-bits={} generation-bits={}",
            self.schema,
            self.profile,
            self.profile_revision,
            self.handle_tag,
            self.handle_tag_bits,
            self.handle_table_bits,
            self.handle_slot_bits,
            self.handle_generation_bits
        );
        let _ = writeln!(
            out,
            "shared-heap-layout slot-bytes={} payload-record-bytes={} grace-steps={} states={} transitions={}",
            self.handle_slot_bytes,
            self.payload_record_bytes,
            self.forwarding_grace_steps,
            self.states.join(","),
            self.transitions
                .iter()
                .map(|transition| format!("{}->{}", transition.from, transition.to))
                .collect::<Vec<_>>()
                .join(",")
        );
        for record in &self.records {
            let _ = writeln!(
                out,
                "shared-heap-record {} bytes={} align={} fields={}",
                record.name,
                record.bytes,
                record.alignment,
                record
                    .fields
                    .iter()
                    .map(|field| format!("{}@{}", field.name, field.offset))
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        let _ = writeln!(
            out,
            "shared-heap-forward-fields {}",
            self.handle_forward
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        let _ = writeln!(
            out,
            "shared-heap-demand alloc={} resolve={} begin={} end={} forward={} pin={} mark={} barrier={} payload-copy={} slots={} max-payload-bytes={}",
            self.demand.alloc_sites,
            self.demand.resolve_sites,
            self.demand.access_begin_sites,
            self.demand.access_end_sites,
            self.demand.forward_sites,
            self.demand.pin_sites,
            self.demand.mark_sites,
            self.demand.barrier_sites,
            self.demand.payload_copy_sites,
            self.demand.handle_slots,
            self.demand.max_payload_bytes
        );
        let _ = writeln!(
            out,
            "shared-heap-fingerprint {}",
            hex_lower(self.fingerprint)
        );
        out
    }
}

/// 登记 `HandleForward` 的字段集合：只允许稳定 handle 身份、payload identity、generation、
/// epoch 与 bytes，两个地址类字段在 verifier 中被拒绝。
pub(crate) fn handle_forward_fields() -> Vec<MessageFieldSchema> {
    let mut fields = vec![
        MessageFieldSchema::new("bytes", FieldKind::Bytes),
        MessageFieldSchema::new("cycle_epoch", FieldKind::Epoch),
        MessageFieldSchema::new("family", FieldKind::KindTag),
        MessageFieldSchema::new("forward_generation", FieldKind::Generation),
        MessageFieldSchema::new("handle_generation", FieldKind::Generation),
        MessageFieldSchema::new("handle_slot", FieldKind::UnitIndex),
        MessageFieldSchema::new("handle_table", FieldKind::DescriptorIndex),
        MessageFieldSchema::new("integrity", FieldKind::Integrity),
        MessageFieldSchema::new("new_payload", FieldKind::PayloadIdentity),
        MessageFieldSchema::new("old_payload", FieldKind::PayloadIdentity),
        MessageFieldSchema::new("state", FieldKind::MessageState),
        MessageFieldSchema::new("target.domain", FieldKind::OwnerDomain),
        MessageFieldSchema::new("target.generation", FieldKind::Generation),
        MessageFieldSchema::new("target.owner_id", FieldKind::OwnerId),
        MessageFieldSchema::new("target.route_key", FieldKind::RouteKey),
        MessageFieldSchema::new("topology_epoch", FieldKind::Epoch),
    ];
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    fields
}

/// 返回 SharedHeap 固定 record 目录；offset 由 `offset_of!` 派生。
fn fixed_layouts() -> Vec<SharedHeapRecordLayout> {
    macro_rules! record {
        ($ty:ty; $($field:ident),+ $(,)?) => {
            SharedHeapRecordLayout {
                name: stringify!($ty).to_owned(),
                bytes: u32::try_from(size_of::<$ty>()).expect("record适配u32"),
                alignment: u32::try_from(align_of::<$ty>()).expect("alignment适配u32"),
                fields: vec![$(SharedHeapRecordField {
                    name: stringify!($field).to_owned(),
                    offset: u32::try_from(offset_of!($ty, $field)).expect("offset适配u32"),
                }),+],
            }
        };
    }
    vec![
        record!(SharedHandleSlot;
            generation, state, current_payload, old_payload, forward_generation, grace_epoch,
            access_guards, pin_leases, mark_tickets, forwarding_leases, owner_id, block_id,
            payload_bytes, reserved,
        ),
        record!(SharedPayloadRecord;
            payload_generation, payload_slot, block_id, block_offset, generation, owner_id,
            bytes, state,
        ),
    ]
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}
