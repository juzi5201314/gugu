//! 真实 `Compilation` 消费者的 typed combining 冷路径吞吐 smoke。
//!
//! harness 用镜像计划的 combining 契约以 combined 模式配置平面，跑「发放 extent → grace
//! → pressure trim（平台 trim 一批 + buddy 合并一批）」，再跑一段热路径循环证明分配/本地
//! 返还/owner drain 一次都没有进入平面。只断言不变量并打印吞吐；确定性正确性由平面级与
//! 世界级单测承担。不进 `nextest`。

use std::time::Instant;

use gugu_compiler::ColdPathHarness;

fn main() {
    let owners: u32 = std::env::var("GUGU_BENCH_OWNERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    let extents: u32 = std::env::var("GUGU_BENCH_EXTENTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    println!("cold-combining bench: owners={owners} extents-per-owner={extents}");
    let started = Instant::now();
    let report = ColdPathHarness::new(owners, extents).run();
    let elapsed = started.elapsed();
    println!(
        "requests={} fast-path-claims={} parked={} merged={} executions={} rounds={} refills={}",
        report.requests,
        report.fast_path_claims,
        report.parked,
        report.merged,
        report.executions,
        report.rounds,
        report.refills,
    );
    println!(
        "trimmed-extents={} trimmed-bytes={} hot-path-entries={} invariants={}",
        report.trimmed_extents,
        report.trimmed_bytes,
        report.hot_path_entries,
        report.invariants_hold,
    );
    let micros = report.elapsed_micros.max(1) as f64;
    println!(
        "elapsed={}us ({:.3}s) throughput={:.3} trimmed-extents/us wall={:?}",
        report.elapsed_micros,
        elapsed.as_secs_f64(),
        report.trimmed_extents as f64 / micros,
        elapsed
    );
    if !report.invariants_hold {
        eprintln!("cold-combining bench: combining 冷路径不变量被破坏");
        std::process::exit(1);
    }
}
