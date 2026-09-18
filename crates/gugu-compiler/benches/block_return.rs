//! 真实 `Compilation` 消费者的 managed block return 吞吐 smoke。
//!
//! 输入是编译器对共享环境夹具的真实产物：harness 用镜像计划契约配置 world，多 owner
//! 分配 → 标死 → cycle → drain。只断言不变量并打印吞吐；确定性正确性由世界级单测承担。
//! 不进 `nextest`。

use std::time::Instant;

use gugu_compiler::BlockReturnHarness;

fn main() {
    let leaves: u32 = std::env::var("GUGU_BENCH_LEAVES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    let owners: u32 = std::env::var("GUGU_BENCH_OWNERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    println!("block-return bench: owners={owners} leaves={leaves}");
    let started = Instant::now();
    let report = BlockReturnHarness::new(owners, leaves).run();
    let elapsed = started.elapsed();
    println!(
        "leaves={} returned-bytes={} invariants={}",
        report.leaves, report.returned_bytes, report.invariants_hold,
    );
    let micros = report.elapsed_micros.max(1) as f64;
    println!(
        "elapsed={}us ({:.3}s) throughput={:.3} leaves/us wall={:?}",
        report.elapsed_micros,
        elapsed.as_secs_f64(),
        report.leaves as f64 / micros,
        elapsed
    );
    if !report.invariants_hold {
        eprintln!("block-return bench: managed block return 不变量被破坏");
        std::process::exit(1);
    }
}
