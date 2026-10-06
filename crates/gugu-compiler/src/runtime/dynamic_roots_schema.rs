//! 镜像内 typed 动态根登记表的机器契约。
//!
//! `std/runtime/scheduler.gg` 在启动时按 `[表头][固定容量记录]` 建表，处理器只保存表地址
//! （`PROCESSOR_DYNAMIC_ROOTS_OFFSET`）。发布与结清都以稳定身份（热记录地址）为单位，条目
//! 只按身份增删，收集器与探针从表进入，不扫描 scheduler 的整片 raw 区。
//!
//! 记录里的 `descriptor` 是**按类别解释的载荷描述**：声明了 trace descriptor 的类别写描述符
//! 地址；spawn 参数包写 lane map（每个车道 3 位，`0` = 非根，`n + 1` = 根种类判别值 `n`，
//! 车道顺序为整数车道 0..8 再接浮点车道）。`count` 是载荷内的车道/元素数。`flags` 位 0 表示
//! 载荷已完整初始化（与栈图 `initialized` 同义）。

use std::mem::{align_of, offset_of, size_of};

/// 表头字节数：容量 + 已发布条数。
pub(crate) const DYNAMIC_ROOT_TABLE_HEADER_BYTES: usize = 16;
/// 表头字段偏移。
pub(crate) const DYNAMIC_ROOT_TABLE_CAPACITY_OFFSET: usize = 0;
pub(crate) const DYNAMIC_ROOT_TABLE_COUNT_OFFSET: usize = 8;
/// 单条记录字节数。
pub(crate) const DYNAMIC_ROOT_RECORD_BYTES: usize = 48;
/// 固定容量（条目数）。发布时表满是不变量错误，绝不静默丢弃已发布载荷。
pub(crate) const DYNAMIC_ROOT_CAPACITY: usize = 256;

/// 记录内字段偏移。
pub(crate) const DYNAMIC_ROOT_IDENTITY_OFFSET: usize = 0;
pub(crate) const DYNAMIC_ROOT_SLOT_OFFSET: usize = 8;
pub(crate) const DYNAMIC_ROOT_DESCRIPTOR_OFFSET: usize = 16;
pub(crate) const DYNAMIC_ROOT_COUNT_OFFSET: usize = 24;
pub(crate) const DYNAMIC_ROOT_KIND_OFFSET: usize = 32;
pub(crate) const DYNAMIC_ROOT_FLAGS_OFFSET: usize = 40;

/// `flags` 位 0：载荷已完整初始化，可以进入根集合。
pub(crate) const DYNAMIC_ROOT_FLAG_INITIALIZED: u64 = 1;

/// 记录种类判别值。
///
/// 每种载荷的 `descriptor` 按该种类的固定形态解释：`SPAWN_PAYLOAD` 与 `RESULT` 写 lane map。
/// 后续类别（协程结果之外的 channel 槽、select 获胜项、Join 结果、active TurnRegion、消息、
/// pin 记录）在各自生产者落地时按同一枚举追加判别值，在此之前不预登记空种类。
pub(crate) const DYNAMIC_ROOT_KIND_SPAWN_PAYLOAD: u64 = 0;
/// 已结束协程的结果字：载荷槽是冷记录里的结果槽，`count` 为 1，描述为 lane map。
pub(crate) const DYNAMIC_ROOT_KIND_RESULT: u64 = 1;
/// channel 环槽：载荷槽是环内的那条槽，描述是**元素类型的受管车道表地址**（表首字为条数，
/// 其后每条是 `(偏移, lane map 编码)`），因为环里存的是任意类型的内联字节，车道偏移不固定。
pub(crate) const DYNAMIC_ROOT_KIND_CHANNEL_SLOT: u64 = 2;

/// 一条动态根记录的机器布局。
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DynamicRootRecordV1 {
    /// 稳定身份：发布者的热记录地址（live coroutine），raw 控制地址本身不是根。
    pub identity: u64,
    /// 真实载荷槽地址；载荷由发布者自持、不可移动。
    pub slot: u64,
    /// 按类别解释的载荷描述：trace descriptor 地址，或参数包的 lane map。
    pub descriptor: u64,
    /// 载荷内元素/车道数，至少 1。
    pub count: u64,
    /// 记录种类判别值。
    pub kind: u64,
    /// 状态位；位 0 = 已完整初始化。
    pub flags: u64,
}

const _: () = {
    assert!(size_of::<DynamicRootRecordV1>() == DYNAMIC_ROOT_RECORD_BYTES);
    assert!(align_of::<DynamicRootRecordV1>() == 8);
    assert!(offset_of!(DynamicRootRecordV1, identity) == DYNAMIC_ROOT_IDENTITY_OFFSET);
    assert!(offset_of!(DynamicRootRecordV1, slot) == DYNAMIC_ROOT_SLOT_OFFSET);
    assert!(offset_of!(DynamicRootRecordV1, descriptor) == DYNAMIC_ROOT_DESCRIPTOR_OFFSET);
    assert!(offset_of!(DynamicRootRecordV1, count) == DYNAMIC_ROOT_COUNT_OFFSET);
    assert!(offset_of!(DynamicRootRecordV1, kind) == DYNAMIC_ROOT_KIND_OFFSET);
    assert!(offset_of!(DynamicRootRecordV1, flags) == DYNAMIC_ROOT_FLAGS_OFFSET);
};

/// 第 `index` 条记录相对表首的字节偏移。
pub(crate) const fn dynamic_root_record_offset(index: usize) -> usize {
    DYNAMIC_ROOT_TABLE_HEADER_BYTES + index * DYNAMIC_ROOT_RECORD_BYTES
}

/// 整表字节数（表头 + 全部记录），启动时一次性从 raw 区分配。
pub(crate) const DYNAMIC_ROOT_TABLE_BYTES: usize =
    dynamic_root_record_offset(DYNAMIC_ROOT_CAPACITY);
