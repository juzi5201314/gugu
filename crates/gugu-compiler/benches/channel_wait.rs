//! channel ping-pong 与 select 提交的真实并发 smoke。
//!
//! 只断言 exactly-once 与账本守恒并打印吞吐；确定性正确性由单测承担。
//! 不进 `nextest`。

use std::time::Instant;

use gugu_compiler::ChannelWaitHarness;

fn main() {
    let producers: u32 = std::env::var("GUGU_BENCH_PRODUCERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    let items: u32 = std::env::var("GUGU_BENCH_ITEMS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    println!("channel-wait bench: producers={producers} items/producer={items} rounds={rounds}");
    let mut clean = true;
    let mut total_items = 0_u64;
    let mut total_micros = 0_u64;
    for round in 0..rounds {
        let started = Instant::now();
        let report = ChannelWaitHarness::new(producers, items).run();
        let elapsed = started.elapsed();
        total_items += report.received;
        total_micros += u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        clean &= report.invariants_hold;
        println!(
            "round {round}: sent={} received={} unique={} select={} invariants={} elapsed={}us",
            report.sent,
            report.received,
            report.unique,
            report.select_commits,
            report.invariants_hold,
            report.elapsed_micros
        );
    }
    let per_micro = if total_micros == 0 {
        0.0
    } else {
        total_items as f64 / total_micros as f64
    };
    println!(
        "total received={total_items} elapsed={total_micros}us throughput={per_micro:.3} item/us"
    );
    if !clean {
        eprintln!("channel-wait bench: exactly-once 或账本不变量被破坏");
        std::process::exit(1);
    }
}
