use super::*;
use crate::a64::foreign::Platform;
use crate::a64::world::{BuildRequest, Definition, Pointer, World};
use crate::{
    NativeImport,
    macho::{self, SectionContent, Segment},
    x64::mangle,
};

fn fixture(key: u8) -> Fixture {
    let mut f = Fixture::new(&[], &[]);
    f.body.instance = [key; 32];
    f.body.target = "x86_64-macos".into();
    f
}

fn bindings() -> glue::RuntimeBindings {
    glue::RuntimeBindings {
        morestack_or_poll: "morestack".into(),
        safepoint_slow: "poll".into(),
        foreign: None,
    }
}

fn build(
    bodies: &[Body],
    definitions: &[Definition],
    imports: &[NativeImport],
) -> Result<World, Error> {
    codegen::build(BuildRequest {
        bodies,
        entry: [1; 32],
        platform: Platform::Darwin,
        bindings: &bindings(),
        definitions,
        imports,
        universe: None,
    })
}

fn data(name: &str, bytes: Vec<u8>) -> Definition {
    Definition {
        symbol: name.into(),
        align: 8,
        segment: Segment::DataConst,
        content: SectionContent::Data(bytes),
        relocations: vec![],
        pointers: vec![],
    }
}

fn symbol(f: &mut Fixture, symbol: Symbol) -> ValueId {
    f.op(Op::SymbolAddr(symbol), &[], &[Type::Ptr])[0]
}

fn direct(target: [u8; 32]) -> Call {
    Call {
        target: CallTarget::Instance(target),
        kind: crate::gir::body::CallKind::Managed,
        parameters: vec![],
        results: vec![],
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: vec![],
        foreign_signature: None,
        foreign_arguments: vec![],
        sret: None,
        poll_free_leaf: true,
    }
}

#[test]
fn multi_body_world_deduplicates_constants_and_preserves_linked_metadata_and_pointers() {
    let mut a = fixture(1);
    let mut b = fixture(2);
    let global = Symbol::Global {
        key: [8; 32],
        thread_local: false,
        bytes: 16,
    };
    let descriptor = Symbol::TypeDescriptor([9; 32]);
    for (f, align) in [(&mut a, 1), (&mut b, 16)] {
        f.body.data.push(Data {
            bytes: vec![7, 8, 9],
            align,
        });
        symbol(f, Symbol::Data(0));
        symbol(f, global.clone());
        symbol(f, descriptor.clone());
        f.op(Op::CoverageCounter(4), &[], &[]);
        f.ret(&[]);
    }
    a.body.poll_interface.leaf = false;
    a.op(Op::Call(direct(b.body.instance)), &[], &[]);
    a.ret(&[]);
    let mut descriptor = data(&mangle::mangle_symbol(&descriptor), vec![0; 8]);
    descriptor.pointers.push(Pointer {
        offset: 0,
        target: abi::symbol(&b.body.instance),
        addend: 0,
    });
    let mut helper = data(
        "00_helper",
        encode::encode(&machine::inst("ret", vec![inst::Operand::Reg(reg::Reg::x(30))]).unwrap())
            .unwrap()
            .bytes()
            .to_vec(),
    );
    helper.segment = Segment::Text;
    helper.content = SectionContent::Code(match helper.content {
        SectionContent::Data(bytes) => bytes,
        _ => unreachable!(),
    });
    let bodies = [a.body, b.body];
    let morestack = Definition::fragment(
        "morestack",
        fragments::morestack_or_poll("context").fragment,
    );
    let context = Definition::fragment("context", fragments::context_switch().fragment);
    let world = build(
        &bodies,
        &[
            descriptor.clone(),
            helper.clone(),
            morestack.clone(),
            context.clone(),
        ],
        &[],
    )
    .unwrap();
    let reversed = build(
        &[bodies[1].clone(), bodies[0].clone()],
        &[context, helper, morestack, descriptor],
        &[],
    )
    .unwrap();
    assert_eq!(world, reversed);
    let constant = world.symbols[&mangle::const_symbol(0)];
    assert_eq!(constant.offset % 16, 0);
    let SectionContent::Data(bytes) = &world.sections[constant.section].content else {
        panic!()
    };
    assert_eq!(
        &bytes[constant.offset as usize..constant.offset as usize + 3],
        &[7, 8, 9]
    );
    assert!(world.symbols.contains_key(&mangle::mangle_symbol(&global)));
    assert!(
        world
            .symbols
            .contains_key(&crate::a64::world::coverage_symbol(4))
    );
    let SectionContent::Data(maps) = &world.sections[world.maps.section].content else {
        panic!()
    };
    let maps = crate::runtime::stackmap_codec::decode_tables(maps).unwrap();
    assert_eq!(maps.functions.len(), 2);
    assert_ne!(maps.functions[0].code_rva, 0);
    for (function, body) in maps.functions.iter().zip(&bodies) {
        assert_eq!(
            function.code_rva,
            world.symbols[&abi::symbol(&body.instance)].offset
        );
    }
    let request = world.macho_request("a64-world-test").unwrap();
    assert_eq!(request.fixups.len(), 1);
    let call = request
        .relocations
        .iter()
        .find(|rel| {
            rel.kind == inst::RelocKind::Call26
                && rel.target == world.symbols[&abi::symbol(&bodies[1].instance)]
        })
        .unwrap();
    let layout = macho::layout(&request).unwrap();
    let pc = layout.address(call.at).unwrap();
    let image = macho::link(&request).unwrap();
    let at = (layout.sections[call.at.section].fileoff + call.at.offset) as usize;
    let instruction = decode::decode(u32::from_le_bytes(
        image.bytes[at..at + 4].try_into().unwrap(),
    ))
    .unwrap();
    let inst::Operand::Target(inst::Target::Offset(delta)) = instruction.operands[0] else {
        panic!()
    };
    assert_eq!(
        (pc as i64 + delta) as u64,
        layout
            .address(call.target)
            .unwrap()
            .checked_add_signed(call.addend)
            .unwrap()
    );
    assert_eq!(
        u32::from_le_bytes(image.bytes[4..8].try_into().unwrap()),
        0x0100000c
    );
    assert_eq!(image, macho::link(&request).unwrap());
}

fn resolve(mut fragment: codegen::LeafFragment, name: &str, address: u64) -> codegen::LeafFragment {
    for rel in fragment.relocations.drain(..) {
        assert_eq!(rel.relocation.target, name);
        let offset = rel.offset as usize;
        let word = u32::from_le_bytes(fragment.bytes[offset..offset + 4].try_into().unwrap());
        fragment.bytes[offset..offset + 4].copy_from_slice(
            &relocation::patch(word, rel.relocation.kind, rel.offset, address).unwrap(),
        );
    }
    fragment
}

#[test]
fn coverage_executes_wrapping_u32_updates_without_touching_neighbors() {
    let mut f = fixture(1);
    f.op(Op::CoverageCounter(3), &[], &[]);
    f.op(Op::CoverageCounter(3), &[], &[]);
    f.ret(&[]);
    assert!(codegen::compile_runtime(&f.body, &bindings()).is_err());
    let fragment = codegen::compile_image(&f.body, &bindings(), &[], None, None).unwrap();
    let fragment = resolve(fragment, &crate::a64::world::coverage_symbol(3), 0x4000);
    for initial in [0_u32, u32::MAX] {
        let mut memory = (0..8)
            .map(|i| (0x4000 + i, 0xa5))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (i, byte) in initial.to_le_bytes().iter().enumerate() {
            memory.insert(0x4000 + i as u64, *byte);
        }
        let (_, _, memory) = vm::execute_seeded(&fragment, &[], &[], memory);
        assert_eq!(
            u32::from_le_bytes(std::array::from_fn(|i| memory[&(0x4000 + i as u64)])),
            initial.wrapping_add(2)
        );
        for i in 4..8 {
            assert_eq!(memory[&(0x4000 + i)], 0xa5);
        }
    }
}

#[test]
fn imported_data_symbol_evaluates_to_the_bound_pointer_not_slot_address() {
    let mut f = Fixture::new(&[], &[Type::Ptr]);
    f.body.target = "x86_64-macos".into();
    let pointer = symbol(
        &mut f,
        Symbol::External {
            key: [0; 32],
            name: "environment".into(),
        },
    );
    f.ret(&[pointer]);
    let fragment = resolve(
        codegen::compile_image(&f.body, &bindings(), &[], None, None).unwrap(),
        "environment",
        0x5000,
    );
    let memory = 0x7000_u64
        .to_le_bytes()
        .into_iter()
        .enumerate()
        .map(|(i, byte)| (0x5000 + i as u64, byte))
        .collect();
    assert_eq!(vm::execute_seeded(&fragment, &[], &[], memory).0[0], 0x7000);
}

#[test]
fn linux_caller_can_feed_the_same_owned_graph_to_the_existing_elf_writer() {
    let mut f = fixture(1);
    f.body.target = "x86_64-linux".into();
    f.body.data.push(Data {
        bytes: vec![1, 2, 3],
        align: 8,
    });
    symbol(&mut f, Symbol::Data(0));
    f.ret(&[]);
    let world = codegen::build(BuildRequest {
        bodies: &[f.body],
        entry: [1; 32],
        platform: Platform::Linux,
        bindings: &bindings(),
        definitions: &[],
        imports: &[],
        universe: None,
    })
    .unwrap();
    assert!(world.macho_request("linux").is_err());
    let request = elf::ElfRequest {
        sections: world.sections.clone(),
        entry: world.entry,
        relatives: vec![],
        relocations: world
            .relocations
            .iter()
            .map(|rel| elf::Relocation {
                at: rel.at,
                target: world.symbols[&rel.relocation.target],
                addend: rel.relocation.addend,
                kind: rel.relocation.kind,
            })
            .collect(),
    };
    assert_eq!(&elf::link(&request).unwrap().bytes[..4], b"\x7fELF");
}

#[test]
fn darwin_data_import_addresses_load_the_slot_but_function_addresses_use_stubs() {
    let mut f = fixture(1);
    let external = |name: &str| Symbol::External {
        key: [0; 32],
        name: name.into(),
    };
    symbol(&mut f, external("environment"));
    let function = symbol(&mut f, external("getpid"));
    f.body.values[function.index()].kind.provenance = Some(Provenance::Code);
    f.ret(&[]);
    let imports = [
        NativeImport::data(macho::LIB_SYSTEM, "environment"),
        NativeImport::function(macho::LIB_SYSTEM, "getpid"),
    ];
    let mut pointer = data("native-pointer", vec![0; 8]);
    pointer.pointers.push(Pointer {
        offset: 0,
        target: "environment".into(),
        addend: 8,
    });
    let world = build(&[f.body], &[pointer], &imports).unwrap();
    let request = world.macho_request("a64-imports").unwrap();
    assert!(
        request
            .symbols
            .iter()
            .any(|symbol| symbol.name == crate::a64::macho::import_stub_symbol("getpid"))
    );
    assert!(
        !request
            .symbols
            .iter()
            .any(|symbol| symbol.name == crate::a64::macho::import_stub_symbol("environment"))
    );
    let SectionContent::Code(code) = &request.sections[0].content else {
        panic!()
    };
    let add = world
        .relocations
        .iter()
        .filter(|rel| rel.relocation.target == "environment")
        .map(|rel| rel.at.offset)
        .max()
        .unwrap() as usize;
    let load = decode::decode(u32::from_le_bytes(
        code[add + 4..add + 8].try_into().unwrap(),
    ))
    .unwrap();
    assert_eq!(table::get(load.form).unwrap().key, "ldr.X.Unsigned");
    assert!(request.fixups.iter().any(|fixup|matches!(fixup,macho::Fixup::Bind {at,addend:8,..} if *at==world.symbols["native-pointer"])));
    macho::link(&request).unwrap();
}

#[test]
fn world_rejects_missing_empty_duplicate_and_incompatible_inputs() {
    assert!(build(&[], &[], &[]).is_err());
    let mut f = fixture(1);
    f.ret(&[]);
    assert!(build(&[f.body.clone(), f.body.clone()], &[], &[]).is_err());
    assert!(build(&[f.body.clone()], &[data("x", vec![])], &[]).is_err());
    assert!(
        build(
            &[f.body.clone()],
            &[data("x", vec![0]), data("x", vec![1])],
            &[]
        )
        .is_err()
    );
    let mut invalid = data("x", vec![0]);
    invalid.align = 3;
    assert!(build(&[f.body.clone()], &[invalid], &[]).is_err());
    let mut g = fixture(2);
    symbol(
        &mut f,
        Symbol::Global {
            key: [7; 32],
            thread_local: false,
            bytes: 8,
        },
    );
    symbol(
        &mut g,
        Symbol::Global {
            key: [7; 32],
            thread_local: false,
            bytes: 16,
        },
    );
    g.ret(&[]);
    assert!(build(&[f.body, g.body], &[], &[]).is_err());
}

#[test]
fn world_never_invents_descriptor_adapter_tls_or_invalid_blob_addresses() {
    for missing in [
        Symbol::TypeDescriptor([3; 32]),
        Symbol::TypeRecords,
        Symbol::TypeNames,
        Symbol::Vtable {
            interface: [3; 32],
            concrete: [4; 32],
        },
        Symbol::FunctionAdapter {
            target: [2; 32],
            has_environment: false,
        },
        Symbol::Data(1),
    ] {
        let mut f = fixture(1);
        symbol(&mut f, missing);
        f.ret(&[]);
        assert!(build(&[f.body], &[], &[]).is_err());
    }
}

#[test]
fn tls_global_lands_in_bss_like_a_plain_global() {
    let tls = Symbol::Global {
        key: [3; 32],
        thread_local: true,
        bytes: 8,
    };
    let mut f = fixture(1);
    let name = mangle::mangle_symbol(&tls);
    symbol(&mut f, tls);
    f.ret(&[]);
    let world = build(&[f.body], &[], &[]).unwrap();
    assert!(world.symbols.contains_key(&name));
    let bss = world
        .sections
        .iter()
        .find(|section| matches!(&section.content, SectionContent::ZeroFill(bytes) if *bytes > 0))
        .expect("TLS global must materialize a __bss zero-fill section");
    assert!(matches!(bss.segment, Segment::Data));
}

#[test]
fn pointer_and_instruction_relocation_bounds_and_targets_are_checked() {
    let mut f = fixture(1);
    f.ret(&[]);
    for (offset, target, addend) in [
        (1, "x", 0),
        (8, "x", 0),
        (u64::MAX - 7, "x", 0),
        (0, "missing", 0),
        (0, "x", 100),
        (0, "x", -1),
    ] {
        let mut d = data("x", vec![0; 8]);
        d.pointers.push(Pointer {
            offset,
            target: target.into(),
            addend,
        });
        let result =
            build(&[f.body.clone()], &[d], &[]).and_then(|world| world.macho_request("invalid"));
        assert!(result.is_err());
    }
    let mut f = fixture(1);
    symbol(&mut f, Symbol::Data(0));
    f.body.data.push(Data {
        bytes: vec![1],
        align: 1,
    });
    f.ret(&[]);
    let mut world = build(&[f.body], &[], &[]).unwrap();
    world.relocations[0].at.offset = u64::MAX;
    assert!(world.macho_request("bad-relocation").is_err());
}

#[test]
fn typed_code_and_data_definitions_cannot_be_interchanged() {
    for target in [
        Symbol::Instance([2; 32]),
        Symbol::FunctionAdapter {
            target: [2; 32],
            has_environment: false,
        },
    ] {
        let mut f = fixture(1);
        symbol(&mut f, target.clone());
        f.ret(&[]);
        assert!(
            build(
                &[f.body],
                &[data(&mangle::mangle_symbol(&target), vec![0; 8])],
                &[]
            )
            .is_err()
        );
    }
    let mut f = fixture(1);
    let descriptor = Symbol::TypeDescriptor([3; 32]);
    symbol(&mut f, descriptor.clone());
    f.ret(&[]);
    let definition = Definition::fragment(
        mangle::mangle_symbol(&descriptor),
        fragments::context_switch().fragment,
    );
    assert!(build(&[f.body], &[definition], &[]).is_err());
}
