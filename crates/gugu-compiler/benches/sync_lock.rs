//! std.sync 互斥锁与原子状态机多线程争用 smoke。
//!
//! 只断言守恒与不变量并打印吞吐；确定性正确性由单测承担。
//! 不进 `nextest`。

use std::time::Instant;

use gugu_compiler::SyncLockHarness;

fn main() {
    let threads: u32 = std::env::var("GUGU_BENCH_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    let iterations: u32 = std::env::var("GUGU_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    println!("sync-lock bench: threads={threads} iterations/thread={iterations} rounds={rounds}");
    let mut clean = true;
    let mut total_ops = 0_u64;
    let mut total_micros = 0_u64;
    for round in 0..rounds {
        let started = Instant::now();
        let report = SyncLockHarness::new(threads, iterations).run();
        let elapsed = started.elapsed();
        total_ops += report.total_operations;
        total_micros += u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        clean &= report.invariants_hold;
        println!(
            "round {round}: ops={} counter={} invariants={} elapsed={}us",
            report.total_operations,
            report.final_counter,
            report.invariants_hold,
            report.elapsed_micros
        );
    }
    let per_micro = if total_micros == 0 {
        0.0
    } else {
        total_ops as f64 / total_micros as f64
    };
    println!("total ops={total_ops} elapsed={total_micros}us throughput={per_micro:.3} op/us");
    if !clean {
        eprintln!("sync-lock bench: 同步与原子不变量被破坏");
        std::process::exit(1);
    }
}
