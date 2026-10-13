use crate::frontend::gir::body::CallKind;
use crate::lir::body::{CallTarget, RuntimeCall};
use crate::{CompileRequest, Compiler, DiagnosticCode, TargetName};

#[test]
fn direct_blocking_call_counts_as_ordinary_and_blocking() {
    let source = "fn main() {}";
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let lir = compilation.lir.as_ref().expect("LIR");
    let baseline = lir.foreign_demand();
    let mut body = lir.bodies()[0].clone();
    body.instructions.push(crate::lir::body::Instruction {
        op: crate::lir::body::Op::Call(crate::lir::body::Call {
            target: CallTarget::Runtime(RuntimeCall::BlockingIo {
                op: 0,
                cancel: true,
            }),
            kind: CallKind::Managed,
            parameters: Vec::new(),
            results: Vec::new(),
            may_unwind: false,
            may_suspend: true,
            may_allocate: false,
            captures_arguments: true,
            by_value: Vec::new(),
            sret: None,
            poll_free_leaf: false,
        }),
        arguments: 0..0,
        results: 0..0,
        memory: None,
        safepoint: None,
        source: body.instructions[0].source.clone(),
    });
    let counted = lir.demand_with_blocking(&body);
    assert_eq!(counted.blocking_io, baseline.blocking_io + 1);
    assert_eq!(counted.ordinary, baseline.ordinary + 1);
}

#[test]
fn std_io_and_path_compile_and_user_path_stays_reserved() {
    let source = "use std.io\nuse std.path\nfn main() {}";
    let cold = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
    let warm = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert_eq!(cold.action_key(), warm.action_key());
    assert_eq!(cold.dump_runtime(), warm.dump_runtime());
    let plan = cold.image_plan().expect("计划");
    assert_eq!(plan.blocking_io_sites(), 0);
    let dump = cold.dump_runtime().expect("dump");
    assert!(dump.contains("blocking-io="));
    let reserved = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        "struct Path {}\nfn main() {}",
        TargetName::X86_64Linux,
    ));
    assert!(
        reserved
            .diagnostics()
            .items()
            .iter()
            .any(|item| { item.code() == DiagnosticCode::ReservedName })
    );
}
