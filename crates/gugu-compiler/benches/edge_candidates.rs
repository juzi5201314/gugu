//! 真实 `Compilation` 消费者的边与候选回收吞吐 smoke。
//!
//! 输入是编译器对 EdgeNode 夹具的真实产物：harness 用镜像计划的 LocalHeap/GC metadata/edge
//! 契约配置 world，按真实类型表建立跨 owner 引用，逐轮跑发布、标记与候选判定。
//!
//! 只断言不变量并打印吞吐；确定性正确性由世界级单测承担。不进 `nextest`。

use std::time::Instant;

use gugu_compiler::EdgeCandidateHarness;

fn main() {
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    let nodes: u32 = std::env::var("GUGU_BENCH_NODES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(256);
    println!("edge-candidates bench: rounds={rounds} nodes-per-owner={nodes}");
    let started = Instant::now();
    let report = EdgeCandidateHarness::new(rounds, nodes).run();
    let elapsed = started.elapsed();
    let stores = report.cross_owner_stores.max(1) as f64;
    println!(
        "types={} stores={} deltas={} applied={} tickets={} work-units={} released={} invariants={}",
        report.managed_types,
        report.cross_owner_stores,
        report.edge_deltas,
        report.applied_edges,
        report.mark_tickets,
        report.candidate_work_units,
        report.blocks_released,
        report.invariants_hold,
    );
    let micros = report.elapsed_micros.max(1) as f64;
    println!(
        "elapsed={}us ({:.3}s) throughput={:.3} stores/us wall={:?}",
        report.elapsed_micros,
        elapsed.as_secs_f64(),
        stores / micros,
        elapsed
    );
    if !report.invariants_hold {
        eprintln!("edge-candidates bench: 边/候选不变量被破坏");
        std::process::exit(1);
    }
}
