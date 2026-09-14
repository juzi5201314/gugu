//! hybrid write barrier 的 card-mark 记账、dedup 与 flush 吞吐 smoke。
//!
//! 只断言守恒与不变量并打印吞吐；确定性正确性由单测承担。不进 `nextest`。

use std::time::Instant;

use gugu_compiler::CardMarkHarness;

fn main() {
    let processors: u32 = std::env::var("GUGU_BENCH_PROCESSORS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    let iterations: u32 = std::env::var("GUGU_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4096);
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    println!(
        "barrier-card-mark bench: processors={processors} iterations={iterations} rounds={rounds}"
    );
    let mut clean = true;
    let mut total_writes = 0_u64;
    let mut total_micros = 0_u64;
    for round in 0..rounds {
        let started = Instant::now();
        let report = CardMarkHarness::new(processors, iterations).run();
        let elapsed = started.elapsed();
        total_writes += report.card_marks;
        total_micros += u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        clean &= report.invariants_hold;
        println!(
            "round {round}: marks={} reuses={} batches={} dirty={} invariants={} elapsed={}us",
            report.card_marks,
            report.slot_reuses,
            report.batches,
            report.dirty_cards,
            report.invariants_hold,
            report.elapsed_micros
        );
    }
    let per_micro = if total_micros == 0 {
        0.0
    } else {
        total_writes as f64 / total_micros as f64
    };
    println!(
        "total marks={total_writes} elapsed={total_micros}us throughput={per_micro:.3} mark/us"
    );
    if !clean {
        eprintln!("barrier-card-mark bench: remembered-set 不变量被破坏");
        std::process::exit(1);
    }
}
