//! ResourceCell release burst 的真实并发冒烟与吞吐测量。
//!
//! 这个 bench 不属于默认测试套件：它在真实 producer 线程上发布 ResourceRelease 消息，
//! 由单 owner 线程消费并驱动受限 cleanup 与 slot 回收，校验 exactly-once cleanup、账本
//! 不变量与 pending release 排空。正确性由 runtime::tests 的确定性交错驱动承担。

use std::time::Instant;

use gugu_compiler::ResourceReleaseHarness;

fn main() {
    let producers: u32 = std::env::var("GUGU_BENCH_PRODUCERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    let items: u32 = std::env::var("GUGU_BENCH_ITEMS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4096);
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);

    println!(
        "resource-release bench: producers={producers} items/producer={items} rounds={rounds}"
    );
    let mut total_items = 0_u64;
    let mut total_micros = 0_u64;
    let mut clean = true;
    for round in 0..rounds {
        let started = Instant::now();
        let report = ResourceReleaseHarness::new(producers, items).run();
        let elapsed = started.elapsed();
        total_items += report.released_items;
        total_micros += u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        clean &= report.invariants_hold;
        println!(
            "round {round}: released={} consumed={} cleanups={} phantom-nulls={} invariants={} elapsed={}us",
            report.released_items,
            report.consumed_items,
            report.cleanups,
            report.phantom_nulls,
            report.invariants_hold,
            report.elapsed_micros
        );
    }
    let per_micro = if total_micros == 0 {
        f64::from(u32::MAX)
    } else {
        total_items as f64 / total_micros as f64
    };
    println!(
        "total items={total_items} elapsed={total_micros}us throughput={per_micro:.3} item/us"
    );
    if !clean {
        eprintln!("resource-release bench: exactly-once cleanup 或账本不变量被破坏");
        std::process::exit(1);
    }
}
