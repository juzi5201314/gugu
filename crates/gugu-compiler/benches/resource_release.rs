//! ResourceRelease 消息的真实并发发布与 owner 消费吞吐测量。
//!
//! 这个 bench 不属于默认测试套件：资源 lease、受限 cleanup 与消息在计时前构造，
//! 计时区间覆盖 producer 线程发布、owner 消费与 slot return，
//! 校验已构造 release 的 exactly-once cleanup 与账本不变量。完整 release request 交错由
//! runtime::tests 承担。

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

    if producers == 0 || items == 0 || rounds == 0 {
        eprintln!("resource-release bench: producers/items/rounds 必须大于 0");
        std::process::exit(2);
    }
    println!(
        "resource-release bench: producers={producers} items/producer={items} rounds={rounds}"
    );
    let mut total_items = 0_u64;
    let mut total_micros = 0_u64;
    let mut clean = true;
    for round in 0..rounds {
        let report = ResourceReleaseHarness::new(producers, items).run();
        total_items += report.released_items;
        total_micros += report.elapsed_micros;
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
    let per_micro = total_items as f64 / total_micros.max(1) as f64;
    println!(
        "total items={total_items} elapsed={total_micros}us throughput={per_micro:.3} item/us"
    );
    if !clean {
        eprintln!("resource-release bench: exactly-once cleanup 或账本不变量被破坏");
        std::process::exit(1);
    }
}
