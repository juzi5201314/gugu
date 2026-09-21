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
    }
}

fn request(entry: &str, symbols: Vec<ObjectSymbol>) -> LinkRequest {
    LinkRequest {
        entry: entry.to_owned(),
        symbols,
        readonly: vec![Blob {
            name: ".gugu.stackmap".to_owned(),
            bytes: b"GUGUSM01".to_vec(),
        }],
        imports: Vec::new(),
        interpreter: None,
        archive: Vec::new(),
    }
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
fn relative_reloc_uses_load_bias() {
    let mut bytes = vec![0_u8; 16];
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes.extend_from_slice(&0x40_u64.to_le_bytes());
    apply_relative(&mut bytes, 16, 1, 0x1000).expect("回填");
    let value = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    assert_eq!(value, 0x1040);
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

#[test]
fn compiled_main_is_static_pie_without_interpreter() {
    let source = "fn main() {}";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
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
