//! PE32+ 的进程内结构检查。不在 Linux 上启动 PE。

use std::collections::BTreeMap;

use super::super::elf::{BootOffsets, LinkCode};
use super::ImageKind;
use super::link::link;

fn boot() -> BootOffsets {
    BootOffsets {
        stack_check: 64,
        map_base: 72,
        poll: 0,
        tlab_cursor: 3520,
        tlab_limit: 3528,
        turn_cursor: 3536,
        turn_limit: 3544,
        barrier: 3560,
    }
}

fn image(kind: ImageKind) -> super::PeImage {
    let code = LinkCode {
        symbol: "main".to_owned(),
        bytes: vec![0xc3],
        included: true,
        relocs: Vec::new(),
    };
    let mut frames = BTreeMap::new();
    frames.insert("main".to_owned(), 0);
    let meta: [(&str, &[u8]); 3] = [
        (".gugustk", &[1, 2]),
        (".gugu.unwind", &[3]),
        (".gugu.meta", &[4]),
    ];
    link(&[code], &[], &meta, "main", boot(), &frames, kind).expect("写出 PE")
}

#[test]
fn exe_has_aslr_nx_imports_and_staticlib() {
    let image = image(ImageKind::Exe);
    let bytes = &image.bytes;
    assert_eq!(&bytes[..2], b"MZ");
    let lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[lfanew..lfanew + 4], b"PE\0\0");
    assert_eq!(
        u16::from_le_bytes(bytes[lfanew + 4..lfanew + 6].try_into().unwrap()),
        0x8664
    );
    assert_eq!(
        u32::from_le_bytes(bytes[lfanew + 8..lfanew + 12].try_into().unwrap()),
        0
    );
    let optional = lfanew + 24;
    assert_eq!(
        u16::from_le_bytes(bytes[optional..optional + 2].try_into().unwrap()),
        0x20b
    );
    assert_eq!(
        u32::from_le_bytes(bytes[optional + 64..optional + 68].try_into().unwrap()),
        0
    );
    assert_eq!(
        u16::from_le_bytes(bytes[optional + 68..optional + 70].try_into().unwrap()),
        3
    );
    assert_eq!(
        u16::from_le_bytes(bytes[optional + 70..optional + 72].try_into().unwrap()),
        0x0160
    );
    assert_eq!(
        u64::from_le_bytes(bytes[optional + 24..optional + 32].try_into().unwrap()),
        super::IMAGE_BASE
    );
    let flags = u16::from_le_bytes(bytes[lfanew + 22..lfanew + 24].try_into().unwrap());
    assert_eq!(flags & 0x2000, 0);
    assert!(bytes.windows(2).all(|window| window != [0x0f, 0x05]));
    assert!(bytes.windows(12).any(|window| window == b"kernel32.dll"));
    assert!(bytes.windows(9).any(|window| window == b"ntdll.dll"));
    assert!(!bytes.windows(10).any(|window| window == b"msvcrt.dll"));
    assert!(image.archive.starts_with(b"!<arch>\n"));
    assert!(image.entry >= 0x1000);
    assert!(image.sections >= 8);
    assert_eq!(image.imports, 7);
}

#[test]
fn cdylib_sets_dll_bit_without_export_directory() {
    let image = image(ImageKind::Dll);
    let bytes = &image.bytes;
    let lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    let flags = u16::from_le_bytes(bytes[lfanew + 22..lfanew + 24].try_into().unwrap());
    assert_eq!(flags & 0x2000, 0x2000);
    let optional = lfanew + 24;
    let export_rva = u32::from_le_bytes(bytes[optional + 112..optional + 116].try_into().unwrap());
    let export_size = u32::from_le_bytes(bytes[optional + 116..optional + 120].try_into().unwrap());
    assert_eq!(export_rva, 0);
    assert_eq!(export_size, 0);
    let reloc_size = u32::from_le_bytes(
        bytes[optional + 112 + 5 * 8 + 4..optional + 112 + 5 * 8 + 8]
            .try_into()
            .unwrap(),
    );
    assert!(reloc_size > 0);
}

#[test]
fn text_section_is_not_writable() {
    let image = image(ImageKind::Exe);
    let bytes = &image.bytes;
    let lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[lfanew + 6..lfanew + 8].try_into().unwrap());
    let start = lfanew + 24 + 240;
    for index in 0..count as usize {
        let at = start + index * 40;
        let name = &bytes[at..at + 8];
        assert!(name.iter().filter(|byte| **byte != 0).count() <= 8);
        let flags = u32::from_le_bytes(bytes[at + 36..at + 40].try_into().unwrap());
        if name.starts_with(b".text") {
            assert_eq!(flags & 0x8000_0000, 0);
            assert_ne!(flags & 0x2000_0000, 0);
        }
        assert!(flags & 0x8000_0000 == 0 || flags & 0x2000_0000 == 0);
    }
}
