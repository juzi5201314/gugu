//! 手工网络协议镜像入口；默认测试只检查字节，不运行进程或网络。
use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};
use std::{env, fs};

fn main() {
    let input = env::args().nth(1).expect("固定 fixture 名称");
    let text = match input.as_str() {
        "bind" => include_str!("../tests/fixtures/net_bind.gg"),
        "accept" => include_str!("../tests/fixtures/net_accept.gg"),
        "resolve" => include_str!("../tests/fixtures/net_resolve.gg"),
        _ => panic!("未知网络 fixture"),
    };
    let target = if env::args().nth(2).as_deref() == Some("windows") {
        TargetName::X86_64Windows
    } else {
        TargetName::X86_64Linux
    };
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS)
        .compile(CompileRequest::single_file("main.gg", text, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let plan = output.image_plan().expect("网络输入形成实际镜像");
    let suffix = if target == TargetName::X86_64Windows {
        ".exe"
    } else {
        ""
    };
    let path = format!("/tmp/gugu-net-{input}{suffix}");
    fs::write(
        &path,
        if target == TargetName::X86_64Windows {
            plan.windows_image()
        } else {
            plan.linux_image()
        },
    )
    .expect("写出网络镜像");
    if env::var_os("GUGU_NET_DUMP").is_some() {
        fs::write(
            format!("/tmp/gugu-net-{input}-lir.txt"),
            output.dump_lir().unwrap(),
        )
        .unwrap();
        fs::write(
            format!("/tmp/gugu-net-{input}-x64.txt"),
            output.dump_x64().unwrap(),
        )
        .unwrap();
    }
    println!("{path}");
}
