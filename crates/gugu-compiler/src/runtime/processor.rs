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
pub(crate) struct PollControl {
    /// bit0 PREEMPT，bit1 GC_STOP；其余位必须为 0。
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
pub(crate) struct ProcessorOwnership {
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
pub(crate) struct PaddedAtomicPtr {
    /// 可见 head。
    pub head: AtomicPtr<()>,
    padding: [u8; 120],
}

/// remote batch 的 128-byte padded head。
#[repr(C, align(128))]
pub(crate) struct RemoteBatchHead {
    /// 该 shard 的 batch 链头。
    pub head: AtomicPtr<()>,
    padding: [u8; 120],
}

/// 七档小栈 cache 的 owner-local heads 与累计字节。
#[repr(C)]
pub(crate) struct StackCacheHeads {
    /// 固定 7 个 class 的 cache head；不实现 cache 算法。
    heads: [UnsafeCell<*mut ()>; STACK_CACHE_CLASSES],
    /// 该 cache 当前累计字节。
    bytes: u64,
}

/// TLAB bump cursor/limit。
#[repr(C)]
pub(crate) struct TlabCursor {
    /// 下一个可分配字节。
    pub cursor: UnsafeCell<u64>,
    /// 当前 span 上限。
    pub limit: UnsafeCell<u64>,
}

/// TurnRegion bump cursor/limit。
#[repr(C)]
pub(crate) struct TurnRegionCursor {
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
pub(crate) struct LogicalProcessorPrefix {
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
    assert!(offset_of!(TurnRegionCursor, cursor) == 0);
    assert!(offset_of!(TurnRegionCursor, limit) == 8);
    assert!(offset_of!(PollControl, poll_flags) == 0);
};

/// 从 `LogicalProcessor*` 到 `poll_flags` 的字节偏移。
pub(crate) fn poll_flags_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, poll) + offset_of!(PollControl, poll_flags))
        .expect("poll_flags 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 `ownership` 的字节偏移。
pub(crate) fn ownership_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, ownership)).expect("ownership 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TLAB cursor 的字节偏移。
pub(crate) fn tlab_cursor_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, tlab) + offset_of!(TlabCursor, cursor))
        .expect("tlab cursor 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TLAB limit 的字节偏移。
pub(crate) fn tlab_limit_offset() -> u32 {
    u32::try_from(offset_of!(LogicalProcessorPrefix, tlab) + offset_of!(TlabCursor, limit))
        .expect("tlab limit 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TurnRegion cursor 的字节偏移。
pub(crate) fn turn_region_cursor_offset() -> u32 {
    u32::try_from(
        offset_of!(LogicalProcessorPrefix, turn_region) + offset_of!(TurnRegionCursor, cursor),
    )
    .expect("turn region cursor 偏移适配 u32")
}

/// 从 `LogicalProcessor*` 到 TurnRegion limit 的字节偏移。
pub(crate) fn turn_region_limit_offset() -> u32 {
    u32::try_from(
        offset_of!(LogicalProcessorPrefix, turn_region) + offset_of!(TurnRegionCursor, limit),
    )
    .expect("turn region limit 偏移适配 u32")
}
