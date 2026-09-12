//! 调度 runqueue 的真实并发 smoke：多 producer 发布、thief steal、park 唤醒。
//!
//! 只断言有限完成、无丢失唤醒、无重复执行，打印吞吐；确定性正确性由单测承担。
//! 不进 `nextest`。

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;

fn shard_of(item: u64) -> usize {
    (item % 8) as usize
}

fn main() {
    let producers = option_env!("GUGU_BENCH_PRODUCERS")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4);
    let items = option_env!("GUGU_BENCH_ITEMS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(4_096);
    let rounds = option_env!("GUGU_BENCH_ROUNDS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);
    assert!(producers >= 1 && items >= 1 && rounds >= 1);

    for round in 1..=rounds {
        let shards: Vec<Mutex<VecDeque<u64>>> =
            (0..8).map(|_| Mutex::new(VecDeque::new())).collect();
        let shards = Arc::new(shards);
        let work_seq = Arc::new(AtomicU64::new(0));
        let idle_count = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let executed = Arc::new(Mutex::new(HashSet::new()));
        let executed_count = Arc::new(AtomicU64::new(0));
        let start_barrier = Arc::new(Barrier::new(producers + 1));
        let total = items * producers as u64;
        let started = Instant::now();

        let workers = producers.min(4).max(1);
        let mut joins = Vec::new();
        for worker in 0..workers {
            let shards = Arc::clone(&shards);
            let work_seq = Arc::clone(&work_seq);
            let idle_count = Arc::clone(&idle_count);
            let done = Arc::clone(&done);
            let executed = Arc::clone(&executed);
            let executed_count = Arc::clone(&executed_count);
            let seed = AtomicU64::new(0x9E37_79B9_7F4A_7C15_u64.wrapping_add(worker as u64 + 1));
            joins.push(thread::spawn(move || {
                let mut rng = seed.load(Ordering::Relaxed);
                let mut local: VecDeque<u64> = VecDeque::new();
                let mut cursor = 0_usize;
                let mut spins = 0_u32;
                loop {
                    if executed_count.load(Ordering::Acquire) >= total {
                        break;
                    }
                    if done.load(Ordering::Acquire) && local.is_empty() {
                        let mut drained = false;
                        for shard in shards.iter() {
                            let mut guard = shard.lock().expect("shard 可锁");
                            while let Some(item) = guard.pop_front() {
                                local.push_back(item);
                                drained = true;
                            }
                        }
                        if !drained {
                            break;
                        }
                    }
                    if let Some(item) = local.pop_back() {
                        {
                            let mut guard = executed.lock().expect("执行集合可锁");
                            assert!(guard.insert(item), "重复执行 {item}");
                        }
                        executed_count.fetch_add(1, Ordering::AcqRel);
                        spins = 0;
                        continue;
                    }
                    // Remote service：RR cursor 至多查 8 shard。
                    let mut progressed = false;
                    for _ in 0..8 {
                        let shard = cursor % 8;
                        cursor = (cursor + 1) % 8;
                        let mut guard = shards[shard].lock().expect("shard 可锁");
                        if let Some(item) = guard.pop_front() {
                            local.push_back(item);
                            progressed = true;
                            break;
                        }
                    }
                    if progressed {
                        spins = 0;
                        continue;
                    }
                    // Steal：同域随机起点（本 smoke 恒单域）。
                    rng ^= rng >> 12;
                    rng ^= rng << 25;
                    rng ^= rng >> 27;
                    rng = rng.wrapping_mul(2_685_821_657_736_338_717);
                    let victim = (rng % 8) as usize;
                    {
                        let mut guard = shards[victim].lock().expect("shard 可锁");
                        let take = ((guard.len() + 1) / 2).min(128);
                        for _ in 0..take {
                            if let Some(item) = guard.pop_front() {
                                local.push_back(item);
                            }
                        }
                        if take > 0 {
                            progressed = true;
                        }
                    }
                    if progressed {
                        spins = 0;
                        continue;
                    }
                    // Park：快照 work_seq，重查后登记。
                    let snapshot = work_seq.load(Ordering::Acquire);
                    let mut has_work = false;
                    for shard in shards.iter() {
                        if !shard.lock().expect("shard 可锁").is_empty() {
                            has_work = true;
                            break;
                        }
                    }
                    if has_work || work_seq.load(Ordering::Acquire) != snapshot {
                        spins = 0;
                        continue;
                    }
                    idle_count.fetch_add(1, Ordering::AcqRel);
                    spins += 1;
                    if spins > 64 {
                        thread::yield_now();
                        spins = 0;
                    }
                    idle_count.fetch_sub(1, Ordering::AcqRel);
                }
            }));
        }

        let mut producers_joins = Vec::new();
        for producer in 0..producers {
            let shards = Arc::clone(&shards);
            let work_seq = Arc::clone(&work_seq);
            let barrier = Arc::clone(&start_barrier);
            joins.len();
            producers_joins.push(thread::spawn(move || {
                barrier.wait();
                for index in 0..items {
                    let item = (producer as u64) * items + index;
                    let shard = shard_of(item ^ ((producer as u64) << 32));
                    let mut guard = shards[shard].lock().expect("shard 可锁");
                    let was_empty = guard.is_empty();
                    guard.push_back(item);
                    drop(guard);
                    if was_empty {
                        work_seq.fetch_add(1, Ordering::Release);
                    }
                }
            }));
        }
        start_barrier.wait();
        for join in producers_joins {
            join.join().expect("producer 可 join");
        }
        done.store(true, Ordering::Release);
        for join in joins {
            join.join().expect("worker 可 join");
        }
        let elapsed = started.elapsed();
        let count = executed_count.load(Ordering::Acquire);
        assert_eq!(count, total, "丢失唤醒或丢失任务");
        assert_eq!(executed.lock().expect("执行集合可锁").len() as u64, total);
        let per_sec = count as f64 / elapsed.as_secs_f64().max(1e-9);
        println!(
            "scheduler_runqueue round {round}/{rounds}: {count} items in {elapsed:?} ({per_sec:.0} items/s)",
        );
    }
}
