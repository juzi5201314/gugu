//! 双目标固定网络输入只写出内存镜像，不访问网络或启动子进程。
use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

fn compile(source: &str, target: TargetName) -> gugu_compiler::Compilation {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS)
        .compile(CompileRequest::single_file("main.gg", source, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

#[test]
fn transport_apis_link_linux_without_resolver_dependencies() {
    let output = compile(
        include_str!("fixtures/net_transport.gg"),
        TargetName::X86_64Linux,
    );
    assert!(!output.image_plan().unwrap().linux_image().is_empty());
    assert!(output.image_plan().unwrap().linux_interpreter().is_empty());
    assert!(output.dump_lir().unwrap().contains("NetCall"));
}

#[test]
fn transport_apis_link_windows_overlapped_operations() {
    let output = compile(
        include_str!("fixtures/net_transport.gg"),
        TargetName::X86_64Windows,
    );
    let image = output.image_plan().unwrap().windows_image();
    for name in [
        "WSARecv",
        "WSASend",
        "WSARecvFrom",
        "WSASendTo",
        "shutdown",
        "getsockopt",
    ] {
        assert!(
            image
                .windows(name.len())
                .any(|value| value == name.as_bytes()),
            "{name}"
        );
    }
}

#[test]
fn numeric_listener_and_datagram_bind_link_static_linux() {
    let output = compile(
        include_str!("fixtures/net_bind.gg"),
        TargetName::X86_64Linux,
    );
    let plan = output.image_plan().unwrap();
    assert!(!plan.linux_image().is_empty());
    assert!(
        plan.linux_interpreter().is_empty(),
        "数字 bind 不引入 libc 解释器"
    );
    let lir = output.dump_lir().unwrap();
    assert!(lir.contains("NetCall"));
    assert!(!lir.contains("ResolverSymbol"));
}

#[test]
fn listener_and_datagram_bind_link_windows_winsock() {
    let output = compile(
        include_str!("fixtures/net_bind.gg"),
        TargetName::X86_64Windows,
    );
    let image = output.image_plan().unwrap().windows_image();
    assert!(
        image
            .windows(b"ws2_32.dll".len())
            .any(|bytes| bytes == b"ws2_32.dll")
    );
    assert!(
        image
            .windows(b"WSAIoctl".len())
            .any(|bytes| bytes == b"WSAIoctl")
    );
    assert!(
        !image
            .windows(b"getaddrinfo".len())
            .any(|bytes| bytes == b"getaddrinfo")
    );
}

#[test]
fn system_resolver_links_target_libc_and_threads_on_linux() {
    let output = compile(
        include_str!("fixtures/net_resolve.gg"),
        TargetName::X86_64Linux,
    );
    let plan = output.image_plan().unwrap();
    assert_eq!(plan.linux_interpreter(), "/lib64/ld-linux-x86-64.so.2");
    for symbol in [
        "libc.so.6",
        "getaddrinfo",
        "freeaddrinfo",
        "pthread_create",
        "pthread_tryjoin_np",
    ] {
        assert!(
            plan.linux_image()
                .windows(symbol.len())
                .any(|bytes| bytes == symbol.as_bytes()),
            "{symbol}"
        );
    }
    assert!(output.dump_lir().unwrap().contains("ResolverSymbol"));
}

#[test]
fn system_resolver_links_winsock_on_windows() {
    let output = compile(
        include_str!("fixtures/net_resolve.gg"),
        TargetName::X86_64Windows,
    );
    for symbol in ["ws2_32.dll", "getaddrinfo", "freeaddrinfo"] {
        assert!(
            output
                .image_plan()
                .unwrap()
                .windows_image()
                .windows(symbol.len())
                .any(|bytes| bytes == symbol.as_bytes()),
            "{symbol}"
        );
    }
}
