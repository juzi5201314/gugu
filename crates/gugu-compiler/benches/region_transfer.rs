//! TurnRegion 生命周期与 `RegionTransfer` 往返的记账、门禁与消息吞吐 smoke。
//!
//! 只断言守恒与不变量并打印吞吐；确定性正确性由单测承担。不进 `nextest`。

use std::time::Instant;

use gugu_compiler::RegionTransferHarness;

fn main() {
    let regions: u32 = std::env::var("GUGU_BENCH_REGIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4096);
    let bumps: u32 = std::env::var("GUGU_BENCH_BUMPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    println!("region-transfer bench: regions={regions} bumps={bumps} rounds={rounds}");
    let mut clean = true;
    let mut total_regions = 0_u64;
    let mut total_transfers = 0_u64;
    let mut total_micros = 0_u64;
    for round in 0..rounds {
        let started = Instant::now();
        let report = RegionTransferHarness::new(regions, bumps).run();
        let elapsed = started.elapsed();
        total_regions += report.regions;
        total_transfers += report.transfers;
        total_micros += u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        clean &= report.invariants_hold;
        println!(
            "round {round}: regions={} bumps={} resets={} promotions={} transfers={} adopted={} invariants={} elapsed={}us",
            report.regions,
            report.bumps,
            report.resets,
            report.promotions,
            report.transfers,
            report.adopted,
            report.invariants_hold,
            report.elapsed_micros
        );
    }
    let per_micro = if total_micros == 0 {
        0.0
    } else {
        total_regions as f64 / total_micros as f64
    };
    println!(
        "total regions={total_regions} transfers={total_transfers} elapsed={total_micros}us throughput={per_micro:.3} region/us"
    );
    if !clean {
        eprintln!("region-transfer bench: region 生命周期不变量被破坏");
        std::process::exit(1);
    }
}
