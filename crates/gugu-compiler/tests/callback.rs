use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

fn u16_at(bytes: &[u8], at: usize) -> usize {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize
}
fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}
fn u64_at(bytes: &[u8], at: usize) -> usize {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}
fn name_at(bytes: &[u8], at: usize) -> &str {
    let end = bytes[at..].iter().position(|byte| *byte == 0).unwrap();
    std::str::from_utf8(&bytes[at..at + end]).unwrap()
}
fn verify_elf_exports(bytes: &[u8]) {
    let ph = u64_at(bytes, 32);
    let size = u16_at(bytes, 54);
    let count = u16_at(bytes, 56);
    let headers: Vec<_> = (0..count).map(|index| ph + index * size).collect();
    let map = |address: usize| {
        let &segment = headers
            .iter()
            .find(|&&at| {
                u32_at(bytes, at) == 1
                    && address >= u64_at(bytes, at + 16)
                    && address - u64_at(bytes, at + 16) < u64_at(bytes, at + 32)
            })
            .unwrap();
        u64_at(bytes, segment + 8) + address - u64_at(bytes, segment + 16)
    };
    let &dynamic = headers.iter().find(|&&at| u32_at(bytes, at) == 2).unwrap();
    let mut tags = std::collections::BTreeMap::new();
    let mut at = u64_at(bytes, dynamic + 8);
    while u64_at(bytes, at) != 0 {
        tags.insert(u64_at(bytes, at), u64_at(bytes, at + 8));
        at += 16;
    }
    let strings = map(tags[&5]);
    let symbols = map(tags[&6]);
    let count = u32_at(bytes, map(tags[&4]) + 4);
    let mut names = Vec::new();
    for index in 1..count {
        let at = symbols + index * 24;
        if u16_at(bytes, at + 6) == 0 {
            continue;
        }
        names.push(name_at(bytes, strings + u32_at(bytes, at)));
        let entry = map(u64_at(bytes, at + 8));
        assert_eq!(bytes[entry], 0x55, "导出地址须指向真实 C prologue");
        assert!(u64_at(bytes, at + 16) > 32);
    }
    assert_eq!(names, ["callback_add", "callback_large", "callback_mixed"]);
}
fn verify_pe_exports(bytes: &[u8]) {
    let pe = u32_at(bytes, 60);
    let optional = pe + 24;
    let table = optional + u16_at(bytes, pe + 20);
    let count = u16_at(bytes, pe + 6);
    let map = |address: usize| {
        let section = (0..count)
            .map(|index| table + index * 40)
            .find(|&at| {
                address >= u32_at(bytes, at + 12)
                    && address - u32_at(bytes, at + 12) < u32_at(bytes, at + 8)
            })
            .unwrap();
        u32_at(bytes, section + 20) + address - u32_at(bytes, section + 12)
    };
    let directory = map(u32_at(bytes, optional + 112));
    let count = u32_at(bytes, directory + 24);
    assert_eq!(count, 3);
    let functions = map(u32_at(bytes, directory + 28));
    let names = map(u32_at(bytes, directory + 32));
    let ordinals = map(u32_at(bytes, directory + 36));
    let pdata = map(u32_at(bytes, optional + 112 + 24));
    let pdata_size = u32_at(bytes, optional + 112 + 28);
    let mut found = Vec::new();
    for index in 0..count {
        found.push(name_at(bytes, map(u32_at(bytes, names + index * 4))));
        let entry = u32_at(bytes, functions + u16_at(bytes, ordinals + index * 2) * 4);
        assert_eq!(bytes[map(entry)], 0x55);
        let record = (pdata..pdata + pdata_size)
            .step_by(12)
            .find(|&at| u32_at(bytes, at) == entry)
            .expect("原生入口须有异常展开记录");
        assert!(u32_at(bytes, record + 4) > entry + 32);
        let unwind = map(u32_at(bytes, record + 8));
        assert_eq!(bytes[unwind], 1);
        assert_eq!(bytes[unwind + 2], 7);
        assert_eq!(bytes[unwind + 3], 5);
        assert_eq!(bytes[unwind + 5], 3, "首项按帧指针恢复栈");
        let codes: Vec<_> = (0..6).map(|index| bytes[unwind + 7 + index * 2]).collect();
        assert_eq!(codes, [0xf0, 0xe0, 0xd0, 0xc0, 0x30, 0x50]);
    }
    assert_eq!(found, ["callback_add", "callback_large", "callback_mixed"]);
}

#[test]
fn source_calls_to_c_exports_use_the_native_entry() {
    use gugu_compiler::{NativeImport, NativeLink};
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let imports = if target == TargetName::X86_64Linux {
            NativeLink::THREAD_SYMBOLS
                .into_iter()
                .map(|name| NativeImport::function("libc.so.6", name))
                .collect()
        } else {
            Vec::new()
        };
        let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(
            CompileRequest::single_file(
                "main.gg",
                include_str!("fixtures/callback_local.gg"),
                target,
            )
            .with_native_link(NativeLink {
                imports,
                interpreter: (target == TargetName::X86_64Linux).then(|| "/target/ld.so".into()),
            }),
        );
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        let dump = output.dump_lir().unwrap();
        assert!(
            dump.contains("name: \"callback_identity\""),
            "C 调用必须引用生成的原生入口：{dump}"
        );
    }
}

#[test]
fn c_exports_materialize_managed_callback_adapters() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(
            CompileRequest::single_file("main.gg", include_str!("fixtures/callback.gg"), target),
        );
        assert!(
            output.is_success(),
            "{target:?}: {:?}",
            output.diagnostics().items()
        );
        assert!(
            output.diagnostics().items().is_empty(),
            "{:?}",
            output.diagnostics().items()
        );
        let plan = output.image_plan().unwrap();
        match target {
            TargetName::X86_64Linux => verify_elf_exports(plan.linux_image()),
            TargetName::X86_64Windows => verify_pe_exports(plan.windows_image()),
            TargetName::X86_64Macos => unreachable!(),
            TargetName::Aarch64Linux | TargetName::Aarch64Macos => unreachable!(),
        }
        let lir = output.dump_lir().unwrap();
        for name in ["callback_add", "callback_mixed", "callback_large"] {
            assert!(
                lir.contains(&format!("{name}$callback")),
                "缺少 {name} 受管适配器"
            );
        }
    }
}
