use super::codegen::BlobRelocation;
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
        symbols,
        readonly: vec![(".gugustk".to_owned(), b"GUGUSM01".to_vec())],
        constants: Vec::new(),
        dll: false,
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
        offset: 8,
        symbol: "main".to_owned(),
    }];
    // 符号 RVA 只有布局知道；用同一请求布局一次拿到它，再比对镜像里回填的字。
    let (_, layout) = link_with_layout(&prepared).expect("布局");
    let expected = layout["main"];
    let image = link(&prepared).expect("镜像");
    let at = image
        .bytes
        .windows(8)
        .position(|window| window == b"GUGUMT01")
        .expect("元数据段必须在镜像内");
    let value = u64::from_le_bytes(image.bytes[at + 8..at + 16].try_into().expect("回填字"));
    assert_eq!(value, expected, "元数据符号必须回填成符号布局给出的 RVA");

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
    assert_eq!(rt0_params::WORDS, 16);
    assert_eq!(
        word(rt0_params::RT0_START),
        0,
        "未绑定 rt0_start 时留 0 走过渡路径"
    );
    assert_eq!(word(rt0_params::ENTRY), defined["main"]);
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
    assert_eq!(payload_word(8), defined["main"], "载荷的 main 入口");
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
    let compilation = Compiler::new().compile(CompileRequest::single_file(
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
