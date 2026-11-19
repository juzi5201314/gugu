use super::*;
use crate::{NativeImport, NativeImportKind};

fn config() -> DynamicLink {
    DynamicLink {
        interpreter: "/lib/ld-musl-aarch64.so.1".into(),
        imports: vec![Import {
            native: NativeImport::function("libc.so", "getaddrinfo"),
            slot: loc(2, 0),
        }],
    }
}

fn tags(bytes: &[u8]) -> std::collections::BTreeMap<u64, u64> {
    let phdr = 64 + 6 * 56;
    let start = u64_at(bytes, phdr + 8) as usize;
    let size = u64_at(bytes, phdr + 32) as usize;
    (start..start + size)
        .step_by(16)
        .map(|at| (u64_at(bytes, at), u64_at(bytes, at + 8)))
        .collect()
}

#[test]
fn interpreter_import_tables_and_glob_dat_are_loader_visible() {
    let image = link_dynamic(&request(), &config()).unwrap();
    assert_eq!(link_dynamic(&request(), &config()).unwrap(), image);
    let bytes = &image.bytes;
    assert_eq!(u16_at(bytes, 56), 7);
    assert_eq!(u16_at(bytes, 60), 7);
    assert_eq!(u64_at(bytes, 24), 64 + 7 * 56);
    assert_eq!(u32_at(bytes, 64 + 56), 3);
    assert_eq!(u32_at(bytes, 64 + 6 * 56), 2);
    let kinds: Vec<_> = (0..7).map(|i| u32_at(bytes, 64 + i * 56)).collect();
    assert_eq!(kinds, [6, 3, 1, 1, 1, 0x6474_e551, 2]);
    let interp = u64_at(bytes, 64 + 56 + 8) as usize;
    assert_eq!(&bytes[interp..interp + 26], b"/lib/ld-musl-aarch64.so.1\0");
    let tags = tags(bytes);
    let strings = tags[&5] as usize;
    assert_eq!(&bytes[strings + tags[&1] as usize..][..8], b"libc.so\0");
    let sym = tags[&6] as usize + 24;
    let name = u32_at(bytes, sym) as usize;
    assert_eq!(&bytes[strings + name..][..12], b"getaddrinfo\0");
    assert_eq!(bytes[sym + 4], 0x12);
    assert_eq!(u16_at(bytes, sym + 6), 0);
    assert_eq!((tags[&8], tags[&9], tags[&11]), (24, 24, 24));
    let rela = tags[&7] as usize;
    assert_eq!(u64_at(bytes, rela), 0x20000);
    assert_eq!(u64_at(bytes, rela + 8), (1 << 32) | 1025);
    assert_eq!(u64_at(bytes, rela + 16), 0);
    assert_eq!(u64_at(bytes, 0x20000), 0);
    let hash = tags[&4] as usize;
    assert_eq!((u32_at(bytes, hash), u32_at(bytes, hash + 4)), (1, 2));
    let shoff = u64_at(bytes, 40) as usize;
    assert_eq!(u32_at(bytes, shoff + 64 + 40), 3);
}

#[test]
fn dependencies_are_deduplicated_and_relative_prefix_is_preserved() {
    let mut request = request();
    request.sections[2].content = SectionContent::Data(vec![0; 24]);
    request.relatives.push(Relative {
        at: loc(2, 16),
        target: loc(0, 0),
        addend: 0,
    });
    let mut config = config();
    config.imports.push(Import {
        native: NativeImport::data("libc.so", "environ"),
        slot: loc(2, 8),
    });
    let image = link_dynamic(&request, &config).unwrap();
    let tags = tags(&image.bytes);
    assert_eq!(tags[&8], 72);
    let rela = tags[&7] as usize;
    assert_eq!(u64_at(&image.bytes, rela + 8), 1027);
    assert_eq!(u64_at(&image.bytes, rela + 24 + 8), (1 << 32) | 1025);
    assert_eq!(u64_at(&image.bytes, rela + 48 + 8), (2 << 32) | 1025);
    let dynamic = u64_at(&image.bytes, 64 + 6 * 56 + 32);
    assert_eq!(dynamic, 11 * 16);
    assert_eq!(image.bytes[tags[&6] as usize + 48 + 4], 0x11);
}

#[test]
fn dynamic_entry_does_not_treat_imports_as_relative_records() {
    let (request, entry) = runtime_request();
    let image = link_entry_dynamic(request, entry, &config()).unwrap();
    let tags = tags(&image.bytes);
    assert_eq!(tags[&8], 48);
    assert_eq!(u64_at(&image.bytes, tags[&7] as usize + 8), 1027);
    assert_eq!(
        u64_at(&image.bytes, tags[&7] as usize + 32),
        (1 << 32) | 1025
    );
    let start = u64_at(&image.bytes, 24) as usize;
    assert_eq!(
        u64_at(&image.bytes, 0x10020 + rt0_params::RELOC_COUNT * 8),
        1
    );
    assert!(start > 64 + 7 * 56);
}

#[test]
fn malformed_dynamic_configuration_is_rejected_without_panicking() {
    let request = request();
    for interpreter in ["", "relative", "/lib/ld\0bad"] {
        let mut config = config();
        config.interpreter = interpreter.into();
        assert!(link_dynamic(&request, &config).is_err());
    }
    let mut empty = config();
    empty.imports.clear();
    assert!(link_dynamic(&request, &empty).is_err());
    for slot in [loc(0, 0), loc(1, 0), loc(2, 1), loc(2, 8), loc(9, 0)] {
        let mut config = config();
        config.imports[0].slot = slot;
        assert!(link_dynamic(&request, &config).is_err());
    }
    for name in ["", "bad\0name"] {
        let mut config = config();
        config.imports[0].native.name = name.into();
        assert!(link_dynamic(&request, &config).is_err());
    }
    let mut duplicate = config();
    duplicate.imports.push(duplicate.imports[0].clone());
    assert!(link_dynamic(&request, &duplicate).is_err());
    let mut library = config();
    library.imports[0].native.kind = NativeImportKind::Library;
    assert!(link_dynamic(&request, &library).is_err());
    let mut relative = request;
    relative.relatives.push(Relative {
        at: loc(2, 0),
        target: loc(0, 0),
        addend: 0,
    });
    assert!(link_dynamic(&relative, &config()).is_err());
}
