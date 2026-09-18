//! 真实 `Compilation` 消费者的共享 payload 搬迁吞吐 smoke。
//!
//! 输入是编译器对共享环境夹具的真实产物：harness 用镜像计划契约配置 world，逐个 handle 走完
//! 分配、字段写入、搬迁、通知消费与 grace 结清，最后跑一轮真实 cycle 观察 sweep 与 block 搬迁。
//!
//! 只断言不变量并打印吞吐；确定性正确性由世界级单测承担。不进 `nextest`。

use std::time::Instant;

use gugu_compiler::SharedForwardHarness;

fn main() {
    let forwards: u32 = std::env::var("GUGU_BENCH_FORWARDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(512);
    let owners: u32 = std::env::var("GUGU_BENCH_OWNERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    println!("shared-forward bench: owners={owners} forwards={forwards}");
    let started = Instant::now();
    let report = SharedForwardHarness::new(owners, forwards).run();
    let elapsed = started.elapsed();
    println!(
        "forwards={} bytes={} freed={} empty-blocks={} invariants={}",
        report.forwards,
        report.forwarded_bytes,
        report.freed_bytes,
        report.empty_blocks,
        report.invariants_hold,
    );
    let micros = report.elapsed_micros.max(1) as f64;
    println!(
        "elapsed={}us ({:.3}s) throughput={:.3} forwards/us wall={:?}",
        report.elapsed_micros,
        elapsed.as_secs_f64(),
        report.forwards as f64 / micros,
        elapsed
    );
    if !report.invariants_hold {
        eprintln!("shared-forward bench: 共享搬迁不变量被破坏");
        std::process::exit(1);
    }
}
