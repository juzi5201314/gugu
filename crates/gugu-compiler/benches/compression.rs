//! checked pointer compression 的 harness 与 decode 吞吐 smoke。
//!
//! 输入是真实 `Compilation` 产物：harness 以显式 cage profile 配置 world，验证 island 化
//! managed arena、压缩根 checked 解码、minor 重编码与 FFI 闸门；随后在真实预留的 cage 上
//! 测量 `scan_roots` 压缩槽解码吞吐。只断言不变量并打印吞吐；确定性正确性由单测承担。
//! 不进 `nextest`。

use std::time::Instant;

use gugu_compiler::CompressionHarness;

fn main() {
    let objects: u32 = std::env::var("GUGU_BENCH_OBJECTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    let owners: u32 = std::env::var("GUGU_BENCH_OWNERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    println!("compression bench: owners={owners} objects={objects}");
    let started = Instant::now();
    let report = CompressionHarness::new(owners, objects).run();
    let elapsed = started.elapsed();
    println!(
        "objects={} decodes={} rejections={} foreign-saves={} islands={} invariants={}",
        report.objects,
        report.decodes,
        report.rejections,
        report.foreign_saves,
        report.islands,
        report.invariants_hold,
    );
    let decode_micros = report.decode_micros.max(1) as f64;
    println!(
        "decode-slots={} decode-words={} decode-us={} decode/us={:.3}",
        report.decode_slots,
        report.decode_words,
        report.decode_micros,
        report.decode_words as f64 / decode_micros,
    );
    println!(
        "elapsed={}us ({:.3}s) wall={:?}",
        report.elapsed_micros,
        elapsed.as_secs_f64(),
        elapsed
    );
    if !report.invariants_hold {
        eprintln!("compression bench: checked pointer compression 不变量被破坏");
        std::process::exit(1);
    }
}
