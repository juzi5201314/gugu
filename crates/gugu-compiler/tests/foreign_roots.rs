//! 真实 C 按值调用的 caller 参数包必须进入最终编码栈图。
use gugu_compiler::{
    CompileRequest, Compiler, EMBEDDED_ARTIFACTS, NativeImport, NativeLink, TargetName,
};

fn verify(target: TargetName) {
    let mut imports: Vec<_> = ["ordinary", "dirty"]
        .into_iter()
        .map(|name| NativeImport::function("probe.dll", name))
        .collect();
    if target == TargetName::X86_64Linux {
        for import in &mut imports {
            import.library = "libprobe.so".into();
        }
        imports.extend(
            NativeLink::THREAD_SYMBOLS
                .into_iter()
                .map(|name| NativeImport::function("libc.so.6", name)),
        );
    }
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(
        CompileRequest::single_file("main.gg", include_str!("fixtures/foreign_roots.gg"), target)
            .with_native_link(NativeLink {
                imports,
                interpreter: (target == TargetName::X86_64Linux).then(|| "/toolchain/ld.so".into()),
            }),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let section =
        gugu_runtime_abi::decode_tables(output.image_plan().unwrap().x64_stackmap_section())
            .unwrap();
    let expected: &[u32] = if target == TargetName::X86_64Windows {
        &[2, 3, 4, 5, 26, 27, 28, 29]
    } else {
        &[2]
    };
    let mut modes = 0;
    for point in &section.safepoints {
        if point.kind != 3 {
            continue;
        }
        modes |= if point.flags & 8 == 0 { 1 } else { 2 };
        let map = &section.maps[point.map_index as usize];
        assert_eq!(map.registers, [0; 5]);
        for word in expected {
            assert!(
                map.slots[4].contains(word),
                "{target:?} bridge 参数包字 {word} 未登记：{:?}",
                map.slots[4]
            );
        }
        assert!(
            !map.slots
                .iter()
                .any(|slots| slots.contains(&0) || slots.contains(&1)),
            "target/stack_bytes 不是根"
        );
    }
    assert_eq!(modes, 3, "必须同时覆盖 ordinary 和 dirty");
}

#[test]
fn foreign_packet_roots_reach_windows_image() {
    verify(TargetName::X86_64Windows);
}
#[test]
fn foreign_packet_roots_reach_linux_image() {
    verify(TargetName::X86_64Linux);
}
