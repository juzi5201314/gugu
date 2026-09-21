//! Linux ELF64 static PIE 的真实启动验收。
//!
//! 编译 hello、panic、捕获闭包与通道程序，写出镜像后由内核装入并执行。
//! 退出码必须来自 rt0、`panic` 或通道状态机，不能由一条立即 `exit` 冒充。
//! 该二进制不进默认测试套件，供 `cargo bench --bench elf_pie` 使用。

use gugu_compiler::{CompileRequest, Compiler, TargetName};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

struct Program {
    name: &'static str,
    source: &'static str,
    code: i32,
    stdout: Option<&'static str>,
}

fn main() {
    let programs = [
        Program {
            name: "hello",
            source: "fn main() {\n let value = 1\n _ = value\n }",
            code: 0,
            stdout: None,
        },
        Program {
            name: "panic",
            source: "fn main() { panic(\"bug\") }",
            code: 101,
            stdout: Some("bug"),
        },
        Program {
            name: "gc",
            source: "fn main() {\n let value = 7\n let closure = fn() int { return value }\n _ = closure()\n }",
            code: 0,
            stdout: None,
        },
        Program {
            name: "channel",
            source: "fn main() {\n let channel = chan[int](1)\n channel.send(1)\n let received = channel.recv()\n _ = received\n }",
            code: 0,
            stdout: None,
        },
    ];
    let mut failed = false;
    for program in programs {
        if !run(&program) {
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}

fn run(program: &Program) -> bool {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        program.source,
        TargetName::X86_64Linux,
    ));
    if !compilation.is_success() {
        eprintln!(
            "{name}: 编译失败 {:?}",
            compilation.diagnostics().items(),
            name = program.name
        );
        return false;
    }
    let plan = compilation.image_plan().expect("镜像计划");
    let bytes = plan.elf_image();
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
        eprintln!("{}: 没有 ELF 镜像", program.name);
        return false;
    }
    let path = format!("/tmp/gugu-elf-{}", program.name);
    if let Err(error) = fs::write(&path, bytes) {
        eprintln!("{}: 写镜像失败 {error}", program.name);
        return false;
    }
    let mut permissions = fs::metadata(&path).expect("元数据").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("可执行位");
    let output = match Command::new(&path).output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!("{}: 启动失败 {error}", program.name);
            return false;
        }
    };
    let code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ok = code == Some(program.code) && program.stdout.is_none_or(|text| stdout.contains(text));
    if ok {
        println!(
            "{name}: exit={code} bytes={len}",
            name = program.name,
            code = program.code,
            len = bytes.len()
        );
    } else {
        eprintln!(
            "{name}: exit={code:?} 期望 {expect} stdout={stdout:?} stderr={stderr:?}",
            name = program.name,
            expect = program.code
        );
    }
    ok
}
