use super::{CompileRequest, Compiler, TargetName, harness::Harness};
use crate::runtime::RuntimeMachineOp;

fn capture(
    harness: &mut Harness<'_>,
    op: RuntimeMachineOp,
    args: &[u64],
    memory: &mut Vec<u8>,
) -> Vec<u64> {
    assert_eq!(op, RuntimeMachineOp::ReportWrite);
    let written = if let Some((fd, length, result)) = harness.script.pop_front() {
        assert_eq!((args[0], args[2]), (fd, length));
        result
    } else {
        args[2]
    };
    let count = if written <= args[2] {
        written as usize
    } else {
        0
    };
    let start = args[1] as usize;
    harness.calls.push((
        args[0],
        memory[start..start + count]
            .iter()
            .map(|byte| u64::from(*byte))
            .collect(),
    ));
    vec![written]
}

fn output(source: &str, script: &[(u64, u64, u64)]) -> Vec<(u64, Vec<u8>)> {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let mut harness = Harness::new(&compiled);
    harness.machine_hook = Some(capture);
    harness.script.extend(script.iter().copied());
    harness.invoke("main.gg", "main", &[], &mut vec![0; 262144]);
    assert!(harness.script.is_empty());
    harness
        .calls
        .into_iter()
        .map(|(fd, bytes)| (fd, bytes.into_iter().map(|byte| byte as u8).collect()))
        .collect()
}

#[test]
fn print_hello_world() {
    assert_eq!(
        output(
            "use std.io.{println}\nfn main() { println(\"hello, gugu\") }",
            &[]
        ),
        vec![(1, b"hello, gugu\n".to_vec())]
    );
}

#[test]
fn print_mixed_values_and_user_impl() {
    assert_eq!(
        output(
            r#"
        use std.io.{println}
        struct Word {}
        impl Print for Word { fn print(self: &Self, out: &Formatter) { out.write_str("word") } }
        fn main() { println("i + 1 = ", 1, true, Word {}) }
    "#,
            &[]
        ),
        vec![(1, b"i + 1 = 1trueword\n".to_vec())]
    );
}

#[test]
fn print_empty_and_stderr() {
    assert_eq!(
        output(
            r#"
        use std.io.{print, println, eprint, eprintln}
        fn main() { print(); eprint(); println(); eprintln("err"); eprint("tail") }
    "#,
            &[]
        ),
        vec![
            (1, b"\n".to_vec()),
            (2, b"err\n".to_vec()),
            (2, b"tail".to_vec())
        ]
    );
}

#[test]
fn print_interpolated_argument() {
    assert_eq!(
        output(
            r#"use std.io.{print}
        fn main() { let i = 3
            print(f"i = {i}") }
    "#,
            &[]
        ),
        vec![(1, b"i = 3".to_vec())]
    );
}

#[test]
fn print_retries_short_writes_in_byte_order() {
    assert_eq!(
        output(
            r#"use std.io.{eprintln}
        fn main() { eprintln("éx") }
    "#,
            &[(2, 4, 1), (2, 3, 2), (2, 1, 1)]
        ),
        vec![(2, vec![0xc3]), (2, vec![0xa9, b'x']), (2, vec![b'\n'])]
    );
}

#[test]
fn print_stops_on_write_zero() {
    assert_eq!(
        output(
            "use std.io.{print}\nfn main() { print(\"abc\") }",
            &[(1, 3, 1), (1, 2, 0)]
        ),
        vec![(1, b"a".to_vec()), (1, vec![])]
    );
}

#[test]
fn print_stops_on_write_error() {
    assert_eq!(
        output(
            "use std.io.{print}\nfn main() { print(\"abc\") }",
            &[(1, 3, u64::MAX)]
        ),
        vec![(1, vec![])]
    );
}

#[test]
fn print_overview_example_compiles_to_linux_elf() {
    let overview = include_str!("../../../../docs/src/spec/overview.md");
    let source = overview
        .split("`main.gg`：")
        .nth(1)
        .unwrap()
        .split("```gugu\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap()
        .replace("use green.{bar}", "fn bar() string = \"bar\"");
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        &source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    assert!(
        compiled
            .image_plan()
            .unwrap()
            .linux_image()
            .starts_with(b"\x7fELF")
    );
}
