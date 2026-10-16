//! 流水线基线：量化"每 session 重建内建世界"的固定成本。
//!
//! 三个量级依次是：空 package 的流水线地板、`fn main() {}` 这类最小用户源（此时内建
//! std/runtime 派生占绝对多数）、以及一个真正 import 了 `std.collections`/`std.io` 的
//! 小程序。每个用例都用新建的 `Compiler`，避免 warm session 掩盖冷启动成本。
//!
//! 不进默认测试套件；`cargo bench --profile test --bench pipeline_baseline` 手工验收。

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

/// 单个用例的重复次数；只报告最后一次，前几次用于摊掉首次缺页与惰性初始化。
const REPEATS: usize = 2;

fn hex_prefix(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(16);
    for byte in &bytes[..8] {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

fn measure(label: &str, request: CompileRequest) -> f64 {
    let mut elapsed_ms = 0.0;
    for _ in 0..REPEATS {
        let start = Instant::now();
        let compilation = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(request.clone());
        elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let fingerprint = compilation
            .lir_fingerprint()
            .map_or_else(|| "none".to_string(), |bytes| hex_prefix(&bytes));
        if !compilation.is_success() {
            println!("{label:<24} FAILED  lir={fingerprint}");
            eprintln!("{:?}", compilation.diagnostics().items());
            std::process::exit(1);
        }
        println!(
            "{label:<24} {elapsed_ms:>8.1} ms  lir={fingerprint}  image={}",
            compilation.image_plan().is_some()
        );
    }
    elapsed_ms
}

fn main() {
    println!(
        "target = {:?}, repeats = {REPEATS}（只报告最后一次）",
        TargetName::X86_64Linux
    );
    let floor = measure(
        "pipeline-floor",
        CompileRequest::empty_package(TargetName::X86_64Linux),
    );
    let minimal = measure(
        "minimal-main",
        CompileRequest::single_file("main.gg", MINIMAL, TargetName::X86_64Linux),
    );
    let library = measure(
        "std-collections-io",
        CompileRequest::single_file("main.gg", LIBRARY_USER, TargetName::X86_64Linux),
    );
    println!(
        "builtin-derivation ≈ {:.1} ms（最小用户源相对地板）",
        minimal - floor
    );
    println!("library-user = {library:.1} ms");
}
