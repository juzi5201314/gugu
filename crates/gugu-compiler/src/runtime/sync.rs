//! std.sync 运行时参照实现：原子、锁、OnceLock、Lazy 与取消。
//!
//! 提供确定性的进程内模型：
//! 1. Atomic 状态机：支持 Relaxed、Acquire、Release、AcqRel、SeqCst；CAS 失败序校验；合法标量类型校验；
//! 2. Mutex / RwLock / Condvar：竞争只挂起协程；non-poisoning；由租约自动解锁；显式 unlock 幂等；
//! 3. OnceLock / Lazy：Uninit -> Initializing -> Ready / Failed；初始化异常转入永久 Failed，不重置；
//! 4. CancelSource / CancelToken：幂等、协作；阻塞操作接缝与安全注销，不隐式杀死子协程或子进程。

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::model::RawModelError;
use super::sync_schema::{
    CANCEL_ACTIVE, CANCEL_CANCELLED, MUTEX_CONTENDED, MUTEX_LOCKED, MUTEX_UNLOCKED, ONCE_FAILED,
    ONCE_INITIALIZING, ONCE_READY, ONCE_UNINIT, ORDERING_ACQ_REL, ORDERING_ACQUIRE,
    ORDERING_RELAXED, ORDERING_RELEASE, ORDERING_SEQ_CST,
};
pub use super::wait::WaitNodeHandle;

/// 内存序枚举。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryOrdering {
    Relaxed = 0,
    Acquire = 1,
    Release = 2,
    AcqRel = 3,
    SeqCst = 4,
}

impl MemoryOrdering {
    pub fn from_u32(val: u32) -> Result<Self, RawModelError> {
        match val {
            ORDERING_RELAXED => Ok(Self::Relaxed),
            ORDERING_ACQUIRE => Ok(Self::Acquire),
            ORDERING_RELEASE => Ok(Self::Release),
            ORDERING_ACQ_REL => Ok(Self::AcqRel),
            ORDERING_SEQ_CST => Ok(Self::SeqCst),
            _ => Err(RawModelError::new("未知内存序编码")),
        }
    }
}

/// 检查类型是否为合法的 Atomic 支持类型。
pub fn is_legal_atomic_type(ty_name: &str) -> bool {
    matches!(
        ty_name,
        "bool"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "int"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uint"
            | "ptr"
            | "*raw"
            | "*byte"
    )
}

/// Atomic 状态机。
#[derive(Clone, Debug, Default)]
pub struct AtomicStateMachine {
    /// 变量值。
    pub value: u64,
    /// 最新 Release 写入时的时钟 epoch。
    pub release_epoch: u64,
    /// 全局 SeqCst 序号。
    pub seq_cst_seq: u64,
    /// 全局逻辑时钟。
    pub global_clock: u64,
    /// 各协程已观察到的最大 release_epoch。
    pub coroutine_views: BTreeMap<u64, u64>,
}

impl AtomicStateMachine {
    pub fn new(initial: u64) -> Self {
        Self {
            value: initial,
            release_epoch: 0,
            seq_cst_seq: 0,
            global_clock: 1,
            coroutine_views: BTreeMap::new(),
        }
    }

    /// 执行 load 操作。
    pub fn load(&mut self, coroutine: u64, order: MemoryOrdering) -> Result<u64, RawModelError> {
        if matches!(order, MemoryOrdering::Release | MemoryOrdering::AcqRel) {
            return Err(RawModelError::new("load 不接受 Release 或 AcqRel 内存序"));
        }
        if matches!(order, MemoryOrdering::Acquire | MemoryOrdering::SeqCst) {
            // 同步 Release 写入的可见性。
            let view = self.coroutine_views.entry(coroutine).or_insert(0);
            *view = (*view).max(self.release_epoch);
        }
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(self.value)
    }

    /// 执行 store 操作。
    pub fn store(
        &mut self,
        coroutine: u64,
        val: u64,
        order: MemoryOrdering,
    ) -> Result<(), RawModelError> {
        if matches!(order, MemoryOrdering::Acquire | MemoryOrdering::AcqRel) {
            return Err(RawModelError::new("store 不接受 Acquire 或 AcqRel 内存序"));
        }
        self.value = val;
        self.global_clock += 1;
        if matches!(order, MemoryOrdering::Release | MemoryOrdering::SeqCst) {
            self.release_epoch = self.global_clock;
            self.coroutine_views.insert(coroutine, self.release_epoch);
        }
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(())
    }

    /// 执行 swap 操作。
    pub fn swap(
        &mut self,
        coroutine: u64,
        val: u64,
        order: MemoryOrdering,
    ) -> Result<u64, RawModelError> {
        let old = self.value;
        self.value = val;
        self.global_clock += 1;
        if matches!(
            order,
            MemoryOrdering::Release | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
        ) {
            self.release_epoch = self.global_clock;
            self.coroutine_views.insert(coroutine, self.release_epoch);
        }
        if matches!(
            order,
            MemoryOrdering::Acquire | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
        ) {
            let view = self.coroutine_views.entry(coroutine).or_insert(0);
            *view = (*view).max(self.release_epoch);
        }
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(old)
    }

    /// 执行 compare_exchange 操作。
    pub fn compare_exchange(
        &mut self,
        coroutine: u64,
        expected: u64,
        desired: u64,
        success: MemoryOrdering,
        failure: MemoryOrdering,
    ) -> Result<Result<(), u64>, RawModelError> {
        if matches!(failure, MemoryOrdering::Release | MemoryOrdering::AcqRel) {
            return Err(RawModelError::new(
                "compare_exchange 失败序不能是 Release 或 AcqRel",
            ));
        }
        let allowed = match failure {
            MemoryOrdering::Relaxed => true,
            MemoryOrdering::Acquire => matches!(
                success,
                MemoryOrdering::Acquire | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
            ),
            MemoryOrdering::SeqCst => success == MemoryOrdering::SeqCst,
            MemoryOrdering::Release | MemoryOrdering::AcqRel => false,
        };
        if !allowed {
            return Err(RawModelError::new("compare_exchange 失败序不能强于成功序"));
        }
        if self.value == expected {
            self.value = desired;
            self.global_clock += 1;
            if matches!(
                success,
                MemoryOrdering::Release | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
            ) {
                self.release_epoch = self.global_clock;
                self.coroutine_views.insert(coroutine, self.release_epoch);
            }
            if matches!(
                success,
                MemoryOrdering::Acquire | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
            ) {
                let view = self.coroutine_views.entry(coroutine).or_insert(0);
                *view = (*view).max(self.release_epoch);
            }
            if success == MemoryOrdering::SeqCst {
                self.seq_cst_seq += 1;
            }
            Ok(Ok(()))
        } else {
            let actual = self.value;
            if matches!(failure, MemoryOrdering::Acquire | MemoryOrdering::SeqCst) {
                let view = self.coroutine_views.entry(coroutine).or_insert(0);
                *view = (*view).max(self.release_epoch);
            }
            if failure == MemoryOrdering::SeqCst {
                self.seq_cst_seq += 1;
            }
            Ok(Err(actual))
        }
    }

    /// 执行 fence 操作。
    pub fn fence(&mut self, order: MemoryOrdering) -> Result<(), RawModelError> {
        if order == MemoryOrdering::Relaxed {
            return Err(RawModelError::new("fence 不接受 Relaxed 内存序"));
        }
        self.global_clock += 1;
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(())
    }
}

/// 互斥锁获取结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutexLockOutcome {
    Acquired,
    Contended { wait_node: u64 },
}

/// 互斥锁。
#[derive(Clone, Debug)]
pub struct Mutex {
    pub owner: Option<u64>,
    pub lock_count: u64,
    pub wait_queue: VecDeque<u64>,
    pub unlocked_explicitly: bool,
}

impl Default for Mutex {
    fn default() -> Self {
        Self::new()
    }
}

impl Mutex {
    pub fn new() -> Self {
        Self {
            owner: None,
            lock_count: 0,
            wait_queue: VecDeque::new(),
            unlocked_explicitly: false,
        }
    }

    pub fn state_code(&self) -> u32 {
        if self.owner.is_none() {
            MUTEX_UNLOCKED
        } else if self.wait_queue.is_empty() {
            MUTEX_LOCKED
        } else {
            MUTEX_CONTENDED
        }
    }

    pub fn lock(&mut self, coroutine: u64, next_node_id: u64) -> MutexLockOutcome {
        if self.owner.is_none() {
            self.owner = Some(coroutine);
            self.lock_count += 1;
            self.unlocked_explicitly = false;
            MutexLockOutcome::Acquired
        } else {
            // 持锁交接使用 coroutine；等待 token 只通过 Contended 返回。
            self.wait_queue.push_back(coroutine);
            MutexLockOutcome::Contended {
                wait_node: next_node_id,
            }
        }
    }

    /// 释放锁：若有等待者，直接将锁交接给该等待者（保证弱公平，且不 poisoning）。
    pub fn unlock(&mut self, coroutine: u64) -> Result<Option<u64>, RawModelError> {
        if self.owner != Some(coroutine) && self.owner.is_some() {
            return Err(RawModelError::new("非持锁协程尝试解锁"));
        }
        self.unlocked_explicitly = true;
        if let Some(next_waiter) = self.wait_queue.pop_front() {
            self.owner = Some(next_waiter);
            self.lock_count += 1;
            Ok(Some(next_waiter))
        } else {
            self.owner = None;
            Ok(None)
        }
    }

    /// 租约结束触发的自动解锁（支持持锁协程 panic 时正常解锁，不 poisoning）。
    pub fn release_on_lease_drop(&mut self, coroutine: u64) -> Option<u64> {
        if self.owner == Some(coroutine) {
            self.unlock(coroutine).ok().flatten()
        } else {
            None
        }
    }
}

/// 读写锁模式。
#[derive(Clone, Debug, Default)]
pub struct RwLock {
    pub readers: BTreeSet<u64>,
    pub writer: Option<u64>,
    /// 交接队列保存协程身份，与等待节点 token 分属两个空间。
    pub read_waiters: VecDeque<u64>,
    pub write_waiters: VecDeque<u64>,
}

impl RwLock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, coroutine: u64, next_node_id: u64) -> MutexLockOutcome {
        if self.writer.is_none() && self.write_waiters.is_empty() {
            self.readers.insert(coroutine);
            MutexLockOutcome::Acquired
        } else {
            self.read_waiters.push_back(coroutine);
            MutexLockOutcome::Contended {
                wait_node: next_node_id,
            }
        }
    }

    pub fn write(&mut self, coroutine: u64, next_node_id: u64) -> MutexLockOutcome {
        if self.writer.is_none() && self.readers.is_empty() {
            self.writer = Some(coroutine);
            MutexLockOutcome::Acquired
        } else {
            self.write_waiters.push_back(coroutine);
            MutexLockOutcome::Contended {
                wait_node: next_node_id,
            }
        }
    }

    pub fn unlock_read(&mut self, coroutine: u64) -> Result<Vec<u64>, RawModelError> {
        if !self.readers.remove(&coroutine) {
            return Err(RawModelError::new("非持读锁协程尝试释放读锁"));
        }
        let mut woken = Vec::new();
        if self.readers.is_empty()
            && let Some(next_writer) = self.write_waiters.pop_front()
        {
            self.writer = Some(next_writer);
            woken.push(next_writer);
        }
        Ok(woken)
    }

    pub fn unlock_write(&mut self, coroutine: u64) -> Result<Vec<u64>, RawModelError> {
        if self.writer != Some(coroutine) {
            return Err(RawModelError::new("非持写锁协程尝试释放写锁"));
        }
        self.writer = None;
        let mut woken = Vec::new();
        if let Some(next_writer) = self.write_waiters.pop_front() {
            self.writer = Some(next_writer);
            woken.push(next_writer);
        } else {
            while let Some(reader) = self.read_waiters.pop_front() {
                self.readers.insert(reader);
                woken.push(reader);
            }
        }
        Ok(woken)
    }

    /// 租约结束触发自动解锁（Panic 展开时自动归还）。
    pub fn release_on_lease_drop(&mut self, coroutine: u64) -> Vec<u64> {
        if self.writer == Some(coroutine) {
            self.unlock_write(coroutine).unwrap_or_default()
        } else if self.readers.contains(&coroutine) {
            self.unlock_read(coroutine).unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

/// 条件变量。
#[derive(Clone, Debug, Default)]
pub struct Condvar {
    pub wait_queue: VecDeque<(u64, u64)>, // (coroutine, wait_node)
    pub sequence: u64,
}

impl Condvar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wait(&mut self, coroutine: u64, wait_node: u64) {
        self.wait_queue.push_back((coroutine, wait_node));
    }

    pub fn notify_one(&mut self) -> Option<(u64, u64)> {
        self.sequence += 1;
        self.wait_queue.pop_front()
    }

    pub fn notify_all(&mut self) -> Vec<(u64, u64)> {
        self.sequence += 1;
        self.wait_queue.drain(..).collect()
    }
}

/// OnceLock / Lazy 状态机。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnceState {
    Uninit,
    Initializing,
    Ready,
    Failed,
}

impl OnceState {
    pub fn code(self) -> u32 {
        match self {
            Self::Uninit => ONCE_UNINIT,
            Self::Initializing => ONCE_INITIALIZING,
            Self::Ready => ONCE_READY,
            Self::Failed => ONCE_FAILED,
        }
    }
}

/// OnceLock / Lazy 控制结构。
#[derive(Clone, Debug)]
pub struct OnceLock {
    pub state: OnceState,
    pub value: Option<u64>,
    pub initiator: Option<u64>,
    pub waiters: Vec<u64>,
}

impl Default for OnceLock {
    fn default() -> Self {
        Self::new()
    }
}

impl OnceLock {
    pub fn new() -> Self {
        Self {
            state: OnceState::Uninit,
            value: None,
            initiator: None,
            waiters: Vec::new(),
        }
    }

    pub fn get(&self) -> Result<Option<u64>, RawModelError> {
        match self.state {
            OnceState::Ready => Ok(self.value),
            OnceState::Failed => Err(RawModelError::new("OnceLock 初始化永久失败 (Failed)")),
            OnceState::Uninit | OnceState::Initializing => Ok(None),
        }
    }

    /// 开始初始化；若已完成或已失败则立即返回结果。
    pub fn start_init(
        &mut self,
        coroutine: u64,
        wait_node: u64,
    ) -> Result<OnceInitAction, RawModelError> {
        match self.state {
            OnceState::Ready => Ok(OnceInitAction::Ready(self.value.expect("value"))),
            OnceState::Failed => Err(RawModelError::new("OnceLock 初始化永久失败 (Failed)")),
            OnceState::Uninit => {
                self.state = OnceState::Initializing;
                self.initiator = Some(coroutine);
                Ok(OnceInitAction::ExecuteInitializer)
            }
            OnceState::Initializing => {
                self.waiters.push(wait_node);
                Ok(OnceInitAction::Wait)
            }
        }
    }

    /// 初始化闭包成功：转入 Ready，唤醒所有等待者。
    pub fn finish_init(&mut self, value: u64) -> Result<Vec<u64>, RawModelError> {
        if self.state != OnceState::Initializing {
            return Err(RawModelError::new("非 Initializing 状态不能完成初始化"));
        }
        self.state = OnceState::Ready;
        self.value = Some(value);
        self.initiator = None;
        Ok(std::mem::take(&mut self.waiters))
    }

    /// 初始化闭包 panic：转入永久 Failed，唤醒所有等待者且以后不再重试。
    pub fn fail_init(&mut self) -> Result<Vec<u64>, RawModelError> {
        if self.state != OnceState::Initializing {
            return Err(RawModelError::new("非 Initializing 状态不能标记失败"));
        }
        self.state = OnceState::Failed;
        self.initiator = None;
        Ok(std::mem::take(&mut self.waiters))
    }

    /// set 操作：已初始化或已失败时交还原值，不覆盖。
    pub fn set(&mut self, val: u64) -> Result<(), u64> {
        match self.state {
            OnceState::Uninit => {
                self.state = OnceState::Ready;
                self.value = Some(val);
                Ok(())
            }
            OnceState::Initializing | OnceState::Ready | OnceState::Failed => Err(val),
        }
    }
}

/// 一次性初始化动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnceInitAction {
    ExecuteInitializer,
    Ready(u64),
    Wait,
}

/// 取消错误结构。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cancelled;

/// 取消源。
#[derive(Clone, Debug)]
pub struct CancelSource {
    pub is_cancelled: bool,
    pub generation: u64,
    pub waiters: Vec<WaitNodeHandle>,
}

impl Default for CancelSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelSource {
    pub fn new() -> Self {
        Self {
            is_cancelled: false,
            generation: 1,
            waiters: Vec::new(),
        }
    }

    pub fn state_code(&self) -> u32 {
        if self.is_cancelled {
            CANCEL_CANCELLED
        } else {
            CANCEL_ACTIVE
        }
    }

    /// 幂等取消。
    pub fn cancel(&mut self) -> Vec<WaitNodeHandle> {
        if self.is_cancelled {
            return Vec::new();
        }
        self.is_cancelled = true;
        self.generation += 1;
        std::mem::take(&mut self.waiters)
    }

    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled
    }

    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// 注册取消等待者。若已取消则立即返回被唤醒。
    pub fn register_waiter(&mut self, wait_node: WaitNodeHandle) -> Result<(), Cancelled> {
        if self.is_cancelled {
            Err(Cancelled)
        } else {
            self.waiters.push(wait_node);
            Ok(())
        }
    }

    /// 安全注销取消等待者（阻塞操作完成或超时后注销）。
    pub fn unregister_waiter(&mut self, wait_node: WaitNodeHandle) {
        self.waiters.retain(|&w| w != wait_node);
    }
}

/// Mutex 句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MutexHandle(pub usize);

/// RwLock 句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RwLockHandle(pub usize);

/// Condvar 句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CondvarHandle(pub usize);

/// OnceLock 句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OnceHandle(pub usize);

/// CancelSource 句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CancelHandle(pub usize);

/// 运行时同步平面。
#[derive(Clone, Debug, Default)]
pub struct SyncPlane {
    pub mutexes: Vec<Mutex>,
    pub rwlocks: Vec<RwLock>,
    pub condvars: Vec<Condvar>,
    pub onces: Vec<OnceLock>,
    pub cancels: Vec<CancelSource>,
    pub atomics: BTreeMap<u64, AtomicStateMachine>,
    pub next_node_id: u64,
}

impl SyncPlane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_wait_node(&mut self) -> u64 {
        self.next_node_id += 1;
        self.next_node_id
    }

    pub fn create_mutex(&mut self) -> MutexHandle {
        let index = self.mutexes.len();
        self.mutexes.push(Mutex::new());
        MutexHandle(index)
    }

    pub fn create_rwlock(&mut self) -> RwLockHandle {
        let index = self.rwlocks.len();
        self.rwlocks.push(RwLock::new());
        RwLockHandle(index)
    }

    pub fn create_condvar(&mut self) -> CondvarHandle {
        let index = self.condvars.len();
        self.condvars.push(Condvar::new());
        CondvarHandle(index)
    }

    pub fn create_once(&mut self) -> OnceHandle {
        let index = self.onces.len();
        self.onces.push(OnceLock::new());
        OnceHandle(index)
    }

    pub fn create_cancel(&mut self) -> CancelHandle {
        let index = self.cancels.len();
        self.cancels.push(CancelSource::new());
        CancelHandle(index)
    }

    pub fn release_coroutine_locks(&mut self, coroutine: u64) -> Vec<u64> {
        let mut woken = Vec::new();
        for mutex in &mut self.mutexes {
            if let Some(w) = mutex.release_on_lease_drop(coroutine) {
                woken.push(w);
            }
        }
        for rwlock in &mut self.rwlocks {
            woken.extend(rwlock.release_on_lease_drop(coroutine));
        }
        woken
    }
}
