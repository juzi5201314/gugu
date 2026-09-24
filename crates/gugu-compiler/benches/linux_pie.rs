//! 内核加载写出的 Linux static PIE，确认 rt0 能进入入口并退出。
//!
//! 不进默认测试套件。`cargo bench -p gugu-compiler --bench linux_pie --profile dev`。

use gugu_compiler::{CompileRequest, Compiler, TargetName};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn main() {
    let programs = [
        ("hello", "fn main() {}", 0, "", ""),
        (
            "panic",
            "fn main() {\n    panic(\"boom\")\n}\n",
            1,
            "",
            "{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"panic\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":\"boom\",\"location\":{\"file\":\"main.gg\",\"line\":2,\"column\":5},\"backtrace\":[],\"exit_code\":1}\n{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"termination\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":null,\"location\":null,\"backtrace\":[],\"exit_code\":1}\n",
        ),
        (
            "grow",
            "fn depth(n: uint) uint { let frame: [uint; 16] = [n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n]\n if n == 0 { return frame[0] }\n frame[int(n % uint(16))] + depth(n - uint(1)) }\nfn main() { if depth(uint(5000)) != uint(12502500) { panic(\"wrong result\") } }\n",
            0,
            "",
            "",
        ),
    ];
    for (name, source, code, stdout, stderr) in programs {
        if std::env::var("GUGU_PIE_CASE").is_ok_and(|filter| filter != name) {
            continue;
        }
        launch(name, source, code, stdout, stderr);
    }
}

fn launch(name: &str, source: &str, code: i32, stdout: &str, stderr: &str) {
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
    let output = Command::new(&path).output().unwrap_or_else(|error| {
        eprintln!("{name} 启动失败: {error}");
        std::process::exit(1);
    });
    let actual_code = output.status.code();
    println!("{name} status={:?}", output.status);
    if actual_code != Some(code) {
        eprintln!(
            "{name} 退出码不符：预期 {code}，实际 {actual_code:?}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        std::process::exit(1);
    }
    if output.stdout != stdout.as_bytes() || output.stderr != stderr.as_bytes() {
        eprintln!(
            "{name} 输出不符：stdout={:?}，stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        std::process::exit(1);
    }
    if name == "grow" {
        corrupt_stackmap(plan.linux_image());
    }
}

fn corrupt_stackmap(image: &[u8]) {
    let mut image = image.to_vec();
    let header = b"GUGUSM01\x02\x00\x08\x01";
    let at = image
        .windows(header.len())
        .position(|bytes| bytes == header)
        .expect("栈图节头");
    image[at] ^= 1;
    let path = std::env::temp_dir().join("gugu-pie-invalid-stackmap");
    fs::write(&path, image).expect("写出损坏栈图镜像");
    let mut permissions = fs::metadata(&path).expect("元数据").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("权限");
    let output = Command::new(&path).output().expect("加载损坏栈图镜像");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        output.stderr,
        b"runtime fatal: invalid stack map or allocation\n"
    );
    assert!(output.stdout.is_empty());
}
