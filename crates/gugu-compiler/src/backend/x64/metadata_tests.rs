//! 分配后的栈图、统一展开表与源码记录。
//!
//! 这些测试走真实编译：源码进入 codegen，runtime walker 再消费写出的三节。

use crate::backend::x64::metadata::{self, METADATA_SCHEMA};
use crate::runtime::frame_walk;
use crate::{CompileRequest, Compiler, TargetName};

const ADDRESSED: &str = "#[repr(C, align(8))]
struct Pair { a: int, b: int }

fn touch(value: int) int {
    value + 1
}

fn main() {
    let pair = Pair { a: 1, b: 2 }
    let field = &pair.b
    let sum = touch(1)
    if *field + sum > 0 { _ = *field }
}
";

const BRIDGE: &str = "extern \"C\" fn external(value: int) int
#[ffi(dirty_cpu)] extern \"C\" fn dirty(value: int) int
fn main() {
    _ = external(1)
    _ = #[ffi(dirty_cpu)] external(2)
    _ = dirty(3)
}
";

fn compile(source: &str) -> crate::Compilation {
    compile_target(source, TargetName::X86_64Linux)
}

fn compile_target(source: &str, target: TargetName) -> crate::Compilation {
    let compilation =
        Compiler::new().compile(CompileRequest::single_file("main.gg", source, target));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
}

fn audit_of(compilation: &crate::Compilation) -> frame_walk::Audit {
    let metadata = compilation.x64_metadata().expect("机器元数据");
    frame_walk::audit(&metadata.stackmap, &metadata.unwind, &metadata.source)
        .expect("walker 必须接受写出的三节")
}

#[test]
fn section_names_follow_the_target() {
    let linux = compile("fn main() { let value = 1\n _ = value }");
    let windows = compile_target(
        "fn main() { let value = 1\n _ = value }",
        TargetName::X86_64Windows,
    );
    assert_eq!(
        linux.image_plan().unwrap().x64_stackmap_section(),
        ".gugu.stackmap"
    );
    assert_eq!(
        windows.image_plan().unwrap().x64_stackmap_section(),
        ".gugustk"
    );
    assert_eq!(
        linux.image_plan().unwrap().x64_metadata_schema(),
        METADATA_SCHEMA
    );
    assert!(metadata::strip_preserves(
        TargetName::X86_64Linux,
        &[".text", ".debug_info", ".comment"]
    ));
    assert!(!metadata::strip_preserves(
        TargetName::X86_64Linux,
        &[".gugu.stackmap"]
    ));
    assert!(!metadata::strip_preserves(
        TargetName::X86_64Windows,
        &[".gugustk"]
    ));
    assert!(!metadata::strip_preserves(
        TargetName::X86_64Linux,
        &[".gugu.unwind"]
    ));
    assert!(!metadata::strip_preserves(
        TargetName::X86_64Linux,
        &[".gugu.meta"]
    ));
}

#[test]
fn repeated_calls_share_identical_map_bytes() {
    let compilation = compile(include_str!("../../lir/fixtures/stackmap.gg"));
    let metadata = compilation.x64_metadata().expect("机器元数据");
    let audit = audit_of(&compilation);
    assert!(audit.functions > 0);
    assert!(audit.safepoints > 0);
    assert!(audit.maps <= audit.safepoints);
    assert!(audit.maps < audit.safepoints, "相同根图必须去重");
    assert_eq!(audit.kinds[0] > 0, true, "普通调用必须有 CallReturn");
    assert_eq!(audit.kinds[2] > 0, true, "yield 必须有 SuspendResume");
    assert!(metadata.landings == audit.landings);
    assert_eq!(
        compilation.image_plan().unwrap().x64_metadata_fingerprint(),
        compile(include_str!("../../lir/fixtures/stackmap.gg"))
            .image_plan()
            .unwrap()
            .x64_metadata_fingerprint()
    );
}

#[test]
fn poll_bridge_and_morestack_are_consumable() {
    let poll = audit_of(&compile(include_str!("../../lir/fixtures/poll.gg")));
    assert!(poll.kinds[1] > 0, "实际 poll 检查必须留下 PollResume");
    assert!(poll.kinds[4] > 0, "入口栈检查必须留下 MorestackEntry");
    let bridge = audit_of(&compile(BRIDGE));
    assert!(bridge.kinds[3] > 0, "extern 调用必须留下 ForeignBridge");
    let addressed = compile(ADDRESSED);
    let interior = audit_of(&addressed);
    assert!(interior.kinds[4] > 0, "入口栈检查必须留下 MorestackEntry");
    assert!(interior.roots > 0, "walker 必须读回已登记的根");
    assert!(interior.copy_allowed > 0, "允许栈复制的安全点必须被计数");
    let metadata = addressed.x64_metadata().expect("机器元数据");
    let hit = first_safepoint(metadata, 4);
    assert_eq!(hit.kind, 4);
    assert_eq!(hit.frame_size % 16 == 8 || hit.frame_size == 0, true);
}

#[test]
fn calls_publish_a_landing_chain() {
    let compilation = compile(include_str!("../../lir/fixtures/stackmap.gg"));
    let metadata = compilation.x64_metadata().expect("机器元数据");
    let audit = audit_of(&compilation);
    assert!(audit.landings > 0, "可展开调用必须生成 landing");
    let landing = first_landing(metadata);
    let found = frame_walk::landing_at(&metadata.unwind, landing.2 + u64::from(landing.0))
        .expect("landing 查询")
        .expect("范围内必须命中 landing");
    assert_eq!(found.cleanup_chain, landing.1);
    assert_eq!(found.landing_pc, landing.3);
    let plan = compilation.image_plan().unwrap();
    assert_eq!(plan.x64_unwind_landings(), audit.landings);
    assert!(plan.x64_source_records() > 0);
    assert!(plan.x64_stackmap_bytes() > 0);
    assert_eq!(plan.x64_unwind_functions(), plan.x64_stackmap_functions());
}

fn first_safepoint(
    metadata: &super::metadata_section::ImageMetadata,
    kind: u8,
) -> frame_walk::SafepointHit {
    let (functions, _, _) = crate::runtime::stackmap_codec::decode(&metadata.stackmap).unwrap();
    for index in 0..functions {
        let record = function_record(&metadata.stackmap, index);
        for slot in 0..record.2 {
            let point = point_record(&metadata.stackmap, record.1 + slot);
            let pc = record.0 + u64::from(point.1);
            let hit = frame_walk::safepoint_at(&metadata.stackmap, pc).expect("安全点");
            if hit.kind == kind {
                return hit;
            }
        }
    }
    panic!("没有 kind {kind} 的安全点");
}

fn function_record(bytes: &[u8], index: u32) -> (u64, u32, u32) {
    let base = 72 + index as usize * 32;
    let rva = u64::from_le_bytes(bytes[base..base + 8].try_into().unwrap());
    let start = u32::from_le_bytes(bytes[base + 16..base + 20].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[base + 20..base + 24].try_into().unwrap());
    (rva, start, count)
}

fn point_record(bytes: &[u8], index: u32) -> (u8, u32) {
    let functions = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let base = 72 + functions as usize * 32 + index as usize * 12;
    let pc = u32::from_le_bytes(bytes[base..base + 4].try_into().unwrap());
    (bytes[base + 8], pc)
}

fn first_landing(metadata: &super::metadata_section::ImageMetadata) -> (u32, u32, u64, u32) {
    let bytes = &metadata.unwind;
    let functions = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    for index in 0..functions {
        let base = 32 + index as usize * 32;
        let rva = u64::from_le_bytes(bytes[base..base + 8].try_into().unwrap());
        let start = u32::from_le_bytes(bytes[base + 20..base + 24].try_into().unwrap());
        let count = u16::from_le_bytes(bytes[base + 24..base + 26].try_into().unwrap());
        if count == 0 {
            continue;
        }
        let at = 32 + functions as usize * 32 + start as usize * 16;
        let pc_start = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let landing_pc = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap());
        let chain = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap());
        return (pc_start, chain, rva, landing_pc);
    }
    panic!("没有 landing");
}
