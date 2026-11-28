use super::{CompileRequest, Compiler, TargetName, accepts};

#[test]
fn pack_loop_bounds_and_nested_control_contracts() {
    assert!(accepts(
        "use std.fmt.{Formatter}\nfn total[Ts: Print...](...args: Ts) string { let out = Formatter::new()\n for a in args { a.print(&out) }\n out.finish() }\nfn main() { _ = total(1, true, \"x\") }"
    ));
    assert!(accepts(
        "fn visit[Ts: Print...](...args: Ts) { for a in args { loop { break } } }\nfn main() { visit(1, true) }"
    ));
    assert!(!accepts(
        "use std.fmt.{Formatter}\nfn total[Ts...](...args: Ts) string { let out = Formatter::new()\n for a in args { a.print(&out) }\n out.finish() }\nfn main() { _ = total() }"
    ));
}

#[test]
fn pack_loop_rejects_bad_callable_bounds_and_direct_break_continue() {
    assert!(!accepts(
        "fn apply[Fs...](...fs: Fs) { for f in fs { f() } }\nfn main() { apply(fn() {}) }"
    ));
    assert!(!accepts(
        "fn apply[Fs: Fn(int)...](...fs: Fs) { for f in fs { f(true) } }\nfn main() { apply(fn(n: int) {}) }"
    ));
    assert!(!accepts(
        "fn apply[Fs: Fn() int...](...fs: Fs) { for f in fs { _ = f() } }\nfn main() { apply(fn() bool = true) }"
    ));
    rejects_control("break");
    rejects_control("continue");
}

fn rejects_control(control: &str) {
    let source = format!(
        "fn visit[Ts: Print...](...args: Ts) {{ for a in args {{ {control} }} }}\nfn main() {{ visit(1, true) }}"
    );
    let output = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
        "main.gg",
        &source,
        TargetName::X86_64Linux,
    ));
    assert!(!output.is_success());
    assert!(
        output
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains(&format!(
                "{control} cannot target a heterogeneous pack loop"
            ))),
        "{:?}",
        output.diagnostics().items()
    );
}
