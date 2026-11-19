//! 手工文件系统镜像验证入口；真实进程与文件操作不进入默认测试套件。
use gugu_compiler::{CompileRequest, Compiler, NativeImport, NativeLink, TargetName};
use std::{env, fs, path::PathBuf};

fn main() {
    let source = PathBuf::from(env::args_os().nth(1).expect("源码路径"));
    let target = match env::args().nth(2).as_deref() {
        Some("windows") => TargetName::X86_64Windows,
        Some("macos") => TargetName::X86_64Macos,
        _ => TargetName::X86_64Linux,
    };
    let text = fs::read_to_string(source).expect("读取手工输入");
    let imports = env::args()
        .skip(3)
        .map(|entry| {
            let (library, symbol) = entry.split_once(':').expect("导入格式为 library:symbol");
            NativeImport::function(library, symbol)
        })
        .collect();
    let compilation = Compiler::with_artifacts(&gugu_compiler::EMBEDDED_ARTIFACTS).compile(
        CompileRequest::single_file("main.gg", text, target).with_native_link(NativeLink {
            imports,
            interpreter: None,
        }),
    );
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    if env::var_os("GUGU_FS_DUMP").is_some() {
        fs::write(
            "/tmp/gugu_fs_x64.txt",
            compilation.dump_x64().expect("机器码 dump"),
        )
        .expect("写出机器码 dump");
        fs::write(
            "/tmp/gugu_fs_lir.txt",
            compilation.dump_lir().expect("LIR dump"),
        )
        .expect("写出 LIR dump");
    }
    let (name, bytes) = match target {
        TargetName::X86_64Linux => ("/tmp/gugu_fs_image", plan.linux_image()),
        TargetName::X86_64Windows => ("/tmp/gugu_fs_image.exe", plan.windows_image()),
        TargetName::X86_64Macos => ("/tmp/gugu_fs_image.macho", plan.macos_image()),
    };
    fs::write(name, bytes).expect("写出手工验证镜像");
    println!("{name}");
}
