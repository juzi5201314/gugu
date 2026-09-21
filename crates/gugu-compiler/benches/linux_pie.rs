//! 内核加载写出的 Linux static PIE，确认 rt0 能进入入口并退出。
//!
//! 不进默认测试套件。`cargo bench -p gugu-compiler --bench linux_pie --profile dev`。

use gugu_compiler::{CompileRequest, Compiler, TargetName};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn main() {
    let programs = [
        ("hello", "fn main() {}", true),
        ("panic", "fn main() { panic(\"boom\") }", false),
        (
            "channel",
            "fn main() { let channel = chan[int](0)\n _ = channel }",
            false,
        ),
        (
            "gc",
            "fn main() { let value = 1\n let closure = fn() int { return value }\n _ = closure() }",
            false,
        ),
    ];
    for (name, source, expect_zero) in programs {
        launch(name, source, expect_zero);
    }
}

fn launch(name: &str, source: &str, expect_zero: bool) {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    if !compilation.is_success() {
        eprintln!("{name} 编译失败: {:?}", compilation.diagnostics().items());
        std::process::exit(1);
    }
    let plan = compilation.image_plan().expect("镜像计划");
    if plan.linux_image_kind() != "static-pie" || !plan.linux_interpreter().is_empty() {
        eprintln!("{name} 不是无解释器的 static PIE");
        std::process::exit(1);
    }
    let path = std::env::temp_dir().join(format!("gugu-pie-{name}"));
    fs::write(&path, plan.linux_image()).expect("写出镜像");
    let mut permissions = fs::metadata(&path).expect("元数据").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("权限");
    let status = Command::new(&path).status().unwrap_or_else(|error| {
        eprintln!("{name} 启动失败: {error}");
        std::process::exit(1);
    });
    println!("{name} status={status}");
    if status.code().is_none() {
        eprintln!("{name} 被信号终止");
        std::process::exit(1);
    }
    if expect_zero && !status.success() {
        eprintln!("{name} 未以 0 退出");
        std::process::exit(1);
    }
}
