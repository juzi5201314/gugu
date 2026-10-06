//! std.sync 的内存序闸门与契约指纹。
//!
//! 锁、条件变量和取消的执行在镜像内 Gugu runtime。这里保留非法序拒绝与 ImagePlan。

use crate::runtime::PlatformProfile;
use crate::runtime::sync_schema::{
    AtomicStateMachine, MemoryOrdering, SYNC_SCHEMA, SyncDemand, SyncRuntimeContract,
    is_legal_atomic_type,
};
use crate::{CompileRequest, Compiler, TargetName};

#[test]
fn atomic_ordering_state_machine_acquire_release_seq_cst() {
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

    let mut sm = AtomicStateMachine::new(10);
    assert_eq!(sm.load(1, MemoryOrdering::Relaxed).unwrap(), 10);
    assert!(sm.load(1, MemoryOrdering::Release).is_err());
    assert!(sm.load(1, MemoryOrdering::AcqRel).is_err());
    assert!(sm.store(1, 20, MemoryOrdering::Acquire).is_err());
    assert!(sm.store(1, 20, MemoryOrdering::AcqRel).is_err());
    assert!(
        sm.compare_exchange(1, 10, 20, MemoryOrdering::SeqCst, MemoryOrdering::Release)
            .is_err()
    );
    assert!(
        sm.compare_exchange(1, 10, 20, MemoryOrdering::Relaxed, MemoryOrdering::Acquire)
            .is_err()
    );
    assert!(sm.fence(MemoryOrdering::Relaxed).is_err());
    assert!(sm.fence(MemoryOrdering::SeqCst).is_ok());

    sm.store(1, 42, MemoryOrdering::Release).unwrap();
    let rel_epoch = sm.release_epoch;
    assert!(rel_epoch > 0);
    let val = sm.load(2, MemoryOrdering::Acquire).unwrap();
    assert_eq!(val, 42);
    assert_eq!(sm.coroutine_views.get(&2), Some(&rel_epoch));

    let seq_before = sm.seq_cst_seq;
    sm.store(1, 100, MemoryOrdering::SeqCst).unwrap();
    assert_eq!(sm.seq_cst_seq, seq_before + 1);
    let _ = sm.load(2, MemoryOrdering::SeqCst).unwrap();
    assert_eq!(sm.seq_cst_seq, seq_before + 2);

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
fn cas_ordering_matrix_rejects_invalid_pairs_without_side_effects() {
    use MemoryOrdering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
    for success in [Relaxed, Acquire, Release, AcqRel, SeqCst] {
        for failure in [Relaxed, Acquire, Release, AcqRel, SeqCst] {
            let allowed = matches!(
                (success, failure),
                (Relaxed, Relaxed)
                    | (Acquire, Relaxed | Acquire)
                    | (Release, Relaxed)
                    | (AcqRel, Relaxed | Acquire)
                    | (SeqCst, Relaxed | Acquire | SeqCst)
            );
            for expected in [7, 6] {
                let mut sm = AtomicStateMachine::new(7);
                sm.store(5, 7, Release).unwrap();
                let before = (
                    sm.value,
                    sm.release_epoch,
                    sm.seq_cst_seq,
                    sm.global_clock,
                    sm.coroutine_views.clone(),
                );
                let result = sm.compare_exchange(8, expected, 9, success, failure);
                if allowed {
                    if expected == 7 {
                        assert_eq!(result, Ok(Ok(())));
                        assert_eq!(sm.value, 9);
                    } else {
                        assert_eq!(result, Ok(Err(7)));
                        assert_eq!(sm.value, 7);
                    }
                } else {
                    assert!(result.is_err(), "{success:?}/{failure:?}");
                    assert_eq!(
                        (
                            sm.value,
                            sm.release_epoch,
                            sm.seq_cst_seq,
                            sm.global_clock,
                            sm.coroutine_views
                        ),
                        before
                    );
                }
            }
        }
    }
}

#[cfg(feature = "slow-tests")]
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
