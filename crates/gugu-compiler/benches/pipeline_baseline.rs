//! 流水线基线：量化"每 session 重建内建世界"的固定成本。
//!
//! 三个量级依次是：空 package 的流水线地板、`fn main() {}` 这类最小用户源（此时内建
//! std/runtime 派生占绝对多数）、以及一个真正 import 了 `std.collections`/`std.io` 的
//! 小程序。每轮都新建 `Compiler`，仅共享生产路径允许跨会话复用的内建数据。
//!
//! 不进默认测试套件；`cargo bench --profile test --bench pipeline_baseline` 手工验收。
//! `GUGU_PIPELINE_CASE` 可选 `pipeline-floor`、`minimal-main`、`std-collections-io`，
//! 单选后在新进程运行，首轮包含进程级惰性初始化，后续轮次测量热共享基底的新会话。
//! `GUGU_PIPELINE_REPEATS` 控制轮次；默认 2。分配采样单独用 heaptrack，避免污染正常计时。

use std::time::Instant;

use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

/// 最小用户源：内建世界派生是这里唯一的可观成本。
const MINIMAL: &str = "fn main() {}\n";

/// 触达 `std.collections` 与 `std.io` 的小程序：外加少量内建实例可达。
const LIBRARY_USER: &str = "\
use std.collections.{BTreeMap, HashMap}
use std.io
use std.path

fn probe(map: HashMap[int, string], tree: BTreeMap[string, int]) Option[int] {
    let key = 1
    tree.for_each_ref(fn(k: &string, v: &int) {})
    map.with_ref(&key, fn(v: &string) int = 0)
}

fn main() {}
";

fn hex_prefix(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(16);
    for byte in &bytes[..8] {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

fn measure(
    label: &str,
    request: CompileRequest,
    input_bytes: usize,
    repeats: usize,
    process_cold: &mut bool,
) {
    let mut expected = None;
    for round in 1..=repeats {
        let start = Instant::now();
        #[expect(
            clippy::disallowed_methods,
            reason = "pipeline bench 测量用户源码完整编译路径"
        )]
        let compilation = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(request.clone());
        let elapsed_us = start.elapsed().as_micros();
        let fingerprint = compilation
            .lir_fingerprint()
            .map_or_else(|| "none".to_string(), |bytes| hex_prefix(&bytes));
        if !compilation.is_success() {
            println!("{label:<24} FAILED  lir={fingerprint}");
            eprintln!("{:?}", compilation.diagnostics().items());
            std::process::exit(1);
        }
        let image_bytes = compilation
            .image_plan()
            .map_or(0, |plan| plan.linux_image().len());
        let identity = (compilation.lir_fingerprint(), image_bytes);
        if let Some(previous) = expected.replace(identity) {
            assert_eq!(previous, identity, "相同输入的冷、热会话必须产生相同结果");
        }
        let state = if std::mem::take(process_cold) {
            "process-cold"
        } else {
            "shared-process"
        };
        println!(
            "{label:<24} round={round}/{repeats} {state:<14} {elapsed_us:>8} µs \
             input={input_bytes} B lir={fingerprint} image={image_bytes} B"
        );
    }
}

fn main() {
    let selected = std::env::var("GUGU_PIPELINE_CASE").ok();
    let repeats = std::env::var("GUGU_PIPELINE_REPEATS")
        .map_or(2, |value| value.parse().expect("轮次必须是正整数"));
    assert!(repeats > 0, "轮次必须是正整数");
    let cases = [
        ("pipeline-floor", None),
        ("minimal-main", Some(MINIMAL)),
        ("std-collections-io", Some(LIBRARY_USER)),
    ];
    assert!(
        selected
            .as_ref()
            .is_none_or(|selected| cases.iter().any(|(label, _)| label == selected)),
        "未知 GUGU_PIPELINE_CASE"
    );
    println!(
        "target = {:?}, repeats = {repeats}",
        TargetName::X86_64Linux
    );
    let mut process_cold = true;
    for (label, source) in cases {
        if selected.as_ref().is_some_and(|selected| selected != label) {
            continue;
        }
        let request = source.map_or_else(
            || CompileRequest::empty_package(TargetName::X86_64Linux),
            |source| CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux),
        );
        measure(
            label,
            request,
            source.map_or(0, str::len),
            repeats,
            &mut process_cold,
        );
    }
}
