//! 分配后的栈图、展开记录与源码记录必须能被 runtime 消费。

use crate::runtime;
use crate::{CompileRequest, Compiler, TargetName};

const CALLS: &str = "fn ping(value: int) int {\n if value <= 0 { return 0 }\n let next = pong(value - 1)\n next + 1\n}\nfn pong(value: int) int {\n if value <= 0 { return 1 }\n let next = ping(value - 1)\n next + 1\n}\nfn main() {\n let index = 0\n let total = 0\n while index < 4 {\n  total = total + ping(index)\n  index = index + 1\n }\n if total > 0 { _ = pong(total) }\n}\n";

fn compile(source: &str, target: TargetName) -> crate::Compilation {
    let compilation =
        Compiler::new().compile(CompileRequest::single_file("main.gg", source, target));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
}

#[test]
fn encoded_metadata_keeps_ranges_and_is_walked() {
    let compilation = compile(CALLS, TargetName::X86_64Linux);
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.x64_stackmap_name(), ".gugu.stackmap");
    assert_eq!(plan.x64_unwind_name(), ".eh_frame");
    assert_eq!(plan.x64_source_name(), ".gugu.src");
    assert!(plan.x64_stackmap_section().starts_with(b"GUGUSM01"));
    assert!(plan.x64_unwind_section().starts_with(b"GUGUUN01"));
    assert!(plan.x64_source_section().starts_with(b"GUGUSRC1"));
    assert!(plan.x64_stackmap_functions() > 0);
    assert!(plan.x64_unwind_functions() >= plan.x64_stackmap_functions());
    assert!(plan.x64_stackmap_safepoints() > 0);
    assert!(plan.x64_stackmap_maps() > 0);
    assert!(plan.x64_stackmap_maps() <= plan.x64_stackmap_safepoints());
    assert!(plan.x64_source_records() > 0);
    assert_ne!(plan.x64_metadata_fingerprint(), [0; 32]);
    let again = compile(CALLS, TargetName::X86_64Linux);
    assert_eq!(
        again
            .image_plan()
            .expect("镜像计划")
            .x64_metadata_fingerprint(),
        plan.x64_metadata_fingerprint()
    );
    let decoded = runtime::decode_tables(plan.x64_stackmap_section()).expect("栈图可解码");
    for pair in decoded.functions.windows(2) {
        let end = pair[0].code_rva + u64::from(pair[0].code_size);
        assert!(pair[0].code_rva < pair[1].code_rva && end <= pair[1].code_rva);
    }
    let mut saw_call = false;
    let mut saw_poll = false;
    let mut saw_entry = false;
    for function in &decoded.functions {
        let start = function.safepoint_start as usize;
        let end = start + function.safepoint_count as usize;
        let points = &decoded.safepoints[start..end];
        assert!(
            points
                .windows(2)
                .all(|pair| pair[0].pc_offset < pair[1].pc_offset)
        );
        for point in points {
            let map = &decoded.maps[point.map_index as usize];
            if matches!(point.kind, 0 | 2 | 3) {
                assert!(map.registers.iter().all(|mask| *mask == 0));
            }
            saw_call |= point.kind == 0;
            saw_poll |= point.kind == 1;
            saw_entry |= point.kind == 4;
        }
    }
    let kinds: Vec<u8> = decoded.safepoints.iter().map(|point| point.kind).collect();
    assert!(saw_call, "缺少 CallReturn：{kinds:?}");
    assert!(saw_poll, "缺少 PollResume：{kinds:?}");
    assert!(saw_entry, "缺少 MorestackEntry：{kinds:?}");
    let consumed =
        runtime::consume_metadata(plan.x64_stackmap_section(), &[], plan.compression_runtime())
            .expect("runtime 必须消费栈图");
    assert_eq!(consumed.functions, plan.x64_stackmap_functions());
    assert_eq!(consumed.safepoints, plan.x64_stackmap_safepoints());
    let names = [
        plan.x64_stackmap_name(),
        plan.x64_unwind_name(),
        plan.x64_source_name(),
        ".debug_info",
        ".symtab",
    ];
    let kept: Vec<_> = names
        .into_iter()
        .filter(|name| !name.starts_with(".debug") && *name != ".symtab")
        .collect();
    assert!(kept.contains(&".gugu.stackmap"));
    assert!(kept.contains(&".eh_frame"));
    assert!(kept.contains(&".gugu.src"));
}

#[test]
fn windows_metadata_uses_pe_section_names() {
    let compilation = compile(CALLS, TargetName::X86_64Windows);
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.x64_stackmap_name(), ".gugustk");
    assert_eq!(plan.x64_unwind_name(), ".xdata");
    assert_eq!(plan.x64_source_name(), ".gugusrc");
    assert!(plan.x64_stackmap_section().starts_with(b"GUGUSM01"));
    assert!(plan.x64_unwind_section().starts_with(b"GUGUUN01"));
    runtime::decode_tables(plan.x64_stackmap_section()).expect("Windows 栈图可解码");
    assert_eq!(plan.linux_image_kind(), "");
    assert!(plan.linux_image().is_empty());
}
