//! 执行实际地址实现的 LIR；分配和字符串原语使用进程内内存，不访问网络。
use super::harness::{Harness, put, word};
use super::{Compilation, CompileRequest, Compiler, TargetName, named};

mod protocol;
mod transport;

fn compilation(target: TargetName) -> Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/net_address.gg"),
        target,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

#[test]
fn numeric_addresses_validate_components_ports_and_unix_paths() {
    let output = compilation(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 65536];
    assert_eq!(
        h.run(named(&output, "vector_snapshot"), &[], &mut memory),
        [1]
    );
    let body = named(&output, "ipv4");
    for (text, value) in [
        ("0.0.0.0", 0),
        ("127.0.0.1", 0x7f000001),
        ("255.255.255.255", 0xffffffff),
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [value], "{text}");
    }
    for text in [
        "",
        "127.1",
        "1.2.3.4.5",
        "01.2.3.4",
        "256.1.2.3",
        "1..2.3",
        "1.2.3.",
        "1.2.3.4\0",
        " 1.2.3.4",
        "１.2.3.4",
        "+1.2.3.4",
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [u64::MAX], "{text:?}");
    }
    let body = named(&output, "ipv6");
    for (text, expected) in [
        ("::", [0; 8]),
        ("::1", [0, 0, 0, 0, 0, 0, 0, 1]),
        ("2001:DB8:0:1::2", [0x2001, 0xdb8, 0, 1, 0, 0, 0, 2]),
        ("::ffff:192.0.2.1", [0, 0, 0, 0, 0, 0xffff, 0xc000, 0x201]),
        ("1:2:3:4:5:6:7:8", [1, 2, 3, 4, 5, 6, 7, 8]),
    ] {
        for (index, expected) in expected.into_iter().enumerate() {
            assert_eq!(
                h.text(body, text, &[index as u64], &mut memory),
                [expected],
                "{text} segment {index}"
            );
        }
    }
    for text in [
        "",
        ":",
        ":::1",
        "1::2::3",
        "1:2:3:4:5:6:7",
        "1:2:3:4:5:6:7:8:9",
        "1:2:3:4:5:6:7:8::",
        "12345::",
        "gg::",
        "::1:",
        "::192.01.2.1",
        "::1%eth0",
    ] {
        assert_eq!(h.text(body, text, &[0], &mut memory), [u64::MAX], "{text}");
    }
    let body = named(&output, "socket");
    for (text, expected) in [
        ("127.0.0.1:0", 0),
        ("[::1]:65535", 65535),
        ("[fe80::1%12]:80", (12 << 16) | 80),
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [expected], "{text}");
    }
    for text in [
        "127.0.0.1:",
        "127.0.0.1:-1",
        "[::1]:65536",
        "[::1]:+80",
        "[::1]:1a",
    ] {
        assert_eq!(
            h.text(body, text, &[], &mut memory),
            [u64::MAX - 1],
            "{text}"
        );
    }
    for text in [
        "::1:80",
        "[]:80",
        "[::1]80",
        "[::1%eth0]:80",
        "[::1%4294967296]:80",
        "localhost:80",
        ":80",
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [u64::MAX], "{text}");
    }
    let body = named(&output, "host_valid");
    for (text, expected) in [
        ("localhost".into(), 1),
        ("example.COM.".into(), 1),
        ("a-b.example".into(), 1),
        ("::1".into(), 1),
        ("fe80::1%12".into(), 1),
        ("".into(), 0),
        ("a\0b".into(), 0),
        ("中文.example".into(), 0),
        ("a..b".into(), 0),
        ("-host".into(), 0),
        ("host-".into(), 0),
        ("a_b".into(), 0),
        ("::1%eth0".into(), 0),
        ("x".repeat(64), 0),
        ("x".repeat(255), 0),
    ] {
        assert_eq!(
            h.text(body, &text, &[], &mut memory),
            [expected],
            "{text:?}"
        );
    }
    let body = named(&output, "unix_path");
    for (text, expected) in [
        ("/tmp/test.sock".into(), 1),
        ("x".repeat(107), 1),
        ("x".repeat(108), 0),
        ("".into(), 0),
        ("a\0b".into(), 0),
    ] {
        assert_eq!(h.text(body, &text, &[], &mut memory), [expected]);
    }
}

#[test]
fn numeric_addresses_format_canonically_on_windows() {
    let output = compilation(TargetName::X86_64Windows);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 65536];
    for (function, text, expected) in [
        (
            "format_socket",
            "[fe80::1%4294967295]:65535",
            "[fe80::1%4294967295]:65535",
        ),
        ("format_socket", "127.0.0.1:0", "127.0.0.1:0"),
        ("format4", "192.0.2.1", "192.0.2.1"),
        ("format6", "0:0:0:0:0:0:0:0", "::"),
        ("format6", "0:0:0:0:0:0:0:1", "::1"),
        ("format6", "1:0:0:2:0:0:3:4", "1::2:0:0:3:4"),
        ("format6", "2001:DB8:0:1:0:0:0:0", "2001:db8:0:1::"),
        ("format6", "::ffff:192.0.2.1", "::ffff:c000:201"),
    ] {
        let result = h.text(named(&output, function), text, &[], &mut memory);
        assert_eq!(result.len(), 2);
        let value = &memory[result[0] as usize..(result[0] + result[1]) as usize];
        assert_eq!(value, expected.as_bytes(), "{text}");
    }
}
