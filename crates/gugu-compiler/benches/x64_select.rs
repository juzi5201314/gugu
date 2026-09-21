//! 选指冒烟：编译含分支的 main，确认片段非空、入口 mangled、rel8 可计数。
//!
//! 不进默认测试套件；`cargo bench --bench x64_select` 手工验收。

use gugu_compiler::{CompileRequest, Compiler, TargetName};

fn main() {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() {\n let n = 7\n if n > 3 { _ = n * 2 } else { _ = n + 1 }\n }",
        TargetName::X86_64Linux,
    ));
    if !compilation.is_success() {
        eprintln!("{:?}", compilation.diagnostics().items());
        std::process::exit(1);
    }
    let plan = compilation.image_plan().expect("镜像计划");
    if plan.x64_encoded_bytes() == 0 {
        eprintln!("片段字节数为 0");
        std::process::exit(1);
    }
    let symbol = plan.x64_entry_symbol();
    if !symbol.starts_with("__gugu_fn_") || symbol.len() != 10 + 64 {
        eprintln!("入口符号非法: {symbol}");
        std::process::exit(1);
    }
    if plan.x64_hot_block_count() == 0 {
        eprintln!("热块数为 0");
        std::process::exit(1);
    }
    let dump = compilation.dump_x64().expect("dump");
    if !dump.contains("x64 schema=5") {
        eprintln!("dump schema 不是 4");
        std::process::exit(1);
    }
    if !dump.contains("x64-frame ") || !dump.contains("x64-stats ") {
        eprintln!("dump 缺少 frame 或 stats 段");
        std::process::exit(1);
    }
    if plan.x64_allocated_values() == 0 {
        eprintln!("分配器没有处理任何虚拟值");
        std::process::exit(1);
    }
    println!(
        "x64_select bytes={} rel8={} hot={} cold={} frame={} slots={} values={} symbol={}",
        plan.x64_encoded_bytes(),
        plan.x64_rel8_count(),
        plan.x64_hot_block_count(),
        plan.x64_cold_block_count(),
        plan.x64_frame_size_max(),
        plan.x64_spill_slot_count(),
        plan.x64_allocated_values(),
        symbol
    );
}
