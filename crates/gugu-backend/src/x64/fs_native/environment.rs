//! 启动目录和环境符号的薄平台边界；快照与互斥由 Gugu 持有。
use super::*;

pub(crate) const LINUX_SYMBOLS: &[&str] = &["environ", "setenv", "unsetenv", "getenv"];
pub(crate) const WINDOWS_SYMBOLS: &[&str] = &[
    "GetEnvironmentStringsW",
    "FreeEnvironmentStringsW",
    "SetEnvironmentVariableW",
    "CompareStringOrdinal",
];

pub(super) fn symbols(target: TargetName) -> FragmentBlob {
    let mut b = Builder::new();
    let names = match target {
        TargetName::X86_64Linux => LINUX_SYMBOLS,
        TargetName::X86_64Windows => WINDOWS_SYMBOLS,
        TargetName::X86_64Macos => &["_NSGetEnviron", "setenv", "unsetenv", "getenv"],
    };
    for (op, &name) in names.iter().enumerate() {
        let next = b.label();
        compare(
            &mut b,
            Rax,
            i32::try_from(op).expect("环境操作编号适配 i32"),
        );
        jump(&mut b, "jne", next);
        match target {
            TargetName::X86_64Linux => {
                let data = op == 0;
                let symbol = if data {
                    format!("__gugu_got_{name}")
                } else {
                    name.into()
                };
                b.emit(
                    if data { "mov" } else { "lea" },
                    if data { MOV } else { &[K::Mem, K::R64] },
                    if data { Access::Read } else { Access::Address },
                    vec![
                        Operand::Rip(
                            RelocTarget::Lir(Symbol::External {
                                key: hash_domain("gugu-native-c-symbol-v1", symbol.as_bytes()),
                                name: symbol,
                            }),
                            0,
                        ),
                        reg(gpr(Rax)),
                    ],
                );
            }
            TargetName::X86_64Windows => {
                b.emit("mov", MOV, Access::Read, vec![iat(name), reg(gpr(Rax))]);
            }
            TargetName::X86_64Macos if op == 0 => {
                adjust(&mut b, "sub", Rsp, 8);
                darwin::c_call(&mut b, name);
                adjust(&mut b, "add", Rsp, 8);
            }
            TargetName::X86_64Macos => darwin::c_address(&mut b, Rax, name),
        }
        ret(&mut b);
        b.define(next);
    }
    b.emit("ud2", &[], Access::Read, vec![]);
    blob("environment_symbol", b)
}

pub(super) fn startup(target: TargetName) -> FragmentBlob {
    let mut b = Builder::new();
    let invalid = b.label();
    let done = b.label();
    push(&mut b, R12);
    rr(&mut b, R12, Rbx);
    load(&mut b, R10, R12, 0);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(Rax)), reg(gpr(R10))]);
    jump(&mut b, "jne", invalid);
    set(&mut b, R12, 72, 0);
    set(&mut b, R12, 80, 0);
    match target {
        TargetName::X86_64Macos => {
            let cwd = b.label();
            compare(&mut b, Rax, 0);
            jump(&mut b, "je", cwd);
            compare(&mut b, Rax, 1);
            jump(&mut b, "jne", invalid);
            set(&mut b, R12, 80, 45);
            jump(&mut b, "jmp", done);
            b.define(cwd);
            load(&mut b, Rdi, R12, 8);
            load(&mut b, Rsi, R12, 16);
            darwin::c_call(&mut b, "getcwd");
            darwin::finish_result(&mut b, 72, 80, 0);
        }
        TargetName::X86_64Linux => {
            let cwd = b.label();
            compare(&mut b, Rax, 0);
            jump(&mut b, "je", cwd);
            compare(&mut b, Rax, 1);
            jump(&mut b, "jne", invalid);
            set(&mut b, R12, 80, 95);
            jump(&mut b, "jmp", done);
            b.define(cwd);
            load(&mut b, Rdi, R12, 8);
            load(&mut b, Rsi, R12, 16);
            number(&mut b, Rax, 79);
            syscall(&mut b);
            compare(&mut b, Rax, 0);
            let success = b.label();
            jump(&mut b, "jge", success);
            b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
            store(&mut b, R12, 80, Rax);
            jump(&mut b, "jmp", done);
            b.define(success);
            store(&mut b, R12, 72, Rax);
        }
        TargetName::X86_64Windows => {
            let cwd = b.label();
            let result = b.label();
            compare(&mut b, Rax, 0);
            jump(&mut b, "je", cwd);
            compare(&mut b, Rax, 1);
            jump(&mut b, "jne", invalid);
            adjust(&mut b, "sub", Rsp, 32);
            load(&mut b, Rcx, R12, 8);
            load(&mut b, Rdx, R12, 16);
            b.emit(
                "shr",
                &[K::Rm64, K::Imm8],
                Access::ReadWrite,
                vec![reg(gpr(Rdx)), imm(1)],
            );
            win_call(&mut b, "GetSystemDirectoryW");
            jump(&mut b, "jmp", result);
            b.define(cwd);
            adjust(&mut b, "sub", Rsp, 32);
            load(&mut b, Rcx, R12, 16);
            b.emit(
                "shr",
                &[K::Rm64, K::Imm8],
                Access::ReadWrite,
                vec![reg(gpr(Rcx)), imm(1)],
            );
            load(&mut b, Rdx, R12, 8);
            win_call(&mut b, "GetCurrentDirectoryW");
            b.define(result);
            dword_result(&mut b);
            let success = b.label();
            let failure = b.label();
            compare(&mut b, Rax, 0);
            jump(&mut b, "je", failure);
            rr(&mut b, R10, Rax);
            adjust(&mut b, "add", R10, 1);
            b.emit(
                "shl",
                &[K::Rm64, K::Imm8],
                Access::ReadWrite,
                vec![reg(gpr(R10)), imm(1)],
            );
            load(&mut b, R11, R12, 16);
            b.emit("cmp", MOV, Access::Read, vec![reg(gpr(R10)), reg(gpr(R11))]);
            jump(&mut b, "jbe", success);
            set(&mut b, R12, 80, 122);
            let cleanup = b.label();
            jump(&mut b, "jmp", cleanup);
            b.define(failure);
            win_call(&mut b, "GetLastError");
            dword_result(&mut b);
            store(&mut b, R12, 80, Rax);
            jump(&mut b, "jmp", cleanup);
            b.define(success);
            store(&mut b, R12, 72, Rax);
            win_call(&mut b, "GetCurrentProcessId");
            dword_result(&mut b);
            store(&mut b, R12, 24, Rax);
            b.define(cleanup);
            adjust(&mut b, "add", Rsp, 32);
        }
    }
    b.define(done);
    load(&mut b, Rax, R12, 80);
    pop(&mut b, R12);
    ret(&mut b);
    b.define(invalid);
    b.emit("ud2", &[], Access::Read, vec![]);
    blob("startup_call", b)
}
