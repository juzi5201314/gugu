#![allow(dead_code, reason = "slow-tests feature 关闭时保留测试 helper")]
use super::body::*;
use super::pass::{GIR_PASS_ORDER, GirPass, GirPassStats};
use super::*;
use crate::{SourceMap, SourceSnapshot, TargetName};

pub fn compile_gir(source: &str) -> (crate::hir::Validated, GirWorldV1) {
    compile_sources(&[("main.gg", source)])
}

fn compile_sources(sources: &[(&str, &str)]) -> (crate::hir::Validated, GirWorldV1) {
    let mut sources = SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| SourceSnapshot::from_str(path, source).unwrap())
            .collect(),
    )
    .unwrap();
    let cfg = crate::cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    let output = crate::bootstrap(
        crate::SourceInput::Sources {
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/gir@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
            embed: None,
        },
        &crate::QueryEngine::new(),
        crate::testing::resources(),
    )
    .unwrap_or_else(|errors| panic!("{errors:?}"));
    (output.hir, output.gir)
}

/// 编译单个源文件并返回完整前端产物（含冻结 `mono.universe`）。
pub fn compile_frontend(source: &str) -> crate::FrontendOutput {
    compile_with(&crate::QueryEngine::new(), source)
}

fn compile_with(queries: &crate::QueryEngine, source: &str) -> crate::FrontendOutput {
    let mut sources =
        SourceMap::new(vec![SourceSnapshot::from_str("main.gg", source).unwrap()]).unwrap();
    let cfg = crate::cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    crate::bootstrap(
        crate::SourceInput::Sources {
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/gir@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
            embed: None,
        },
        queries,
        crate::testing::resources(),
    )
    .unwrap()
}

fn entry_body<'a>(hir: &'a crate::hir::Validated, world: &'a GirWorldV1) -> &'a GirBody {
    let entry = hir.module().entry.unwrap();
    world
        .bodies
        .iter()
        .find(|body| body.owner == entry)
        .expect("入口 body")
}

#[test]
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
fn verifier_rejects_mismatched_cleanup() {
    let (hir, mut gir) = compile_gir("fn helper() {}\nfn main() { defer helper() }");
    let body = gir.bodies.first_mut().unwrap();
    if let Some(record) = body.exit_records.first_mut() {
        record.plan = u32::MAX;
    }
    assert!(verify(hir.module(), body).is_err());
}

#[test]
#[ignore = "slow full compilation"]
fn verifier_rejects_send_without_cancelled() {
    let (hir, mut gir) = compile_gir("fn main() { _ = 0 }");
    let body = gir.bodies.first_mut().unwrap();
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
            .position(|ty| matches!(ty, crate::hir::Type::Unit))
            .map(|index| crate::hir::TypeId(index as u32))
            .unwrap_or(crate::hir::TypeId(0)),
        value: body::ConstValue::Unit,
    });
    assert!(verify(hir.module(), body).is_err());
}

#[test]
#[ignore = "slow full compilation"]
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
        crate::canonical::to_vec(&cold.gir.bodies).unwrap(),
        crate::canonical::to_vec(&warm.gir.bodies).unwrap()
    );
}

#[test]
#[ignore = "slow full compilation"]
fn dump_is_deterministic() {
    let (hir, gir) = compile_gir("fn main() { _ = 1 }");
    let first = dump_world(hir.module(), &gir, GirPassStats::default());
    let second = dump_world(hir.module(), &gir, GirPassStats::default());
    assert_eq!(first, second);
    // 版本字与实际 `GIR_REVISION` 同源：字面量会随版本递增静默过期。
    assert!(first.contains(&format!("gir-revision {GIR_REVISION}")));
    assert!(first.contains("body owner=main"));
    assert!(first.contains("placement schema"));
}

fn named_body<'a>(
    hir: &'a crate::hir::Validated,
    world: &'a GirWorldV1,
    name: &str,
) -> &'a GirBody {
    world
        .bodies
        .iter()
        .find(|body| hir.module().definitions[body.owner.index()].name == name)
        .unwrap_or_else(|| panic!("缺少 body {name}"))
}

fn has_rvalue(body: &GirBody, pred: impl Fn(&Rvalue) -> bool) -> bool {
    body.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, rvalue) if pred(rvalue)),
    )
}

#[test]
fn addr_of_keeps_the_place_instead_of_materializing_a_copy() {
    // 回归：`addr_of` 的实参是位点。若先把实参求值成临时量再取址，拿到的是副本的地址：
    // 写穿失效，两个 addr_of 结果之间也不再是对象内的真实偏移。位点会保留投射链
    // （下标/字段/解引用），而被物化的临时量一定落在空投射链的局部上。
    let source = "static CELLS: [uint; 2] = [1, 2]\nfn main() { let p = unsafe { std.ptr.addr_of(CELLS[1]) }\n _ = p }";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    let mut addresses = 0;
    for statement in &body.statements {
        let StatementKind::Assign(_, rvalue) = &statement.kind else {
            continue;
        };
        if let Rvalue::RawAddress(place) = rvalue {
            addresses += 1;
            assert!(
                !body.projections[place.range()].is_empty(),
                "addr_of 必须保留位点的投射链，而不是指向物化后的临时量"
            );
        }
    }
    assert_eq!(addresses, 1, "addr_of 必须恰好产生一条取址");
}

#[test]
#[ignore = "slow full compilation"]
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

#[derive(Debug, Eq, PartialEq)]
enum ResourceEvent {
    Write,
    Release,
}

fn named_local(hir: &hir::Validated, body: &GirBody, name: &str) -> LocalId {
    let owner = hir
        .module()
        .owners
        .iter()
        .find(|owner| owner.definition == body.owner)
        .unwrap();
    let index = body
        .locals
        .iter()
        .position(|local| {
            local
                .hir_local
                .is_some_and(|id| owner.locals[id.index()].name == name)
        })
        .unwrap_or_else(|| panic!("缺少绑定 {name}"));
    LocalId(u32::try_from(index).unwrap())
}

fn resource_path(body: &GirBody, place: Place, local: LocalId, path: &[u32]) -> bool {
    place.local == local && body.projections[place.range()].len() == path.len()
        && body.projections[place.range()].iter().zip(path).all(|(projection, expected)| {
            matches!(projection, Projection::Field { index, .. } | Projection::TupleField { index, .. } if index == expected)
        })
}

fn resource_events(body: &GirBody, local: LocalId, path: &[u32]) -> Vec<ResourceEvent> {
    let range = &body.blocks[body.entry.index()].statements;
    body.statements[usize::try_from(range.start).unwrap()..usize::try_from(range.end).unwrap()]
        .iter()
        .filter_map(|statement| match statement.kind {
            StatementKind::Assign(place, _) if resource_path(body, place, local, path) => {
                Some(ResourceEvent::Write)
            }
            StatementKind::ResourceAction {
                action: ResourceActionKind::ReleaseLease,
                place,
                ..
            } if resource_path(body, place, local, path) => Some(ResourceEvent::Release),
            _ => None,
        })
        .collect()
}

#[test]
#[ignore = "slow full compilation"]
fn resource_fields_first_copy_never_releases_uninitialized() {
    let (hir, gir) = compile_gir(include_str!("fixtures/resource_fields.gg"));
    let body = entry_body(&hir, &gir);
    for name in ["source", "destination"] {
        let local = named_local(&hir, body, name);
        for path in [&[1, 1][..], &[1, 2], &[2]] {
            let events = resource_events(body, local, path);
            assert_eq!(
                events.first(),
                Some(&ResourceEvent::Write),
                "{name}{path:?}: {events:?}"
            );
        }
    }
    for name in ["pair", "pair_copy"] {
        let local = named_local(&hir, body, name);
        for field in [1, 2] {
            let events = resource_events(body, local, &[field]);
            assert_eq!(
                events.first(),
                Some(&ResourceEvent::Write),
                "{name}.{field}: {events:?}"
            );
        }
    }
}

#[test]
#[ignore = "slow full compilation"]
fn resource_projection_overwrite_releases_only_live_fields() {
    use ResourceEvent::{Release, Write};
    let (hir, gir) = compile_gir(include_str!("fixtures/resource_fields.gg"));
    let body = entry_body(&hir, &gir);
    let destination = named_local(&hir, body, "destination");
    assert_eq!(
        resource_events(body, destination, &[1, 1]),
        [Write, Release, Write]
    );
    assert_eq!(
        resource_events(body, destination, &[2]),
        [Write, Release, Write]
    );
    assert_eq!(
        resource_events(body, destination, &[1, 2]),
        [Write, Release, Write, Release, Write, Release, Write]
    );
    let pair = named_local(&hir, body, "pair_copy");
    for field in [1, 2] {
        assert_eq!(
            resource_events(body, pair, &[field]),
            [Write, Release, Write]
        );
    }
    let clone_body = named_body(&hir, &gir, "clone_outer");
    let argument = named_local(&hir, clone_body, "source");
    for path in [&[1, 1][..], &[1, 2], &[2]] {
        assert!(
            clone_body
                .statements
                .iter()
                .any(|statement| matches!(statement.kind,
            StatementKind::ResourceAction { action: ResourceActionKind::ReleaseLease, place, .. }
                if resource_path(clone_body, place, argument, path)))
        );
    }
    assert!(
        !clone_body
            .statements
            .iter()
            .any(|statement| matches!(statement.kind,
        StatementKind::ResourceAction { action: ResourceActionKind::ReleaseLease, place, .. }
            if clone_body.locals[place.local.index()].kind == LocalKind::Return))
    );
}

#[test]
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
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
#[ignore = "slow full compilation"]
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
        let dump = dump_world(hir.module(), &gir, GirPassStats::default());
        assert!(dump.contains(&format!("gir-revision {GIR_REVISION}")));
        assert!(dump.contains("exit plan="));
    }
}

#[test]
fn scoped_view_fixture_is_paired() {
    let module = crate::hir::Module::default();
    let mut body = minimal_body();
    let token = LocalId(1);
    body.locals.push(body::GirLocal {
        ty: crate::hir::TypeId(0),
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
        owner: crate::hir::DefId(0),
        owner_key: [0; 32],
        kind: BodyKind::Function,
        signature: body::Signature {
            parameters: Vec::new(),
            result: crate::hir::TypeId(0),
            effects: 0,
        },
        generic_params: 0,
        locals: vec![body::GirLocal {
            ty: crate::hir::TypeId(0),
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
                location: crate::hir::Location {
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
            location: crate::hir::Location {
                source: 0,
                start: 0,
                end: 0,
                expansion: 0,
            },
            hir_scope: crate::hir::ScopeId(0),
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

#[test]
fn gir_pass_order_is_fixed() {
    assert_eq!(
        GIR_PASS_ORDER,
        &[
            GirPass::Inline,
            GirPass::SimplifyCfg,
            GirPass::SparseConditionalConstants,
            GirPass::CopyPropagationAndGvn,
            GirPass::BoundsCheckElimination,
            GirPass::CowAndResourceElision,
        ]
    );
    assert_eq!(super::pass::GIR_PIPELINE_REVISION, 4);
}

#[test]
#[ignore = "slow full compilation"]
fn inline_expands_eligible_call_and_removes_call_site() {
    let output = compile_with(
        &crate::QueryEngine::new(),
        include_str!("fixtures/inline.gg"),
    );
    assert!(output.gir_stats.inlined >= 1, "纯常量函数必须被内联");
    let entry = output.hir.module().entry.expect("入口 owner");
    let index = output
        .gir
        .bodies
        .iter()
        .position(|body| body.owner == entry)
        .expect("入口 body");
    let concrete = output
        .gir
        .concrete
        .iter()
        .find(|concrete| concrete.generic_body as usize == index)
        .expect("入口具体体");
    assert!(
        !concrete
            .body
            .blocks
            .iter()
            .any(|block| matches!(block.terminator, Terminator::Call { .. })),
        "唯一调用点必须被内联掉"
    );
}

#[test]
#[ignore = "slow full compilation"]
fn bounds_check_elimination_consumes_analysis_proofs() {
    let output = compile_with(
        &crate::QueryEngine::new(),
        include_str!("fixtures/bounds.gg"),
    );
    assert!(
        output.gir_stats.checks_elided >= 1,
        "边界检查必须消费 analysis 证明"
    );
}

#[test]
#[ignore = "slow full compilation"]
fn gir_pipeline_runs_every_registered_pass() {
    let output = compile_with(
        &crate::QueryEngine::new(),
        include_str!("fixtures/bounds.gg"),
    );
    assert_eq!(output.gir_stats.passes, GIR_PASS_ORDER.len() as u32);
}

/// 带捕获环境的闭包在 placement 上形成 region 计划，并记录到分配点的 region 编号。
#[test]
#[ignore = "slow full compilation"]
fn capturing_closure_forms_region_plan() {
    let (hir, gir) = compile_gir(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let body = named_body(&hir, &gir, "main");
    let index = body_index(&gir, body);
    let plan = gir
        .placement
        .regions
        .iter()
        .find(|plan| plan.body == index && plan.allocations > 0)
        .expect("闭包环境必须形成 region 计划");
    assert_eq!(plan.export, 0, "无外部事实时 export summary 必须闭合");
    assert!(!plan.exits.is_empty(), "region 计划必须声明出口");
    assert!(plan.exits.iter().all(|exit| !exit.transfer));
    let site = gir
        .placement
        .allocs
        .iter()
        .find(|alloc| alloc.body == index && alloc.region == Some(plan.region))
        .expect("分配点必须带 region 编号");
    assert_eq!(site.kind, super::placement::PlacementKind::TurnRegion);
}

/// sender 之后不再使用闭包时，channel send 出口整体移交所有权。
#[test]
#[ignore = "slow full compilation"]
fn channel_send_marks_transfer_exit() {
    let (hir, gir) = compile_gir(
        "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n }",
    );
    let body = named_body(&hir, &gir, "main");
    let index = body_index(&gir, body);
    let plan = gir
        .placement
        .regions
        .iter()
        .find(|plan| plan.body == index)
        .expect("移交必须留下 region 计划");
    assert_eq!(plan.exits.len(), 1);
    assert!(plan.exits[0].transfer, "send 出口必须标记移交");
    let site = gir
        .placement
        .allocs
        .iter()
        .find(|alloc| alloc.body == index && alloc.region == Some(plan.region))
        .expect("分配点必须带 region 编号");
    assert_ne!(
        site.export & super::placement::ExportFlags::TRANSFER,
        0,
        "移交语义必须记录在分配点的 export summary 上"
    );
}

/// sender 仍在使用闭包时该段不得选 `TurnRegion`：语义要求退回 stable storage。
#[test]
#[ignore = "slow full compilation"]
fn live_sender_rejects_region_plan() {
    let (hir, gir) = compile_gir(
        "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n _ = closure()\n }",
    );
    let body = named_body(&hir, &gir, "main");
    let index = body_index(&gir, body);
    assert!(
        gir.placement.regions.iter().all(|plan| plan.body != index),
        "跨 send 仍存活的闭包不得进入 region"
    );
    let site = gir
        .placement
        .allocs
        .iter()
        .find(|alloc| {
            alloc.body == index
                && alloc.statement
                    == gir
                        .placement
                        .allocs
                        .iter()
                        .filter(|alloc| alloc.body == index)
                        .map(|alloc| alloc.statement)
                        .max()
                        .expect("分配点")
        })
        .expect("环境分配点");
    assert_eq!(site.kind, super::placement::PlacementKind::SharedHeap);
    assert_eq!(site.region, None);
}

/// 普通 channel 不产生 managed 分配点，也不获得 transfer 语义。
#[test]
#[ignore = "slow full compilation"]
fn channel_allocation_is_runtime_raw_without_region() {
    let (hir, gir) = compile_gir(
        "fn main() {\n let channel = chan[int](1)\n channel.send(1)\n let received = channel.recv()\n _ = received\n }",
    );
    let body = named_body(&hir, &gir, "main");
    let index = body_index(&gir, body);
    assert!(gir.placement.regions.iter().all(|plan| plan.body != index));
    // channel 句柄的存储由 runtime raw 平面持有：它不产生分配点记录，所以既不可能是
    // region 候选，也不可能带 transfer 语义。
    let mut channel_sites = 0;
    for (statement, kind) in body.statements.iter().enumerate() {
        if let StatementKind::Assign(
            _,
            Rvalue::Intrinsic {
                op: IntrinsicOp::ChanNew,
                ..
            },
        ) = &kind.kind
        {
            channel_sites += 1;
            let statement = statement as u32;
            assert!(
                gir.placement
                    .allocs
                    .iter()
                    .all(|alloc| alloc.body != index || alloc.statement != statement),
                "channel 建立点不得产生 managed 分配点"
            );
        }
    }
    assert!(channel_sites >= 1, "样例必须包含 channel 建立点");
    assert_eq!(gir.placement.counts().turn_region, 0);
}

/// 被内层闭包捕获的局部带 ALIAS|ESCAPE，内层环境仍可独占 region。
#[test]
#[ignore = "slow full compilation"]
fn captured_local_is_aliased_while_inner_environment_stays_private() {
    let (hir, gir) = compile_gir(
        "fn main() {\n let value = 1\n let outer = fn() int { return value }\n let inner = fn() int { return outer() }\n _ = inner()\n }",
    );
    let body = named_body(&hir, &gir, "main");
    let index = body_index(&gir, body);
    let captured = body
        .locals
        .iter()
        .enumerate()
        .find(|(_, local)| local.hir_local.is_some() && !local.address_taken)
        .map(|(index, _)| index as u32);
    if let Some(captured) = captured {
        let record = super::placement::record_of(&gir.placement, index, LocalId(captured));
        if let Some(record) = record {
            assert_ne!(
                record.export
                    & (super::placement::ExportFlags::ALIAS
                        | super::placement::ExportFlags::ESCAPE),
                0,
                "被捕获的局部必须标记 ALIAS|ESCAPE"
            );
        }
    }
    assert!(
        gir.placement
            .allocs
            .iter()
            .any(|alloc| alloc.body == index && alloc.region.is_some()),
        "内层环境仍必须独占一个 region"
    );
}

#[test]
#[ignore = "slow full compilation"]
fn runtime_machine_calls_lower_to_machine_statements() {
    let source = "fn main() { unsafe { let code: uint = 0\n std.runtime.process_exit(code)\n _ = std.runtime.current_slot() } }";
    let (hir, gir) = compile_gir(source);
    let body = entry_body(&hir, &gir);
    verify(hir.module(), body).unwrap();
    assert!(body.statements.iter().any(|statement| matches!(
        &statement.kind,
        StatementKind::RuntimeMachineCall {
            op: crate::runtime::RuntimeMachineOp::ProcessExit,
            ..
        }
    )));
    assert!(body.statements.iter().any(|statement| matches!(
        &statement.kind,
        StatementKind::RuntimeMachineCall {
            op: crate::runtime::RuntimeMachineOp::CurrentSlot,
            ..
        }
    )));
    let dump = dump_world(hir.module(), &gir, GirPassStats::default());
    assert!(dump.contains("RuntimeMachineCall process_exit"));
    assert!(dump.contains("RuntimeMachineCall current_slot"));
}
