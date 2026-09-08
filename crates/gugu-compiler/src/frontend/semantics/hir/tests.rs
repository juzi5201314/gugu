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

fn entry_owner(module: &hir::Module) -> &hir::Owner {
    module
        .owners
        .iter()
        .find(|owner| Some(owner.definition) == module.entry)
        .unwrap()
}

fn plan_actions<'a>(owner: &'a hir::Owner, plan: u32) -> &'a [hir::CleanupAction] {
    let plan = &owner.cleanup_plans[plan as usize];
    &owner.cleanup_actions[plan.actions.start as usize..plan.actions.end as usize]
}

fn cleanup_index(owner: &hir::Owner, statement_start: u32) -> u32 {
    owner
        .cleanup
        .iter()
        .position(|cleanup| {
            owner.statements[cleanup.statement.index()].location.start == statement_start
        })
        .map(|index| index as u32)
        .unwrap()
}

#[test]
fn cleanup_plans_order_block_and_function_exit_actions() {
    // 位置：a=块 defer，r=直线 defer ret，f=分支内 defer ret，b=循环块 defer，c=循环内 defer ret。
    let source = "fn helper() {}\nfn main() {\n defer helper()\n defer ret helper()\n if true { defer ret helper() }\n loop {\n  defer helper()\n  defer ret helper()\n  if true { break }\n  return\n }\n}";
    let hir = compile(source);
    let owner = entry_owner(hir.module());
    let at = |needle: &str, nth: usize| {
        source
            .match_indices(needle)
            .nth(nth)
            .map(|(index, _)| index as u32)
            .unwrap()
    };
    let a = cleanup_index(owner, at("defer helper()", 0));
    let r = cleanup_index(owner, at("defer ret helper()", 0));
    let f = cleanup_index(owner, at("defer ret helper()", 1));
    let b = cleanup_index(owner, at("defer helper()", 1));
    let c = cleanup_index(owner, at("defer ret helper()", 2));
    assert_eq!(
        owner.cleanup[r as usize].registration,
        hir::Registration::Static
    );
    assert_eq!(
        owner.cleanup[f as usize].registration,
        hir::Registration::Flag
    );
    assert_eq!(
        owner.cleanup[c as usize].registration,
        hir::Registration::Chain
    );
    assert_eq!(
        owner.cleanup[a as usize].registration,
        hir::Registration::Static
    );
    let exits: Vec<_> = owner
        .expressions
        .iter()
        .filter_map(|expression| match &expression.kind {
            hir::ExprKind::Exit { target, plan, .. } => Some((*target, *plan)),
            _ => None,
        })
        .collect();
    let (_, break_plan) = exits
        .iter()
        .find(|(target, _)| matches!(target, hir::ExitTarget::Break(_)))
        .unwrap();
    let (_, return_plan) = exits
        .iter()
        .find(|(target, _)| matches!(target, hir::ExitTarget::Return))
        .unwrap();
    use hir::CleanupAction::{Action, DrainChain};
    use hir::Registration::{Flag, Static};
    // break 只离开循环体块：执行本轮块 defer b，不碰函数出口动作。
    assert_eq!(
        plan_actions(owner, *break_plan),
        &[Action {
            cleanup: b,
            guard: Static
        }]
    );
    // return：由内向外块 defer（b 再 a），随后函数出口按站点 LIFO 并在每个站点前消费更晚的链记录。
    assert_eq!(
        plan_actions(owner, *return_plan),
        &[
            Action {
                cleanup: b,
                guard: Static
            },
            Action {
                cleanup: a,
                guard: Static
            },
            DrainChain { until: Some(f) },
            Action {
                cleanup: f,
                guard: Flag
            },
            DrainChain { until: Some(r) },
            Action {
                cleanup: r,
                guard: Static
            },
            DrainChain { until: None },
        ]
    );
    // 函数作用域入口的 Unwind 计划固定为 0：尚无注册，只消费链底。
    assert_eq!(owner.scopes[0].unwind_plan, 0);
    assert_eq!(plan_actions(owner, 0), &[DrainChain { until: None }]);
    // 注册 a 之后的 Unwind 计划包含 a；注册 r 之后包含 a 与 r。
    assert_eq!(
        plan_actions(owner, owner.cleanup[a as usize].unwind_plan),
        &[
            Action {
                cleanup: a,
                guard: Static
            },
            DrainChain { until: None }
        ]
    );
    assert_eq!(
        plan_actions(owner, owner.cleanup[r as usize].unwind_plan),
        &[
            Action {
                cleanup: a,
                guard: Static
            },
            DrainChain { until: Some(r) },
            Action {
                cleanup: r,
                guard: Static
            },
            DrainChain { until: None }
        ]
    );
    // 循环体块正常结束只执行该块 defer；函数体块正常结束执行 a。
    let block_plans: Vec<_> = owner
        .expressions
        .iter()
        .filter_map(|expression| match &expression.kind {
            hir::ExprKind::Block {
                end_plan: Some(plan),
                ..
            } => Some(plan_actions(owner, *plan).to_vec()),
            _ => None,
        })
        .collect();
    assert_eq!(block_plans.len(), 2);
    assert!(block_plans.contains(&vec![Action {
        cleanup: a,
        guard: Static
    }]));
    assert!(block_plans.contains(&vec![Action {
        cleanup: b,
        guard: Static
    }]));
}

#[test]
fn freeze_rejects_cleanup_plans_that_disagree_with_registrations() {
    let hir = compile(
        "fn helper() {}\nfn main() { defer helper()\n if true { return }\n defer ret helper() }",
    );
    let module = hir.module();
    let owner_index = module
        .owners
        .iter()
        .position(|owner| Some(owner.definition) == module.entry)
        .unwrap();
    let mut wrong_guard = module.clone();
    let owner = &mut wrong_guard.owners[owner_index];
    let site = owner
        .cleanup
        .iter()
        .position(|cleanup| cleanup.function_exit)
        .unwrap() as u32;
    for action in &mut owner.cleanup_actions {
        if let hir::CleanupAction::Action { cleanup, guard } = action
            && *cleanup == site
        {
            *guard = hir::Registration::Flag;
        }
    }
    assert!(hir::Validated::freeze(wrong_guard).is_err());
    let mut wrong_exit = module.clone();
    let owner = &mut wrong_exit.owners[owner_index];
    let plan = owner
        .expressions
        .iter()
        .find_map(|expression| match &expression.kind {
            hir::ExprKind::Exit { plan, .. } => Some(*plan),
            _ => None,
        })
        .unwrap();
    owner.cleanup_plans[plan as usize].exit = hir::ExitKind::BlockEnd(hir::ScopeId(1));
    assert!(hir::Validated::freeze(wrong_exit).is_err());
    let mut dropped_chain = module.clone();
    dropped_chain.owners[owner_index]
        .cleanup_actions
        .push(hir::CleanupAction::DrainChain { until: None });
    dropped_chain.owners[owner_index].cleanup_plans[0]
        .actions
        .end += 1;
    assert!(hir::Validated::freeze(dropped_chain).is_err());
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
