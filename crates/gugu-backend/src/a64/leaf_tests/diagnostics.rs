use super::*;

fn location(start: u32, end: u32) -> crate::hir::Location {
    crate::hir::Location {
        source: 0,
        start,
        end,
        expansion: 0,
    }
}

#[test]
fn instruction_failure_keeps_its_source_through_backend_contexts() {
    let mut fixture = Fixture::new(&[], &[Type::I64]);
    let value = fixture.constant(Type::I64, 1);
    fixture.ret(&[value]);
    let expression = location(16, 22);
    fixture.body.instructions[0].source.location = expression.clone();
    fixture.body.instructions[0].arguments = 1..2;
    fixture.body.blocks[0].source.location = location(0, 24);
    let error = codegen::compile_leaf(&fixture.body).unwrap_err();
    assert!(
        error.0.contains("invalid operands/memory/safepoint"),
        "{error}"
    );
    assert_eq!(error.1, Some(expression));
    assert!(error.0.starts_with("A64 a64_leaf_fixture:"));
}

#[test]
fn undefined_symbol_points_to_its_reference_and_preserves_exact_errors() {
    let mut fixture = Fixture::new(&[], &[]);
    fixture.constant(Type::I64, 1);
    fixture.op(
        Op::SymbolAddr(Symbol::External {
            key: [1; 32],
            name: "getpid".into(),
        }),
        &[],
        &[Type::Ptr],
    );
    fixture.ret(&[]);
    let reference = location(20, 28);
    fixture.body.instructions[0].source.location = location(10, 11);
    fixture.body.instructions[1].source.location = reference.clone();
    let bodies = [fixture.body];
    for platform in [
        crate::a64::foreign::Platform::Linux,
        crate::a64::foreign::Platform::Darwin,
    ] {
        let error = Error::undefined_symbol(&platform.symbol("getpid"))
            .context("link")
            .locate_symbol(&bodies, platform);
        assert_eq!(error.1, Some(reference.clone()));
        assert!(
            Error::undefined_symbol("unrelated")
                .locate_symbol(&bodies, platform)
                .1
                .is_none()
        );
        let exact = location(40, 44);
        let error = Error::undefined_symbol(&platform.symbol("getpid"))
            .with_location(Some(&exact))
            .locate_symbol(&bodies, platform);
        assert_eq!(error.1, Some(exact));
    }
}
