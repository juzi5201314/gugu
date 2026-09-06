use super::*;
use crate::frontend::semantics::tests::frontend;

fn compile(source: &str) -> hir::Validated {
    frontend(&[("main.gg", source)], &crate::QueryEngine::new())
        .unwrap()
        .hir
}

#[test]
fn hir_preserves_real_bodies_and_isolates_nested_owners() {
    let hir = compile(
        "fn helper(x: int) int = x + 1\nfn main() { let x = 1\n let call = fn() = x\n static cached: int = helper(2)\n let task = async { x }\n _ = call()\n _ = task.wait()\n x = 2 }",
    );
    let module = hir.module();
    let helper = module
        .owners
        .iter()
        .find(|owner| module.definitions[owner.definition.index()].name == "helper")
        .unwrap();
    assert!(matches!(
        helper.expressions[helper.body.index()].kind,
        hir::ExprKind::Binary {
            operation: ast::BinOp::Add,
            ..
        }
    ));
    for kind in [
        hir::DefinitionKind::Closure,
        hir::DefinitionKind::Async,
        hir::DefinitionKind::LocalStatic,
    ] {
        assert!(
            module
                .owners
                .iter()
                .any(|owner| module.definitions[owner.definition.index()].kind == kind)
        );
    }
    for owner in module
        .owners
        .iter()
        .filter(|owner| !owner.captures.is_empty())
    {
        for capture in &owner.captures {
            let parent = module
                .owners
                .iter()
                .find(|parent| parent.definition == capture.owner)
                .unwrap();
            assert_eq!(parent.definition, module.entry.unwrap());
            assert_eq!(parent.locals[capture.source.index()].name, "x");
            assert_eq!(
                owner.locals[capture.local.index()].ty,
                parent.locals[capture.source.index()].ty
            );
        }
    }
}

#[test]
fn frozen_hir_is_identical_across_cache_and_source_order() {
    let sources = [
        (
            "main.gg",
            "use util.{increment}\nfn main() { _ = increment(2) }",
        ),
        ("util.gg", "pub fn increment(value: int) int = value + 1"),
    ];
    let queries = crate::QueryEngine::new();
    let cold = frontend(&sources, &queries).unwrap();
    let warm = frontend(&[sources[1], sources[0]], &queries).unwrap();
    let reordered = frontend(&[sources[1], sources[0]], &crate::QueryEngine::new()).unwrap();
    assert_eq!(cold.hir, warm.hir);
    assert_eq!(cold.hir, reordered.hir);
    assert_eq!(
        serde_json::to_vec(cold.hir.module()).unwrap(),
        serde_json::to_vec(reordered.hir.module()).unwrap()
    );
}

#[test]
fn hir_retains_pre_adjustment_representation_and_literal_bytes() {
    let hir = compile(
        "use std.any.{Any}\nstruct Value { number: int }\nfn main() { let value: dyn Any = Value { number: 1 }\n let bytes = b\"\\x80\\u{4e2d}\"\n let text = f\"{{value}}={1:04}\"\n _ = bytes\n _ = text }",
    );
    let module = hir.module();
    let owner = module
        .owners
        .iter()
        .find(|owner| Some(owner.definition) == module.entry)
        .unwrap();
    let (index, expression) = owner
        .expressions
        .iter()
        .enumerate()
        .find(|(_, expression)| matches!(expression.kind, hir::ExprKind::Construct { .. }))
        .unwrap();
    assert!(matches!(
        module.types[owner.expression_inputs[index].index()],
        hir::Type::Named { .. }
    ));
    assert!(matches!(
        module.types[owner.expression_types[index].index()],
        hir::Type::Dyn(_)
    ));
    assert!(
        owner.adjustments
            [expression.adjustments.start as usize..expression.adjustments.end as usize]
            .iter()
            .any(|adjustment| matches!(adjustment, hir::Adjustment::Erase(_)))
    );
    assert!(owner.expressions.iter().any(|expression| matches!(&expression.kind, hir::ExprKind::Literal(hir::Literal::Bytes(bytes)) if bytes == &[0x80, 0xe4, 0xb8, 0xad])));
    assert!(
        owner
            .string_parts
            .iter()
            .any(|part| matches!(part, hir::StringPart::Text(text) if text == "{value}="))
    );
}

#[test]
fn freeze_rejects_invalid_type_ids_and_capture_owners() {
    let hir = compile(
        "fn unrelated(x: int) int = x\nfn main() { let x = 1\n let call = fn() = x\n _ = call() }",
    );
    let mut invalid_type = hir.module().clone();
    invalid_type.owners[0].expression_inputs[0] = hir::TypeId(u32::MAX);
    assert!(hir::Validated::freeze(invalid_type).is_err());
    let mut wrong_owner = hir.module().clone();
    let unrelated = wrong_owner
        .owners
        .iter()
        .find(|owner| wrong_owner.definitions[owner.definition.index()].name == "unrelated")
        .unwrap()
        .definition;
    let capture = &mut wrong_owner
        .owners
        .iter_mut()
        .find(|owner| !owner.captures.is_empty())
        .unwrap()
        .captures[0];
    capture.owner = unrelated;
    capture.source = hir::LocalId(0);
    assert!(hir::Validated::freeze(wrong_owner).is_err());
}

#[test]
fn freeze_rejects_field_and_cleanup_targets_outside_their_domain() {
    let hir = compile(
        "struct Value { number: int }\nfn main() { let value = Value { number: 1 }\n _ = value.number\n loop { defer {}\n break } }",
    );
    let mut wrong_field = hir.module().clone();
    let expression = wrong_field
        .owners
        .iter_mut()
        .flat_map(|owner| &mut owner.expressions)
        .find(|expression| matches!(expression.kind, hir::ExprKind::Field { .. }))
        .unwrap();
    let hir::ExprKind::Field { index, .. } = &mut expression.kind else {
        unreachable!()
    };
    *index = u32::MAX;
    assert!(hir::Validated::freeze(wrong_field).is_err());
    let mut wrong_cleanup = hir.module().clone();
    let expression = wrong_cleanup
        .owners
        .iter_mut()
        .flat_map(|owner| &mut owner.expressions)
        .find(|expression| matches!(expression.kind, hir::ExprKind::Exit { .. }))
        .unwrap();
    let hir::ExprKind::Exit { cleanup, .. } = &mut expression.kind else {
        unreachable!()
    };
    cleanup.end -= 1;
    assert!(hir::Validated::freeze(wrong_cleanup).is_err());
}

#[test]
fn formatting_counts_are_typed_captures_not_unresolved_names() {
    use crate::frontend::semantics::tests::accepts;
    assert!(!accepts(
        "fn main() { let width: int\n _ = f\"{1:width$}\" }"
    ));
    assert!(!accepts(
        "fn main() { let width = true\n _ = f\"{1:width$}\" }"
    ));
    assert!(!accepts("fn main() { _ = f\"{1:missing$}\" }"));
    assert!(!accepts(
        "fn main() { let width: int\n let render = fn() = f\"{1:width$}\"\n _ = render() }"
    ));
    assert!(accepts(
        "fn main() { let width: int\n let render = fn() = f\"{1:width$}\"\n width = 4\n _ = render() }"
    ));
}
