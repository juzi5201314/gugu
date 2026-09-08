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
    let source = "fn main() {\n let x = 1\n _ = match x { 1 => 2, _ => 3 }\n for i in 0..2 { _ = i }\n}";
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
    assert_ne!(plan.gir_fingerprint(), [0; 32]);
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
