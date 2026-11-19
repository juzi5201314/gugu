use super::{CompileRequest, Compiler, TargetName, accepts};

#[test]
fn pack_loop_checks_declared_bounds() {
    assert!(accepts(
        "use std.fmt.{Formatter}\nfn total[Ts: Print...](...args: Ts) string { let out = Formatter::new()\n for a in args { a.print(&out) }\n out.finish() }\nfn main() { _ = total(1, true, \"x\") }"
    ));
}

#[test]
fn pack_loop_callable_bound_still_checks_signature() {
    assert!(!accepts(
        "fn apply[Fs...](...fs: Fs) { for f in fs { f() } }\nfn main() { apply(fn() {}) }"
    ));
    assert!(!accepts(
        "fn apply[Fs: Fn(int)...](...fs: Fs) { for f in fs { f(true) } }\nfn main() { apply(fn(n: int) {}) }"
    ));
    assert!(!accepts(
        "fn apply[Fs: Fn() int...](...fs: Fs) { for f in fs { _ = f() } }\nfn main() { apply(fn() bool = true) }"
    ));
}

#[test]
fn pack_loop_requires_declared_bound_even_for_empty_call() {
    assert!(!accepts(
        "use std.fmt.{Formatter}\nfn total[Ts...](...args: Ts) string { let out = Formatter::new()\n for a in args { a.print(&out) }\n out.finish() }\nfn main() { _ = total() }"
    ));
}

fn rejects_control(control: &str) {
    let source = format!(
        "fn visit[Ts: Print...](...args: Ts) {{ for a in args {{ {control} }} }}\nfn main() {{ visit(1, true) }}"
    );
    let output = Compiler::shared().compile(CompileRequest::single_file(
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

#[test]
fn pack_loop_rejects_break() {
    rejects_control("break");
}

#[test]
fn pack_loop_rejects_continue() {
    rejects_control("continue");
}

#[test]
fn pack_loop_allows_break_in_nested_runtime_loop() {
    assert!(accepts(
        "fn visit[Ts: Print...](...args: Ts) { for a in args { loop { break } } }\nfn main() { visit(1, true) }"
    ));
}
