use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, put, word},
    root_trace,
};

const REPEAT: &str = r#"
fn repeat[T, comptime n: int](x: T) [T; n] {
    let a: [T; n]
    for i in 0..n { a[i] = x }
    a
}
fn run() int {
    let a = repeat::[int, 2](7)
    let b = repeat::[int, 4](9)
    let empty = repeat::[int, 0](11)
    let c: [int; 3]
    for i in 0..3 { c[i] = i }
    a[0] + a[1] + b[0] + b[3] + empty.len() + c[0] + c[1] + c[2]
}
fn main() { _ = run() }
"#;

#[test]
fn concrete_generic_and_empty_arrays_execute() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        REPEAT,
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    assert_eq!(
        harness.invoke("main.gg", "run", &[], &mut vec![0; 65536]),
        [35]
    );
}

#[test]
fn init_loop_instruction_count_does_not_scale_with_array_length() {
    let artifact = crate::testing::provider()
        .artifact(TargetName::X86_64Linux)
        .unwrap();
    for (element, rhs, read, lengths) in [
        ("int", "i", "a[0]", [4, 1_000_000]),
        ("&Cell", "p", "a[0].x", [4, 128]),
    ] {
        let counts: Vec<_> = lengths.into_iter().map(|n| {
            let source = format!("struct Cell {{ x: int }}\nfn fill(p: &Cell) int {{ let a: [{element}; {n}]\n for i in 0..{n} {{ a[i] = {rhs} }}\n {read} }}\nfn main() {{ let cell = Cell {{ x: 7 }}\n _ = fill(&cell) }}");
            let (_, lir) = protocol(&source, false, crate::runtime::RuntimeResources::new(artifact));
            let body = lir.bodies().iter().find(|body| body.name == "fill").unwrap();
            (body.blocks.len(), body.instructions.len())
        }).collect();
        assert_eq!(
            counts[0], counts[1],
            "{element}: init-loop code must not scale with N"
        );
    }
}

const MANAGED: &str = r#"
use std.runtime as runtime
struct Cell { x: int }
enum Item { Full(&Cell), Empty(int) }
fn rhs(i: int) Item {
    let value = if i == 1 { Item.Empty(777) } else {
        let cell = Cell { x: i }
        Item.Full(&cell)
    }
    runtime.collect()
    value
}
fn value(item: Item) int { match item { Item.Empty(n) => n, Item.Full(p) => p.x } }
fn run() int {
    let a: [Item; 3]
    for i in 0..3 { a[i] = rhs(i) }
    value(a[0]) + value(a[1]) + value(a[2])
}
fn main() { _ = run() }
"#;

fn collect(harness: &mut Harness<'_>, memory: &mut Vec<u8>) {
    let mut roots: Vec<_> = harness
        .root_frames
        .as_ref()
        .unwrap()
        .borrow()
        .values()
        .flatten()
        .copied()
        .collect();
    assert!(!roots.contains(&777), "非活动 enum 字段不得作为根");
    let mut marked = std::collections::BTreeSet::new();
    let cursor = harness.allocate(memory, 8192);
    let mut next = 0;
    while next < roots.len() {
        let root = roots[next];
        next += 1;
        let Some((&at, &size)) = harness.gc_objects.range(..=root).next_back() else {
            continue;
        };
        if root >= at + size as u64 || !marked.insert(at) {
            continue;
        }
        roots.extend(trace_children(harness, at, size, cursor, memory));
    }
    assert!(!roots.contains(&777), "堆 descriptor 必须按活动变体追踪");
    let unreachable: Vec<_> = harness
        .gc_objects
        .iter()
        .filter(|(at, _)| !marked.contains(at))
        .map(|(at, size)| (*at, *size))
        .collect();
    for (at, size) in unreachable {
        memory[at as usize..at as usize + size].fill(0xa5);
        harness.gc_objects.remove(&at);
    }
    harness.calls.push((0, roots));
}

fn trace_children(
    harness: &mut Harness<'_>,
    at: u64,
    size: usize,
    cursor: u64,
    memory: &mut Vec<u8>,
) -> Vec<u64> {
    let metadata = harness.metadata.as_ref().unwrap();
    let key = harness.gc_descriptors[&at];
    let entry = metadata
        .world
        .types
        .iter()
        .find(|entry| entry.type_key == key)
        .unwrap();
    let start = entry.trace_offset as usize;
    let descriptor = metadata.world.trace_program[start..start + entry.trace_len as usize].to_vec();
    let address = harness.allocate(memory, descriptor.len());
    memory[address as usize..address as usize + descriptor.len()].copy_from_slice(&descriptor);
    let path = "std/runtime/trace.gg";
    harness.invoke(path, "trace_begin", &[cursor, 64], memory);
    let opened = harness.invoke(
        path,
        "trace_open",
        &[cursor, address, descriptor.len() as u64],
        memory,
    )[0];
    if opened == 1 {
        return Vec::new();
    }
    assert_eq!(opened, 0);
    let mut roots = Vec::new();
    loop {
        let status = harness.invoke(
            path,
            "trace_advance",
            &[cursor, address, descriptor.len() as u64, at, size as u64],
            memory,
        )[0];
        if status == 2 {
            return roots;
        }
        assert_eq!(status, 0);
        let offset = harness.invoke(path, "trace_last_word", &[cursor], memory)[0];
        roots.push(word(memory, at + offset * 8));
    }
}

#[test]
fn collecting_in_rhs_preserves_prefix_and_rhs_temporary_and_enum_gates() {
    for stack in [false, true] {
        check_managed_array(stack);
    }
}

fn check_managed_array(stack: bool) {
    let artifact = crate::testing::provider()
        .artifact(TargetName::X86_64Linux)
        .unwrap();
    let (frontend, lir) = protocol(
        MANAGED,
        stack,
        crate::runtime::RuntimeResources::new(artifact),
    );
    if stack {
        assert!(lir.bodies().iter().any(|body| body.name == "run"
            && body.instructions.iter().any(|instruction| matches!(
                instruction.op,
                crate::lir::body::Op::RootInitAddr(_)
            ))));
    }
    let mut harness = Harness::from_bodies(frontend.hir.module(), lir.bodies());
    harness.set_sizes(&frontend.gir);
    harness.metadata = Some(
        crate::frontend::gc::derive(
            &frontend.mono.universe,
            &frontend.gir,
            frontend.hir.module(),
        )
        .unwrap(),
    );
    harness.root_frames = Some(Default::default());
    harness.collect_hook = Some(collect);
    assert_eq!(
        harness.invoke("main.gg", "run", &[], &mut vec![0; 262144]),
        [779]
    );
    assert_eq!(harness.calls.len(), 3);
    assert_eq!(
        harness
            .gc_objects
            .values()
            .filter(|size| **size == 8)
            .count(),
        2,
        "两个输出对象均须存活"
    );
}

#[test]
fn init_loop_root_events_reach_generated_images() {
    for target in [
        TargetName::X86_64Linux,
        TargetName::X86_64Windows,
        TargetName::X86_64Macos,
    ] {
        let output = Compiler::shared()
            .compile_full_for_test(CompileRequest::single_file("main.gg", MANAGED, target));
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        assert!(!output.image_plan().unwrap().gc_type_section().is_empty());
        assert!(
            output
                .lir
                .as_ref()
                .unwrap()
                .bodies()
                .iter()
                .any(|body| body.name == "run" && !body.root_inits.is_empty())
        );
    }
}

fn resource_protocol(source: &str) -> (crate::frontend::FrontendOutput, crate::lir::Validated) {
    protocol(
        source,
        false,
        crate::runtime::RuntimeResources::new(resource_artifact()),
    )
}

fn protocol(
    source: &str,
    stack_arrays: bool,
    resources: crate::runtime::RuntimeResources<'_>,
) -> (crate::frontend::FrontendOutput, crate::lir::Validated) {
    use crate::frontend::{self, PrefixMode, SourceInput, cfg::CfgContext};
    let target = TargetName::X86_64Linux;
    let queries = crate::QueryEngine::new();
    let mut sources = crate::SourceMap::new(vec![
        crate::SourceSnapshot::from_str("main.gg", source).unwrap(),
    ])
    .unwrap();
    let mut frontend = frontend::bootstrap_with(
        SourceInput::Sources {
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "array-init",
            require_main: true,
            cfg: &CfgContext::target_only(target),
            external_packages: &Default::default(),
            embed: None,
        },
        &queries,
        resources,
        PrefixMode::Install,
    )
    .unwrap();
    if stack_arrays {
        for record in &mut frontend.gir.placement.records {
            if frontend.gir.bodies[record.body as usize].locals[record.local as usize]
                .element_initialization
            {
                record.export = 0;
            }
        }
    }
    let lir = crate::lir::build(
        &frontend.hir,
        &frontend.gir,
        &frontend.mono,
        target,
        crate::CompressionPolicyV1::disabled(),
        &crate::backend::x64::oracle::X64Oracle::new(target.descriptor().cpu_baseline),
        &queries,
        &sources,
        (&frontend.installed, resources.prebuilt()),
    )
    .unwrap();
    (frontend, lir)
}

fn resource_artifact() -> &'static crate::runtime::RuntimeArtifactV1 {
    static ARTIFACT: std::sync::LazyLock<crate::runtime::RuntimeArtifactV1> =
        std::sync::LazyLock::new(|| {
            let base = crate::testing::provider()
                .artifact(TargetName::X86_64Linux)
                .unwrap();
            let mut sources: Vec<_> = base
                .payload
                .sources
                .iter()
                .map(|blob| gugu_compiler_core::RuntimeSourceInput {
                    logical_path: &blob.logical_path,
                    source: &blob.source,
                    role: blob.role,
                })
                .collect();
            sources.push(gugu_compiler_core::RuntimeSourceInput {
                logical_path: "std/array_init_probe.gg",
                source: include_str!("fixtures/array_init_resource.gg"),
                role: gugu_compiler_core::RuntimeSourceRole::StandardLibrary,
            });
            let artifact = gugu_compiler_core::RuntimeArtifactBuilder::build(
                gugu_compiler_core::TargetName::X86_64Linux,
                &sources,
            )
            .unwrap();
            crate::frontend::prebuild::materialize_tables(TargetName::X86_64Linux, artifact)
                .unwrap()
        });
    &ARTIFACT
}

fn check_leases(function: &str, panic: bool, expected: &[u64]) {
    let source = format!("use std.array_init_probe as probe\nfn main() {{ probe.{function}() }}");
    let (frontend, lir) = resource_protocol(&source);
    let mut harness = Harness::from_bodies(frontend.hir.module(), lir.bodies());
    harness.set_sizes(&frontend.gir);
    harness.metadata = Some(
        crate::frontend::gc::derive(
            &frontend.mono.universe,
            &frontend.gir,
            frontend.hir.module(),
        )
        .unwrap(),
    );
    harness.root_frames = Some(Default::default());
    let mut memory = vec![0; 262144];
    for i in 0..4 {
        put(&mut memory, 1024 + i * 256, 2);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.invoke("std/array_init_probe.gg", function, &[], &mut memory)
    }));
    if panic {
        assert!(result.unwrap_err().is::<root_trace::InterpretedPanic>());
    } else {
        assert!(result.unwrap().is_empty());
    }
    for (i, count) in expected.iter().enumerate() {
        assert_eq!(word(&memory, 1024 + i as u64 * 256), *count, "control {i}");
    }
    assert!(harness.close_requests.is_empty(), "外部租约必须仍然存活");
}

#[test]
fn overwriting_after_init_releases_old_lease_exactly_once() {
    check_leases("overwrite", false, &[1, 1, 1, 1]);
}

#[test]
fn panicking_rhs_releases_only_the_initialized_prefix() {
    check_leases("partial", true, &[1, 1, 2, 2]);
}

#[test]
fn empty_resource_init_loop_never_evaluates_rhs() {
    check_leases("empty", false, &[2, 2, 2, 2]);
}

#[test]
fn repeated_local_init_loops_release_each_iterations_leases() {
    check_leases("repeated_local", false, &[1, 1, 1, 1]);
}

#[test]
fn repeated_local_init_loop_panic_does_not_release_prior_iterations_leases() {
    check_leases("repeated_local_panic", true, &[1, 1, 2, 2]);
}
