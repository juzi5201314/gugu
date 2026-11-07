//! LogicalProcessor 热前缀布局神谕；backend 只消费契约里的 offset_of 结果。
//!
//! 本模块固定 poll / ownership / deque head-tail / remote batch / stack cache /
//! TLAB / TurnRegion 的 repr(C) 前缀。deque 算法仍在 `scheduler.rs`；写屏障
//! `CardMarkBuffer` 不进入此前缀，热路走 runtime call。stack cache 占位防止后续
//! 字段插入把 TLAB 偏移挤走。

use std::cell::UnsafeCell;
use std::mem::{align_of, offset_of, size_of};
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64};

use super::scheduler_schema::{SCHED_LOCAL_CAPACITY, SCHED_REMOTE_SHARDS};
use super::stack::STACK_CACHE_CLASSES;

/// 独占一条 cache line 的 poll 控制块。
#[repr(C, align(64))]
pub struct PollControl {
    /// bit0 PREEMPT，bit1 GC_STOP，bit2 回调/owner drain 服务通知。
    pub poll_flags: AtomicU32,
    reserved: u32,
    /// collector 发布的 requested epoch。
    pub requested_gc_epoch: AtomicU64,
    /// 只有 processor owner 写 ack。
    pub ack_gc_epoch: AtomicU64,
    padding: [u8; 40],
}

/// 独占一条 cache line 的 processor 拥有态。
#[repr(C, align(64))]
pub struct ProcessorOwnership {
    /// Idle / Bound / Retiring。
    pub state: AtomicU32,
    reserved: u32,
    /// 当前绑定的 WorkerThread。
    pub owner: AtomicPtr<()>,
    /// 当前 Running / attached Foreign 的 CoroutineHot。
    pub current_coroutine: AtomicPtr<()>,
    /// 只在 current lifecycle 为 Running 时解释。
    pub run_started_ns: AtomicU64,
    padding: [u8; 32],
}

/// 128-byte padded atomic pointer；local head/tail 与 remote batch 共用此形状。
#[repr(C, align(128))]
pub struct PaddedAtomicPtr {
    /// 可见 head。
    pub head: AtomicPtr<()>,
    padding: [u8; 120],
}

/// remote batch 的 128-byte padded head。
#[repr(C, align(128))]
pub struct RemoteBatchHead {
    /// 该 shard 的 batch 链头。
    pub head: AtomicPtr<()>,
    padding: [u8; 120],
}

/// 七档小栈 cache 的 owner-local heads 与累计字节。
#[repr(C)]
pub struct StackCacheHeads {
    /// 固定 7 个 class 的 cache head；不实现 cache 算法。
    heads: [UnsafeCell<*mut ()>; STACK_CACHE_CLASSES],
    /// 该 cache 当前累计字节。
    bytes: u64,
}

/// TLAB bump cursor/limit。
#[repr(C)]
pub struct TlabCursor {
    /// 下一个可分配字节。
    pub cursor: UnsafeCell<u64>,
    /// 当前 span 上限。
    pub limit: UnsafeCell<u64>,
}

/// TurnRegion bump cursor/limit。
#[repr(C)]
pub struct TurnRegionCursor {
    /// 下一个可分配字节。
    pub cursor: UnsafeCell<u64>,
    /// 当前 region 上限。
    pub limit: UnsafeCell<u64>,
}

/// LogicalProcessor 热前缀：poll 到 TurnRegion 的固定顺序。
///
/// 字段顺序与 `docs/src/internals/scheduler.md` 字面一致。slots 容量等于
/// `SCHED_LOCAL_CAPACITY`（256），remote 分片等于 `SCHED_REMOTE_SHARDS`（8）。
#[repr(C)]
pub struct LogicalProcessorPrefix {
    /// offset 0。
    pub poll: PollControl,
    /// offset 64。
    pub ownership: ProcessorOwnership,
    /// offset 128；thief-visible local head。
    pub local_head: PaddedAtomicPtr,
    /// offset 256；owner local tail。
    pub local_tail: PaddedAtomicPtr,
    /// offset 384；容量 256 的 deque slot 数组。
    pub slots: [UnsafeCell<*mut ()>; SCHED_LOCAL_CAPACITY as usize],
    /// offset 2432；8 个 remote batch head。
    pub remote: [RemoteBatchHead; SCHED_REMOTE_SHARDS as usize],
    /// offset 3456；占位以免后续字段挤走 TLAB。
    pub stack_cache: StackCacheHeads,
    /// offset 3520。
    pub tlab: TlabCursor,
    /// offset 3536。
    pub turn_region: TurnRegionCursor,
}

/// processor 页中 edge staging scratch 指针的固定偏移；位于 TurnRegion 后的保留区。
pub const PROCESSOR_EDGE_SCRATCH_OFFSET: usize = 3560;
/// processor 页字节数；页尾槽位全部落在这一页内。
pub const PROCESSOR_PAGE_BYTES: usize = 4096;
/// 抢占时优先恢复的协程槽（`run_next`）。
pub const PROCESSOR_RUN_NEXT_OFFSET: usize = 3552;
/// 队列溢出链头。
pub const PROCESSOR_OVERFLOW_CARRY_OFFSET: usize = 3568;
/// 屏障状态页指针：card buffer、stamp 表与额度账本都在独立 raw 页上。
pub const PROCESSOR_BARRIER_SCRATCH_OFFSET: usize = 3576;
/// 当前 coroutine 栈被标记周期扫描时的 epoch；与 arena 的 mark_epoch 不等即仍为 grey。
pub const PROCESSOR_STACK_EPOCH_OFFSET: usize = 3584;
/// 本切片单 arena 的基址槽；arena registry 接管后换成稳定 descriptor 查询。
pub const PROCESSOR_ARENA_OFFSET: usize = 3592;
/// 标记周期是否开启；epoch 单调推进，本标志单独表达「当前有活跃标记周期」。
pub const PROCESSOR_MARK_ACTIVE_OFFSET: usize = 3600;
/// trace 游标页指针：descriptor 解释的可暂停状态都在独立 raw 页上。
pub const PROCESSOR_TRACE_CURSOR_OFFSET: usize = 3608;
/// GC 请求记录：分配侧向收集器发起请求的非移动控制记录（首字是 kind）。
pub const PROCESSOR_GC_REQUEST_OFFSET: usize = 3616;
/// 30 个 owner-local 统计字；尾部紧邻协程栈账本。
pub const PROCESSOR_COUNTERS_OFFSET: usize = 3624;
pub const PROCESSOR_COUNTERS_COUNT: usize = 30;
pub const PROCESSOR_REGION_PAYLOAD_BYTES_OFFSET: usize = PROCESSOR_COUNTERS_OFFSET + 25 * 8;
/// 活跃协程根扫描登记链。
pub const PROCESSOR_COROUTINE_REGISTRY_OFFSET: usize = 4000;
/// 已退出协程的栈回收链；不包含 Join 完成载荷。
pub const PROCESSOR_RETIRED_STACKS_OFFSET: usize = 4008;
/// 存活协程数。
pub const PROCESSOR_LIVE_COUNT_OFFSET: usize = 4032;
/// 子协程入口片段地址（`prepare_child` 写入子上下文 rip）。
pub const PROCESSOR_CHILD_ENTER_OFFSET: usize = 4072;
/// runtime 状态区指针；`rt0_start` 写入、镜像 runtime 全程读取。
pub const PROCESSOR_STATE_OFFSET: usize = 4080;
/// 系统调度栈使用独立、进程期存活的槽；不能借用可回收协程控制页。
pub const PROCESSOR_SYSTEM_SLOT_OFFSET: usize = 4088;
/// 堆游标/上界：bump 分配器只在这里记账，不与 arena 记账混用。
pub const PROCESSOR_HEAP_CURSOR_OFFSET: usize = 4048;
pub const PROCESSOR_HEAP_LIMIT_OFFSET: usize = 4056;
/// typed 动态根登记表地址：`boot` 建立，收集器与探针从这里进入，不扫整片 raw 区。
pub const PROCESSOR_DYNAMIC_ROOTS_OFFSET: usize = 4064;
const _: () = {
    assert!(size_of::<PollControl>() == 64);
    assert!(align_of::<PollControl>() == 64);
    assert!(size_of::<ProcessorOwnership>() == 64);
    assert!(align_of::<ProcessorOwnership>() == 64);
    assert!(offset_of!(LogicalProcessorPrefix, poll) == 0);
    assert!(offset_of!(LogicalProcessorPrefix, ownership) == 64);
    assert!(
        offset_of!(LogicalProcessorPrefix, ownership) - offset_of!(LogicalProcessorPrefix, poll)
            >= 64
    );
    assert!(size_of::<PaddedAtomicPtr>() == 128);
    assert!(align_of::<PaddedAtomicPtr>() == 128);
    assert!(size_of::<RemoteBatchHead>() == 128);
    assert!(align_of::<RemoteBatchHead>() == 128);
    assert!(offset_of!(LogicalProcessorPrefix, local_head) == 128);
    assert!(offset_of!(LogicalProcessorPrefix, local_tail) == 256);
    assert!(offset_of!(LogicalProcessorPrefix, slots) == 384);
    assert!(offset_of!(LogicalProcessorPrefix, remote) == 2432);
    assert!(offset_of!(LogicalProcessorPrefix, stack_cache) == 3456);
    assert!(offset_of!(LogicalProcessorPrefix, tlab) == 3520);
    assert!(offset_of!(TlabCursor, cursor) == 0);
    assert!(offset_of!(TlabCursor, limit) == 8);
    assert!(offset_of!(LogicalProcessorPrefix, turn_region) == 3536);
    assert!(
        offset_of!(LogicalProcessorPrefix, turn_region) + size_of::<TurnRegionCursor>()
            <= PROCESSOR_EDGE_SCRATCH_OFFSET
    );
    assert!(offset_of!(TurnRegionCursor, cursor) == 0);
    assert!(offset_of!(TurnRegionCursor, limit) == 8);
    assert!(offset_of!(PollControl, poll_flags) == 0);
    // 前缀之后的固定槽位：run_next → edge scratch 指针 → overflow carry → 屏障状态 → 栈 epoch
    // → arena 基址 → 堆记账 → 子入口 → 状态区。
    assert!(PROCESSOR_RUN_NEXT_OFFSET == offset_of!(LogicalProcessorPrefix, turn_region) + 16);
    assert!(PROCESSOR_EDGE_SCRATCH_OFFSET == PROCESSOR_RUN_NEXT_OFFSET + 8);
    assert!(PROCESSOR_OVERFLOW_CARRY_OFFSET == PROCESSOR_EDGE_SCRATCH_OFFSET + 8);
    assert!(PROCESSOR_BARRIER_SCRATCH_OFFSET == PROCESSOR_OVERFLOW_CARRY_OFFSET + 8);
    assert!(PROCESSOR_STACK_EPOCH_OFFSET == PROCESSOR_BARRIER_SCRATCH_OFFSET + 8);
    assert!(PROCESSOR_ARENA_OFFSET == PROCESSOR_STACK_EPOCH_OFFSET + 8);
    assert!(PROCESSOR_MARK_ACTIVE_OFFSET == PROCESSOR_ARENA_OFFSET + 8);
    assert!(PROCESSOR_TRACE_CURSOR_OFFSET == PROCESSOR_MARK_ACTIVE_OFFSET + 8);
    assert!(PROCESSOR_GC_REQUEST_OFFSET == PROCESSOR_TRACE_CURSOR_OFFSET + 8);
    assert!(PROCESSOR_RETIRED_STACKS_OFFSET == PROCESSOR_COROUTINE_REGISTRY_OFFSET + 8);
    assert!(PROCESSOR_RETIRED_STACKS_OFFSET + 8 == 4016);
    assert!(PROCESSOR_LIVE_COUNT_OFFSET > PROCESSOR_GC_REQUEST_OFFSET);
    assert!(PROCESSOR_LIVE_COUNT_OFFSET > PROCESSOR_OVERFLOW_CARRY_OFFSET);
    assert!(PROCESSOR_HEAP_CURSOR_OFFSET == PROCESSOR_LIVE_COUNT_OFFSET + 16);
    assert!(PROCESSOR_HEAP_LIMIT_OFFSET == PROCESSOR_HEAP_CURSOR_OFFSET + 8);
    assert!(PROCESSOR_DYNAMIC_ROOTS_OFFSET == PROCESSOR_HEAP_LIMIT_OFFSET + 8);
    assert!(PROCESSOR_CHILD_ENTER_OFFSET == PROCESSOR_DYNAMIC_ROOTS_OFFSET + 8);
    assert!(PROCESSOR_STATE_OFFSET == PROCESSOR_CHILD_ENTER_OFFSET + 8);
    assert!(PROCESSOR_STATE_OFFSET + 8 <= PROCESSOR_PAGE_BYTES);
};

/// 从 `LogicalProcessor*` 到 `poll_flags` 的字节偏移。
pub fn poll_flags_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, poll) + offset_of!(PollControl, poll_flags))
        .expect("poll_flags 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 `ownership` 的字节偏移。
pub fn ownership_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, ownership)).expect("ownership 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TLAB cursor 的字节偏移。
pub fn tlab_cursor_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, tlab) + offset_of!(TlabCursor, cursor))
        .expect("tlab cursor 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TLAB limit 的字节偏移。
pub fn tlab_limit_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, tlab) + offset_of!(TlabCursor, limit))
        .expect("tlab limit 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TurnRegion cursor 的字节偏移。
pub fn turn_region_cursor_offset() -> u32 {
    u32::try_from(
        offset_of!(LogicalProcessorPrefix, turn_region) + offset_of!(TurnRegionCursor, cursor),
    )
    .expect("turn region cursor 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TurnRegion limit 的字节偏移。
pub fn turn_region_limit_offset() -> u32 {
    u32::try_from(
        offset_of!(LogicalProcessorPrefix, turn_region) + offset_of!(TurnRegionCursor, limit),
    )
    .expect("turn region limit 偏移适配 u32")
}
