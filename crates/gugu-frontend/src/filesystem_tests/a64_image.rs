//! aarch64 目标的前端整程序驱动验收：进程内编译到可执行镜像，校验镜像身份与
//! 镜像计划槽位。镜像字节不在这里执行（测试不跑真实子进程），执行验证由手工
//! probe 与平台示例完成。

use super::{CompileRequest, Compiler, TargetName};

fn compile(source: &str, target: TargetName) -> super::Compilation {
    let output = Compiler::shared()
        .compile_full_for_test(CompileRequest::single_file("main.gg", source, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

// 同一完整镜像编译同时覆盖 #275 的丢弃值 if-let 链，避免重复装配 runtime。
const PROGRAM: &str = r#"
fn choose(v: Result[int, string]) {
    if let Ok(x) = v { x } else if let Err(e) = v { e }
}
fn main() { choose(Err("e")) }
"#;

/// aarch64-macos 与 aarch64-linux 各经整程序装配产出 Mach-O 64 / ELF64 镜像且 action key 互异。
#[test]
fn aarch64_targets_emit_distinct_macho_and_elf_images() {
    let (macos, linux) = std::thread::scope(|scope| {
        let macos = scope.spawn(|| compile(PROGRAM, TargetName::Aarch64Macos));
        let linux = compile(PROGRAM, TargetName::Aarch64Linux);
        (macos.join().expect("aarch64-macos compile thread"), linux)
    });
    let macos_program = macos
        .a64_program()
        .expect("aarch64-macos 编译必须产出 a64 装配产物");
    assert!(
        macos_program.image.len() > 0x1000,
        "a64 镜像必须含完整装载布局"
    );
    assert_eq!(
        &macos_program.image[..4],
        &[0xcf, 0xfa, 0xed, 0xfe],
        "aarch64-macos 镜像必须是 Mach-O 64（MH_MAGIC_64）"
    );
    let macos_plan = macos.image_plan().expect("可执行编译必须有镜像计划");
    assert_eq!(&macos_plan.macos_image()[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    assert_eq!(macos_plan.macos_image(), macos_program.image.as_slice());

    let linux_program = linux
        .a64_program()
        .expect("aarch64-linux 编译必须产出 a64 装配产物");
    assert_eq!(
        &linux_program.image[..4],
        &[0x7f, 0x45, 0x4c, 0x46],
        "aarch64-linux 镜像必须是 ELF（0x7f ELF）"
    );
    // ELF e_machine 字段（offset 18）必须是 EM_AARCH64（183）。
    let machine = u16::from_le_bytes(
        linux_program.image[18..20]
            .try_into()
            .expect("e_machine 字段"),
    );
    assert_eq!(
        machine, 183,
        "aarch64-linux 镜像的 e_machine 必须是 EM_AARCH64"
    );
    let linux_plan = linux.image_plan().expect("可执行编译必须有镜像计划");
    assert_eq!(&linux_plan.linux_image()[..4], &[0x7f, 0x45, 0x4c, 0x46]);
    assert_eq!(
        linux_plan.linux_interpreter(),
        "/lib/ld-linux-aarch64.so.1",
        "aarch64-linux 镜像必须声明 glibc 解释器"
    );

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
    let output = Compiler::shared().compile_full_for_test(CompileRequest::single_file(
        "main.gg",
        "use std.io.{println}\nfn main() { println(\"hello\") }\n",
        TargetName::Aarch64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
}

const DYN_PROGRAM: &str = r#"use std.io.{println}
trait Describable {
    fn describe(self: &Self) string
}
struct Dog { pub name: string }
impl Describable for Dog {
    fn describe(self: &Self) string { self.name }
}
fn show(d: dyn Describable) {
    println(d.describe())
}
fn main() {
    show(Dog { name: "rex" })
}
"#;

/// issue #152：`dyn Trait` 擦除不再经过不存在的 runtime `dynamic_erase`；
/// aarch64 两个对象格式都能装配出含 vtable shim 的镜像。
#[test]
fn aarch64_dyn_trait_erasure_assembles() {
    for target in [TargetName::Aarch64Macos, TargetName::Aarch64Linux] {
        let output = compile(DYN_PROGRAM, target);
        let lir = output.dump_lir().expect("LIR");
        assert!(lir.contains("$vtable"), "缺少 vtable shim：{lir}");
        assert!(
            lir.contains("SymbolAddr(Vtable"),
            "擦除必须引用编译期 vtable 符号：{lir}"
        );
        assert!(!lir.contains("DynamicErase"), "dyn 擦除不能走 runtime glue");
    }
}

const DYN_BY_VALUE_PROGRAM: &str = r#"use std.io.{println}
trait Valuable {
    fn value(self) int
}
struct Num { pub x: int, pub y: int }
impl Valuable for Num {
    fn value(self) int { self.x + self.y }
}
fn show(v: dyn Valuable) {
    println(v.value())
}
fn main() {
    show(Num { x: 3, y: 4 })
}
"#;

/// 按值 receiver 的动态调用以 dyn 胖对入栈；shim 必须从载荷重读两条直接 ABI 车道。
#[test]
fn aarch64_dyn_trait_by_value_receiver_assembles() {
    let output = compile(DYN_BY_VALUE_PROGRAM, TargetName::Aarch64Linux);
    let lir = output.dump_lir().expect("LIR");
    assert!(
        lir.contains("PtrOffset"),
        "双车道 receiver 必须读取第二个字：{lir}"
    );
    assert!(lir.contains("$vtable"), "缺少按值 receiver shim：{lir}");
}
