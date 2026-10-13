use super::codegen::BlobRelocation;
use super::elf::{
    Blob, LinkRequest, ObjectReloc, ObjectSymbol, RelocKindCode, apply_relative, claim_slot,
    extract_archive, link, pack_archive, pc_rel32, reject_writable_executable,
};
use crate::{CompileRequest, Compiler, TargetName};
use std::collections::BTreeSet;

fn symbol(name: &str, bytes: Vec<u8>, relocs: Vec<ObjectReloc>) -> ObjectSymbol {
    ObjectSymbol {
        name: name.to_owned(),
        bytes,
        relocs,
        aliases: Vec::new(),
    }
}

fn request(entry: &str, symbols: Vec<ObjectSymbol>) -> LinkRequest {
    LinkRequest {
        entry: entry.to_owned(),
        symbols,
        readonly: vec![
            Blob {
                name: ".gugu.stackmap".to_owned(),
                bytes: b"GUGUSM01".to_vec(),
            },
            Blob {
                name: ".eh_frame".to_owned(),
                bytes: b"GUGUUN01".to_vec(),
            },
        ],
        imports: Vec::new(),
        interpreter: None,
        archive: Vec::new(),
        bound_glues: BTreeSet::new(),
        type_descriptors: std::collections::BTreeMap::new(),
        globals: std::collections::BTreeMap::new(),
        blob_relocations: Vec::new(),
    }
}

#[test]
fn rt0_captures_envp_without_biasing_native_pointer() {
    let bytes = super::elf::rt0_bytes(0);
    assert!(bytes.windows(3).any(|window| window == [0x49, 0x89, 0xf0]));
    assert!(bytes.windows(14).any(|window| {
        window
            == [
                0x48, 0x8b, 0x53, 0x48, 0x48, 0x01, 0xc2, 0x4c, 0x89, 0x82, 0x78, 0x00, 0x00, 0x00,
            ]
    }));
}

#[test]
fn metadata_blob_symbol_relocations_are_filled_at_link_time() {
    let mut prepared = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    let mut meta = b"GUGUMT01".to_vec();
    meta.extend_from_slice(&[0u8; 8]);
    prepared.readonly.push(Blob {
        name: ".gugu.meta".to_owned(),
        bytes: meta,
    });
    prepared.blob_relocations = vec![BlobRelocation {
        offset: 8,
        symbol: "_start".to_owned(),
    }];
    let image = link(&prepared).expect("镜像");
    let at = image
        .bytes
        .windows(8)
        .position(|window| window == b"GUGUMT01")
        .expect("元数据段必须在镜像内");
    let value = u64::from_le_bytes(image.bytes[at + 8..at + 16].try_into().expect("回填字"));
    assert_eq!(value, image.entry, "元数据符号必须回填成链接地址");

    // 符号缺失必须失败，不能留零值让运行期读到无效地址。
    prepared.blob_relocations = vec![BlobRelocation {
        offset: 8,
        symbol: "missing_symbol".to_owned(),
    }];
    let error = link(&prepared).expect_err("缺失符号必须失败");
    assert!(
        error.contains("missing_symbol"),
        "失败信息必须点名符号：{error}"
    );
}

#[test]
fn static_pie_has_no_interpreter_and_keeps_metadata() {
    let image = link(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("static PIE 可写出");
    assert_eq!(image.kind, "static-pie");
    assert!(image.interpreter.is_empty());
    assert_eq!(image.load_count, 3);
    assert_eq!(&image.bytes[0..4], &[0x7f, b'E', b'L', b'F']);
    assert_eq!(
        u16::from_le_bytes(image.bytes[16..18].try_into().unwrap()),
        3
    );
    assert_eq!(
        u16::from_le_bytes(image.bytes[18..20].try_into().unwrap()),
        62
    );
    let phnum = u16::from_le_bytes(image.bytes[56..58].try_into().unwrap());
    assert_eq!(phnum, 5, "static PIE 只有 PHDR、三个 LOAD 与 RELRO");
    let mut loads = 0_u32;
    let mut relro = false;
    let mut interp = false;
    for index in 0..phnum {
        let at = 64 + usize::from(index) * 56;
        let kind = u32::from_le_bytes(image.bytes[at..at + 4].try_into().unwrap());
        let flags = u32::from_le_bytes(image.bytes[at + 4..at + 8].try_into().unwrap());
        match kind {
            1 => {
                loads += 1;
                assert!(flags & 3 != 3, "LOAD 不能同时可写可执行: {flags}");
            }
            3 => interp = true,
            0x6474_e552 => relro = true,
            _ => {}
        }
    }
    assert_eq!(loads, 3);
    assert!(relro);
    assert!(!interp);
    let entry = u64::from_le_bytes(image.bytes[24..32].try_into().unwrap());
    assert_eq!(entry, image.entry);
    assert_eq!(
        &image.bytes[entry as usize..entry as usize + 3],
        &[0x48, 0x8d, 0x1d]
    );
    assert!(image.bytes.windows(8).any(|window| window == b"GUGUSM01"));
    assert!(
        image
            .bytes
            .windows(5)
            .any(|window| window == [0xb8, 60, 0, 0, 0])
    );
    let again = link(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("重放");
    assert_eq!(again.fingerprint, image.fingerprint);
}

#[test]
fn boot_processor_poll_control_starts_clear() {
    use crate::runtime::entry::rt0_params;

    let image = link(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("static PIE 可写出");
    let params = usize::try_from(image.entry + super::elf::rt0_len()).expect("参数块地址");
    let processor = usize::try_from(u64::from_le_bytes(
        image.bytes[params + rt0_params::PROCESSOR * 8..params + (rt0_params::PROCESSOR + 1) * 8]
            .try_into()
            .unwrap(),
    ))
    .expect("processor 地址");
    assert_eq!(&image.bytes[processor..processor + 64], &[0; 64]);
    assert_eq!(
        &image.bytes[processor + 64..processor + 4096],
        &[0xff; 4032]
    );
}

/// 类型段与 metadata 段的载荷地址必须登记进相对重定位：rt0 补加载偏移后运行期读到的是镜像内
/// 真实地址；两段不存在时字段保持 0，且不得被登记成「0 + bias」。
#[test]
fn payload_registers_type_and_metadata_relocations_only_when_present() {
    use crate::runtime::entry::rt0_params;
    let mut bare = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    let mut with_sections = bare.clone();
    let types = with_sections
        .readonly
        .iter()
        .position(|blob| blob.name == ".gugu.types");
    assert!(types.is_none(), "夹具默认不带类型段");
    for name in [".gugu.types", ".gugu.meta"] {
        with_sections.readonly.push(Blob {
            name: name.to_owned(),
            bytes: vec![0_u8; 16],
        });
    }
    for (prepared, present) in [(&mut bare, false), (&mut with_sections, true)] {
        let image = link(prepared).expect("镜像");
        let params =
            usize::try_from(image.entry).expect("入口地址") + super::elf::rt0_len() as usize;
        let word = |index: usize| {
            let at = params + index * 8;
            u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("参数块 word"))
        };
        let payload = usize::try_from(word(rt0_params::PAYLOAD_VA)).expect("载荷地址");
        let payload_word = |offset: usize| {
            let at = payload + offset;
            u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("载荷 word"))
        };
        let slots: Vec<u64> = {
            let table = usize::try_from(word(rt0_params::RELOC_TABLE)).expect("重定位表");
            let count = usize::try_from(word(rt0_params::RELOC_COUNT)).expect("重定位条数");
            (0..count)
                .map(|index| {
                    let at = table + index * 16;
                    u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("重定位槽"))
                })
                .collect()
        };
        for (offset, field) in [(88_usize, "gc_types"), (104, "gc_meta")] {
            assert_eq!(
                slots.contains(&(payload as u64 + offset as u64)),
                present,
                "{field} 的重定位登记必须与段是否存在一致：present={present}"
            );
            assert_eq!(
                payload_word(offset) != 0,
                present,
                "{field} 的载荷值必须与段是否存在一致"
            );
        }
        assert_eq!(
            payload_word(96) != 0,
            present,
            "gc_types_len 必须与类型段是否存在一致"
        );
        assert_eq!(
            payload_word(112) != 0,
            present,
            "gc_meta_len 必须与 metadata 段是否存在一致"
        );
        assert_eq!(
            payload_word(120),
            0,
            "boot_environment 由 rt0 运行期写入，镜像内必须为 0 且不登记重定位"
        );
    }
}

#[test]
fn relative_reloc_uses_load_bias() {
    let mut bytes = vec![0_u8; 16];
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes.extend_from_slice(&0x40_u64.to_le_bytes());
    apply_relative(&mut bytes, 16, 1, 0x1000).expect("回填");
    let value = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    assert_eq!(value, 0x1040);
}

#[test]
fn rt0_param_block_carries_runtime_entry_and_reserved_regions() {
    use crate::runtime::entry::{Rt0Payload, rt0_params};
    let image = link(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("static PIE 可写出");
    let params = usize::try_from(image.entry).expect("入口地址") + super::elf::rt0_len() as usize;
    let word = |index: usize| {
        let at = params + index * 8;
        u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("参数块 word"))
    };
    assert_eq!(rt0_params::WORDS, 16);
    assert_eq!(
        word(rt0_params::RT0_START),
        0,
        "未绑定 rt0_start 时留 0 走过渡路径"
    );
    let payload = usize::try_from(word(rt0_params::PAYLOAD_VA)).expect("载荷地址");
    assert_eq!(word(rt0_params::PAYLOAD_LEN), Rt0Payload::bytes());
    let payload_word = |offset: usize| {
        let at = payload + offset;
        u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("载荷 word"))
    };
    assert_eq!(payload_word(0), Rt0Payload::REVISION, "载荷版本");
    assert_eq!(
        payload_word(8),
        word(rt0_params::ENTRY),
        "载荷的 main 入口与参数块一致"
    );
    assert_eq!(payload_word(16), word(rt0_params::INITIAL_SLOT));
    assert_eq!(payload_word(24), word(rt0_params::RUNTIME_STATE));
    assert_eq!(payload_word(32), 0, "加载偏移写回槽在载荷内且初值为 0");
    let slot = word(rt0_params::INITIAL_SLOT);
    let state = word(rt0_params::RUNTIME_STATE);
    assert_eq!(slot % 64, 0, "初始协程槽按 64 对齐");
    assert_eq!(state % 8, 0, "runtime 状态区按 8 对齐");
    assert_eq!(slot % 4096, 0, "保留区独占页，rt0 的页粒度收尾不能覆盖它");
    let protect_end = word(rt0_params::PROTECT_VA) + word(rt0_params::PROTECT_LEN);
    assert!(
        slot >= protect_end,
        "保留可写区必须在 rt0 收尾的只读范围之外：slot={slot} protect_end={protect_end}"
    );
    let table = usize::try_from(word(rt0_params::RELOC_TABLE)).expect("重定位表地址");
    let count = usize::try_from(word(rt0_params::RELOC_COUNT)).expect("重定位条数");
    let payload_end = u64::try_from(payload).expect("载荷地址") + Rt0Payload::bytes();
    let in_payload = (0..count)
        .filter(|index| {
            let at = table + index * 16;
            let target = u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("重定位槽"));
            (u64::try_from(payload).expect("载荷地址")..payload_end).contains(&target)
        })
        .count();
    assert_eq!(in_payload, 7, "载荷七个地址字段都登记为相对重定位");
}

#[test]
fn duplicate_slot_and_out_of_range_pc_are_rejected() {
    let mut seen = BTreeSet::new();
    claim_slot(&mut seen, 8).expect("第一次占用");
    assert!(claim_slot(&mut seen, 8).is_err());
    assert!(pc_rel32(0, 0x1_0000_0000, 0).is_err());
    assert!(reject_writable_executable(true, true).is_err());
    let image = link(&request(
        "main",
        vec![symbol(
            "main",
            vec![0, 0, 0, 0, 0xc3],
            vec![
                ObjectReloc {
                    offset: 0,
                    kind: RelocKindCode::PcRel32,
                    target: "main".to_owned(),
                    addend: 0,
                },
                ObjectReloc {
                    offset: 0,
                    kind: RelocKindCode::PcRel32,
                    target: "main".to_owned(),
                    addend: 0,
                },
            ],
        )],
    ));
    assert!(image.is_err(), "重复槽必须在写出前失败");
    let unknown = link(&request(
        "main",
        vec![symbol(
            "main",
            vec![0xc3],
            vec![ObjectReloc {
                offset: 0,
                kind: RelocKindCode::Unknown(99),
                target: "main".to_owned(),
                addend: 0,
            }],
        )],
    ));
    assert!(unknown.is_err());
    assert!(
        link(&request(
            "missing",
            vec![symbol("main", vec![0xc3], Vec::new())]
        ))
        .is_err()
    );
}

#[test]
fn dynamic_import_adds_interpreter_from_the_descriptor() {
    let mut request = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    request
        .imports
        .push(("libc.so.6".to_owned(), "write".to_owned()));
    request.interpreter = Some("/lib64/ld-linux-x86-64.so.2".to_owned());
    let image = link(&request).expect("动态 PIE");
    assert_eq!(image.kind, "dynamic-pie");
    assert_eq!(image.interpreter, "/lib64/ld-linux-x86-64.so.2");
    let phnum = u16::from_le_bytes(image.bytes[56..58].try_into().unwrap());
    let mut saw = false;
    for index in 0..phnum {
        let at = 64 + usize::from(index) * 56;
        let kind = u32::from_le_bytes(image.bytes[at..at + 4].try_into().unwrap());
        if kind == 3 {
            saw = true;
            let va = u64::from_le_bytes(image.bytes[at + 16..at + 24].try_into().unwrap()) as usize;
            let len =
                u64::from_le_bytes(image.bytes[at + 32..at + 40].try_into().unwrap()) as usize;
            let path = &image.bytes[va..va + len];
            assert_eq!(path, b"/lib64/ld-linux-x86-64.so.2\0");
        }
    }
    assert!(saw);
    assert_eq!(phnum, 7);
    assert!(image.bytes.windows(9).any(|window| window == b"libc.so.6"));
    let mut dynamic = false;
    for index in 0..phnum {
        let at = 64 + usize::from(index) * 56;
        let kind = u32::from_le_bytes(image.bytes[at..at + 4].try_into().unwrap());
        if kind == 2 {
            dynamic = true;
            let va = u64::from_le_bytes(image.bytes[at + 16..at + 24].try_into().unwrap()) as usize;
            let len =
                u64::from_le_bytes(image.bytes[at + 32..at + 40].try_into().unwrap()) as usize;
            let bytes = &image.bytes[va..va + len];
            assert!(bytes.windows(8).any(|window| window == 1_i64.to_le_bytes()));
            assert_ne!(&image.bytes[va..va + 8], b"/lib64/ld");
        }
    }
    assert!(dynamic);
    assert!(image.bytes.windows(2).any(|window| window == [0xff, 0x25]));
}

#[test]
fn archive_extracts_only_the_named_member() {
    let archive = pack_archive(&[("write", b"write-bytes"), ("read", b"read-bytes")]);
    let got = extract_archive(&archive, &["write".to_owned()]).expect("抽取");
    assert_eq!(got, vec![b"write-bytes".to_vec()]);
    assert!(extract_archive(&archive, &["missing".to_owned()]).is_err());
}

#[test]
fn archive_member_resolves_an_unresolved_c_symbol() {
    let mut missing = request(
        "main",
        vec![symbol(
            "main",
            vec![0xe8, 0, 0, 0, 0, 0xc3],
            vec![ObjectReloc {
                offset: 1,
                kind: RelocKindCode::PcRel32,
                target: "write".to_owned(),
                addend: 0,
            }],
        )],
    );
    missing.archive = pack_archive(&[("other", &[0xc3])]);
    assert!(link(&missing).is_err(), "归档缺少被引用的 C 符号时必须失败");
    missing.archive = pack_archive(&[("write", &[0xc3])]);
    assert!(link(&missing).is_ok(), "归档成员应接入同一链接");
}

/// panic 消息字面量必须真的进镜像：`Symbol::Data` 的全局下标是符号名唯一来源，
/// 缺少全局数据池时字面量会指向别处（实测读到的是代码字节）。
#[test]
#[ignore = "slow full compilation"]
fn panic_message_constant_reaches_the_image() {
    let source = "fn main() { panic(\"boom\") }";
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    let bytes = plan.linux_image();
    assert!(
        bytes.windows(4).any(|window| window == b"boom"),
        "常量数据未进镜像"
    );
}

#[test]
#[ignore = "slow full compilation"]
fn compiled_main_is_static_pie_without_interpreter() {
    let source = "fn main() {}";
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.linux_image_kind(), "static-pie");
    assert_eq!(plan.windows_image_kind(), "");
    assert!(plan.windows_image().is_empty());
    assert!(plan.linux_interpreter().is_empty());
    assert_eq!(plan.linux_load_segments(), 3);
    let bytes = plan.linux_image();
    assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F']);
    assert!(bytes.windows(8).any(|window| window == b"GUGUSM01"));
    assert!(bytes.windows(8).any(|window| window == b"GUGUUN01"));
    assert!(bytes.windows(8).any(|window| window == b"GUGUSRC1"));
    let entry = plan.linux_entry_vaddr() as usize;
    assert_eq!(&bytes[entry..entry + 3], &[0x48, 0x8d, 0x1d]);
    let again = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    let again = again.image_plan().expect("重放");
    assert_eq!(
        again.linux_image_fingerprint(),
        plan.linux_image_fingerprint()
    );
    assert_eq!(again.linux_image(), plan.linux_image());
}
