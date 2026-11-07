//! worker 全忙时取消 admission 等待者；遍历四节点的全部取消组合。
use super::*;

const WAITERS: [u64; 4] = [20000, 20064, 20128, 20192];
fn queued_memory(mask: u8) -> Vec<u8> {
    let mut memory = request_memory();
    memory.resize(49152, 0);
    for slot in 0..8 {
        put(
            &mut memory,
            REQUEST + slot * 128 + 104,
            HOT + (slot + 1) * 64,
        );
        put(&mut memory, REQUEST + slot * 128 + 112, 0);
    }
    for (index, &hot) in WAITERS.iter().enumerate() {
        let cold = 30000 + index as u64 * 512;
        let cancel = 40000 + index as u64 * 8;
        put(&mut memory, hot + 48, cold);
        put(
            &mut memory,
            cold + 272,
            WAITERS.get(index + 1).copied().unwrap_or(0),
        );
        put(&mut memory, cold + 464, cancel);
        put(&mut memory, cancel, u64::from(mask >> index & 1));
    }
    put(&mut memory, POOL, WAITERS[0]);
    put(&mut memory, POOL + 8, WAITERS[3]);
    put(&mut memory, POOL + 32, 4);
    memory
}

#[test]
fn pressure_admission_keeps_inflight_credit_and_resumes_fifo_after_drain() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.block_budget = 1024;
    let mut memory = queued_memory(0);
    // 一个在途请求、四个排队请求；八个槽中仍有七个可用。
    for slot in 1..8 {
        put(&mut memory, REQUEST + slot * 128 + 104, 0);
    }
    put(&mut memory, PROCESSOR + 3888, 1);
    put(&mut memory, PROCESSOR + 3952, 1);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert!(harness.started.is_empty(), "压力期间不能继续扩张 worker");
    assert_eq!(word(&memory, POOL + 32), 4);
    assert_eq!(word(&memory, REQUEST + 104), HOT + 64);
    assert!(harness.reserved.is_empty());
    // 真实完成由 fs_poll 钩子报告，原 owner 必须有合法 cold 记录。
    put(&mut memory, HOT + 64 + 48, 42000);
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.started, [REQUEST]);
    assert_eq!(word(&memory, REQUEST + 104), WAITERS[0]);
    assert_eq!(word(&memory, POOL + 32), 3);
    assert_eq!(harness.woken, [HOT + 64]);
    // 压力仍在时取消排队中间项，不等待在途 worker，不消耗 credit。
    harness.poll_complete = false;
    put(&mut memory, 40016, 1);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, POOL + 32), 2);
    assert_eq!(harness.started.len(), 1);
    assert_eq!(harness.woken, [HOT + 64, WAITERS[2]]);
    // 压力解除后恢复通常容量，保留原在途请求并按 FIFO 启动两名幸存者。
    put(&mut memory, PROCESSOR + 3888, 0);
    put(&mut memory, PROCESSOR + 3952, 1 << 20);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.started, [REQUEST, REQUEST + 128, REQUEST + 256]);
    assert_eq!(word(&memory, REQUEST + 104), WAITERS[0]);
    assert_eq!(word(&memory, REQUEST + 128 + 104), WAITERS[1]);
    assert_eq!(word(&memory, REQUEST + 256 + 104), WAITERS[3]);
    assert_eq!(word(&memory, POOL + 32), 0);
}

#[test]
fn pressure_admission_preserves_all_existing_workers() {
    let output = compile();
    for state in [1, 2] {
        for active in [1, 3, 8] {
            let mut harness = ResourceHarness::new(&output);
            let mut memory = queued_memory(0);
            for slot in active..8 {
                put(&mut memory, REQUEST + slot * 128 + 104, 0);
            }
            put(&mut memory, PROCESSOR + 3888, state);
            harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
            for slot in 0..active {
                assert_eq!(
                    word(&memory, REQUEST + slot * 128 + 104),
                    HOT + (slot + 1) * 64
                );
            }
            assert_eq!(word(&memory, POOL), WAITERS[0]);
            assert_eq!(word(&memory, POOL + 32), 4);
            assert!(harness.started.is_empty() && harness.woken.is_empty());
            assert!(harness.reserved.is_empty() && harness.released.is_empty());
        }
    }
}

#[test]
fn cancelled_admission_waiters_leave_full_pool_without_waiting_for_worker() {
    let output = compile();
    for mask in 0..16 {
        let mut harness = ResourceHarness::new(&output);
        let mut memory = queued_memory(mask);
        let (cancelled, live): (Vec<_>, Vec<_>) = WAITERS
            .iter()
            .copied()
            .enumerate()
            .partition(|(index, _)| mask >> index & 1 != 0);
        harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
        assert_eq!(
            harness.woken,
            cancelled.iter().map(|(_, hot)| *hot).collect::<Vec<_>>(),
            "mask={mask}"
        );
        assert!(harness.started.is_empty(), "满额不得启动新 worker");
        assert!(harness.reserved.is_empty(), "取消不得分配栈");
        assert!(harness.released.is_empty(), "在途 payload 不能提前归还");
        assert_eq!(word(&memory, POOL + 32), live.len() as u64);
        assert_eq!(
            word(&memory, POOL + 8),
            live.last().map_or(0, |(_, hot)| *hot)
        );
        let mut cursor = word(&memory, POOL);
        for (_, hot) in &live {
            assert_eq!(cursor, *hot, "幸存队列保持 FIFO");
            cursor = word(&memory, word(&memory, cursor + 48) + 272);
        }
        assert_eq!(cursor, 0);
        for (_, hot) in &cancelled {
            let wait = word(&memory, hot + 48) + 272;
            assert_eq!(word(&memory, wait), 0, "唤醒前摘除队列链接");
            assert_eq!(
                (word(&memory, wait + 80), word(&memory, wait + 88)),
                (0, u64::MAX)
            );
        }
        harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
        assert_eq!(harness.woken.len(), cancelled.len(), "只能唤醒一次");
        for slot in 0..8 {
            assert_eq!(
                word(&memory, REQUEST + slot * 128 + 104),
                HOT + (slot + 1) * 64
            );
        }
        // 只释放一个槽，下一次 service 必须选择剩余 FIFO 的头。
        put(&mut memory, REQUEST + 104, 0);
        harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
        assert_eq!(harness.started.len(), usize::from(!live.is_empty()));
        assert_eq!(
            word(&memory, REQUEST + 104),
            live.first().map_or(0, |(_, hot)| *hot)
        );
    }
}
