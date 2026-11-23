//! aarch64 目标的前端整程序驱动验收：进程内编译到可执行镜像，校验镜像身份与
//! 镜像计划槽位。镜像字节不在这里执行（测试不跑真实子进程），执行验证由手工
//! probe 与平台示例完成。

use super::{CompileRequest, Compiler, TargetName};

fn compile(source: &str, target: TargetName) -> super::Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file("main.gg", source, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

const PROGRAM: &str = "fn main() {}\n";

/// aarch64-macos 经整程序装配产出 Mach-O 64 镜像，镜像计划落到 macos 槽。
#[test]
fn aarch64_macos_emits_macho_image() {
    let output = compile(PROGRAM, TargetName::Aarch64Macos);
    let program = output
        .a64_program()
        .expect("aarch64-macos 编译必须产出 a64 装配产物");
    assert!(program.image.len() > 0x1000, "a64 镜像必须含完整装载布局");
    assert_eq!(
        &program.image[..4],
        &[0xcf, 0xfa, 0xed, 0xfe],
        "aarch64-macos 镜像必须是 Mach-O 64（MH_MAGIC_64）"
    );
    let plan = output.image_plan().expect("可执行编译必须有镜像计划");
    assert_eq!(&plan.macos_image()[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    assert_eq!(plan.macos_image(), program.image.as_slice());
}

/// aarch64-linux 经整程序装配产出 ELF64 镜像（携带 PT_INTERP），镜像计划落到 linux 槽。
#[test]
fn aarch64_linux_emits_elf_image() {
    let output = compile(PROGRAM, TargetName::Aarch64Linux);
    let program = output
        .a64_program()
        .expect("aarch64-linux 编译必须产出 a64 装配产物");
    assert_eq!(
        &program.image[..4],
        &[0x7f, 0x45, 0x4c, 0x46],
        "aarch64-linux 镜像必须是 ELF（0x7f ELF）"
    );
    // ELF e_machine 字段（offset 18）必须是 EM_AARCH64（183）。
    let machine = u16::from_le_bytes(program.image[18..20].try_into().expect("e_machine 字段"));
    assert_eq!(machine, 183, "aarch64-linux 镜像的 e_machine 必须是 EM_AARCH64");
    let plan = output.image_plan().expect("可执行编译必须有镜像计划");
    assert_eq!(&plan.linux_image()[..4], &[0x7f, 0x45, 0x4c, 0x46]);
    assert_eq!(
        plan.linux_interpreter(),
        "/lib/ld-linux-aarch64.so.1",
        "aarch64-linux 镜像必须声明 glibc 解释器"
    );
}

/// 同一程序在两个 aarch64 目标上的 action key 不同（编码器与镜像都随目标变化）。
#[test]
fn aarch64_targets_have_distinct_action_keys() {
    let macos = compile(PROGRAM, TargetName::Aarch64Macos);
    let linux = compile(PROGRAM, TargetName::Aarch64Linux);
    assert_ne!(
        macos.action_key(),
        linux.action_key(),
        "aarch64-macos 与 aarch64-linux 的 action key 必须不同"
    );
}


/// 使用 std runtime（println → report_write glue、堆分配、格式化）的完整程序
/// 在 aarch64-linux 上端到端装配成功。
#[test]
fn aarch64_runtime_program_assembles() {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "use std.io.{println}\nfn main() { println(\"hello\") }\n",
        TargetName::Aarch64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
}
