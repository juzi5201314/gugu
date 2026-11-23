//! Whole-program assembly tests: minimal bodies plus a modeled runtime catalog.
use super::*;
use crate::lir::body::*;

fn source() -> crate::gir::body::SourceInfo {
    crate::gir::body::SourceInfo {
        location: crate::hir::Location {
            source: 0,
            start: 0,
            end: 0,
            expansion: u32::MAX,
        },
        scope: crate::gir::body::ScopeId(0),
    }
}

fn body(instance: [u8; 32], target: &str) -> Body {
    let memory = ValueId(0);
    Body {
        revision: REVISION,
        instance,
        owner: [0; 32],
        name: "a64_program_fixture".into(),
        target: target.into(),
        signature: Signature {
            parameters: vec![],
            results: vec![],
            sret: None,
            by_value: vec![],
        },
        values: vec![Value {
            kind: ValueType::scalar(Type::Mem),
            definition: Definition::Parameter {
                block: BlockId(0),
                index: 0,
            },
            origin: Origin::None,
            uses: 0..0,
        }],
        blocks: vec![crate::lir::body::Block {
            parameters: 0..1,
            instructions: 0..0,
            predecessors: 0..0,
            terminator: Terminator::Return {
                values: 0..0,
                memory,
            },
            source: source(),
            cleanup: false,
        }],
        instructions: vec![],
        operands: vec![],
        parameters: vec![Parameter {
            value: memory,
            source: None,
        }],
        edges: vec![],
        predecessors: vec![],
        switch_cases: vec![],
        stack_slots: vec![],
        lifetimes: vec![],
        root_inits: vec![],
        safepoints: vec![],
        barrier_permits: vec![],
        no_safepoint_regions: vec![],
        source_scopes: vec![],
        uses: vec![],
        data: vec![],
        assembly: vec![],
        environments: vec![],
        entry: BlockId(0),
        poll_summary: Default::default(),
        poll_interface: PollInterface {
            leaf: true,
            summary: Default::default(),
        },
    }
}

const ENTRY: [u8; 32] = [9; 32];

fn fixture(target: &str) -> (&'static [Body], runtime::Catalog<'static>) {
    let mut bodies = vec![body(ENTRY, target)];
    let mut entries = vec![];
    for (index, glue) in [
        "rt0_start",
        "callback_poll",
        "gc_dispatch",
        "foreign_dispatch",
        "foreign_dirty_dispatch",
    ]
    .into_iter()
    .enumerate()
    {
        let key = [index as u8 + 1; 32];
        bodies.push(body(key, target));
        entries.push((glue, key));
    }
    let bodies: &'static [Body] = Box::leak(bodies.into_boxed_slice());
    (bodies, runtime::Catalog::model(bodies, &entries))
}

#[test]
fn darwin_program_image_links_the_full_startup_contract() {
    let (bodies, catalog) = fixture("aarch64-macos");
    let program = program::build(&program::ProgramRequest {
        bodies,
        entry: ENTRY,
        platform: foreign::Platform::Darwin,
        catalog: &catalog,
        gc_types: &[],
        gc_meta: &[],
        interpreter: "",
        identifier: "test.a64",
        universe: None,
    })
    .unwrap();
    assert_eq!(&program.image[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
}

#[test]
fn linux_program_image_links_the_full_startup_contract() {
    let (bodies, catalog) = fixture("aarch64-linux");
    let program = program::build(&program::ProgramRequest {
        bodies,
        entry: ENTRY,
        platform: foreign::Platform::Linux,
        catalog: &catalog,
        gc_types: &[0xAA; 16],
        gc_meta: &[0xBB; 16],
        interpreter: "/lib/ld-linux-aarch64.so.1",
        identifier: "",
        universe: None,
    })
    .unwrap();
    assert_eq!(&program.image[..4], b"\x7fELF");
    assert!(
        program
            .image
            .windows(26)
            .any(|w| w == b"/lib/ld-linux-aarch64.so.1"),
        "dynamic image must carry PT_INTERP"
    );
}
