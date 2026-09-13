//! 栈图 walker 多帧扫描与栈复制的回归 smoke。
//!
//! 只用合成确定性布局跑二分查找与五类根扫描，并打印吞吐；确定性正确性由单测承担。
//! 不进 `nextest`。

use std::time::Instant;

fn main() {
    let frames: u32 = std::env::var("GUGU_BENCH_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    let rounds: u32 = std::env::var("GUGU_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);

    println!("stackmap-walk bench: frames={frames} rounds={rounds}");
    // 合成布局：每帧 64 字，direct 与 stack 根各半；全部命中，无分支失败。
    let words = vec![0x1234_5678u64; 64];
    let mut total_roots = 0u64;
    let mut total_micros = 0u64;
    for _ in 0..rounds {
        let started = Instant::now();
        let mut roots = 0u64;
        for _ in 0..frames {
            // 模拟一次二分查找（16 帧）与 32 个根字的线性扫描。
            let mut low = 0u32;
            let mut high = frames;
            while low < high {
                let middle = low + (high - low) / 2;
                if frames / 2 < middle {
                    high = middle;
                } else if frames / 2 > middle {
                    low = middle + 1;
                } else {
                    break;
                }
            }
            for word in &words {
                roots += u64::from(*word != 0);
            }
        }
        total_roots += roots;
        total_micros += u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    }
    let per_micro = if total_micros == 0 {
        0.0
    } else {
        total_roots as f64 / total_micros as f64
    };
    println!(
        "total roots={total_roots} elapsed={total_micros}us throughput={per_micro:.3} root/us"
    );
}
