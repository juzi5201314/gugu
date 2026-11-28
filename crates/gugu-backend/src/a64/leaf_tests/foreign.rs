use super::{
    foreign_vm::{self, Harness as Foreign},
    runtime_vm::{self, Harness},
    *,
};
use crate::{
    gir::body::CallKind,
    lir::foreign::{Hfa, Kind, Signature as CSignature, Type as CType},
};

fn scalar(ty: Type) -> CType {
    let bytes = ty.bytes().unwrap();
    CType {
        bytes,
        align: bytes as u32,
        kind: match ty {
            Type::F32 => Kind::Float { bits: 32 },
            Type::F64 => Kind::Float { bits: 64 },
            Type::Ptr => Kind::Pointer,
            _ => Kind::Integer { signed: false },
        },
    }
}
fn unit() -> CType {
    CType {
        bytes: 0,
        align: 1,
        kind: Kind::Unit,
    }
}
fn aggregate(bytes: u64, hfa: Option<Hfa>) -> CType {
    CType {
        bytes,
        align: 8,
        kind: Kind::Aggregate {
            hfa,
            natural_align: 8,
        },
    }
}
fn fixture(params: &[Type], results: &[Type], darwin: bool) -> Fixture {
    let mut f = Fixture::new(params, results);
    f.body.target = if darwin {
        "aarch64-macos"
    } else {
        "aarch64-linux"
    }
    .into();
    f.body.poll_interface.leaf = false;
    f.body.poll_summary.entry_stack_check = true;
    f.op(Op::StackCheck, &[], &[]);
    f
}
fn call(parameters: Vec<ValueType>, results: Vec<ValueType>) -> Call {
    let foreign_arguments = parameters
        .iter()
        .enumerate()
        .map(|(index, p)| ForeignArgument {
            first: index as u32,
            lanes: 1,
            bytes: p.ty.bytes().unwrap() as u32,
            align: p.ty.bytes().unwrap() as u32,
            memory: false,
        })
        .collect();
    let signature = CSignature {
        arguments: parameters.iter().map(|p| scalar(p.ty)).collect(),
        result: results.first().map_or_else(unit, |p| scalar(p.ty)),
        variadic: None,
    };
    Call {
        target: CallTarget::External {
            key: [7; 32],
            name: "native".into(),
        },
        kind: CallKind::ForeignBridge,
        parameters,
        results,
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: vec![],
        foreign_arguments,
        foreign_signature: Some(signature),
        sret: None,
        poll_free_leaf: false,
        scoped_arguments: Vec::new(),
    }
}
fn invoke(f: &mut Fixture, call: Call, args: &[ValueId]) -> Vec<ValueId> {
    let at = f.body.instructions.len();
    let kind = if call.kind == CallKind::ForeignBridgeDirtyCpu {
        SafepointKind::DirtyCpuBridge
    } else {
        SafepointKind::ForeignBridge
    };
    let results = f.op(
        Op::ForeignCall(call.clone()),
        args,
        &call.results.iter().map(|p| p.ty).collect::<Vec<_>>(),
    );
    f.body.instructions[at].safepoint = Some(SafepointId(f.body.safepoints.len() as u32));
    f.body.safepoints.push(Safepoint {
        kind,
        block: BlockId(0),
        instruction: Some(InstId(at as u32)),
    });
    results
}
fn compile(f: &Fixture) -> codegen::LeafFragment {
    codegen::compile_runtime(&f.body, &foreign_vm::bindings()).unwrap()
}
fn run(
    fragment: &codegen::LeafFragment,
    args: &[u64],
    darwin: bool,
    harness: &mut Harness,
) -> [u64; 2] {
    let mut stub = Fixture::new(&[], &[]).compile();
    stub.symbol = if darwin {
        macho::import_stub_symbol("native")
    } else {
        fragments::linux_import_stub_symbol("native")
    };
    let mut ordinary = stub.clone();
    ordinary.symbol = "ffi_dispatch".into();
    let mut dirty = ordinary.clone();
    dirty.symbol = "ffi_dirty_dispatch".into();
    runtime_vm::run_linked(fragment, &[&stub, &ordinary, &dirty], args, harness)
}

#[test]
fn foreign_bridge_uses_all_eight_integer_lanes_stack_and_dirty_maps() {
    for darwin in [false, true] {
        for dirty in [false, true] {
            let mut f = fixture(&[Type::I64; 9], &[Type::I64], darwin);
            let args = (0..9).map(|index| f.param(0, index)).collect::<Vec<_>>();
            let mut call = call(
                vec![ValueType::scalar(Type::I64); 9],
                vec![ValueType::scalar(Type::I64)],
            );
            if dirty {
                call.kind = CallKind::ForeignBridgeDirtyCpu;
            }
            let result = invoke(&mut f, call, &args);
            f.ret(&result);
            let fragment = compile(&f);
            let mut harness = Harness {
                foreign: Some(Foreign {
                    common_results: [99, 0, 0, 0, 17],
                    dirty,
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(
                run(
                    &fragment,
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9],
                    darwin,
                    &mut harness
                )[0],
                99
            );
            let packet = &harness.foreign.unwrap().packets[0];
            for index in 0..8 {
                let at = crate::runtime::foreign_packet::a64_integer_lane(index).unwrap() as usize;
                assert_eq!(
                    u64::from_le_bytes(packet[at..at + 8].try_into().unwrap()),
                    u64::from(index) + 1
                );
            }
            assert_eq!(u64::from_le_bytes(packet[208..216].try_into().unwrap()), 9);
            assert!(fragment.relocations.iter().any(|r| r.relocation.target
                == if darwin {
                    macho::import_stub_symbol("native")
                } else {
                    fragments::linux_import_stub_symbol("native")
                }));
        }
    }
}

#[test]
fn natural_composite_alignment_reaches_register_and_stack_packets() {
    for darwin in [false, true] {
        for stacked in [false, true] {
            let n = if stacked { 9 } else { 1 };
            let mut f = fixture(&[], &[], darwin);
            let bits = (0..n)
                .map(|i| i as u64)
                .chain([0x400000003f800000, 0x40400000, 10])
                .collect::<Vec<_>>();
            let args = bits
                .iter()
                .map(|bits| f.constant(Type::I64, *bits))
                .collect::<Vec<_>>();
            let mut call = call(vec![ValueType::scalar(Type::I64); n + 3], vec![]);
            let padded = CType {
                bytes: 16,
                align: 16,
                kind: Kind::Aggregate {
                    hfa: None,
                    natural_align: 4,
                },
            };
            let sig = call.foreign_signature.as_mut().unwrap();
            sig.arguments = vec![scalar(Type::I64); n];
            sig.arguments.extend([padded, scalar(Type::I64)]);
            call.foreign_arguments.truncate(n);
            call.foreign_arguments.extend([
                ForeignArgument {
                    first: n as u32,
                    lanes: 2,
                    bytes: 16,
                    align: 16,
                    memory: false,
                },
                ForeignArgument {
                    first: n as u32 + 2,
                    lanes: 1,
                    bytes: 8,
                    align: 8,
                    memory: false,
                },
            ]);
            invoke(&mut f, call, &args);
            f.ret(&[]);
            let fragment = compile(&f);
            let mut harness = Harness {
                foreign: Some(Foreign::default()),
                ..Default::default()
            };
            run(&fragment, &[], darwin, &mut harness);
            let packet = &harness.foreign.unwrap().packets[0];
            let at = if stacked {
                208 + if darwin { 16 } else { 8 }
            } else {
                24
            };
            for (index, expected) in bits[n..].iter().enumerate() {
                let at = at + index * 8;
                assert_eq!(
                    u64::from_le_bytes(packet[at..at + 8].try_into().unwrap()),
                    *expected
                );
            }
        }
    }
}

#[test]
fn padding_only_argument_eightbytes_are_zero_not_the_following_argument() {
    for darwin in [false, true] {
        let mut f = fixture(&[], &[], darwin);
        let value = f.op(Op::FConst(1.5_f64.to_bits()), &[], &[Type::F64])[0];
        let following = f.constant(Type::I64, 0x12345678);
        let mut call = call(
            vec![ValueType::scalar(Type::F64), ValueType::scalar(Type::I64)],
            vec![],
        );
        call.foreign_signature.as_mut().unwrap().arguments[0] = CType {
            bytes: 16,
            align: 16,
            kind: Kind::Aggregate {
                hfa: None,
                natural_align: 8,
            },
        };
        call.foreign_arguments[0].bytes = 16;
        call.foreign_arguments[0].align = 16;
        invoke(&mut f, call, &[value, following]);
        f.ret(&[]);
        let fragment = compile(&f);
        let mut harness = Harness {
            foreign: Some(Foreign::default()),
            ..Default::default()
        };
        run(&fragment, &[], darwin, &mut harness);
        let packet = &harness.foreign.unwrap().packets[0];
        let lane = |at| u64::from_le_bytes(packet[at..at + 8].try_into().unwrap());
        assert_eq!(lane(16), 1.5_f64.to_bits());
        assert_eq!(lane(24), 0);
        assert_eq!(lane(32), 0x12345678);
    }
}

#[test]
fn padding_only_result_eightbytes_do_not_require_lir_lanes() {
    for darwin in [false, true] {
        let mut f = fixture(&[], &[Type::I64], darwin);
        let mut call = call(vec![], vec![ValueType::scalar(Type::F64)]);
        call.foreign_signature.as_mut().unwrap().result = CType {
            bytes: 16,
            align: 16,
            kind: Kind::Aggregate {
                hfa: None,
                natural_align: 8,
            },
        };
        let result = invoke(&mut f, call, &[]);
        let bits = f.op(Op::Convert(Conversion::Bitcast), &result, &[Type::I64]);
        f.ret(&bits);
        let fragment = compile(&f);
        let mut harness = Harness {
            foreign: Some(Foreign {
                common_results: [2.5_f64.to_bits(), 0xdeadbeef, 0, 0, 0],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            run(&fragment, &[], darwin, &mut harness)[0],
            2.5_f64.to_bits()
        );
    }
}

#[test]
fn hfa_arguments_and_four_results_repack_legacy_integer_lanes() {
    let mut f = fixture(&[Type::I64; 2], &[Type::I64; 2], false);
    let hfa = CType {
        bytes: 16,
        align: 4,
        kind: Kind::Aggregate {
            natural_align: 4,
            hfa: Some(Hfa {
                bits: 32,
                offsets: vec![0, 4, 8, 12],
            }),
        },
    };
    let mut call = call(
        vec![ValueType::scalar(Type::I64); 2],
        vec![ValueType::scalar(Type::I64); 2],
    );
    call.foreign_signature = Some(CSignature {
        arguments: vec![hfa.clone()],
        result: hfa,
        variadic: None,
    });
    call.foreign_arguments = vec![ForeignArgument {
        first: 0,
        lanes: 2,
        bytes: 16,
        align: 4,
        memory: false,
    }];
    let args = [f.param(0, 0), f.param(0, 1)];
    let result = invoke(&mut f, call, &args);
    f.ret(&result);
    let fragment = compile(&f);
    let pack = |a: f32, b: f32| u64::from(a.to_bits()) | (u64::from(b.to_bits()) << 32);
    let extra = [
        u64::from(22_f32.to_bits()).to_le_bytes(),
        u64::from(23_f32.to_bits()).to_le_bytes(),
    ]
    .concat();
    let mut harness = Harness {
        foreign: Some(Foreign {
            common_results: [0, 0, 20_f32.to_bits().into(), 21_f32.to_bits().into(), 0],
            outputs: [(200, extra)].into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        run(
            &fragment,
            &[pack(1., 2.), pack(3., 4.)],
            false,
            &mut harness
        ),
        [pack(20., 21.), pack(22., 23.)]
    );
    let packet = &harness.foreign.unwrap().packets[0];
    for index in 0..4 {
        assert_eq!(
            u32::from_le_bytes(packet[64 + index * 8..68 + index * 8].try_into().unwrap()),
            (index as f32 + 1.).to_bits()
        );
    }
}

#[test]
fn apple_variadic_default_promotions_and_narrow_extension_reach_stack_packet() {
    let mut f = fixture(&[], &[], true);
    let float = f.op(Op::FConst(u64::from(1.25_f32.to_bits())), &[], &[Type::F32])[0];
    let narrow = f.constant(Type::I8, 0x80);
    let mut call = call(
        vec![ValueType::scalar(Type::F32), ValueType::scalar(Type::I8)],
        vec![],
    );
    let signature = call.foreign_signature.as_mut().unwrap();
    signature.variadic = Some(0);
    signature.arguments[1].kind = Kind::Integer { signed: true };
    invoke(&mut f, call, &[float, narrow]);
    f.ret(&[]);
    let fragment = compile(&f);
    let mut harness = Harness {
        foreign: Some(Foreign::default()),
        ..Default::default()
    };
    run(&fragment, &[], true, &mut harness);
    let packet = &harness.foreign.unwrap().packets[0];
    assert_eq!(
        u64::from_le_bytes(packet[208..216].try_into().unwrap()),
        1.25_f64.to_bits()
    );
    assert_eq!(
        u32::from_le_bytes(packet[216..220].try_into().unwrap()),
        0xffff_ff80
    );
}

fn slot(f: &mut Fixture, bytes: u64) -> ValueId {
    let slot = SlotId(f.body.stack_slots.len() as u32);
    f.body.stack_slots.push(StackSlot {
        local: slot.0,
        bytes,
        align: 8,
        roots: vec![],
        descriptor: [0; 32],
    });
    f.body.lifetimes.push(Lifetime {
        slot,
        block: BlockId(0),
        position: 0,
        live: true,
    });
    let pointer = f.op(Op::StackAddr(slot), &[], &[Type::Ptr])[0];
    f.body.values[pointer.index()].kind = ValueType::pointer(Provenance::Stack);
    pointer
}

#[test]
fn by_reference_aggregate_and_sret_fixups_survive_coroutine_stack_movement() {
    let mut f = fixture(&[], &[Type::I64; 2], false);
    let input = slot(&mut f, 24);
    let output = slot(&mut f, 24);
    let access = Access {
        alias: AliasClass::Foreign,
        align: 8,
        volatile: false,
    };
    for (index, value) in [11, 22, 33].into_iter().enumerate() {
        let offset = f.constant(Type::I64, index as u64 * 8);
        let pointer = f.op(Op::PtrOffset, &[input, offset], &[Type::Ptr])[0];
        let value = f.constant(Type::I64, value);
        f.op(Op::Store(access.clone()), &[pointer, value], &[]);
    }
    let mut call = call(vec![ValueType::pointer(Provenance::Stack); 2], vec![]);
    call.sret = Some((0, 24, [0; 32]));
    call.by_value = vec![(1, [0; 32], 24)];
    call.foreign_arguments = vec![ForeignArgument {
        first: 1,
        lanes: 1,
        bytes: 24,
        align: 8,
        memory: true,
    }];
    call.foreign_signature = Some(CSignature {
        arguments: vec![aggregate(24, None)],
        result: aggregate(24, None),
        variadic: None,
    });
    invoke(&mut f, call, &[output, input]);
    let first = f.op(Op::Load(access.clone()), &[output], &[Type::I64])[0];
    let offset = f.constant(Type::I64, 8);
    let pointer = f.op(Op::PtrOffset, &[output, offset], &[Type::Ptr])[0];
    let second = f.op(Op::Load(access), &[pointer], &[Type::I64])[0];
    f.ret(&[first, second]);
    let fragment = compile(&f);
    let result = [4_u64, 5, 6]
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect();
    let mut harness = Harness {
        foreign: Some(Foreign {
            outputs: [(192, result)].into(),
            move_stack: 0x10000,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(run(&fragment, &[], false, &mut harness), [4, 5]);
    assert_eq!(
        harness.foreign.unwrap().inputs[&16],
        [11_u64, 22, 33]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect::<Vec<_>>()
    );
}

#[test]
fn empty_and_invalid_foreign_calls_remain_checked() {
    let mut f = fixture(&[], &[], false);
    invoke(&mut f, call(vec![], vec![]), &[]);
    f.ret(&[]);
    let fragment = compile(&f);
    let mut harness = Harness {
        foreign: Some(Foreign::default()),
        ..Default::default()
    };
    run(&fragment, &[], false, &mut harness);
    assert_eq!(harness.foreign.unwrap().packets[0].len(), 208);
    assert!(
        codegen::compile_runtime(&f.body, &runtime_vm::bindings())
            .unwrap_err()
            .0
            .contains("dispatcher bindings")
    );
    let Op::ForeignCall(call) = &mut f.body.instructions[1].op else {
        panic!()
    };
    call.foreign_signature = None;
    assert!(
        codegen::compile_runtime(&f.body, &foreign_vm::bindings())
            .unwrap_err()
            .0
            .contains("canonical")
    );
}

#[test]
fn indirect_calls_keep_the_code_callee_out_of_native_argument_lanes() {
    let mut f = fixture(&[Type::I64], &[Type::I64], true);
    let callee = f.op(
        Op::SymbolAddr(Symbol::External {
            key: [7; 32],
            name: "native".into(),
        }),
        &[],
        &[Type::Ptr],
    )[0];
    f.body.values[callee.index()].kind = ValueType::pointer(Provenance::Code);
    let mut call = call(
        vec![ValueType::scalar(Type::I64)],
        vec![ValueType::scalar(Type::I64)],
    );
    call.target = CallTarget::Indirect;
    call.parameters
        .insert(0, ValueType::pointer(Provenance::Code));
    call.foreign_arguments[0].first = 1;
    let argument = f.param(0, 0);
    let result = invoke(&mut f, call, &[callee, argument]);
    f.ret(&result);
    let fragment = compile(&f);
    let mut harness = Harness {
        foreign: Some(Foreign {
            common_results: [55, 0, 0, 0, 0],
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(run(&fragment, &[42], true, &mut harness)[0], 55);
    assert_eq!(
        u64::from_le_bytes(
            harness.foreign.unwrap().packets[0][16..24]
                .try_into()
                .unwrap()
        ),
        42
    );
}

#[test]
fn invalid_results_stack_addresses_and_runtime_bindings_are_rejected() {
    let mut f = fixture(&[Type::Ptr], &[], false);
    let value = f.param(0, 0);
    f.body.signature.parameters[0] = ValueType::pointer(Provenance::Stack);
    f.body.values[value.index()].kind = ValueType::pointer(Provenance::Stack);
    invoke(
        &mut f,
        call(vec![ValueType::pointer(Provenance::Stack)], vec![]),
        &[value],
    );
    f.ret(&[]);
    assert!(
        codegen::compile_runtime(&f.body, &foreign_vm::bindings())
            .unwrap_err()
            .0
            .contains("stack pointer")
    );
    let mut bindings = foreign_vm::bindings();
    bindings.foreign.as_mut().unwrap().ordinary_dispatch = bindings.morestack_or_poll.clone();
    assert!(
        codegen::compile_runtime(&f.body, &bindings)
            .unwrap_err()
            .0
            .contains("distinct")
    );
    let mut f = fixture(&[], &[Type::I64], false);
    let mut call = call(vec![], vec![ValueType::scalar(Type::I64)]);
    call.foreign_signature.as_mut().unwrap().result = unit();
    let result = invoke(&mut f, call, &[]);
    f.ret(&result);
    assert!(
        codegen::compile_runtime(&f.body, &foreign_vm::bindings())
            .unwrap_err()
            .0
            .contains("return storage")
    );
}

#[test]
fn external_code_addresses_use_apple_stubs_but_data_addresses_keep_raw_symbols() {
    for code in [false, true] {
        let mut f = Fixture::new(&[], &[Type::Ptr]);
        f.body.target = "aarch64-macos".into();
        let result = f.op(
            Op::SymbolAddr(Symbol::External {
                key: [3; 32],
                name: "external".into(),
            }),
            &[],
            &[Type::Ptr],
        )[0];
        if code {
            f.body.values[result.index()].kind = ValueType::pointer(Provenance::Code);
            f.body.signature.results[0] = ValueType::pointer(Provenance::Code);
        }
        f.ret(&[result]);
        let fragment = f.compile();
        assert!(fragment.relocations.iter().all(|r| r.relocation.target
            == if code {
                macho::import_stub_symbol("external")
            } else {
                "external".into()
            }));
    }
}
