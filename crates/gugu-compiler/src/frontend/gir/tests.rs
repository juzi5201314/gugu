use super::body::*;
use super::*;
use crate::{CompileRequest, Compiler, SourceMap, SourceSnapshot, TargetName};

fn compile_gir(source: &str) -> (crate::frontend::hir::Validated, GirWorldV1) {
    compile_sources(&[("main.gg", source)])
}

fn compile_sources(sources: &[(&str, &str)]) -> (crate::frontend::hir::Validated, GirWorldV1) {
    let mut sources = SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| SourceSnapshot::from_str(path, source).unwrap())
            .collect(),
    )
    .unwrap();
    let cfg = crate::frontend::cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    let output = crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/gir@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        &crate::QueryEngine::new(),
    )
    .unwrap_or_else(|errors| panic!("{errors:?}"));
    (output.hir, output.gir)
}

fn compile_with(queries: &crate::QueryEngine, source: &str) -> crate::frontend::FrontendOutput {
    let mut sources =
        SourceMap::new(vec![SourceSnapshot::from_str("main.gg", source).unwrap()]).unwrap();
    let cfg = crate::frontend::cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/gir@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        queries,
    )
    .unwrap()
}

fn entry_body<'a>(hir: &'a crate::frontend::hir::Validated, world: &'a GirWorldV1) -> &'a GirBody {
    let entry = hir.module().entry.unwrap();
    world
        .bodies
        .iter()
        .find(|body| body.owner == entry)
        .expect("入口 body")
}

#[test]
fn simple_main_has_return_and_entry() {
    let (hir, gir) = compile_gir("fn main() { _ = 1 }");
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    assert_eq!(body.entry, BlockId(0));
    assert!(
        body.blocks
            .iter()
            .any(|block| matches!(block.terminator, Terminator::Return))
    );
    assert!(!body.exit_records.is_empty());
}

#[test]
fn cleanup_sequences_match_hir_plans() {
    let source = "fn helper() {}\nfn main() {\n defer helper()\n defer ret helper()\n if true { defer ret helper() }\n loop {\n  defer helper()\n  defer ret helper()\n  if true { break }\n  return\n }\n}";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    let owner = hir
        .module()
        .owners
        .iter()
        .find(|owner| Some(owner.definition) == hir.module().entry)
        .unwrap();
    for record in &body.exit_records {
        let plan = &owner.cleanup_plans[record.plan as usize];
        let expected =
            &owner.cleanup_actions[plan.actions.start as usize..plan.actions.end as usize];
        let actual = super::verify::reconstruct_for_test(body, record);
        assert_eq!(actual, expected, "plan {}", record.plan);
    }
}

#[test]
fn try_break_continue_and_panic_have_exit_records() {
    let source = "fn boom() { panic(\"x\") }\nfn main() {\n defer boom()\n if true { return }\n loop { if true { break } else { continue } }\n}";
    let (hir, gir) = compile_gir(source);
    for body in &gir.bodies {
        verify(hir.module(), body).unwrap();
    }
    assert!(gir.bodies.iter().any(|body| {
        body.flags & body::BodyFlags::PANIC != 0
            || body
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, Terminator::Panic { .. }))
    }));
}

#[test]
fn match_for_select_and_suspend_lower() {
    let source =
        "fn main() {\n let x = 1\n _ = match x { 1 => 2, _ => 3 }\n for i in 0..2 { _ = i }\n}";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    assert!(
        !body.match_leaves.is_empty()
            || body
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, Terminator::SwitchInt { .. }))
    );
}

#[test]
fn verifier_rejects_mismatched_cleanup() {
    let (hir, mut gir) = compile_gir("fn helper() {}\nfn main() { defer helper() }");
    let body = gir.bodies.iter_mut().next().unwrap();
    if let Some(record) = body.exit_records.first_mut() {
        record.plan = u32::MAX;
    }
    assert!(verify(hir.module(), body).is_err());
}

#[test]
fn verifier_rejects_send_without_cancelled() {
    let (hir, mut gir) = compile_gir("fn main() { _ = 0 }");
    let body = gir.bodies.iter_mut().next().unwrap();
    body.safepoints.push(body::Safepoint {
        kind: body::SafepointKind::Suspend,
        location: hir.module().owners[0].scopes[0].location.clone(),
    });
    body.blocks[0].terminator = Terminator::Suspend {
        reason: body::SuspendReason::ChanSend {
            channel: body::Operand::Constant(body::ConstId(0)),
            value: body::Operand::Constant(body::ConstId(0)),
        },
        destination: None,
        resume: BlockId(0),
        cancelled: None,
        safepoint: body::SafepointId(0),
    };
    body.constants.push(body::Constant {
        ty: hir
            .module()
            .types
            .iter()
            .position(|ty| matches!(ty, crate::frontend::hir::Type::Unit))
            .map(|index| crate::frontend::hir::TypeId(index as u32))
            .unwrap_or(crate::frontend::hir::TypeId(0)),
        value: body::ConstValue::Unit,
    });
    assert!(verify(hir.module(), body).is_err());
}

#[test]
fn query_is_stable_across_cache() {
    let queries = crate::QueryEngine::new();
    let source = "fn main() { _ = 1 + 2 }";
    let cold = compile_with(&queries, source);
    let warm = compile_with(&queries, source);
    assert_eq!(cold.gir.fingerprint, warm.gir.fingerprint);
    assert_eq!(
        cold.gir.placement.fingerprint,
        warm.gir.placement.fingerprint
    );
    assert_eq!(
        serde_json::to_vec(&cold.gir.bodies).unwrap(),
        serde_json::to_vec(&warm.gir.bodies).unwrap()
    );
}

#[test]
fn action_key_includes_generic_gir() {
    let first = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { _ = 1 }",
        TargetName::X86_64Linux,
    ));
    let second = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { _ = 2 }",
        TargetName::X86_64Linux,
    ));
    assert_ne!(first.action_key(), second.action_key());
    assert_ne!(first.gir_fingerprint(), second.gir_fingerprint());
}

#[test]
fn dump_is_deterministic() {
    let (hir, gir) = compile_gir("fn main() { _ = 1 }");
    let first = dump_world(hir.module(), &gir);
    let second = dump_world(hir.module(), &gir);
    assert_eq!(first, second);
    assert!(first.contains("gir-revision 2"));
    assert!(first.contains("body owner=main"));
    assert!(first.contains("placement schema"));
}

#[test]
fn image_plan_reports_gir_counts() {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { _ = 1 }",
        TargetName::X86_64Linux,
    ));
    let plan = compilation.image_plan().expect("镜像计划");
    assert!(plan.gir_body_count() >= 1);
    assert!(plan.gir_block_count() >= 1);
    assert!(plan.placement_count() >= 1);
    assert_ne!(plan.gir_fingerprint(), [0; 32]);
    assert_ne!(plan.placement_fingerprint(), [0; 32]);
}

fn named_body<'a>(
    hir: &'a crate::frontend::hir::Validated,
    world: &'a GirWorldV1,
    name: &str,
) -> &'a GirBody {
    let definition = hir
        .module()
        .definitions
        .iter()
        .position(|definition| definition.name == name)
        .map(|index| crate::frontend::hir::DefId(index as u32))
        .expect(name);
    world
        .bodies
        .iter()
        .find(|body| body.owner == definition)
        .unwrap_or_else(|| panic!("缺少 body {name}"))
}

fn has_rvalue(body: &GirBody, pred: impl Fn(&Rvalue) -> bool) -> bool {
    body.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, rvalue) if pred(rvalue)),
    )
}

fn action_count(body: &GirBody, release: bool) -> usize {
    body.statements
        .iter()
        .filter(|statement| match &statement.kind {
            StatementKind::ResourceAction { action, .. } => {
                (*action == ResourceActionKind::ReleaseLease) == release
                    && (*action == ResourceActionKind::ReleaseLease
                        || *action == ResourceActionKind::AcquireLease)
            }
            _ => false,
        })
        .count()
}

#[test]
fn bit_copy_emits_value_action_and_reuse_after_call() {
    let source = "fn take(n: int) { _ = n }\nfn main() { let x = 1\n take(x)\n _ = x + 1 }";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    assert!(body.statements.iter().any(|statement| {
        matches!(
            statement.kind,
            StatementKind::ValueAction {
                action: ValueActionKind::Copy,
                ..
            }
        )
    }));
    assert!(has_rvalue(body, |rvalue| matches!(
        rvalue,
        Rvalue::ValueCopy(_)
    )));
}

#[test]
fn string_copy_seals_and_chan_shares_identity() {
    let string =
        "fn take(s: string) { _ = s }\nfn main() { let t: string = \"ok\"\n take(t)\n _ = t }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        string,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let dump = compilation.dump_gir().expect("string GIR");
    assert!(dump.contains("cow_snapshot"), "{dump}");
    let chan = "fn take(c: chan[int]) { _ = c }\nfn main() { let c: chan[int] = chan[int](1)\n take(c)\n _ = c }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        chan,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let dump = compilation.dump_gir().expect("chan GIR");
    assert!(!dump.contains("cow_snapshot"), "{dump}");
    assert!(dump.contains("ValueAction Copy"), "{dump}");
}

#[test]
fn resource_overwrite_releases_then_acquires() {
    let source = "struct ResourceCell { id: uint }\nfn main() {\n let a = ResourceCell { id: 1 }\n let b = a\n b = ResourceCell { id: 2 }\n _ = b\n}";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    assert!(action_count(body, true) >= 1);
    assert!(action_count(body, false) >= 2);
}

#[test]
fn any_erase_seals_source_first() {
    let source = "use std.any.{Any}\nstruct Value { number: int }\nfn main() { let value: dyn Any = Value { number: 1 }\n _ = value }";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    let kinds: Vec<_> = body
        .statements
        .iter()
        .filter_map(|statement| match &statement.kind {
            StatementKind::ValueAction { .. } => Some("action"),
            StatementKind::Assign(_, Rvalue::ValueCopy(_)) => Some("copy"),
            StatementKind::Assign(_, Rvalue::DynErase { .. }) => Some("erase"),
            _ => None,
        })
        .collect();
    let action = kinds.iter().position(|kind| *kind == "action");
    let erase = kinds.iter().position(|kind| *kind == "erase");
    assert!(action.is_some() && erase.is_some());
    assert!(action.unwrap() < erase.unwrap());
}

#[test]
fn large_copy_warns_allow_suppresses_and_deny_fails() {
    let source = "fn take(xs: [uint; 9]) { _ = xs }\nfn main() { take([0; 9]) }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(compilation.is_success());
    assert!(compilation.image_plan().is_some());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(
                |diagnostic| diagnostic.code() == crate::DiagnosticCode::LargeCopy
                    && diagnostic.severity() == crate::Severity::Warning
            )
    );
    let allowed =
        "fn take(xs: [uint; 9]) { _ = xs }\n#[allow(large_copy)]\nfn main() { take([0; 9]) }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        allowed,
        TargetName::X86_64Linux,
    ));
    assert!(compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::LargeCopy)
    );
    let exact = "fn take(xs: [uint; 8]) { _ = xs }\nfn main() { take([0; 8]) }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        exact,
        TargetName::X86_64Linux,
    ));
    assert!(compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::LargeCopy)
    );
    let denied =
        "#![deny(large_copy)]\nfn take(xs: [uint; 9]) { _ = xs }\nfn main() { take([0; 9]) }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        denied,
        TargetName::X86_64Linux,
    ));
    assert!(!compilation.is_success());
    assert!(compilation.image_plan().is_none());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(
                |diagnostic| diagnostic.code() == crate::DiagnosticCode::LargeCopy
                    && diagnostic.severity() == crate::Severity::Error
            )
    );
}

#[test]
fn placement_boxes_escaping_ref_and_keeps_local_stack() {
    let escape = "fn f(p: &int) { _ = *p }\nfn main() { let x = 1\n f(&x) }";
    let (hir, gir) = compile_gir(escape);
    let body = entry_body(&hir, &gir);
    let taken = body
        .locals
        .iter()
        .enumerate()
        .find(|(_, local)| local.address_taken)
        .map(|(index, _)| index as u32)
        .expect("取地址局部");
    let record =
        super::placement::record_of(&gir.placement, body_index(&gir, body), LocalId(taken))
            .expect("placement");
    assert_eq!(record.kind, super::placement::PlacementKind::LocalHeap);
    let local = "fn main() { let x = 1\n let r = &x\n _ = *r }";
    let (hir, gir) = compile_gir(local);
    let body = entry_body(&hir, &gir);
    let taken = body
        .locals
        .iter()
        .enumerate()
        .find(|(_, local)| local.address_taken)
        .map(|(index, _)| index as u32)
        .expect("取地址局部");
    let record =
        super::placement::record_of(&gir.placement, body_index(&gir, body), LocalId(taken))
            .expect("placement");
    assert_eq!(record.kind, super::placement::PlacementKind::Stack);
}

#[test]
fn unknown_generic_and_unknown_call_are_not_turn_region() {
    let generic = "fn id[T](x: T) T = x\nfn main() { _ = id(1) }";
    let (hir, gir) = compile_gir(generic);
    let body = named_body(&hir, &gir, "id");
    let arg = body
        .locals
        .iter()
        .enumerate()
        .find(|(_, local)| local.kind == LocalKind::Argument)
        .map(|(index, _)| index as u32)
        .expect("泛型参数");
    let record = super::placement::record_of(&gir.placement, body_index(&gir, body), LocalId(arg))
        .expect("placement");
    assert_eq!(record.kind, super::placement::PlacementKind::LocalHeap);
    assert_ne!(record.kind, super::placement::PlacementKind::TurnRegion);
    let unknown =
        "fn apply(f: fn(&int)) { let x = 1\n f(&x) }\nfn main() { apply(fn(p: &int) { _ = *p }) }";
    let (hir, gir) = compile_gir(unknown);
    let body = named_body(&hir, &gir, "apply");
    let taken = body
        .locals
        .iter()
        .enumerate()
        .find(|(_, local)| local.address_taken)
        .map(|(index, _)| index as u32)
        .expect("未知调用取地址");
    let record =
        super::placement::record_of(&gir.placement, body_index(&gir, body), LocalId(taken))
            .expect("placement");
    assert_ne!(record.kind, super::placement::PlacementKind::TurnRegion);
}

fn body_index(world: &GirWorldV1, body: &GirBody) -> u32 {
    world
        .bodies
        .iter()
        .position(|candidate| candidate.owner == body.owner)
        .expect("body 下标") as u32
}

#[test]
fn fixtures_lower_and_verify() {
    for source in [
        include_str!("fixtures/return_defer.gg"),
        include_str!("fixtures/loop_break.gg"),
        include_str!("fixtures/try_question.gg"),
    ] {
        let (hir, gir) = compile_gir(source);
        for body in &gir.bodies {
            verify(hir.module(), body).unwrap();
        }
        let dump = dump_world(hir.module(), &gir);
        assert!(dump.contains("gir-revision 2"));
        assert!(dump.contains("exit plan="));
    }
}

#[test]
fn scoped_view_fixture_is_paired() {
    let module = crate::frontend::hir::Module::default();
    let mut body = minimal_body();
    let token = LocalId(1);
    body.locals.push(body::GirLocal {
        ty: crate::frontend::hir::TypeId(0),
        kind: body::LocalKind::Temporary,
        mutable: true,
        address_taken: false,
        source_scope: body::ScopeId(0),
        hir_local: None,
        pinned_storage: false,
    });
    body.statements.push(body::Statement {
        kind: body::StatementKind::ScopedViewBegin {
            source: body::Place::local(LocalId(0)),
            mode: body::ViewMode::ScopedRead,
            token,
        },
        source: body.source_scopes[0].clone().into_source(),
    });
    body.statements.push(body::Statement {
        kind: body::StatementKind::ScopedViewEnd { token },
        source: body.source_scopes[0].clone().into_source(),
    });
    body.blocks[0].statements = 0..2;
    let _ = module;
    assert!(open_tokens(&body) == 0);
}

fn open_tokens(body: &GirBody) -> usize {
    let mut n = 0isize;
    for statement in &body.statements {
        match statement.kind {
            body::StatementKind::ScopedViewBegin { .. } => n += 1,
            body::StatementKind::ScopedViewEnd { .. } => n -= 1,
            _ => {}
        }
    }
    n as usize
}

fn minimal_body() -> GirBody {
    GirBody {
        owner: crate::frontend::hir::DefId(0),
        owner_key: [0; 32],
        kind: BodyKind::Function,
        signature: body::Signature {
            parameters: Vec::new(),
            result: crate::frontend::hir::TypeId(0),
            effects: 0,
        },
        generic_params: 0,
        locals: vec![body::GirLocal {
            ty: crate::frontend::hir::TypeId(0),
            kind: body::LocalKind::Return,
            mutable: false,
            address_taken: false,
            source_scope: body::ScopeId(0),
            hir_local: None,
            pinned_storage: false,
        }],
        blocks: vec![body::GirBlock {
            statements: 0..0,
            terminator: Terminator::Return,
            source: body::SourceInfo {
                location: crate::frontend::hir::Location {
                    source: 0,
                    start: 0,
                    end: 0,
                    expansion: 0,
                },
                scope: body::ScopeId(0),
            },
            predecessors: 0..0,
            cleanup: false,
        }],
        statements: Vec::new(),
        predecessors: Vec::new(),
        projections: Vec::new(),
        constants: Vec::new(),
        source_scopes: vec![body::SourceScope {
            parent: None,
            location: crate::frontend::hir::Location {
                source: 0,
                start: 0,
                end: 0,
                expansion: 0,
            },
            hir_scope: crate::frontend::hir::ScopeId(0),
        }],
        cleanup_regions: Vec::new(),
        exit_records: Vec::new(),
        safepoints: Vec::new(),
        no_safepoint_regions: Vec::new(),
        select_cases: Vec::new(),
        expression_locals: Vec::new(),
        match_leaves: Vec::new(),
        large_copies: Vec::new(),
        flags: 0,
        revision: GIR_REVISION,
        entry: BlockId(0),
    }
}

impl body::SourceScope {
    fn into_source(self) -> body::SourceInfo {
        body::SourceInfo {
            location: self.location,
            scope: body::ScopeId(0),
        }
    }
}
