//! 非阻塞 socket 与 poller 的薄边界；调度、取消和资源状态保留在 Gugu。
use super::*;

pub(crate) fn required(fragments: &[crate::x64::codegen::FragmentPayload], glue: &str) -> bool {
    let target = RelocTarget::Lir(mangle::runtime_symbol(glue));
    fragments.iter().any(|fragment| {
        fragment
            .relocations
            .iter()
            .any(|relocation| relocation.target == target)
    })
}

pub(super) fn resolver_fragment(target: TargetName) -> FragmentBlob {
    let mut b = Builder::new();
    for (op, name) in [(0, "getaddrinfo"), (1, "freeaddrinfo")] {
        let next = b.label();
        compare(&mut b, Rax, op);
        jump(&mut b, "jne", next);
        match target {
            TargetName::X86_64Linux => {
                b.emit(
                    "lea",
                    &[K::Mem, K::R64],
                    Access::Address,
                    vec![
                        Operand::Rip(
                            RelocTarget::Lir(Symbol::External {
                                key: hash_domain("gugu-native-c-symbol-v1", name.as_bytes()),
                                name: name.into(),
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
            TargetName::X86_64Macos => darwin::c_address(&mut b, Rax, name),
        };
        ret(&mut b);
        b.define(next);
    }
    b.emit("ud2", &[], Access::Read, vec![]);
    blob("resolver_symbol", b)
}

pub(super) fn fragment(target: TargetName) -> FragmentBlob {
    let mut code = match target {
        TargetName::X86_64Linux => linux(),
        TargetName::X86_64Windows => windows(),
        TargetName::X86_64Macos => darwin(),
    };
    let callback = code.label();
    // 关闭回调独立采用 C ABI；net_call 的 op=19 返回它的 code pointer。
    let start = code.instruction_count();
    code.define(callback);
    match target {
        TargetName::X86_64Macos => {
            adjust(&mut code, "sub", Rsp, 8);
            darwin::c_call(&mut code, "close");
            let done = code.label();
            compare(&mut code, Rax, 0);
            jump(&mut code, "je", done);
            darwin::errno(&mut code);
            code.define(done);
            adjust(&mut code, "add", Rsp, 8);
            ret(&mut code);
        }
        TargetName::X86_64Linux => {
            number(&mut code, Rax, 3);
            syscall(&mut code);
            code.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
            ret(&mut code);
        }
        TargetName::X86_64Windows => {
            adjust(&mut code, "sub", Rsp, 40);
            win_call(&mut code, "closesocket");
            dword_result(&mut code);
            let success = code.label();
            compare(&mut code, Rax, 0);
            jump(&mut code, "je", success);
            win_call(&mut code, "WSAGetLastError");
            dword_result(&mut code);
            code.define(success);
            adjust(&mut code, "add", Rsp, 40);
            ret(&mut code);
        }
    }
    let signal_handler = (target == TargetName::X86_64Macos).then(|| {
        let start = code.instruction_count();
        ret(&mut code);
        start
    });
    let encoded = encode::assemble(&code.finish().sequence).expect("网络片段编码");
    let offset = |start| {
        encoded
            .instruction_offsets
            .iter()
            .find(|(index, _)| *index == start)
            .expect("网络入口偏移")
            .1
    };
    let mut entries = vec![
        FragmentEntry {
            glue: "net_call",
            offset: 0,
        },
        FragmentEntry {
            glue: "net_close",
            offset: offset(start),
        },
    ];
    if let Some(start) = signal_handler {
        entries.push(FragmentEntry {
            glue: "darwin_signal_handler",
            offset: offset(start),
        });
    }
    FragmentBlob {
        alignment: 16,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries,
        relocations: encoded.relocations,
    }
}

// 网络请求九个参数后接 value/error；与 net_platform.gg 的布局逐项校验。
const RESULT: i32 = 72;
const FAILURE: i32 = 80;
fn darwin() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    rr(&mut b, R12, Rbx);
    set(&mut b, R12, RESULT, 0);
    set(&mut b, R12, FAILURE, 0);
    let done = b.label();
    close_address(&mut b, done);
    let native = b.label();
    compare(&mut b, Rax, 30);
    jump(&mut b, "jne", native);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(mangle::runtime_symbol("darwin_signal_handler")),
                0,
            ),
            reg(gpr(Rax)),
        ],
    );
    store(&mut b, R12, RESULT, Rax);
    jump(&mut b, "jmp", done);
    b.define(native);
    let methods = [
        (0, "socket", -1),
        (1, "setsockopt", -1),
        (2, "bind", -1),
        (3, "listen", -1),
        (4, "accept", -1),
        (5, "getsockname", -1),
        (6, "getpeername", -1),
        (7, "close", -1),
        (8, "kqueue", -1),
        (9, "kevent", -1),
        (10, "poll", -1),
        (21, "connect", -1),
        (22, "shutdown", -1),
        (23, "getsockopt", -1),
        (24, "recvfrom", -1),
        (25, "sendto", -1),
        (26, "fcntl", -1),
        (27, "recvmsg", -1),
        (28, "disconnectx", -1),
        (29, "sigaction", -1),
    ];
    let native_done = b.label();
    super::darwin::dispatch(&mut b, &methods, 0, RESULT, FAILURE, native_done);
    b.define(native_done);
    b.define(done);
    load(&mut b, Rax, R12, RESULT);
    pop(&mut b, R12);
    ret(&mut b);
    b
}
const LINUX: &[(i32, i32)] = &[
    (0, 41),
    (1, 54),
    (2, 49),
    (3, 50),
    (4, 288),
    (5, 51),
    (6, 52),
    (7, 3),
    (8, 291),
    (9, 233),
    (10, 232),
    (21, 42), // connect
    (22, 48), // shutdown
    (23, 55), // getsockopt
    (24, 45), // recvfrom
    (25, 44), // sendto
];
fn close_address(b: &mut Builder, done: LabelId) {
    let next = b.label();
    compare(b, Rax, 19);
    jump(b, "jne", next);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(RelocTarget::Lir(mangle::runtime_symbol("net_close")), 0),
            reg(gpr(Rax)),
        ],
    );
    store(b, R12, RESULT, Rax);
    jump(b, "jmp", done);
    b.define(next);
}

pub(super) fn linux() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    rr(&mut b, R12, Rbx);
    set(&mut b, R12, FAILURE, 0);
    let done = b.label();
    close_address(&mut b, done);
    for &(op, number_value) in LINUX {
        let next = b.label();
        compare(&mut b, Rax, op);
        jump(&mut b, "jne", next);
        for (index, r) in [Rdi, Rsi, Rdx, R10, R8, R9].into_iter().enumerate() {
            load(&mut b, r, R12, index as i32 * 8);
        }
        number(&mut b, Rax, number_value);
        syscall(&mut b);
        jump(&mut b, "jmp", done);
        b.define(next);
    }
    number(&mut b, Rax, 22);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
    b.define(done);
    let success = b.label();
    compare(&mut b, Rax, 0);
    jump(&mut b, "jge", success);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
    store(&mut b, R12, FAILURE, Rax);
    number(&mut b, Rax, 0);
    b.define(success);
    store(&mut b, R12, RESULT, Rax);
    pop(&mut b, R12);
    ret(&mut b);
    b
}

/// 网络导入属于目标登记，不能从宿主 DLL 或 PATH 推断。
pub(crate) const WINDOWS_SYMBOLS: &[&str] = &[
    "socket",
    "setsockopt",
    "bind",
    "listen",
    "accept",
    "getsockname",
    "getpeername",
    "closesocket",
    "ioctlsocket",
    "WSAStartup",
    "WSACleanup",
    "WSAGetLastError",
    "WSAIoctl",
    "connect",
    "shutdown",
    "getsockopt",
    "WSARecvFrom",
    "WSASendTo",
    "WSARecv",
    "WSASend",
];
// code、符号、返回分类：0=SOCKET、1=SOCKET_ERROR、2=直接错误码、3=BOOL、4=HANDLE。
const WINDOWS: &[(i32, &str, u8)] = &[
    (0, "socket", 0),
    (1, "setsockopt", 1),
    (2, "bind", 1),
    (3, "listen", 1),
    (4, "accept", 0),
    (5, "getsockname", 1),
    (6, "getpeername", 1),
    (7, "closesocket", 1),
    (8, "CreateIoCompletionPort", 4),
    (9, "WSAIoctl", 1),
    (10, "GetQueuedCompletionStatus", 3),
    (11, "ioctlsocket", 1),
    (12, "WSAStartup", 2),
    (13, "WSACleanup", 1),
    (17, "CancelIoEx", 3),
    (20, "CloseHandle", 3),
    (21, "connect", 1),
    (22, "shutdown", 1),
    (23, "getsockopt", 1),
    (26, "WSARecvFrom", 1),
    (27, "WSASendTo", 1),
    (28, "WSARecv", 1),
    (29, "WSASend", 1),
];
fn windows_arguments(b: &mut Builder) {
    for (index, r) in [Rcx, Rdx, R8, R9].into_iter().enumerate() {
        load(b, r, R12, index as i32 * 8);
    }
    for index in 0..5 {
        load(b, Rax, R12, 32 + index * 8);
        store(b, Rsp, 32 + index * 8, Rax);
    }
}
fn windows_extensions(b: &mut Builder, done: LabelId) {
    for op in [14, 15, 31] {
        let next = b.label();
        compare(b, Rax, op);
        jump(b, "jne", next);
        windows_arguments(b);
        b.emit(
            "call",
            &[K::Mem],
            Access::Read,
            vec![mem_base(gpr(R12), 88)],
        );
        if op == 15 {
            number(b, Rax, 0);
        } else {
            dword_result(b);
            store(b, R12, RESULT, Rax);
            compare(b, Rax, 0);
            jump(b, "jne", done);
            win_call(b, "WSAGetLastError");
            dword_result(b);
            store(b, R12, FAILURE, Rax);
        }
        store(b, R12, RESULT, Rax);
        jump(b, "jmp", done);
        b.define(next);
    }
}
pub(super) fn windows() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    adjust(&mut b, "sub", Rsp, 80);
    rr(&mut b, R12, Rbx);
    set(&mut b, R12, FAILURE, 0);
    let done = b.label();
    close_address(&mut b, done);
    windows_extensions(&mut b, done);
    for &(op, name, kind) in WINDOWS {
        let next = b.label();
        compare(&mut b, Rax, op);
        jump(&mut b, "jne", next);
        windows_arguments(&mut b);
        win_call(&mut b, name);
        if kind != 0 && kind != 4 {
            dword_result(&mut b);
        }
        store(&mut b, R12, RESULT, Rax);
        if kind == 2 {
            store(&mut b, R12, FAILURE, Rax);
        } else {
            let sentinel = match kind {
                0 => -1,
                1 => u32::MAX as i32,
                _ => 0,
            };
            if kind == 1 {
                b.emit(
                    "cmp",
                    &[K::Rm32, K::Imm32],
                    Access::Read,
                    vec![reg(gpr(Rax)), imm(u32::MAX as u64)],
                );
            } else {
                compare(&mut b, Rax, sentinel);
            }
            jump(&mut b, "jne", done);
            win_call(
                &mut b,
                if kind == 3 || kind == 4 {
                    "GetLastError"
                } else {
                    "WSAGetLastError"
                },
            );
            dword_result(&mut b);
            store(&mut b, R12, FAILURE, Rax);
        }
        jump(&mut b, "jmp", done);
        b.define(next);
    }
    set(&mut b, R12, RESULT, 0);
    set(&mut b, R12, FAILURE, 10022);
    b.define(done);
    load(&mut b, Rax, R12, RESULT);
    adjust(&mut b, "add", Rsp, 80);
    pop(&mut b, R12);
    ret(&mut b);
    b
}
