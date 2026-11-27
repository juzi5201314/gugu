use super::codegen::{BlobRelocation, BlobRelocationKind};
use super::pe::{
    ObjectReloc, ObjectSymbol, PeRequest, RelocKindCode, extract_coff_archive, link,
    link_with_layout, pack_coff_archive, reject_writable_executable,
};
use crate::{CompileRequest, Compiler, TargetName};

fn symbol(name: &str, bytes: Vec<u8>, relocs: Vec<ObjectReloc>) -> ObjectSymbol {
    ObjectSymbol {
        name: name.to_owned(),
        bytes,
        relocs,
        aliases: Vec::new(),
    }
}

fn request(entry: &str, symbols: Vec<ObjectSymbol>) -> PeRequest {
    PeRequest {
        entry: entry.to_owned(),
        cage_bytes: 0,
        symbols,
        readonly: vec![(".gugustk".to_owned(), b"GUGUSM01".to_vec())],
        constants: Vec::new(),
        dll: false,
        imports: Vec::new(),
        bound_glues: std::collections::BTreeSet::new(),
        type_descriptors: std::collections::BTreeMap::new(),
        globals: std::collections::BTreeMap::new(),
        blob_relocations: Vec::new(),
    }
}

#[test]
fn metadata_blob_symbol_relocations_are_filled_at_link_time() {
    let mut prepared = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    let mut meta = b"GUGUMT01".to_vec();
    meta.extend_from_slice(&[0u8; 8]);
    prepared.readonly.push((".ggmeta".to_owned(), meta));
    prepared.blob_relocations = vec![BlobRelocation {
        blob: ".ggmeta".to_owned(),
        offset: 8,
        symbol: "main".to_owned(),
        kind: BlobRelocationKind::Rva,
    }];
    // 符号 RVA 只有布局知道；用同一请求布局一次拿到它，再比对镜像里回填的字。
    let (_, layout) = link_with_layout(&prepared).expect("布局");
    let image = link(&prepared).expect("镜像");
    let expected = u64::from(image.entry_rva) + layout["main"];
    let at = image
        .bytes
        .windows(8)
        .position(|window| window == b"GUGUMT01")
        .expect("元数据段必须在镜像内");
    let value = u64::from_le_bytes(image.bytes[at + 8..at + 16].try_into().expect("回填字"));
    assert_eq!(value, expected, "元数据符号必须回填成符号布局给出的 RVA");

    prepared.blob_relocations = vec![BlobRelocation {
        blob: ".ggmeta".to_owned(),
        offset: 8,
        symbol: "missing_symbol".to_owned(),
        kind: BlobRelocationKind::Rva,
    }];
    let error = link(&prepared).expect_err("缺失符号必须失败");
    assert!(
        error.contains("missing_symbol"),
        "失败信息必须点名符号：{error}"
    );
}

#[test]
fn absolute_blob_relocations_are_image_addresses_and_relocated() {
    let mut prepared = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    prepared.constants.push((
        "vtable".to_owned(),
        vec![
            0, 0, 0, 0, 0, 0, 0, 0, b'V', b'T', b'A', b'B', b'L', b'E', b'0', b'1',
        ],
    ));
    prepared.blob_relocations = vec![BlobRelocation {
        blob: "vtable".to_owned(),
        offset: 0,
        symbol: "main".to_owned(),
        kind: BlobRelocationKind::Absolute,
    }];
    let (_, layout) = link_with_layout(&prepared).expect("布局");
    let image = link(&prepared).expect("镜像");
    let marker = image
        .bytes
        .windows(8)
        .position(|window| window == b"VTABLE01")
        .expect("vtable 必须在镜像内");
    let at = marker - 8;
    let value = u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("回填字"));
    let expected = 0x0000_0001_4000_0000 + u64::from(image.entry_rva) + layout["main"];
    assert_eq!(value, expected, "vtable 必须回填可解引用虚拟地址");
    assert!(image.reloc_count >= 1, "绝对 vtable 入口必须进入 .reloc");
}

#[test]
fn exe_is_pe32_plus_without_crt_and_keeps_metadata() {
    let image = link(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("exe");
    assert_eq!(image.kind, "exe");
    assert_eq!(image.import_dlls, 2);
    assert_eq!(&image.bytes[0..2], b"MZ");
    assert_eq!(&image.bytes[64..68], b"PE\0\0");
    assert_eq!(
        u16::from_le_bytes(image.bytes[68..70].try_into().unwrap()),
        0x8664
    );
    assert_eq!(
        u16::from_le_bytes(image.bytes[88..90].try_into().unwrap()),
        0x20b
    );
    let image_base = u64::from_le_bytes(image.bytes[112..120].try_into().unwrap());
    assert_eq!(image_base, 0x0000_0001_4000_0000);
    assert_eq!(
        u32::from_le_bytes(image.bytes[120..124].try_into().unwrap()),
        4096
    );
    assert_eq!(
        u32::from_le_bytes(image.bytes[124..128].try_into().unwrap()),
        512
    );
    assert_eq!(
        u32::from_le_bytes(image.bytes[72..76].try_into().unwrap()),
        0,
        "COFF 时间戳为 0"
    );
    assert_eq!(
        u32::from_le_bytes(image.bytes[152..156].try_into().unwrap()),
        0,
        "校验和为 0"
    );
    assert_eq!(
        u16::from_le_bytes(image.bytes[156..158].try_into().unwrap()),
        3
    );
    let dll = u16::from_le_bytes(image.bytes[158..160].try_into().unwrap());
    assert_eq!(dll & 0x0160, 0x0160, "ASLR、高熵 ASLR 与 NX");
    let entry = file_offset(&image.bytes, image.entry_rva);
    assert_eq!(
        &image.bytes[entry..entry + 4],
        &[0x48, 0x83, 0xec, 0x28],
        "入口必须先留出 shadow space"
    );
    assert!(image.bytes.windows(8).any(|window| window == b"GUGUSM01"));
    assert!(
        image
            .bytes
            .windows(12)
            .any(|window| window == b"kernel32.dll")
    );
    assert!(image.bytes.windows(9).any(|window| window == b"ntdll.dll"));
    assert!(
        image
            .bytes
            .windows(11)
            .any(|window| window == b"ExitProcess")
    );
    assert!(
        image
            .bytes
            .windows(18)
            .any(|window| window == b"NtTerminateProcess")
    );
    assert!(
        !image
            .bytes
            .windows(10)
            .any(|window| window == b"msvcrt.dll")
    );
    assert!(
        !image
            .bytes
            .windows(7)
            .any(|window| window == [0xb8, 0x3c, 0, 0, 0, 0x0f, 0x05])
    );
    let again = link(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("重放");
    assert_eq!(again.fingerprint, image.fingerprint);
}

fn file_offset(bytes: &[u8], rva: u32) -> usize {
    let count = u16::from_le_bytes(bytes[70..72].try_into().unwrap()) as usize;
    for index in 0..count {
        let at = 328 + index * 40;
        let virtual_size = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap());
        let virtual_address = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap());
        let raw = u32::from_le_bytes(bytes[at + 20..at + 24].try_into().unwrap());
        if rva >= virtual_address && rva - virtual_address < virtual_size.max(1) {
            return (raw + (rva - virtual_address)) as usize;
        }
    }
    panic!("RVA {rva:#x} 不在任何节内");
}

/// PE 节表里覆盖该 RVA 的节是否可写（`IMAGE_SCN_MEM_WRITE`）。
fn writable_section(bytes: &[u8], rva: u64) -> bool {
    let lfanew =
        usize::try_from(u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap())).unwrap();
    let count = usize::from(u16::from_le_bytes(
        bytes[lfanew + 6..lfanew + 8].try_into().unwrap(),
    ));
    let optional = usize::from(u16::from_le_bytes(
        bytes[lfanew + 20..lfanew + 22].try_into().unwrap(),
    ));
    let table = lfanew + 24 + optional;
    (0..count).any(|index| {
        let at = table + index * 40;
        let size = u64::from(u32::from_le_bytes(
            bytes[at + 8..at + 12].try_into().unwrap(),
        ));
        let va = u64::from(u32::from_le_bytes(
            bytes[at + 12..at + 16].try_into().unwrap(),
        ));
        let flags = u32::from_le_bytes(bytes[at + 36..at + 40].try_into().unwrap());
        (va..va + size.max(1)).contains(&rva) && flags & 0x8000_0000 != 0
    })
}

/// 参数块与保留可写区契约：与 ELF 侧共用同一份 `rt0_params`/`Rt0Payload`；PE 没有自举重定位，
/// 参数与载荷里的地址都是 RVA，加载基址由入口 stub 写进载荷的 bias 字段。
#[test]
fn cage_counter_record_is_in_writable_data() {
    let mut prepared = request(
        "main",
        vec![symbol(
            "main",
            vec![0; 4],
            vec![ObjectReloc {
                offset: 0,
                kind: RelocKindCode::Rva32,
                target: "__gugu_cage_control".into(),
                addend: 0,
            }],
        )],
    );
    prepared.cage_bytes = 8388608;
    let (image, defined) = link_with_layout(&prepared).unwrap();
    let address = defined["__gugu_cage_control"];
    let at = file_offset(&image.bytes, image.entry_rva + defined["main"] as u32);
    assert_eq!(
        u32::from_le_bytes(image.bytes[at..at + 4].try_into().unwrap()) as u64,
        address
    );
    assert!(writable_section(&image.bytes, address));
    assert!(writable_section(&image.bytes, address + 63));
    let config = file_offset(&image.bytes, address as u32 + 56);
    assert_eq!(
        u64::from_le_bytes(image.bytes[config..config + 8].try_into().unwrap()),
        8388608
    );
}

#[test]
fn runtime_entry_keeps_param_block_and_writable_reserved_region() {
    use crate::runtime::entry::{Rt0Payload, rt0_params};
    let (image, defined) = link_with_layout(&request(
        "main",
        vec![symbol("main", vec![0xc3], Vec::new())],
    ))
    .expect("exe 可写出");
    // `defined` 里 `.text` 符号是节内偏移；`.text` 的 RVA 就是入口 RVA。
    let params = u64::from(image.entry_rva) + defined["__gugu_rt0_params"];
    let word = |index: usize| {
        let at = file_offset(
            &image.bytes,
            u32::try_from(params + index as u64 * 8).expect("参数块 RVA"),
        );
        u64::from_le_bytes(image.bytes[at..at + 8].try_into().expect("参数块 word"))
    };
    assert_eq!(
        word(rt0_params::RT0_START),
        0,
        "未绑定 rt0_start 时留 0 走过渡路径"
    );
    assert_eq!(
        word(rt0_params::ENTRY),
        u64::from(image.entry_rva) + defined["main"]
    );
    let payload = word(rt0_params::PAYLOAD_VA);
    assert_eq!(payload, defined["__gugu_rt0_payload"]);
    assert_eq!(word(rt0_params::PAYLOAD_LEN), Rt0Payload::bytes());
    let slot = word(rt0_params::INITIAL_SLOT);
    let state = word(rt0_params::RUNTIME_STATE);
    assert_eq!(slot, defined["__gugu_initial_slot"]);
    assert_eq!(state, defined["__gugu_runtime_state"]);
    assert_eq!(slot % 64, 0, "初始协程槽按 64 对齐");
    assert_eq!(state % 8, 0, "runtime 状态区按 8 对齐");
    let payload_at = file_offset(&image.bytes, u32::try_from(payload).expect("载荷 RVA"));
    let payload_word = |offset: usize| {
        u64::from_le_bytes(
            image.bytes[payload_at + offset..payload_at + offset + 8]
                .try_into()
                .expect("载荷 word"),
        )
    };
    assert_eq!(payload_word(0), Rt0Payload::REVISION, "载荷版本");
    assert_eq!(
        payload_word(8),
        u64::from(image.entry_rva) + defined["main"],
        "载荷的 main RVA"
    );
    assert_eq!(payload_word(16), slot);
    assert_eq!(payload_word(24), state);
    assert_eq!(payload_word(32), 0, "加载基址由入口 stub 写回");
    assert!(
        writable_section(&image.bytes, payload),
        "保留区必须落在可写节：runtime 要在其中发布 mailbox 与初始槽"
    );
}

#[test]
fn duplicate_slot_missing_entry_and_wx_are_rejected() {
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
    assert!(
        link(&request(
            "missing",
            vec![symbol("main", vec![0xc3], Vec::new())]
        ))
        .is_err()
    );
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
}

#[test]
fn explicit_import_thunks_point_to_aligned_names_and_exact_iat() {
    let mut request = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    request.imports = vec![
        crate::NativeImport::function("odd.dll", "probe_a"),
        crate::NativeImport::function("ODD.DLL", "probe_b"),
        crate::NativeImport::function("even.dll", "probe_c"),
    ];
    let (image, layout) = link_with_layout(&request).unwrap();
    assert_eq!(image.import_dlls, 4);
    let bytes = &image.bytes;
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[70..72].try_into().unwrap()) as usize;
    let offset = |rva: usize| {
        (0..count)
            .find_map(|index| {
                let section = 328 + index * 40;
                let base = u32_at(section + 12);
                let size = u32_at(section + 8);
                (rva >= base && rva < base + size).then(|| u32_at(section + 20) + rva - base)
            })
            .expect("RVA 必须落在真实节")
    };
    let directory = 88 + 112;
    let mut descriptor = offset(u32_at(directory + 8));
    let iat_base = u32_at(directory + 96);
    let mut iat_bytes = 0;
    while u32_at(descriptor) != 0 {
        let mut ilt = offset(u32_at(descriptor));
        let iat = u32_at(descriptor + 16);
        assert_eq!(iat, iat_base + iat_bytes);
        while u64_at(ilt) != 0 {
            let name = u64_at(ilt);
            assert_eq!(name % 2, 0, "Hint/Name 必须对齐");
            assert_eq!(u64_at(offset(iat_base + iat_bytes)), name);
            iat_bytes += 8;
            ilt += 8;
        }
        assert_eq!(u64_at(offset(iat_base + iat_bytes)), 0);
        iat_bytes += 8;
        descriptor += 20;
    }
    assert_eq!(
        u32_at(directory + 100),
        iat_bytes,
        "IAT 范围不得包含 runtime 状态"
    );
    let text_rva = u32_at(328 + 12);
    for name in ["probe_a", "probe_b", "probe_c"] {
        let rva = text_rva + layout[name] as usize;
        let thunk = offset(rva);
        assert_eq!(&bytes[thunk..thunk + 2], &[0xff, 0x25]);
        let displacement = i32::from_le_bytes(bytes[thunk + 2..thunk + 6].try_into().unwrap());
        let slot = (rva + 6).wrapping_add_signed(displacement as isize);
        assert!((iat_base..iat_base + iat_bytes).contains(&slot));
        let name_at = offset(u64_at(offset(slot))) + 2;
        assert_eq!(&bytes[name_at..name_at + name.len()], name.as_bytes());
    }
    request
        .imports
        .push(crate::NativeImport::function("another.dll", "probe_a"));
    assert!(link(&request).unwrap_err().contains("同时来自"));
}

#[test]
fn runtime_core_iat_relocations_resolve_to_registered_slots() {
    let names = [
        "CreateEventW",
        "GetSystemInfo",
        "GetSystemTimePreciseAsFileTime",
        "SetEvent",
    ];
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for name in names {
        code.extend_from_slice(&[0xff, 0x15, 0, 0, 0, 0]);
        relocs.push(ObjectReloc {
            offset: code.len() as u32 - 4,
            kind: RelocKindCode::PcRel32,
            target: format!("__gugu_iat_{name}"),
            addend: 0,
        });
    }
    code.push(0xc3);
    let (image, layout) =
        link_with_layout(&request("main", vec![symbol("main", code, relocs)])).expect("exe");
    let bytes = &image.bytes;
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let directory = 88 + 112;
    let iat_base = u32_at(directory + 96);
    let iat_end = iat_base + u32_at(directory + 100);
    let start = u64::from(image.entry_rva) + layout["main"];
    for (index, name) in names.into_iter().enumerate() {
        let instruction = start + index as u64 * 6;
        let at = file_offset(bytes, instruction as u32);
        assert_eq!(&bytes[at..at + 2], &[0xff, 0x15]);
        let displacement = i32::from_le_bytes(bytes[at + 2..at + 6].try_into().unwrap());
        let slot = (instruction + 6).wrapping_add_signed(i64::from(displacement)) as usize;
        assert!(
            (iat_base..iat_end).contains(&slot),
            "{name} 的调用必须读 IAT，而不是文本或缺失符号桩"
        );
        let hint_name = file_offset(bytes, u64_at(file_offset(bytes, slot as u32)) as u32);
        assert_eq!(
            &bytes[hint_name + 2..hint_name + 2 + name.len()],
            name.as_bytes()
        );
    }
}

#[test]
fn unregistered_iat_relocation_is_a_link_error() {
    let image = link(&request(
        "main",
        vec![symbol(
            "main",
            vec![0xff, 0x15, 0, 0, 0, 0, 0xc3],
            vec![ObjectReloc {
                offset: 2,
                kind: RelocKindCode::PcRel32,
                target: "__gugu_iat_NotARegisteredImport".to_owned(),
                addend: 0,
            }],
        )],
    ));
    let error = image.expect_err("未登记 IAT 目标必须失败");
    assert!(
        error.contains("__gugu_iat_NotARegisteredImport"),
        "失败信息必须点名导入槽：{error}"
    );
}

#[test]
fn cdylib_sets_image_file_dll_without_changing_imports() {
    let mut request = request("main", vec![symbol("main", vec![0xc3], Vec::new())]);
    request.dll = true;
    let image = link(&request).expect("cdylib");
    assert_eq!(image.kind, "cdylib");
    let characteristics = u16::from_le_bytes(image.bytes[86..88].try_into().unwrap());
    assert_eq!(characteristics & 0x2002, 0x2002);
    assert_eq!(image.import_dlls, 2);
    assert!(
        !image
            .bytes
            .windows(10)
            .any(|window| window == b"msvcrt.dll")
    );
}

#[test]
fn coff_archive_extracts_only_the_named_member() {
    let archive = pack_coff_archive(&[("write", b"write-bytes"), ("read", b"read-bytes")]);
    let got = extract_coff_archive(&archive, &["write".to_owned()]).expect("抽取");
    assert_eq!(got, vec![b"write-bytes".to_vec()]);
    assert!(extract_coff_archive(&archive, &["missing".to_owned()]).is_err());
}

#[test]
#[ignore = "slow full compilation"]
fn compiled_windows_main_is_an_exe_and_linux_image_is_absent() {
    let source = "fn main() {}";
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Windows,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.linux_image_kind(), "");
    assert!(plan.linux_image().is_empty());
    assert_eq!(plan.windows_image_kind(), "exe");
    assert_eq!(plan.windows_import_dlls(), 2);
    let bytes = plan.windows_image();
    assert_eq!(&bytes[0..2], b"MZ");
    assert_eq!(&bytes[64..68], b"PE\0\0");
    assert!(bytes.windows(8).any(|window| window == b"GUGUSM01"));
    assert!(bytes.windows(8).any(|window| window == b"GUGUUN01"));
    let again = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Windows,
    ));
    let again = again.image_plan().expect("重放");
    assert_eq!(
        again.windows_image_fingerprint(),
        plan.windows_image_fingerprint()
    );
}
