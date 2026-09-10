//! ResourceCell slab、lease 状态机与统一 release 入口的确定性参照实现。
//!
//! ResourceCell 是外部资源的共享逻辑身份：地址稳定、不含 managed pointer，从专用 slab
//! 分配。cell header 固定 64 byte，字段顺序与宽度由 \`CELL_HEADER_LAYOUT\` 固定。lease、
//! close、release 与回收由状态位线性化：\`SHARED\`、\`CLOSED\`、\`RELEASE_QUEUED\`、
//! \`RELEASE_DONE\`、\`RECLAIMING\`，状态永不回到 Local。
//!
//! close 与最后一个 lease 竞争同一个 release 线性化点：首个 \`request_release\` 赢家把
//! \`ReleaseTicket\` 送入 release queue 并执行一次受限 cleanup，其它路径只结束自身 lease。
//! 只有同时观察到 \`leases == 0\` 与 \`RELEASE_DONE\` 的一方能把 slot 归还 free
//! structure 并推进 generation。受限 cleanup 不执行用户代码：它只递增计数器并记录 ticket。

use super::provider::{RangeId, RangeProvider};
use super::size_class::{
    DropScanPolicy, RESOURCE_SLOT_HEADER_BYTES, RuntimeSizeClass, RuntimeSizeClassId,
};
use super::slab::{
    Epoch, OwnerAccounting, RawInvariant, SlabDescriptorId, SlabGeneration, SlabTable, SlotState,
};
use super::{RAW_SLAB_PAGE_BYTES, RESOURCE_CLASS_LADDER, RESOURCE_DEDICATED_ALIGN_LIMIT};

/// ResourceCell header 字节数；class 尺寸包含这段 header。
pub(crate) const CELL_HEADER_BYTES: u32 = RESOURCE_SLOT_HEADER_BYTES;
/// 超过该 payload 或对齐时改用独立 non-moving 整页 mapping。
pub(crate) const DEDICATED_PAYLOAD_LIMIT: u32 =
    RESOURCE_CLASS_LADDER[RESOURCE_CLASS_LADDER.len() - 1] - CELL_HEADER_BYTES;

/// \`state\` bit 0：已发布为共享状态。
pub(crate) const STATE_SHARED: u32 = 1;
/// \`state\` bit 1：显式 close 已经建立关闭线性化点。
pub(crate) const STATE_CLOSED: u32 = 1 << 1;
/// \`state\` bit 2：release 请求已经赢得唯一入队点。
pub(crate) const STATE_RELEASE_QUEUED: u32 = 1 << 2;
/// \`state\` bit 3：受限 cleanup 已完成。
pub(crate) const STATE_RELEASE_DONE: u32 = 1 << 3;
/// \`state\` bit 4：回收方已经取得唯一回收权。
pub(crate) const STATE_RECLAIMING: u32 = 1 << 4;
/// 已登记状态位的并集；其它位必须为 0。
pub(crate) const STATE_KNOWN_MASK: u32 =
    STATE_SHARED | STATE_CLOSED | STATE_RELEASE_QUEUED | STATE_RELEASE_DONE | STATE_RECLAIMING;

/// 发布后的 \`owner_coroutine\`：不再由单一创建协程更新。
pub(crate) const OWNER_SHARED: u64 = u64::MAX;
/// 统一 release 入口名；File/socket/process/lock/FFI 都经它登记。
pub(crate) const UNIFIED_RELEASE_ENTRY: &str = "std.resource.release";
/// cell \`flags\` bit 0：最后一个 lease 以 detach 语义结束。
pub(crate) const CELL_FLAG_DETACHED: u8 = 1;

/// cell header 的一个字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CellField {
    pub(crate) name: &'static str,
    pub(crate) offset: u32,
    pub(crate) bytes: u32,
}

impl CellField {
    const fn new(name: &'static str, offset: u32, bytes: u32) -> Self {
        Self {
            name,
            offset,
            bytes,
        }
    }
}

/// cell header 的规范字段顺序；偏移与 \`ResourceCell\` 字段一一对应。
pub(crate) const CELL_HEADER_LAYOUT: [CellField; 12] = [
    CellField::new("leases", 0, 8),
    CellField::new("state", 8, 4),
    CellField::new("payload_size", 12, 4),
    CellField::new("owner_coroutine", 16, 8),
    CellField::new("release_glue", 24, 8),
    CellField::new("release_descriptor_id", 32, 4),
    CellField::new("slab_class", 36, 2),
    CellField::new("payload_align_log2", 38, 1),
    CellField::new("flags", 39, 1),
    CellField::new("next_free", 40, 8),
    CellField::new("generation", 48, 8),
    CellField::new("reserved", 56, 8),
];

/// 校验 header 字段连续、无重叠且合计等于 \`CELL_HEADER_BYTES\`。
pub(crate) fn verify_header_layout() -> Result<(), RawInvariant> {
    let mut offset = 0_u32;
    for field in CELL_HEADER_LAYOUT {
        if field.offset != offset || field.bytes == 0 {
            return Err(RawInvariant::new(format!(
                "ResourceCell header 字段 {} 的偏移或宽度不连续",
                field.name
            )));
        }
        offset += field.bytes;
    }
    if offset != CELL_HEADER_BYTES {
        return Err(RawInvariant::new(
            "ResourceCell header 字段合计不等于登记字节数",
        ));
    }
    Ok(())
}

/// 状态迁移表的一项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CellTransition {
    pub(crate) from: &'static str,
    pub(crate) to: &'static str,
    pub(crate) trigger: &'static str,
}

/// ResourceCell 允许的状态迁移；实现只能沿这些边推进。
pub(crate) const CELL_TRANSITIONS: [CellTransition; 6] = [
    CellTransition {
        from: "Local",
        to: "Shared",
        trigger: "publish",
    },
    CellTransition {
        from: "Local",
        to: "Closed",
        trigger: "close",
    },
    CellTransition {
        from: "Shared",
        to: "Closed",
        trigger: "close",
    },
    CellTransition {
        from: "Closed",
        to: "ReleaseQueued",
        trigger: "release-request",
    },
    CellTransition {
        from: "ReleaseQueued",
        to: "ReleaseDone",
        trigger: "restricted-release",
    },
    CellTransition {
        from: "ReleaseDone",
        to: "Reclaimed",
        trigger: "try-reclaim",
    },
];

/// 一个资源种类登记项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceKindEntry {
    pub(crate) id: u8,
    pub(crate) name: &'static str,
    pub(crate) release_entry: &'static str,
    pub(crate) close_idempotent: bool,
}

/// File/socket/process/lock/FFI 的统一 release 入口目录。
pub(crate) const RESOURCE_KINDS: [ResourceKindEntry; 5] = [
    ResourceKindEntry {
        id: 0,
        name: "File",
        release_entry: UNIFIED_RELEASE_ENTRY,
        close_idempotent: true,
    },
    ResourceKindEntry {
        id: 1,
        name: "Socket",
        release_entry: UNIFIED_RELEASE_ENTRY,
        close_idempotent: true,
    },
    ResourceKindEntry {
        id: 2,
        name: "Process",
        release_entry: UNIFIED_RELEASE_ENTRY,
        close_idempotent: true,
    },
    ResourceKindEntry {
        id: 3,
        name: "Lock",
        release_entry: UNIFIED_RELEASE_ENTRY,
        close_idempotent: true,
    },
    ResourceKindEntry {
        id: 4,
        name: "Foreign",
        release_entry: UNIFIED_RELEASE_ENTRY,
        close_idempotent: true,
    },
];

/// 按稠密编号取资源种类。
pub(crate) fn kind_entry(id: u8) -> Option<&'static ResourceKindEntry> {
    RESOURCE_KINDS.iter().find(|entry| entry.id == id)
}

/// release 描述符禁止出现的能力位。
pub(crate) struct ReleaseFlags;

impl ReleaseFlags {
    /// payload 含 managed pointer 或 managed 引用。
    pub(crate) const HAS_MANAGED_PAYLOAD: u32 = 1;
    /// cleanup 捕获 owner 或协程状态。
    pub(crate) const CAPTURES_OWNER: u32 = 1 << 1;
    /// cleanup 会分配。
    pub(crate) const MAY_ALLOCATE: u32 = 1 << 2;
    /// cleanup 会 panic。
    pub(crate) const MAY_PANIC: u32 = 1 << 3;
    /// cleanup 会获取 Gugu 锁。
    pub(crate) const ACQUIRES_LOCK: u32 = 1 << 4;
    /// cleanup 会等待 channel 或 join。
    pub(crate) const AWAITS_CHANNEL: u32 = 1 << 5;
    /// cleanup 会启动协程。
    pub(crate) const SPAWNS: u32 = 1 << 6;
    /// 全部禁止位的并集；release 描述符必须为 0。
    pub(crate) const FORBIDDEN_MASK: u32 = Self::HAS_MANAGED_PAYLOAD
        | Self::CAPTURES_OWNER
        | Self::MAY_ALLOCATE
        | Self::MAY_PANIC
        | Self::ACQUIRES_LOCK
        | Self::AWAITS_CHANNEL
        | Self::SPAWNS;
}

/// 一个 release 描述符：受限 cleanup 的唯一登记形状。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReleaseDescriptor {
    pub(crate) kind_id: u8,
    pub(crate) release_glue: u64,
    pub(crate) release_descriptor_id: u32,
    pub(crate) payload_size: u32,
    pub(crate) payload_align_log2: u8,
    pub(crate) flags: u32,
}

impl ReleaseDescriptor {
    /// 校验权限位为空、glue 已登记且编号与种类一致。
    pub(crate) fn verify(&self, registry: &ReleaseRegistry) -> Result<(), RawInvariant> {
        if self.flags & ReleaseFlags::FORBIDDEN_MASK != 0 {
            return Err(RawInvariant::new(format!(
                "release 描述符 {} 申请了禁止的 cleanup 能力",
                self.release_descriptor_id
            )));
        }
        if !registry.glue_registered(self.release_glue) {
            return Err(RawInvariant::new(format!(
                "release glue {} 未在目录中登记",
                self.release_glue
            )));
        }
        let entry = kind_entry(self.kind_id)
            .ok_or_else(|| RawInvariant::new("release 描述符引用了未登记的资源种类"))?;
        if entry.release_entry != UNIFIED_RELEASE_ENTRY
            || self.release_descriptor_id != u32::from(entry.id)
        {
            return Err(RawInvariant::new(
                "release 描述符没有经统一 release 入口登记",
            ));
        }
        Ok(())
    }
}

/// 每个资源种类的 release 描述符与 glue 目录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReleaseRegistry {
    entries: Vec<ReleaseDescriptor>,
    glues: Vec<u64>,
}

impl ReleaseRegistry {
    /// 由资源种类目录构造：每个种类一个描述符，glue 编号由种类编号派生。
    pub(crate) fn builtin() -> Result<Self, RawInvariant> {
        let entries = RESOURCE_KINDS
            .iter()
            .map(|entry| ReleaseDescriptor {
                kind_id: entry.id,
                release_glue: 100 + u64::from(entry.id),
                release_descriptor_id: u32::from(entry.id),
                payload_size: 0,
                payload_align_log2: 0,
                flags: 0,
            })
            .collect::<Vec<_>>();
        let glues = entries.iter().map(|entry| entry.release_glue).collect();
        let registry = Self { entries, glues };
        registry.verify()?;
        Ok(registry)
    }

    /// 返回登记的全部 release 描述符。
    pub(crate) fn descriptors(&self) -> &[ReleaseDescriptor] {
        &self.entries
    }

    /// 返回统一 release 入口名。
    pub(crate) const fn unified_entry(&self) -> &'static str {
        UNIFIED_RELEASE_ENTRY
    }

    /// 判断 glue 编号是否已登记。
    pub(crate) fn glue_registered(&self, glue: u64) -> bool {
        self.glues.contains(&glue)
    }

    /// 按种类编号取描述符。
    pub(crate) fn for_kind(&self, kind_id: u8) -> Result<ReleaseDescriptor, RawInvariant> {
        self.entries
            .iter()
            .find(|entry| entry.kind_id == kind_id)
            .copied()
            .ok_or_else(|| RawInvariant::new("资源种类没有登记的 release 描述符"))
    }

    /// 校验描述符编号稠密、全部经统一入口且无禁止能力。
    pub(crate) fn verify(&self) -> Result<(), RawInvariant> {
        if self.entries.len() != RESOURCE_KINDS.len() {
            return Err(RawInvariant::new("release 目录与资源种类数量不一致"));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if usize::try_from(entry.release_descriptor_id).expect("编号适配宿主") != index {
                return Err(RawInvariant::new("release 描述符编号不稠密"));
            }
            entry.verify(self)?;
        }
        Ok(())
    }
}

/// 一次 release 请求的稳定记录；只携带逻辑序号与 generation。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReleaseTicket {
    pub(crate) descriptor: SlabDescriptorId,
    pub(crate) index: u32,
    pub(crate) generation: SlabGeneration,
    pub(crate) detached: bool,
}

/// lease 结束的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseOutcome {
    /// 仍有多方持有 lease。
    StillLeased,
    /// 当前执行者结束了最后一个 lease。
    LastLease,
}

/// 显式 close 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloseOutcome {
    /// 本次调用首次建立关闭线性化点。
    ClosedNow,
    /// 之前已经关闭；幂等返回成功。
    AlreadyClosed,
}

/// 一个 ResourceCell 的 64-byte header。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceCell {
    pub(crate) leases: u64,
    pub(crate) state: u32,
    pub(crate) payload_size: u32,
    pub(crate) owner_coroutine: u64,
    pub(crate) release_glue: u64,
    pub(crate) release_descriptor_id: u32,
    pub(crate) slab_class: u16,
    pub(crate) payload_align_log2: u8,
    pub(crate) flags: u8,
    pub(crate) next_free: u64,
    pub(crate) generation: u64,
    pub(crate) reserved: u64,
}

impl ResourceCell {
    fn empty() -> Self {
        Self {
            leases: 0,
            state: 0,
            payload_size: 0,
            owner_coroutine: 0,
            release_glue: 0,
            release_descriptor_id: 0,
            slab_class: 0,
            payload_align_log2: 0,
            flags: 0,
            next_free: 0,
            generation: 0,
            reserved: 0,
        }
    }

    /// 判断 \`SHARED\` 位。
    pub(crate) const fn is_shared(&self) -> bool {
        self.state & STATE_SHARED != 0
    }

    /// 判断 \`CLOSED\` 位。
    pub(crate) const fn is_closed(&self) -> bool {
        self.state & STATE_CLOSED != 0
    }

    /// 判断 \`RELEASE_QUEUED\` 位。
    pub(crate) const fn is_release_queued(&self) -> bool {
        self.state & STATE_RELEASE_QUEUED != 0
    }

    /// 判断 \`RELEASE_DONE\` 位。
    pub(crate) const fn is_release_done(&self) -> bool {
        self.state & STATE_RELEASE_DONE != 0
    }

    /// 判断 \`RECLAIMING\` 位。
    pub(crate) const fn is_reclaiming(&self) -> bool {
        self.state & STATE_RECLAIMING != 0
    }

    /// 判断 detach 标记。
    pub(crate) const fn is_detached(&self) -> bool {
        self.flags & CELL_FLAG_DETACHED != 0
    }
}

/// 与 slab descriptor 平行的 ResourceCell header 表。
///
/// 每个 slot 一个 header，按稠密 \`(descriptor, index)\` 下标访问；不使用映射容器。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResourceCellTable {
    cells: Vec<Vec<ResourceCell>>,
    cleanups: u64,
    records: Vec<ReleaseTicket>,
}

impl ResourceCellTable {
    /// 创建空表。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 为一个 descriptor 确保存在 \`slots\` 个 header。
    pub(crate) fn ensure(&mut self, descriptor: SlabDescriptorId, slots: u32) {
        while self.cells.len() <= descriptor.index() {
            self.cells.push(Vec::new());
        }
        let row = &mut self.cells[descriptor.index()];
        if row.len() < slots as usize {
            row.resize(slots as usize, ResourceCell::empty());
        }
    }

    /// 返回某个 slot 的 header。
    pub(crate) fn get(
        &self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<&ResourceCell, RawInvariant> {
        self.cells
            .get(descriptor.index())
            .and_then(|row| row.get(index as usize))
            .ok_or_else(|| RawInvariant::new("资源 cell 编号越过 slab"))
    }

    fn get_mut(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<&mut ResourceCell, RawInvariant> {
        self.cells
            .get_mut(descriptor.index())
            .and_then(|row| row.get_mut(index as usize))
            .ok_or_else(|| RawInvariant::new("资源 cell 编号越过 slab"))
    }

    /// 登记一个新 cell；创建协程持有首个 lease。
    pub(crate) fn place(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
        descriptor_record: &ReleaseDescriptor,
        payload_size: u32,
        payload_align_log2: u8,
        slab_class: u16,
        owner_coroutine: u64,
        generation: u64,
    ) -> Result<(), RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.leases != 0 || cell.state != 0 {
            return Err(RawInvariant::new("资源 slot 复用前没有完成回收"));
        }
        *cell = ResourceCell {
            leases: 1,
            state: 0,
            payload_size,
            owner_coroutine,
            release_glue: descriptor_record.release_glue,
            release_descriptor_id: descriptor_record.release_descriptor_id,
            slab_class,
            payload_align_log2,
            flags: 0,
            next_free: 0,
            generation,
            reserved: 0,
        };
        Ok(())
    }

    /// 从 raw header 恢复 cell；供复用路径与确定性测试构造边界状态。
    pub(crate) fn restore(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
        cell: ResourceCell,
    ) -> Result<(), RawInvariant> {
        *self.get_mut(descriptor, index)? = cell;
        Ok(())
    }

    /// 复制资源值：增加一个 lease。
    pub(crate) fn acquire(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<(), RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.is_reclaiming() {
            return Err(RawInvariant::new("正在回收的资源 cell 不能新增 lease"));
        }
        if cell.leases == u64::MAX {
            return Err(RawInvariant::new("ResourceCell lease 计数达到上界"));
        }
        cell.leases += 1;
        Ok(())
    }

    /// 结束一个 lease。
    pub(crate) fn release_lease(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<LeaseOutcome, RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.leases == 0 {
            return Err(RawInvariant::new("release 引用了没有 lease 的资源 cell"));
        }
        cell.leases -= 1;
        if cell.leases == 0 {
            Ok(LeaseOutcome::LastLease)
        } else {
            Ok(LeaseOutcome::StillLeased)
        }
    }

    /// 从仅创建协程可访问单向变为共享状态；状态永不回到 Local。
    pub(crate) fn publish(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<(), RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.is_reclaiming() || cell.is_shared() || cell.is_closed() {
            return Err(RawInvariant::new(
                "资源 cell 只能在未关闭的非共享状态发布一次",
            ));
        }
        cell.state |= STATE_SHARED;
        cell.owner_coroutine = OWNER_SHARED;
        Ok(())
    }

    /// 显式幂等 close；首个成功者建立关闭线性化点。
    pub(crate) fn close(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<CloseOutcome, RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.is_reclaiming() {
            return Err(RawInvariant::new("正在回收的资源 cell 不能 close"));
        }
        if cell.is_closed() {
            return Ok(CloseOutcome::AlreadyClosed);
        }
        cell.state |= STATE_CLOSED;
        Ok(CloseOutcome::ClosedNow)
    }

    /// 抢唯一的 release 入队点；已入队时返回 \`false\`。
    pub(crate) fn request_release(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<bool, RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.is_reclaiming() {
            return Err(RawInvariant::new("正在回收的资源 cell 不能重复入队"));
        }
        if cell.is_release_queued() {
            return Ok(false);
        }
        cell.state |= STATE_RELEASE_QUEUED;
        Ok(true)
    }

    /// 执行一次受限 cleanup；重复调用返回 \`false\` 且不重复记录。
    pub(crate) fn complete_release(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
        generation: SlabGeneration,
        detached: bool,
    ) -> Result<bool, RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.generation != generation.raw() {
            return Err(RawInvariant::new(
                "release 引用了过期 generation 的资源 cell",
            ));
        }
        if cell.is_release_done() {
            return Ok(false);
        }
        if !cell.is_release_queued() {
            return Err(RawInvariant::new("受限 cleanup 必须先取得 release 入队点"));
        }
        cell.state |= STATE_RELEASE_DONE;
        self.cleanups += 1;
        self.records.push(ReleaseTicket {
            descriptor,
            index,
            generation,
            detached,
        });
        Ok(true)
    }

    /// 取得唯一回收权：要求 \`leases == 0\` 且 cleanup 已完成。
    pub(crate) fn try_begin_reclaim(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<bool, RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if cell.is_reclaiming() || !cell.is_release_done() || cell.leases != 0 {
            return Ok(false);
        }
        cell.state |= STATE_RECLAIMING;
        Ok(true)
    }

    /// 放弃回收权；只用于已经原子取得回收权的路径。
    pub(crate) fn clear_reclaiming(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<(), RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if !cell.is_reclaiming() {
            return Err(RawInvariant::new("clear 回收权引用未取得回收权的 cell"));
        }
        cell.state &= !STATE_RECLAIMING;
        Ok(())
    }

    /// 推进 generation 并把 header 还原为空 cell，slot 可以复用。
    pub(crate) fn finish_reclaim(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<u64, RawInvariant> {
        let cell = self.get_mut(descriptor, index)?;
        if !cell.is_reclaiming() {
            return Err(RawInvariant::new("回收完成引用未取得回收权的 cell"));
        }
        cell.generation += 1;
        let generation = cell.generation;
        *cell = ResourceCell::empty();
        cell.generation = generation;
        Ok(generation)
    }

    /// 进程终止路径：把全部 lease 归零。
    pub(crate) fn force_release_all(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<(), RawInvariant> {
        self.get_mut(descriptor, index)?.leases = 0;
        Ok(())
    }

    /// 标记 detach：最后一个 lease 以 detach 语义结束。
    pub(crate) fn mark_detached(
        &mut self,
        descriptor: SlabDescriptorId,
        index: u32,
    ) -> Result<(), RawInvariant> {
        self.get_mut(descriptor, index)?.flags |= CELL_FLAG_DETACHED;
        Ok(())
    }

    /// 返回受限 cleanup 的次数。
    pub(crate) const fn cleanups(&self) -> u64 {
        self.cleanups
    }

    /// 返回按发生顺序记录的 release 记录。
    pub(crate) fn records(&self) -> &[ReleaseTicket] {
        &self.records
    }

    /// 校验 header 数量、状态位集合与 lease/release 不变量。
    pub(crate) fn verify(&self, table: &SlabTable) -> Result<(), RawInvariant> {
        for (index, descriptor) in table.descriptors().iter().enumerate() {
            if descriptor.domain != super::slab::MemoryDomainId::RESOURCE {
                continue;
            }
            let descriptor_id =
                SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32"));
            let row = self
                .cells
                .get(index)
                .ok_or_else(|| RawInvariant::new("ResourceCell header 行缺失"))?;
            if row.len() != descriptor.slot_count() as usize {
                return Err(RawInvariant::new(
                    "ResourceCell header 行长度与 slab slot 数不一致",
                ));
            }
            for (slot, cell) in row.iter().enumerate() {
                if cell.state & !STATE_KNOWN_MASK != 0 {
                    return Err(RawInvariant::new("ResourceCell 状态位含未登记 bit"));
                }
                if cell.reserved != 0 {
                    return Err(RawInvariant::new("ResourceCell reserved 字段必须为 0"));
                }
                let state = table.state(
                    descriptor_id,
                    u32::try_from(slot).expect("slot 下标适配 u32"),
                )?;
                match state {
                    SlotState::Live | SlotState::ReturnQueued | SlotState::Dead => {
                        if cell.leases == 0 && !cell.is_release_queued() {
                            return Err(RawInvariant::new(
                                "活跃资源 slot 必须持有 lease 或已经入队 release",
                            ));
                        }
                    }
                    SlotState::Returned => {
                        if cell.leases != 0 || cell.is_reclaiming() {
                            return Err(RawInvariant::new(
                                "已归还资源 slot 的 lease 或回收状态没有清空",
                            ));
                        }
                    }
                }
                if cell.is_reclaiming() && (cell.leases != 0 || !cell.is_release_done()) {
                    return Err(RawInvariant::new(
                        "回收权必须在 lease 归零且 cleanup 完成后取得",
                    ));
                }
                if cell.is_release_done() && !cell.is_release_queued() {
                    return Err(RawInvariant::new("cleanup 完成必须伴随 release 入队位"));
                }
            }
        }
        Ok(())
    }
}

/// 资源 slot 的引用；地址稳定，等同于 ResourceCell pointer。
pub(crate) type ResourceHandle = super::slab::RawSlot;

/// 判断 descriptor 是否是 Resource domain 的记录。
pub(crate) fn is_resource_descriptor(
    table: &SlabTable,
    descriptor: SlabDescriptorId,
) -> Result<bool, RawInvariant> {
    Ok(table
        .descriptor(descriptor)
        .ok_or_else(|| RawInvariant::new("引用未知 slab 描述符"))?
        .domain
        == super::slab::MemoryDomainId::RESOURCE)
}

/// 拒绝把 resource class descriptor 卷入整区 reset。
pub(crate) fn reject_region_reset(
    table: &SlabTable,
    descriptor: SlabDescriptorId,
) -> Result<(), RawInvariant> {
    if is_resource_descriptor(table, descriptor)? {
        return Err(RawInvariant::new(
            "资源值不能进入 managed region 的整区 reset",
        ));
    }
    Ok(())
}

/// 返回 Resource domain 使用的 class 编号是否属于专用整页 mapping。
pub(crate) const RESOURCE_DEDICATED_CLASS_ID: RuntimeSizeClassId =
    RuntimeSizeClassId::from_raw(u16::MAX);

/// 构造独立 non-moving 整页 mapping 的 class；slot 只承载一个 cell。
pub(crate) fn dedicated_class(
    stride: u32,
    alignment: u32,
) -> Result<RuntimeSizeClass, RawInvariant> {
    if stride <= CELL_HEADER_BYTES || stride % alignment != 0 {
        return Err(RawInvariant::new("专用整页 mapping 的 stride 或对齐非法"));
    }
    Ok(RuntimeSizeClass {
        id: RESOURCE_DEDICATED_CLASS_ID,
        payload_bytes: stride - CELL_HEADER_BYTES,
        slot_stride: stride,
        header_bytes: CELL_HEADER_BYTES,
        alignment,
        slots_per_span: 1,
        metadata_bytes: 0,
        link_usable: true,
        clear_mask: u32::MAX,
        poison: true,
        policy: DropScanPolicy::ResourceLease,
        domain: super::slab::MemoryDomainId::RESOURCE,
    })
}

/// 把 payload 与对齐向上取整到整页 mapping 的 stride。
pub(crate) fn dedicated_stride(payload_bytes: u32, alignment: u32) -> Result<u32, RawInvariant> {
    let align = alignment.max(RESOURCE_DEDICATED_ALIGN_LIMIT + 1).max(1);
    let page = align.max(u32::try_from(RAW_SLAB_PAGE_BYTES).expect("页大小适配 u32"));
    let header = CELL_HEADER_BYTES;
    let total = u64::from(header)
        .checked_add(u64::from(payload_bytes))
        .ok_or_else(|| RawInvariant::new("专用 mapping 的 payload 大小溢出"))?;
    let rounded = total.div_ceil(u64::from(page)) * u64::from(page);
    u32::try_from(rounded).map_err(|_| RawInvariant::new("专用 mapping 的 stride 超出编码宽度"))
}

/// 构造 Resource domain 的一段已提交 range 的统计。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceRangeCommit {
    pub(crate) range: RangeId,
    pub(crate) bytes: u64,
}

/// 预留并提交一段 Resource domain 的 mapping；调用者负责登记 descriptor。
pub(crate) fn reserve_mapping(
    provider: &mut dyn RangeProvider,
    accounting: &mut OwnerAccounting,
    bytes: u64,
    alignment: u32,
    epoch: Epoch,
) -> Result<ResourceRangeCommit, RawInvariant> {
    let _ = epoch;
    let range = provider.reserve_aligned(
        bytes,
        u64::from(alignment),
        super::slab::MemoryDomainId::RESOURCE,
    )?;
    provider.commit(range)?;
    accounting.commit(bytes);
    accounting.take_from_cache(bytes);
    Ok(ResourceRangeCommit { range, bytes })
}
