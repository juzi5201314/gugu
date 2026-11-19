//! 协程唯一的 hot/cold 控制块、context 与分段地址稳定存储。

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use super::slab::RawInvariant;

pub const POLL_SENTINEL: usize = isize::MAX as usize;
pub const STACK_SCAN_LOCKED: u64 = 1 << 5;
pub const ENQUEUED: u64 = 1 << 4;
pub const FOREIGN_DETACHED: u64 = 1 << 6;
pub const BATCH_PUBLISHING: u64 = 1 << 7;
pub const FOREIGN_GENERATION_SHIFT: u32 = 8;
pub const COLD_COMPACTED: u8 = 1;
const CONTROL_PAGE_SLOTS: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CoroutineState {
    New,
    Runnable,
    Running,
    Parking,
    Waiting,
    Foreign,
    DirtyWaiting,
    Dead,
}

impl CoroutineState {
    pub fn from_word(word: u64) -> Result<Self, RawInvariant> {
        let state = match word & 15 {
            0 => Self::New,
            1 => Self::Runnable,
            2 => Self::Running,
            3 => Self::Parking,
            4 => Self::Waiting,
            5 => Self::Foreign,
            6 => Self::DirtyWaiting,
            7 => Self::Dead,
            _ => return Err(RawInvariant::new("协程 lifecycle 未登记")),
        };
        if word & ENQUEUED != 0 && state != Self::Runnable
            || word & FOREIGN_DETACHED != 0 && state != Self::Foreign
            || word & BATCH_PUBLISHING != 0 && word & ENQUEUED == 0
        {
            return Err(RawInvariant::new("协程状态位与 lifecycle 不相容"));
        }
        Ok(state)
    }
}

/// x86_64 换栈边界保存的六个机器字；不是平台 C ABI。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct CoroutineContext {
    /// 恢复时的栈指针。
    pub rsp: usize,
    /// 恢复时的指令地址。
    pub rip: usize,
    /// 保存的 rbx。
    pub rbx: usize,
    /// 保存的 rbp。
    pub rbp: usize,
    /// 保存的 r12。
    pub r12: usize,
    /// 保存的 r13。
    pub r13: usize,
}

/// A64 private switch ABI; x27/x28 are supplied by the restoring runtime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct A64Context {
    pub sp: u64,
    pub pc: u64,
    pub x19_to_x26: [u64; 8],
    pub x29: u64,
    pub d8_to_d15: [u64; 8],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct A64MorestackMailbox {
    pub hot: u64,
    pub entry_sp: u64,
    pub candidate: u64,
    pub required: u64,
    pub scratch: u64,
    pub mutator_context: u64,
    pub system_context: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct A64MorestackScratch {
    pub return_pc: u64,
    pub caller_lr: u64,
    pub vector: [[u8; 16]; 8],
    pub gpr: [u64; 9],
}

#[derive(Debug)]
#[repr(C, align(64))]
pub struct CoroutineHot {
    pub state: AtomicU64,
    pub run_link_next: UnsafeCell<*mut CoroutineHot>,
    pub current_processor: AtomicPtr<()>,
    pub preferred_processor: AtomicPtr<()>,
    pub preferred_processor_id: AtomicU64,
    pub wait_word: AtomicU64,
    pub cold_index: u64,
    pub run_batch_len: UnsafeCell<u8>,
    reserved: [u8; 7],
}

impl CoroutineHot {
    fn new(cold_index: u64) -> Self {
        Self {
            state: AtomicU64::new(CoroutineState::New as u64),
            run_link_next: UnsafeCell::new(std::ptr::null_mut()),
            current_processor: AtomicPtr::new(std::ptr::null_mut()),
            preferred_processor: AtomicPtr::new(std::ptr::null_mut()),
            preferred_processor_id: AtomicU64::new(0),
            wait_word: AtomicU64::new(0),
            cold_index,
            run_batch_len: UnsafeCell::new(0),
            reserved: [0; 7],
        }
    }

    pub fn lifecycle(&self) -> Result<CoroutineState, RawInvariant> {
        CoroutineState::from_word(self.state.load(Ordering::Acquire))
    }

    /// context/bounds 的写入先于该 Release；接手者必须 Acquire 认领完整状态字。
    pub fn transition(&self, from: CoroutineState, to: CoroutineState) -> Result<(), RawInvariant> {
        let legal = matches!(
            (from, to),
            (CoroutineState::New, CoroutineState::Runnable)
                | (CoroutineState::Runnable, CoroutineState::Running)
                | (CoroutineState::Running, CoroutineState::Runnable)
                | (CoroutineState::Running, CoroutineState::Parking)
                | (CoroutineState::Running, CoroutineState::Waiting)
                | (CoroutineState::Running, CoroutineState::Foreign)
                | (CoroutineState::Running, CoroutineState::DirtyWaiting)
                | (CoroutineState::Running, CoroutineState::Dead)
                | (CoroutineState::Parking, CoroutineState::Waiting)
                | (CoroutineState::Parking, CoroutineState::Running)
                | (CoroutineState::Waiting, CoroutineState::Runnable)
                | (CoroutineState::Foreign, CoroutineState::Running)
                | (CoroutineState::DirtyWaiting, CoroutineState::Foreign)
        );
        if !legal {
            return Err(RawInvariant::new("协程状态转换未登记"));
        }
        let old = self.state.load(Ordering::Acquire);
        if CoroutineState::from_word(old)? != from || old & STACK_SCAN_LOCKED != 0 {
            return Err(RawInvariant::new("协程已被执行者或 scanner 认领"));
        }
        let mut next = (old & !255) | to as u64;
        if to == CoroutineState::Runnable {
            next |= ENQUEUED;
        }
        if from == CoroutineState::Running
            && matches!(to, CoroutineState::Foreign | CoroutineState::DirtyWaiting)
        {
            let generation = (old >> FOREIGN_GENERATION_SHIFT)
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("协程 foreign generation 溢出"))?;
            next = (next & 255) | (generation << FOREIGN_GENERATION_SHIFT);
        }
        if from == CoroutineState::Foreign {
            next &= !FOREIGN_DETACHED;
        }
        self.state
            .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RawInvariant::new("协程状态转换丢失所有权"))?;
        Ok(())
    }
    /// 同一 generation 内把 attached `Foreign` 发布为 detached；lifecycle 值不变。
    pub fn retake_detached(&self) -> Result<(), RawInvariant> {
        let old = self.state.load(Ordering::Acquire);
        if CoroutineState::from_word(old)? != CoroutineState::Foreign
            || old & STACK_SCAN_LOCKED != 0
            || old & FOREIGN_DETACHED != 0
        {
            return Err(RawInvariant::new("协程已被执行者或 scanner 认领"));
        }
        let next = old | FOREIGN_DETACHED;
        self.state
            .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RawInvariant::new("协程状态转换丢失所有权"))?;
        Ok(())
    }
    /// 从非 runnable owner 认领单个节点：一次 CAS 置 `Runnable|ENQUEUED|BATCH_PUBLISHING`。
    pub fn claim_for_batch(&self) -> Result<(), RawInvariant> {
        let old = self.state.load(Ordering::Acquire);
        let from = CoroutineState::from_word(old)?;
        if !matches!(from, CoroutineState::Waiting | CoroutineState::Foreign)
            || old & STACK_SCAN_LOCKED != 0
            || old & ENQUEUED != 0
        {
            return Err(RawInvariant::new("协程已被执行者或 scanner 认领"));
        }
        let next = (old & !255) | CoroutineState::Runnable as u64 | ENQUEUED | BATCH_PUBLISHING;
        self.state
            .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RawInvariant::new("协程状态转换丢失所有权"))?;
        Ok(())
    }
    /// 把已排队的 `Runnable` 取为 `Running`，同时清除排队与 batch 认领位。
    pub fn take_running(&self) -> Result<(), RawInvariant> {
        let old = self.state.load(Ordering::Acquire);
        if CoroutineState::from_word(old)? != CoroutineState::Runnable
            || old & STACK_SCAN_LOCKED != 0
            || old & ENQUEUED == 0
        {
            return Err(RawInvariant::new("协程已被执行者或 scanner 认领"));
        }
        let next = (old & !255 & !ENQUEUED & !BATCH_PUBLISHING) | CoroutineState::Running as u64;
        self.state
            .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RawInvariant::new("协程状态转换丢失所有权"))?;
        Ok(())
    }
    /// 显式 yield：`Running` 回到 `Runnable|ENQUEUED`，不置 batch 认领位。
    pub fn yield_to_runnable(&self) -> Result<(), RawInvariant> {
        let old = self.state.load(Ordering::Acquire);
        if CoroutineState::from_word(old)? != CoroutineState::Running
            || old & STACK_SCAN_LOCKED != 0
        {
            return Err(RawInvariant::new("协程已被执行者或 scanner 认领"));
        }
        let next = (old & !255 & !BATCH_PUBLISHING) | CoroutineState::Runnable as u64 | ENQUEUED;
        self.state
            .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RawInvariant::new("协程状态转换丢失所有权"))?;
        Ok(())
    }
}

#[derive(Debug, Default)]
#[repr(C, align(64))]
pub struct StackDescriptor {
    pub stack_check: AtomicUsize,
    pub stack_low: usize,
    pub stack_high: usize,
    pub capacity: usize,
    pub recent_high_water: usize,
    pub last_grow_gc_epoch: u32,
    pub low_use_gc_cycles: u8,
    pub flags: u8,
    reserved: u16,
    padding: [u8; 16],
}

impl StackDescriptor {
    pub fn install(&mut self, low: usize, capacity: usize) -> Result<(), RawInvariant> {
        let high = low
            .checked_add(capacity)
            .filter(|high| *high < POLL_SENTINEL)
            .ok_or_else(|| RawInvariant::new("协程 stack range 不在低半 canonical 地址空间"))?;
        if capacity < 512 || !capacity.is_power_of_two() || !low.is_multiple_of(16) {
            return Err(RawInvariant::new("协程 stack class 或对齐非法"));
        }
        let poisoned = self.stack_check.load(Ordering::Acquire) == POLL_SENTINEL;
        self.stack_low = low;
        self.stack_high = high;
        self.capacity = capacity;
        self.stack_check.store(
            if poisoned { POLL_SENTINEL } else { low },
            Ordering::Release,
        );
        Ok(())
    }

    pub fn allows_frame(&self, rsp: usize, required: usize) -> bool {
        // 内部ABI刻意按机器字重解释符号位：下溢candidate必须变成负值，不能作饱和转换。
        (rsp.wrapping_sub(required) as isize) >= self.stack_check.load(Ordering::Acquire) as isize
    }

    pub fn poison(&self) {
        self.stack_check.store(POLL_SENTINEL, Ordering::Release);
    }

    pub fn clear_poll(&self) {
        self.stack_check.store(self.stack_low, Ordering::Release);
    }

    pub fn used(&self, rsp: usize) -> Result<usize, RawInvariant> {
        if rsp < self.stack_low || rsp > self.stack_high {
            return Err(RawInvariant::new("保存的 rsp 越过协程 stack bounds"));
        }
        Ok(self.stack_high - rsp)
    }
}

#[derive(Debug)]
#[repr(C, align(64))]
pub struct CoroutineSlot {
    pub hot: CoroutineHot,
    pub stack: StackDescriptor,
}

#[derive(Debug, Default)]
#[repr(C, align(16))]
pub struct MorestackScratch {
    pub return_pc: u64,
    pub gpr: [u64; 9],
    pub xmm: [[u8; 16]; 8],
}

/// 非移动 runtime record 的 generation-tagged 句柄；0 表示没有登记。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct RuntimeRecordRef {
    pub index: u64,
    pub generation: u64,
}

#[derive(Debug, Default)]
#[repr(C)]
pub struct ForeignBridgeState {
    mode: u64,
    call_stub: u64,
    frame_offset: u64,
    frame_size: u64,
    lease_word: u64,
    dirty_link: u64,
    bridge_credit: u64,
    error_state: u64,
}

impl ForeignBridgeState {
    pub const ORDINARY: u64 = 1;
    pub const DIRTY: u64 = 2;

    pub fn publish(
        &mut self,
        mode: u64,
        call_stub: u64,
        frame_offset: u64,
        frame_size: u64,
        lease_word: u64,
        credit: u64,
    ) -> Result<(), RawInvariant> {
        if self.mode != 0 {
            return Err(RawInvariant::new("ForeignBridge 记录已经发布"));
        }
        if mode != Self::ORDINARY && mode != Self::DIRTY {
            return Err(RawInvariant::new("ForeignBridge 模式未登记"));
        }
        self.mode = mode;
        self.call_stub = call_stub;
        self.frame_offset = frame_offset;
        self.frame_size = frame_size;
        self.lease_word = lease_word;
        self.bridge_credit = credit;
        self.error_state = 0;
        Ok(())
    }

    pub const fn mode(&self) -> u64 {
        self.mode
    }

    pub const fn error_state(&self) -> u64 {
        self.error_state
    }

    pub const fn credit(&self) -> u64 {
        self.bridge_credit
    }

    pub fn set_credit(&mut self, credit: u64) -> Result<(), RawInvariant> {
        if self.bridge_credit != 0 || credit == 0 {
            return Err(RawInvariant::new("BridgeCredit 不能重复发放"));
        }
        self.bridge_credit = credit;
        Ok(())
    }

    pub fn set_lease(&mut self, lease: u64) {
        self.lease_word = lease;
    }

    pub fn capture_error(&mut self, errno: u64) {
        self.error_state = errno;
    }

    /// 额度最多归还一次。
    pub fn take_credit(&mut self) -> Result<u64, RawInvariant> {
        if self.bridge_credit == 0 {
            return Err(RawInvariant::new("BridgeCredit 不能归还两次"));
        }
        let credit = self.bridge_credit;
        self.bridge_credit = 0;
        Ok(credit)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionValue {
    Bits(u64),
    Managed { handle: u64, descriptor: u64 },
    Panic { handle: u64, descriptor: u64 },
}

#[derive(Debug, Default)]
#[repr(C)]
pub struct CompletionRecord {
    /// 0=未完成，1=位值，2=managed，3=panic；bit 2 表示 panic 已处理。
    pub status: AtomicU64,
    pub payload: u64,
    pub descriptor: u64,
    pub join_leases: u64,
}

impl CompletionRecord {
    /// 旧栈完成语义transfer后，先登记GC barrier，再Release公开完整记录。
    pub fn publish(
        &mut self,
        value: CompletionValue,
        barrier: impl FnOnce(u64, u64),
    ) -> Result<(), RawInvariant> {
        if self.status.load(Ordering::Acquire) != 0 {
            return Err(RawInvariant::new("协程完成记录只能发布一次"));
        }
        let (status, payload, descriptor) = match value {
            CompletionValue::Bits(bits) => (1, bits, 0),
            CompletionValue::Managed { handle, descriptor } => (2, handle, descriptor),
            CompletionValue::Panic { handle, descriptor } => (3, handle, descriptor),
        };
        if status != 1 && (payload == 0 || descriptor == 0) {
            return Err(RawInvariant::new(
                "完成记录缺少有效 handle 或类型 descriptor",
            ));
        }
        self.payload = payload;
        self.descriptor = descriptor;
        if status != 1 {
            barrier(payload, descriptor);
        }
        self.status.store(status, Ordering::Release);
        Ok(())
    }

    pub fn read(&self) -> Result<CompletionValue, RawInvariant> {
        match self.status.load(Ordering::Acquire) & 3 {
            1 => Ok(CompletionValue::Bits(self.payload)),
            2 => Ok(CompletionValue::Managed {
                handle: self.payload,
                descriptor: self.descriptor,
            }),
            3 => {
                self.status.fetch_or(4, Ordering::AcqRel);
                Ok(CompletionValue::Panic {
                    handle: self.payload,
                    descriptor: self.descriptor,
                })
            }
            _ => Err(RawInvariant::new("协程尚未发布完成记录")),
        }
    }
}

#[derive(Debug, Default)]
#[repr(C, align(64))]
pub struct CoroutineCold {
    pub id: u64,
    pub context: CoroutineContext,
    pub morestack_scratch: MorestackScratch,
    pub wait_record: [u64; 4],
    pub foreign_bridge: ForeignBridgeState,
    pub join_state: CompletionRecord,
    pub coroutine_locals: RuntimeRecordRef,
    pub panic_state: RuntimeRecordRef,
    pub select_rng: [u64; 4],
    pub select_scratch: [u64; 4],
    pub gc_scan_epoch: AtomicU64,
    /// 载荷发布状态字：位 0 = spawn 参数包已发布，位 1 = 结果字含根车道，位 2 = 结果载荷已发布。
    /// spawn 在入队前发布、结束时就地结清；结果在结束时发布、被 Join/select 取走时结清。
    /// 未发布过载荷的协程（bootstrap/main）三位置零，不允许也无从结清。
    pub payload_state: AtomicU64,
    /// 等待本协程完成的协程地址；不复用本协程的原生请求区。
    pub join_waiter: u64,
    /// 外部线程持有的回调节点；零表示普通协程，完成清理后才发布节点完成。
    pub callback_node: u64,
    /// 回调热记录、冷记录、参数包及 registry 的同页映射；普通协程为零。
    pub callback_storage: u64,
    /// 测试 harness 子树归属：spawn 时从当前协程继承的组记录地址；零表示不属于任何测试组。
    /// 组记录两车道 `{pending, panics}`，结束与未处理 panic 的记账由镜像 runtime 完成。
    pub subtree_group: u64,
    /// 完成结果为未交付 panic 时置位；Join/select 晚到取走时清零并把组 panic 数减回。
    pub panic_result: u64,
}

/// morestack 入口片段与镜像 runtime 交接的固定邮箱；位于 runtime 状态区首部。
///
/// 片段写入 `hot`/`entry_rsp`/`candidate`/`required` 后切到 `system_context`；系统侧的镜像
/// runtime 用 `required` 决策新栈，修正 `cold` 与 `system_context` 后再恢复协程。布局由
/// `CoroutineRuntimeContract.records` 交给 `coroutine_layout` 与 Gugu 侧结构逐字段交叉校验。
#[repr(C, align(8))]
pub struct MorestackMailbox {
    /// 片段入口的 `r14`（`CoroutineSlot*`，与 `CoroutineHot*` 同址）。
    pub hot: u64,
    /// 片段入口 `rsp`。
    pub entry_rsp: u64,
    /// 片段入口候选低水位 `r11`。
    pub candidate: u64,
    /// `entry_rsp + 8 - candidate`，由片段算出，是栈决策的实参。
    pub required: u64,
    /// 当前协程 `CoroutineCold*`；runtime 绑定协程时发布，片段只读。
    pub cold: u64,
    /// 当前 worker 的系统上下文；rt0 切入协程前保存，片段以它为恢复目标。
    pub system_context: CoroutineContext,
    /// main 协程返回时由返回哨兵记录的退出值。
    pub return_value: u64,
    /// 当前 worker 的 `LogicalProcessor*`；返回哨兵恢复上下文时用它重建 `r15`。
    pub processor: u64,
}

const _: () = {
    assert!(size_of::<CoroutineHot>() == 64 && align_of::<CoroutineHot>() == 64);
    assert!(size_of::<StackDescriptor>() == 64 && align_of::<StackDescriptor>() == 64);
    assert!(size_of::<CoroutineSlot>() == 128 && align_of::<CoroutineSlot>() == 64);
    assert!(std::mem::offset_of!(CoroutineSlot, stack) == 64);
    assert!(size_of::<CoroutineContext>() == 48);
    assert!(size_of::<MorestackScratch>() == 208);
    assert!(size_of::<CoroutineCold>() == 576 && align_of::<CoroutineCold>() == 64);
    assert!(std::mem::offset_of!(CoroutineCold, payload_state) == 504);
    assert!(size_of::<MorestackMailbox>() == 104 && align_of::<MorestackMailbox>() == 8);
    assert!(std::mem::offset_of!(MorestackMailbox, system_context) == 40);
    assert!(std::mem::offset_of!(MorestackMailbox, return_value) == 88);
    assert!(std::mem::offset_of!(MorestackMailbox, processor) == 96);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoroutineHandle {
    pub index: u32,
    pub generation: u64,
}

#[derive(Debug)]
struct ControlIdentity {
    generation: u64,
    occupied: bool,
    next_free: Option<u32>,
}

/// 下标稠密、页内512个slot；只扩页表，不移动已公开的hot/cold地址。
#[derive(Debug, Default)]
pub struct CoroutineTable {
    hot_pages: Vec<Box<[CoroutineSlot]>>,
    cold_pages: Vec<Box<[CoroutineCold]>>,
    identities: Vec<ControlIdentity>,
    free: Option<u32>,
    next_id: u64,
}

impl CoroutineTable {
    pub fn allocate(&mut self) -> Result<CoroutineHandle, RawInvariant> {
        let id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("CoroutineId 溢出"))?;
        let index = if let Some(index) = self.free {
            self.free = self.identities[usize::try_from(index).expect("控制块下标")].next_free;
            index
        } else {
            let index = u32::try_from(self.identities.len())
                .map_err(|_| RawInvariant::new("协程控制表溢出"))?;
            let at = usize::try_from(index).expect("控制块下标");
            if at.is_multiple_of(CONTROL_PAGE_SLOTS) {
                self.hot_pages.push(
                    (0..CONTROL_PAGE_SLOTS)
                        .map(|offset| CoroutineSlot {
                            hot: CoroutineHot::new(
                                u64::from(index) + u64::try_from(offset).expect("页内偏移"),
                            ),
                            stack: StackDescriptor::default(),
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                );
            }
            let cold_slots = 65536 / size_of::<CoroutineCold>();
            if at.is_multiple_of(cold_slots) {
                self.cold_pages.push(
                    (0..cold_slots)
                        .map(|_| CoroutineCold::default())
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                );
            }
            self.identities.push(ControlIdentity {
                generation: 1,
                occupied: false,
                next_free: None,
            });
            index
        };
        self.next_id = id;
        let identity = &mut self.identities[usize::try_from(index).expect("控制块下标")];
        identity.occupied = true;
        identity.next_free = None;
        let handle = CoroutineHandle {
            index,
            generation: identity.generation,
        };
        let (slot, cold) = self.get_mut(handle)?;
        slot.hot = CoroutineHot::new(u64::from(index));
        slot.stack = StackDescriptor::default();
        *cold = CoroutineCold {
            id,
            ..CoroutineCold::default()
        };
        cold.join_state.join_leases = 1;
        Ok(handle)
    }

    fn locate(&self, handle: CoroutineHandle) -> Result<usize, RawInvariant> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        let identity = self
            .identities
            .get(index)
            .filter(|identity| identity.occupied && identity.generation == handle.generation)
            .ok_or_else(|| RawInvariant::new("过期的协程控制块 handle"))?;
        debug_assert!(identity.generation != 0);
        Ok(index)
    }

    pub fn get(
        &self,
        handle: CoroutineHandle,
    ) -> Result<(&CoroutineSlot, &CoroutineCold), RawInvariant> {
        let index = self.locate(handle)?;
        let cold_slots = 65536 / size_of::<CoroutineCold>();
        Ok((
            &self.hot_pages[index / CONTROL_PAGE_SLOTS][index % CONTROL_PAGE_SLOTS],
            &self.cold_pages[index / cold_slots][index % cold_slots],
        ))
    }

    pub fn get_mut(
        &mut self,
        handle: CoroutineHandle,
    ) -> Result<(&mut CoroutineSlot, &mut CoroutineCold), RawInvariant> {
        let index = self.locate(handle)?;
        let cold_slots = 65536 / size_of::<CoroutineCold>();
        Ok((
            &mut self.hot_pages[index / CONTROL_PAGE_SLOTS][index % CONTROL_PAGE_SLOTS],
            &mut self.cold_pages[index / cold_slots][index % cold_slots],
        ))
    }

    /// 页保持映射时才复用slot；整页释放由owner queue-page grace持有外层所有权。
    pub fn release(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let (slot, cold) = self.get(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Dead
            || slot.stack.capacity != 0
            || cold.join_state.join_leases != 0
            || slot.hot.state.load(Ordering::Acquire) & STACK_SCAN_LOCKED != 0
        {
            return Err(RawInvariant::new(
                "协程控制块仍有stack、Join或scanner所有权",
            ));
        }
        let identity = &mut self.identities[usize::try_from(handle.index).expect("控制块下标")];
        identity.generation = identity
            .generation
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("协程控制块 generation 溢出"))?;
        identity.occupied = false;
        identity.next_free = self.free;
        self.free = Some(handle.index);
        Ok(())
    }
}
