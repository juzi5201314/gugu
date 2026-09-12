//! std.sync 同步与取消协议的确定性回归；不睡眠、不读 OS 熵。

use super::RawWorld;
use super::coroutine_impl::CoroutineEntry;
use crate::runtime::PlatformProfile;
use crate::runtime::coroutine::{CoroutineHandle, CoroutineState};
use crate::runtime::message::BatchLimits;
use crate::runtime::sync::{
    AtomicStateMachine, Cancelled, MemoryOrdering, MutexLockOutcome, is_legal_atomic_type,
};
use crate::runtime::sync_schema::{
    CANCEL_ACTIVE, CANCEL_CANCELLED, MUTEX_CONTENDED, MUTEX_LOCKED, MUTEX_UNLOCKED, ONCE_FAILED,
    ONCE_INITIALIZING, ONCE_READY, ONCE_UNINIT, SYNC_SCHEMA, SyncDemand, SyncRuntimeContract,
};
use crate::{CompileRequest, Compiler, TargetName};

fn entry() -> CoroutineEntry {
    CoroutineEntry {
        pc: 0x1000,
        required_frame: 64,
    }
}

fn plane(owners: u32) -> RawWorld {
    RawWorld::new(7, owners, 64, BatchLimits::default()).expect("raw world")
}

fn booted() -> RawWorld {
    let mut world = plane(2);
    world
        .boot(
            vec![],
            vec![("GUGU_RUNTIME_STACK_MAX".to_owned(), "64KiB".to_owned())],
            "/".to_owned(),
            2,
            entry(),
        )
        .expect("boot");
    world
}

fn running(world: &mut RawWorld) -> CoroutineHandle {
    let child = world
        .spawn_user_coroutine(0, entry())
        .expect("创建")
        .expect("接纳");
    world.enter_coroutine(child).expect("切入");
    child
}

#[test]
fn atomic_ordering_state_machine_acquire_release_seq_cst() {
    // 1. 内存序转换与非法值拒绝
    assert_eq!(
        MemoryOrdering::from_u32(0).unwrap(),
        MemoryOrdering::Relaxed
    );
    assert_eq!(
        MemoryOrdering::from_u32(1).unwrap(),
        MemoryOrdering::Acquire
    );
    assert_eq!(
        MemoryOrdering::from_u32(2).unwrap(),
        MemoryOrdering::Release
    );
    assert_eq!(MemoryOrdering::from_u32(3).unwrap(), MemoryOrdering::AcqRel);
    assert_eq!(MemoryOrdering::from_u32(4).unwrap(), MemoryOrdering::SeqCst);
    assert!(MemoryOrdering::from_u32(99).is_err());

    // 2. 合法标量类型校验
    assert!(is_legal_atomic_type("bool"));
    assert!(is_legal_atomic_type("int8"));
    assert!(is_legal_atomic_type("int32"));
    assert!(is_legal_atomic_type("int64"));
    assert!(is_legal_atomic_type("uint64"));
    assert!(is_legal_atomic_type("ptr"));
    assert!(is_legal_atomic_type("*raw"));
    assert!(is_legal_atomic_type("*byte"));
    assert!(!is_legal_atomic_type("f32"));
    assert!(!is_legal_atomic_type("f64"));
    assert!(!is_legal_atomic_type("string"));
    assert!(!is_legal_atomic_type("Record"));

    // 3. 状态机操作与规则校验
    let mut sm = AtomicStateMachine::new(10);
    assert_eq!(sm.load(1, MemoryOrdering::Relaxed).unwrap(), 10);

    // load 不接受 Release / AcqRel
    assert!(sm.load(1, MemoryOrdering::Release).is_err());
    assert!(sm.load(1, MemoryOrdering::AcqRel).is_err());

    // store 不接受 Acquire / AcqRel
    assert!(sm.store(1, 20, MemoryOrdering::Acquire).is_err());
    assert!(sm.store(1, 20, MemoryOrdering::AcqRel).is_err());

    // CAS 校验：失败序不能为 Release / AcqRel，且失败序不能强于成功序
    assert!(
        sm.compare_exchange(1, 10, 20, MemoryOrdering::SeqCst, MemoryOrdering::Release)
            .is_err()
    );
    assert!(
        sm.compare_exchange(1, 10, 20, MemoryOrdering::Relaxed, MemoryOrdering::Acquire)
            .is_err()
    );

    // fence 不接受 Relaxed
    assert!(sm.fence(MemoryOrdering::Relaxed).is_err());
    assert!(sm.fence(MemoryOrdering::SeqCst).is_ok());

    // 4. Release-Acquire 步调一致
    sm.store(1, 42, MemoryOrdering::Release).unwrap();
    let rel_epoch = sm.release_epoch;
    assert!(rel_epoch > 0);

    // 协程 2 执行 Acquire load，观察并同步视图
    let val = sm.load(2, MemoryOrdering::Acquire).unwrap();
    assert_eq!(val, 42);
    assert_eq!(sm.coroutine_views.get(&2), Some(&rel_epoch));

    // 5. SeqCst 全局序递增
    let seq_before = sm.seq_cst_seq;
    sm.store(1, 100, MemoryOrdering::SeqCst).unwrap();
    assert_eq!(sm.seq_cst_seq, seq_before + 1);
    let _ = sm.load(2, MemoryOrdering::SeqCst).unwrap();
    assert_eq!(sm.seq_cst_seq, seq_before + 2);

    // 6. CAS 成功与失败
    let cas_res = sm
        .compare_exchange(1, 100, 200, MemoryOrdering::SeqCst, MemoryOrdering::SeqCst)
        .unwrap();
    assert_eq!(cas_res, Ok(()));
    assert_eq!(sm.value, 200);

    let cas_fail = sm
        .compare_exchange(1, 999, 300, MemoryOrdering::SeqCst, MemoryOrdering::SeqCst)
        .unwrap();
    assert_eq!(cas_fail, Err(200));
}

#[test]
fn mutex_guard_non_poisoning_and_lease_auto_unlock() {
    let mut world = booted();
    let c1 = running(&mut world);
    let c2 = running(&mut world);
    let c3 = running(&mut world);

    let m = world.mutex_new();
    assert_eq!(world.sync.mutexes[m.0].state_code(), MUTEX_UNLOCKED);

    // 协程 1 获取锁
    let out1 = world.mutex_lock(m, c1).expect("lock c1");
    assert_eq!(out1, MutexLockOutcome::Acquired);
    assert_eq!(world.sync.mutexes[m.0].state_code(), MUTEX_LOCKED);
    assert_eq!(world.sync.mutexes[m.0].owner, Some(u64::from(c1.index)));

    // 协程 2 发生竞争，进入等待队列
    let out2 = world.mutex_lock(m, c2).expect("lock c2");
    assert!(matches!(out2, MutexLockOutcome::Contended { .. }));
    assert_eq!(world.sync.mutexes[m.0].state_code(), MUTEX_CONTENDED);
    assert_eq!(world.sync.mutexes[m.0].wait_queue.len(), 1);

    // 非持锁协程尝试解锁失败
    assert!(world.mutex_unlock(m, c3).is_err());

    // 协程 1 显式解锁，直接交接给协程 2（保证弱公平且无 poisoning）
    world.mutex_unlock(m, c1).expect("unlock c1");
    assert_eq!(world.sync.mutexes[m.0].owner, Some(u64::from(c2.index)));

    // 协程 2 在持有锁期间由于 panic / 协程完成，其租约结束触发自动解锁
    let woken = world.sync.release_coroutine_locks(u64::from(c2.index));
    assert!(woken.is_empty());
    assert_eq!(world.sync.mutexes[m.0].owner, None);
    assert_eq!(world.sync.mutexes[m.0].state_code(), MUTEX_UNLOCKED);

    // 协程 3 可以正常获取锁，无 poisoning 异常
    let out3 = world.mutex_lock(m, c3).expect("lock c3");
    assert_eq!(out3, MutexLockOutcome::Acquired);
    assert_eq!(world.sync.mutexes[m.0].owner, Some(u64::from(c3.index)));
}

#[test]
fn rwlock_snapshot_and_readers_writers() {
    let mut world = booted();
    let c1 = running(&mut world);
    let c2 = running(&mut world);
    let c3 = running(&mut world);
    let c4 = running(&mut world);

    let rw = world.rwlock_new();

    // 多个读锁可并发持有
    assert_eq!(
        world.rwlock_read(rw, c1).unwrap(),
        MutexLockOutcome::Acquired
    );
    assert_eq!(
        world.rwlock_read(rw, c2).unwrap(),
        MutexLockOutcome::Acquired
    );
    assert_eq!(world.sync.rwlocks[rw.0].readers.len(), 2);

    // 写锁必须等待所有读锁释放
    let w_out = world.rwlock_write(rw, c3).unwrap();
    assert!(matches!(w_out, MutexLockOutcome::Contended { .. }));

    // 读锁逐个释放
    world.rwlock_unlock_read(rw, c1).unwrap();
    assert_eq!(world.sync.rwlocks[rw.0].writer, None);

    // c2 租约掉落触发自动释放读锁，唤醒写者 c3
    let woken = world.sync.release_coroutine_locks(u64::from(c2.index));
    assert_eq!(woken.len(), 1);
    assert_eq!(world.sync.rwlocks[rw.0].writer, Some(u64::from(c3.index)));

    // 写者持有期间，新的读者 c4 被挂起
    let r_out = world.rwlock_read(rw, c4).unwrap();
    assert!(matches!(r_out, MutexLockOutcome::Contended { .. }));

    // 写者解锁写锁，唤醒读者 c4
    world.rwlock_unlock_write(rw, c3).unwrap();
    assert!(
        world.sync.rwlocks[rw.0]
            .readers
            .contains(&u64::from(c4.index))
    );
    assert_eq!(world.sync.rwlocks[rw.0].writer, None);
}

#[test]
fn condvar_atomic_wait_and_notifications() {
    let mut world = booted();
    let c1 = running(&mut world);
    let c2 = running(&mut world);
    let c3 = running(&mut world);

    let m = world.mutex_new();
    let cv = world.condvar_new();

    // c1 获取互斥锁
    world.mutex_lock(m, c1).unwrap();

    // c1 阻塞在 condvar 上：原子释放 mutex 并挂入 condvar
    world.condvar_wait(cv, m, c1).unwrap();
    assert_eq!(world.sync.mutexes[m.0].owner, None);
    assert_eq!(world.sync.condvars[cv.0].wait_queue.len(), 1);

    // c2 获取互斥锁并也在 condvar 上等待
    world.mutex_lock(m, c2).unwrap();
    world.condvar_wait(cv, m, c2).unwrap();
    assert_eq!(world.sync.condvars[cv.0].wait_queue.len(), 2);

    // c3 获取互斥锁，调用 notify_one 唤醒一个等待者
    world.mutex_lock(m, c3).unwrap();
    let woken_one = world.condvar_notify_one(cv).unwrap();
    assert_eq!(woken_one, Some(u64::from(c1.index)));
    assert_eq!(world.sync.condvars[cv.0].wait_queue.len(), 1);

    // c3 调用 notify_all 唤醒全部剩余等待者
    let woken_all = world.condvar_notify_all(cv).unwrap();
    assert_eq!(woken_all, vec![u64::from(c2.index)]);
    assert!(world.sync.condvars[cv.0].wait_queue.is_empty());
}

#[test]
fn once_lock_and_lazy_permanent_failed_on_panic() {
    let mut world = booted();
    let c1 = running(&mut world);
    let c2 = running(&mut world);

    // 1. 成功初始化路径
    let o1 = world.once_new();
    assert_eq!(world.sync.onces[o1.0].state.code(), ONCE_UNINIT);
    assert_eq!(world.once_get(o1).unwrap(), None);

    let act1 = world.once_start_init(o1, c1).unwrap();
    assert_eq!(
        act1,
        crate::runtime::sync::OnceInitAction::ExecuteInitializer
    );
    assert_eq!(world.sync.onces[o1.0].state.code(), ONCE_INITIALIZING);

    // 初始化中，其他协程进入等待
    let act2 = world.once_start_init(o1, c2).unwrap();
    assert_eq!(act2, crate::runtime::sync::OnceInitAction::Wait);

    // 完成初始化
    let woken = world.once_finish_init(o1, 12345).unwrap();
    assert_eq!(woken.len(), 1);
    assert_eq!(world.sync.onces[o1.0].state.code(), ONCE_READY);
    assert_eq!(world.once_get(o1).unwrap(), Some(12345));

    // set 遇到已初始化交还原值
    assert_eq!(world.once_set(o1, 999), Err(999));

    // 2. panic / 失败路径：永久进入 Failed，不重试
    let o2 = world.once_new();
    let _ = world.once_start_init(o2, c1).unwrap();
    let _ = world.once_start_init(o2, c2).unwrap();

    // 模拟闭包失败/展开
    let woken_fail = world.once_fail_init(o2).unwrap();
    assert_eq!(woken_fail.len(), 1);
    assert_eq!(world.sync.onces[o2.0].state.code(), ONCE_FAILED);

    // 永久 Failed：后续 get 直接返回 Err
    assert!(world.once_get(o2).is_err());

    // 后续 start_init 也永久失败，不再重新执行初始化闭包
    assert!(world.once_start_init(o2, c1).is_err());

    // set 遇到 Failed 同样交还原值
    assert_eq!(world.once_set(o2, 888), Err(888));
}

#[test]
fn cancel_source_and_token_idempotent_cooperative() {
    let mut world = booted();
    let cs = world.cancel_source_new();

    assert_eq!(world.sync.cancels[cs.0].state_code(), CANCEL_ACTIVE);
    assert_eq!(world.cancel_token_is_cancelled(cs).unwrap(), false);
    assert_eq!(world.cancel_token_check(cs), Ok(()));

    // 注册等待者
    world.sync.cancels[cs.0].register_waiter(100).unwrap();
    world.sync.cancels[cs.0].register_waiter(200).unwrap();

    // 触发取消
    let woken = world.cancel_source_cancel(cs).unwrap();
    assert_eq!(woken, vec![100, 200]);
    assert_eq!(world.sync.cancels[cs.0].state_code(), CANCEL_CANCELLED);
    assert_eq!(world.cancel_token_is_cancelled(cs).unwrap(), true);
    assert_eq!(world.cancel_token_check(cs), Err(Cancelled));

    // 幂等取消：二次取消安全，返回空唤醒列表
    let woken2 = world.cancel_source_cancel(cs).unwrap();
    assert!(woken2.is_empty());

    // 取消后注册等待者立即返回 Err(Cancelled)
    assert_eq!(
        world.sync.cancels[cs.0].register_waiter(300),
        Err(Cancelled)
    );
}

#[test]
fn cancel_seam_with_wait_queues() {
    let mut world = booted();
    let c_waiter = running(&mut world);
    let c_child = running(&mut world);

    let ch = world.channel_new(4).expect("channel");
    let cs = world.cancel_source_new();

    // 1. Channel 取消接缝：取消源已取消时，channel_recv_cancel 立即返回 Err(Cancelled) 且不污染通道
    world.cancel_source_cancel(cs).unwrap();
    let res = world
        .channel_recv_cancel(ch, cs, c_waiter)
        .expect("channel recv cancel");
    assert_eq!(res, Err(Cancelled));

    // 通道本身完好，可以正常收发数据
    let send_out = world.channel_send(ch, c_waiter, 42, false).expect("send");
    assert!(matches!(
        send_out,
        crate::runtime::channel::SendOutcome::Sent { .. }
    ));

    // 2. Join 取消接缝：取消只取消 waiter，绝不杀死 child 协程
    let cs2 = world.cancel_source_new();
    world.cancel_source_cancel(cs2).unwrap();

    let join_res = world
        .join_wait_cancel(c_child, cs2, c_waiter)
        .expect("join wait cancel");
    assert_eq!(join_res, Err(Cancelled));

    // 验证子协程仍然活着且处于正常调度状态，绝未被 kill
    let child_state = world
        .controls
        .get(c_child)
        .expect("child control")
        .0
        .hot
        .lifecycle()
        .expect("lifecycle");
    assert_ne!(child_state, CoroutineState::Dead);
}

#[test]
fn sync_contract_and_image_plan_cli() {
    let demand = SyncDemand {
        atomic_ops: 2,
        mutex_ops: 1,
        rwlock_ops: 1,
        condvar_ops: 1,
        once_ops: 1,
        lazy_ops: 0,
        cancel_ops: 2,
    };
    let contract =
        SyncRuntimeContract::build(demand, PlatformProfile::Linux).expect("sync contract");
    contract.verify().expect("verify");
    assert_eq!(contract.schema(), SYNC_SCHEMA);
    assert_eq!(contract.primitive_count(), 5);

    // 确定性指纹与敏感度
    let contract2 =
        SyncRuntimeContract::build(demand, PlatformProfile::Linux).expect("sync contract 2");
    assert_eq!(contract.fingerprint(), contract2.fingerprint());

    let demand_diff = SyncDemand {
        atomic_ops: 3,
        ..demand
    };
    let contract3 =
        SyncRuntimeContract::build(demand_diff, PlatformProfile::Linux).expect("sync contract 3");
    assert_ne!(contract.fingerprint(), contract3.fingerprint());

    // 验证 ImagePlan 中的 sync 输出
    let compiler = Compiler::new();
    let res = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let value = 1\n _ = value }",
        TargetName::X86_64Linux,
    ));
    assert!(res.is_success(), "{:?}", res.diagnostics().items());
    let plan = res.image_plan().expect("image_plan");
    assert_eq!(plan.sync_runtime().schema(), SYNC_SCHEMA);
    assert_ne!(plan.sync_contract_fingerprint(), [0_u8; 32]);
    let dump = res.dump_runtime().expect("dump");
    assert!(dump.contains("sync schema=1"));
}
