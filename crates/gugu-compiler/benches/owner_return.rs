//! owner-directed return 的真实并发冒烟与吞吐测量。
//!
//! 这个 bench 不属于默认测试套件：它使用真实 producer 线程压 MPSC batch 发布，校验最终
//! exactly-once 计数与账本不变量，并打印吞吐。正确性验证由 `runtime::tests` 的确定性
//! 交错驱动承担，这里只回答“同一份实现在真实并发下是否仍然守恒”。

use std::time::Instant;

use gugu_compiler::OwnerReturnHarness;

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
        "owner-return bench: producers={producers} items/producer={items} rounds={rounds} classes=?"
    );
    let mut total_items = 0_u64;
    let mut total_micros = 0_u64;
    let mut clean = true;
    for round in 0..rounds {
        let started = Instant::now();
        let report = OwnerReturnHarness::new(producers, items).run();
        let elapsed = started.elapsed();
        total_items += report.published_items;
        total_micros += u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        clean &= report.invariants_hold;
        println!(
            "round {round}: published={} consumed={} phantom-nulls={} classes={} invariants={} elapsed={}us",
            report.published_items,
            report.consumed_items,
            report.phantom_nulls,
            report.size_classes,
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
        eprintln!("owner-return bench: exactly-once 或账本不变量被破坏");
        std::process::exit(1);
    }
}
