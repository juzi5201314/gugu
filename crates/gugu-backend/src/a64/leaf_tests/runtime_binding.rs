use super::*;
use crate::a64::runtime::Catalog;
use crate::gir::body::CallKind;
use crate::lir::body::{RuntimeCall, Safepoint, SafepointId, SafepointKind};

fn function(key: u8, parameters: &[Type], results: &[Type]) -> Fixture {
    let mut f = Fixture::new(parameters, results);
    f.body.instance = [key; 32];
    f
}

fn call(target: RuntimeCall, parameters: &[ValueType], results: &[ValueType], leaf: bool) -> Call {
    let (may_unwind, may_suspend, may_allocate, captures_arguments) = target.effects();
    Call {
        target: CallTarget::Runtime(target),
        kind: CallKind::Managed,
        parameters: parameters.to_vec(),
        results: results.to_vec(),
        may_unwind,
        may_suspend,
        may_allocate,
        captures_arguments,
        by_value: vec![],
        foreign_signature: None,
        foreign_arguments: vec![],
        sret: None,
        poll_free_leaf: leaf,
    }
}

fn anchor(f: &mut Fixture, site: usize) {
    let id = SafepointId(f.body.safepoints.len() as u32);
    f.body.instructions[site].safepoint = Some(id);
    f.body.safepoints.push(Safepoint {
        kind: f.body.instructions[site].op.safepoint_kind().unwrap(),
        block: BlockId(0),
        instruction: Some(InstId(site as u32)),
    });
}

fn compile(body: &Body, catalog: &Catalog<'_>) -> Result<codegen::LeafFragment, Error> {
    codegen::compile_image(body, &runtime_vm::bindings(), &[], Some(catalog))
}

#[test]
fn source_runtime_call_uses_the_actual_body_and_preserves_register_stack_abi() {
    let parameters = [Type::I64; 10];
    let mut callee = function(2, &parameters, &[Type::I64]);
    let value = callee.op(
        Op::Integer(IntOp::Add),
        &[callee.param(0, 0), callee.param(0, 9)],
        &[Type::I64],
    );
    callee.ret(&value);
    let mut caller = function(1, &parameters, &[Type::I64]);
    caller.body.poll_interface.leaf = false;
    let args = (0..10).map(|i| caller.param(0, i)).collect::<Vec<_>>();
    let result = caller.op(
        Op::Call(call(
            RuntimeCall::StringCompare,
            &caller.body.signature.parameters.clone(),
            &caller.body.signature.results.clone(),
            true,
        )),
        &args,
        &[Type::I64],
    );
    caller.ret(&result);
    let bodies = [caller.body, callee.body];
    let catalog = Catalog::model(&bodies, &[("string_compare", [2; 32])]);
    let fragment = compile(&bodies[0], &catalog).unwrap();
    assert!(codegen::compile_runtime(&bodies[0], &runtime_vm::bindings()).is_err());
    assert!(
        fragment
            .relocations
            .iter()
            .any(|rel| rel.relocation.target == abi::symbol(&[2; 32]))
    );
    assert_eq!(fragment.frame.outgoing, 16);
    let callee = codegen::compile_runtime(&bodies[1], &runtime_vm::bindings()).unwrap();
    assert_eq!(
        runtime_vm::run_linked(
            &fragment,
            &[&callee],
            &(1..=10).collect::<Vec<_>>(),
            &mut runtime_vm::Harness::default()
        )[0],
        11
    );
}

#[test]
fn runtime_allocation_call_keeps_canonical_allocation_map_and_managed_argument() {
    let mut callee = function(2, &[Type::Ptr], &[Type::Ptr]);
    callee.ret(&[callee.param(0, 0)]);
    let mut caller = function(1, &[Type::Ptr], &[Type::Ptr]);
    caller.body.poll_interface.leaf = false;
    let managed = ValueType::pointer(Provenance::GcHeap);
    let arg = caller.param(0, 0);
    caller.body.values[arg.index()].kind = managed;
    caller.body.signature.parameters[0] = managed;
    caller.body.signature.results[0] = managed;
    let result = caller.op(
        Op::Call(call(RuntimeCall::ValueCopy, &[managed], &[managed], false)),
        &[arg],
        &[Type::Ptr],
    );
    caller.body.values[result[0].index()].kind = managed;
    anchor(&mut caller, 0);
    caller.ret(&result);
    assert_eq!(caller.body.safepoints[0].kind, SafepointKind::Allocation);
    let bodies = [caller.body, callee.body];
    let catalog = Catalog::model(&bodies, &[("value_copy", [2; 32])]);
    let fragment = compile(&bodies[0], &catalog).unwrap();
    assert!(
        fragment
            .runtime_sites
            .iter()
            .any(|site| site.kind == sites::SiteKind::CallReturn)
    );
    let maps =
        crate::runtime::decode_tables(&fragment.metadata.as_ref().unwrap().stackmaps).unwrap();
    assert!(
        maps.maps
            .iter()
            .any(|map| map.slots[0].iter().any(|word| *word != 0))
    );
    let callee = codegen::compile_runtime(&bodies[1], &runtime_vm::bindings()).unwrap();
    assert_eq!(
        runtime_vm::run_linked(
            &fragment,
            &[&callee],
            &[0x4000],
            &mut runtime_vm::Harness::default()
        )[0],
        0x4000
    );
}

#[test]
fn implicit_handle_operation_uses_checked_word_lanes_and_a_real_frame() {
    let mut callee = function(2, &[Type::I64], &[Type::I64]);
    callee.ret(&[callee.param(0, 0)]);
    let mut caller = function(1, &[Type::Ptr], &[Type::Ptr]);
    caller.body.poll_interface.leaf = false;
    let result = caller.op(Op::ResolveSharedHandle, &[caller.param(0, 0)], &[Type::Ptr]);
    anchor(&mut caller, 0);
    caller.ret(&result);
    let bodies = [caller.body, callee.body];
    let catalog = Catalog::model(&bodies, &[("gc_resolve_shared_handle", [2; 32])]);
    let fragment = compile(&bodies[0], &catalog).unwrap();
    assert!(fragment.frame.size >= 32);
    assert_eq!(fragment.frame.max_leaf_reserve, 4096);
    assert!(
        fragment
            .runtime_sites
            .iter()
            .any(|site| site.kind == sites::SiteKind::CallReturn)
    );
    let callee = codegen::compile_runtime(&bodies[1], &runtime_vm::bindings()).unwrap();
    assert_eq!(
        runtime_vm::run_linked(
            &fragment,
            &[&callee],
            &[0x4100],
            &mut runtime_vm::Harness::default()
        )[0],
        0x4100
    );
}

#[test]
fn region_identity_and_export_summary_are_real_synthesized_arguments() {
    let mut callee = function(2, &[Type::I64, Type::I64], &[]);
    let zero = callee.constant(Type::Ptr, 0x5000);
    let access = crate::lir::body::Access {
        alias: AliasClass::Foreign,
        align: 8,
        volatile: false,
    };
    callee.op(Op::Store(access.clone()), &[zero, callee.param(0, 0)], &[]);
    let step = callee.constant(Type::I64, 8);
    let second = callee.op(Op::PtrOffset, &[zero, step], &[Type::Ptr])[0];
    callee.op(Op::Store(access), &[second, callee.param(0, 1)], &[]);
    callee.ret(&[]);
    let mut caller = function(1, &[], &[]);
    caller.body.poll_interface.leaf = false;
    caller.op(
        Op::RegionPublish {
            region: 7,
            export: 3,
        },
        &[],
        &[],
    );
    caller.ret(&[]);
    let bodies = [caller.body, callee.body];
    let catalog = Catalog::model(&bodies, &[("gc_region_publish", [2; 32])]);
    let fragment = compile(&bodies[0], &catalog).unwrap();
    let callee = codegen::compile_runtime(&bodies[1], &runtime_vm::bindings()).unwrap();
    let stubs = runtime_vm::stubs();
    let (_, memory) = vm::execute_inner(
        &fragment,
        &[&callee, &stubs[0], &stubs[1]],
        &[],
        Some(&mut runtime_vm::Harness::default()),
    );
    let word = |at| u64::from_le_bytes(std::array::from_fn(|i| memory[&(at + i as u64)]));
    assert_eq!(word(0x5000), 7);
    assert_eq!(word(0x5008), 3);
}

#[test]
fn select_keeps_its_authoritative_kind_and_suspend_semantics() {
    for has_default in [false, true] {
        let mut callee = function(2, &[], &[]);
        callee.ret(&[]);
        let mut caller = function(1, &[], &[]);
        caller.body.poll_interface.leaf = false;
        caller.op(
            Op::Call(call(
                RuntimeCall::SelectCommit {
                    cases: 0,
                    has_default,
                },
                &[],
                &[],
                false,
            )),
            &[],
            &[],
        );
        anchor(&mut caller, 0);
        caller.ret(&[]);
        assert_eq!(caller.body.safepoints[0].kind, SafepointKind::Select);
        let bodies = [caller.body, callee.body];
        let catalog = Catalog::model(&bodies, &[("select_commit", [2; 32])]);
        let fragment = compile(&bodies[0], &catalog).unwrap();
        assert!(fragment.runtime_sites.iter().any(|site| site.kind
            == if has_default {
                sites::SiteKind::CallReturn
            } else {
                sites::SiteKind::SuspendResume
            }));
    }
}

#[test]
fn missing_wrong_abi_effects_leaf_proofs_and_safepoints_are_errors() {
    for failure in 0..5 {
        let mut callee = function(2, &[Type::I64], &[Type::I64]);
        callee.ret(&[callee.param(0, 0)]);
        let mut caller = function(1, &[Type::I64], &[Type::I64]);
        caller.body.poll_interface.leaf = false;
        let mut target = call(
            RuntimeCall::StringCompare,
            &caller.body.signature.parameters,
            &caller.body.signature.results,
            true,
        );
        if failure == 2 {
            target.may_allocate = true;
        }
        if failure == 3 {
            callee.body.poll_interface.leaf = false;
        }
        if failure == 4 {
            target.poll_free_leaf = false;
        }
        let result = caller.op(Op::Call(target), &[caller.param(0, 0)], &[Type::I64]);
        caller.ret(&result);
        if failure == 1 {
            callee.body.signature.parameters.clear();
        }
        let bodies = [caller.body, callee.body];
        let entries = if failure == 0 {
            vec![]
        } else {
            vec![("string_compare", [2; 32])]
        };
        assert!(compile(&bodies[0], &Catalog::model(&bodies, &entries)).is_err());
    }
}

#[test]
fn runtime_ops_never_invent_maps_or_nonleaf_calls_inside_unmapped_regions() {
    for mapped in [false, true] {
        let returns = if mapped { &[Type::I64][..] } else { &[] };
        let mut callee = function(2, &[Type::I64], returns);
        callee.body.poll_interface.leaf = false;
        let parameter = [callee.param(0, 0)];
        callee.ret(if mapped { &parameter } else { &[] });
        let mut caller = function(1, if mapped { &[Type::I64] } else { &[] }, returns);
        caller.body.poll_interface.leaf = false;
        let op = if mapped {
            Op::ResolveSharedHandle
        } else {
            Op::RegionReset { region: 0 }
        };
        let arguments = if mapped {
            vec![caller.param(0, 0)]
        } else {
            vec![]
        };
        let result = caller.op(op, &arguments, if mapped { &[Type::I64] } else { &[] });
        caller.ret(if mapped { &result } else { &[] });
        let bodies = [caller.body, callee.body];
        let name = if mapped {
            "gc_resolve_shared_handle"
        } else {
            "gc_region_reset"
        };
        let error = compile(&bodies[0], &Catalog::model(&bodies, &[(name, [2; 32])])).unwrap_err();
        assert!(
            error.to_string().contains(if mapped {
                "authoritative safepoint"
            } else {
                "proven leaf"
            }),
            "{error}"
        );
    }
}

#[test]
fn runtime_world_requires_real_compiled_callees_not_same_named_fragments() {
    use crate::{
        a64::{
            foreign::Platform,
            world::{BuildRequest, Definition},
        },
        macho::{SectionContent, Segment},
    };
    let mut callee = function(2, &[], &[]);
    callee.ret(&[]);
    let mut caller = function(1, &[], &[]);
    caller.body.poll_interface.leaf = false;
    caller.op(
        Op::Call(call(RuntimeCall::StringCompare, &[], &[], true)),
        &[],
        &[],
    );
    caller.ret(&[]);
    for f in [&mut caller, &mut callee] {
        f.body.target = "x86_64-macos".into();
    }
    let bodies = [caller.body, callee.body];
    let catalog = Catalog::model(&bodies, &[("string_compare", [2; 32])]);
    let mut definitions = runtime_vm::stubs()
        .into_iter()
        .map(|fragment| Definition {
            symbol: fragment.symbol,
            align: 16,
            segment: Segment::Text,
            content: SectionContent::Code(fragment.bytes),
            relocations: fragment.relocations,
            pointers: vec![],
        })
        .collect::<Vec<_>>();
    let bindings = runtime_vm::bindings();
    let request = |count| BuildRequest {
        bodies: &bodies[..count],
        entry: [1; 32],
        platform: Platform::Darwin,
        bindings: &bindings,
        definitions: &definitions,
        imports: &[],
    };
    let world = codegen::build_runtime(request(2), &catalog).unwrap();
    assert!(world.symbols.contains_key(&abi::symbol(&[2; 32])));
    world.macho_request("source-bound-runtime").unwrap();
    let error = codegen::build_runtime(request(1), &catalog).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires its compiled source body")
    );
    let mut alias = definitions[0].clone();
    alias.symbol = abi::symbol(&[2; 32]);
    definitions.push(alias);
    let error = codegen::build_runtime(
        BuildRequest {
            bodies: &bodies[..1],
            entry: [1; 32],
            platform: Platform::Darwin,
            bindings: &bindings,
            definitions: &definitions,
            imports: &[],
        },
        &catalog,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires its compiled source body")
    );
}

#[test]
fn frozen_catalog_rejects_body_drift_and_an_empty_request() {
    let mut f = function(1, &[], &[]);
    f.body.target = "x86_64-macos".into();
    f.ret(&[]);
    let bodies = [f.body];
    let catalog = Catalog::model(&bodies, &[]);
    let mut changed = bodies[0].clone();
    changed.name.push('!');
    for bodies in [&[][..], &[changed][..]] {
        let result = codegen::build_runtime(
            crate::a64::world::BuildRequest {
                bodies,
                entry: [1; 32],
                platform: crate::a64::foreign::Platform::Darwin,
                bindings: &runtime_vm::bindings(),
                definitions: &[],
                imports: &[],
            },
            &catalog,
        );
        assert!(result.is_err());
    }
}

#[test]
fn frozen_proof_markers_emit_no_calls_and_do_not_change_the_leaf_frame() {
    use crate::gir::body::{NoSafepointReason, ViewMode};
    let mut f = function(1, &[Type::Ptr], &[Type::Ptr]);
    f.body
        .no_safepoint_regions
        .push(NoSafepointReason::OwnershipPublish);
    f.op(Op::NoSafepointBegin(0), &[], &[]);
    f.op(
        Op::ScopedViewBegin {
            mode: ViewMode::ScopedRead,
            token: 4,
        },
        &[f.param(0, 0)],
        &[],
    );
    f.op(Op::ScopedViewEnd { token: 4 }, &[], &[]);
    f.op(Op::NoSafepointEnd(0), &[], &[]);
    f.ret(&[f.param(0, 0)]);
    let bodies = [f.body];
    let catalog = Catalog::model(&bodies, &[]);
    let fragment = compile(&bodies[0], &catalog).unwrap();
    assert_eq!(fragment.frame.size, 0);
    assert!(fragment.relocations.is_empty());
    assert_eq!(vm::run(&fragment, &[0x5000])[0], 0x5000);
}
