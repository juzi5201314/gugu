use super::*;
use sha2::{Digest, Sha256};

fn request(arch: Arch) -> MachORequest {
    let section = |segment, name: &str, align, content| Section {
        segment,
        name: name.to_owned(),
        align,
        content,
    };
    MachORequest {
        arch,
        identifier: "gugu.test".to_owned(),
        sections: vec![
            section(
                Segment::Text,
                "__text",
                4,
                SectionContent::Code(vec![0xc3; 12]),
            ),
            section(
                Segment::Text,
                "__const",
                16,
                SectionContent::Data(b"hi\0".to_vec()),
            ),
            section(
                Segment::DataConst,
                "__got",
                8,
                SectionContent::Data(vec![0xaa; 16]),
            ),
            section(Segment::Data, "__data", 8, SectionContent::Data(vec![0; 8])),
            section(Segment::Data, "__bss", 16, SectionContent::ZeroFill(0x40)),
        ],
        libraries: Vec::new(),
        imports: vec![
            Import {
                dylib: LIB_SYSTEM.to_owned(),
                name: "_exit".to_owned(),
                weak: false,
            },
            Import {
                dylib: "/usr/lib/libz.1.dylib".to_owned(),
                name: "_zlibVersion".to_owned(),
                weak: true,
            },
        ],
        fixups: vec![
            Fixup::Bind {
                at: at(2, 0),
                import: 0,
                addend: 0,
            },
            Fixup::Bind {
                at: at(2, 8),
                import: 1,
                addend: 4,
            },
            Fixup::Rebase {
                at: at(3, 0),
                target: at(1, 1),
            },
        ],
        symbols: vec![Symbol {
            name: "_main".to_owned(),
            at: at(0, 0),
        }],
        entry: at(0, 4),
    }
}

fn at(section: usize, offset: u64) -> Location {
    Location { section, offset }
}

#[test]
fn native_imports_use_dyld_slots_on_both_architectures() {
    use crate::{NativeImport, NativeImportKind};

    for arch in [Arch::X86_64, Arch::Arm64] {
        let mut request = request(arch);
        let slots = append_native_imports(
            &mut request,
            &[
                NativeImport::function(LIB_SYSTEM, "write"),
                NativeImport::library("/usr/lib/libresolv.9.dylib"),
                NativeImport::data(LIB_SYSTEM, "environ"),
            ],
        )
        .unwrap();
        assert_eq!(slots["write"].kind, NativeImportKind::Function);
        assert_eq!(slots["environ"].kind, NativeImportKind::Data);
        assert_eq!(slots.len(), 2);
        assert_eq!(request.imports[2].name, "_write");
        assert_eq!(request.imports[3].name, "_environ");
        assert!(dylibs(&request).contains(&"/usr/lib/libresolv.9.dylib"));
        let stubs = if arch == Arch::X86_64 {
            let stubs = crate::x64::macho::append_import_stubs(&mut request, &slots).unwrap();
            assert_eq!(stubs.len(), 1);
            Some(stubs["write"])
        } else {
            None
        };
        let layout = layout(&request).unwrap();
        let file = link(&request).unwrap().bytes;
        let slot = slots["write"].at;
        let got = layout.sections[slot.section];
        assert_eq!(u64_at(&file, (got.fileoff + slot.offset) as usize) >> 63, 1);
        if let Some(stub) = stubs {
            let code = layout.sections[stub.section];
            let pc = (code.fileoff + stub.offset) as usize;
            assert_eq!(&file[pc..pc + 2], &[0xff, 0x25]);
            let displacement = u32_at(&file, pc + 2) as i32;
            assert_eq!(
                (code.addr + stub.offset + 6).checked_add_signed(i64::from(displacement)),
                Some(got.addr + slot.offset)
            );
        }
    }
}

#[test]
fn arm64_import_stubs_resolve_page_and_pageoff_to_each_bind_slot() {
    use crate::{NativeImport, a64::macho as a64_macho};

    let mut request = request(Arch::Arm64);
    request.imports[0].name = "_abort".into();
    request.sections.push(Section {
        segment: Segment::DataConst,
        name: "__padding".into(),
        align: 8,
        content: SectionContent::Data(vec![0; 0x1008]),
    });
    let slots = append_native_imports(
        &mut request,
        &[
            NativeImport::function(LIB_SYSTEM, "write"),
            NativeImport::data(LIB_SYSTEM, "environ"),
            NativeImport::function(LIB_SYSTEM, "exit"),
        ],
    )
    .unwrap();
    let stubs = a64_macho::append_import_stubs(&mut request, &slots).unwrap();
    let names = ["__gugu_darwin_c_exit", "__gugu_darwin_c_write"];
    assert_eq!(stubs.keys().map(String::as_str).collect::<Vec<_>>(), names);
    let layout = layout(&request).unwrap();
    let file = link(&request).unwrap().bytes;
    for (index, (name, c_name)) in names.into_iter().zip(["exit", "write"]).enumerate() {
        assert_eq!(a64_macho::import_stub_symbol(c_name), name);
        let stub = stubs[name];
        assert_eq!(stub.offset, index as u64 * 12);
        assert_eq!(request.sections[stub.section].name, "__stubs");
        assert!(request.symbols.contains(&Symbol {
            name: name.into(),
            at: stub
        }));
        let code = layout.sections[stub.section];
        let off = (code.fileoff + stub.offset) as usize;
        let adrp = u32_at(&file, off);
        let ldr = u32_at(&file, off + 4);
        assert_eq!(adrp & 0x9f00_001f, 0x9000_0010);
        assert_eq!(ldr & 0xffc0_03ff, 0xf940_0210);
        assert_eq!(u32_at(&file, off + 8), 0xd61f_0200);
        let pages = ((((adrp >> 5) & 0x7ffff) << 2) | ((adrp >> 29) & 3)) as i32;
        let target_page = ((code.addr + stub.offset) & !0xfff)
            .checked_add_signed(i64::from((pages << 11) >> 11) * 4096)
            .unwrap();
        let slot = slots[c_name].at;
        assert_eq!(
            target_page + u64::from((ldr >> 10) & 0xfff) * 8,
            layout.address(slot).unwrap()
        );
        let got = layout.sections[slot.section];
        assert_eq!(u64_at(&file, (got.fileoff + slot.offset) as usize) >> 63, 1);
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn arm64_darwin_entry_fixture() -> (MachORequest, crate::a64::macho::DarwinEntry) {
    use crate::{a64::macho::DarwinEntry, runtime::entry::Rt0Payload};

    let mut request = request(Arch::Arm64);
    request.imports.clear();
    request.fixups.clear();
    request.sections[3].align = 8;
    request.sections[3].content = SectionContent::Data(vec![0; 8 + Rt0Payload::bytes() as usize]);
    request.sections[4].content = SectionContent::ZeroFill(128 + 4096);
    let entry = DarwinEntry {
        rt0_start: at(0, 4),
        payload: at(3, 8),
        initial_slot: at(4, 0),
        processor: at(4, 128),
        runtime: None,
    };
    (request, entry)
}

#[test]
fn arm64_darwin_entry_resolves_startup_and_exit() {
    use crate::{
        a64::macho::link_entry,
        runtime::entry::{Rt0Payload, rt0_params},
    };
    use std::mem::offset_of;

    let (request, entry) = arm64_darwin_entry_fixture();
    let file = link_entry(request, entry).unwrap().bytes;
    let (main, main_file) = section_address(&file, "__lc_main");
    let (params, params_file) = section_address(&file, "__rt0_params");
    let (boot, boot_file) = section_address(&file, "__rt0");
    let (body, _) = section_address(&file, "__text");
    let (exit, _) = section_address(&file, "__stubs");
    let (payload, payload_file) = section_address(&file, "__data");
    let (slot, _) = section_address(&file, "__bss");

    assert_eq!(
        u64_at(&file, command(&file, 0x8000_0028) + 8),
        main_file as u64
    );
    let words = (0..9)
        .map(|i| u32_at(&file, main_file + i * 4))
        .collect::<Vec<_>>();
    assert_eq!(&words[..2], &[0x9100_0024, 0x1100_0003]);
    assert_eq!(&words[4..6], &[0xf940_0001, 0xcb01_0001]);
    assert_eq!(words[8], 0xd420_0000);
    let page = (((words[2] >> 5) & 0x7ffff) << 2) | ((words[2] >> 29) & 3);
    assert_eq!(words[2] & 0x9f00_001f, 0x9000_0000);
    assert_eq!(words[3] & 0xffc0_03ff, 0x9100_0000);
    assert_eq!(
        (main & !0xfff) + u64::from(page) * 4096 + u64::from((words[3] >> 10) & 0xfff),
        params
    );
    assert_eq!(branch_target(words[6], main + 24), boot);
    assert_eq!(branch_target(words[7], main + 28), exit);
    let rt0_call = crate::a64::fragments::rt0("test").relocations[0].offset;
    assert_eq!(
        branch_target(
            u32_at(&file, boot_file + rt0_call as usize),
            boot + u64::from(rt0_call)
        ),
        body + 4
    );
    let param = |word| u64_at(&file, params_file + word * 8);
    assert_eq!(param(rt0_params::BIAS_BASELINE), params - IMAGE_BASE);
    assert_eq!(param(rt0_params::RT0_START), body + 4 - IMAGE_BASE);
    assert_eq!(param(rt0_params::PAYLOAD_VA), payload + 8 - IMAGE_BASE);
    assert_eq!(param(rt0_params::INITIAL_SLOT), slot - IMAGE_BASE);
    assert_eq!(param(rt0_params::PROCESSOR), slot + 128 - IMAGE_BASE);
    assert_eq!(
        u64_at(&file, payload_file + 8 + offset_of!(Rt0Payload, revision)),
        Rt0Payload::REVISION
    );
    assert_eq!(
        u64_at(
            &file,
            payload_file + 8 + offset_of!(Rt0Payload, initial_slot)
        ),
        slot - IMAGE_BASE
    );
    assert!(
        file.windows(b"_exit\0".len())
            .any(|name| name == b"_exit\0")
    );
}

#[test]
fn arm64_darwin_entry_reuses_existing_exit_binding() {
    use crate::{NativeImportKind, a64::macho as a64_macho};

    for (bound, prebuilt) in [(true, false), (true, true), (false, false)] {
        let (mut request, entry) = arm64_darwin_entry_fixture();
        request.imports.push(Import {
            dylib: LIB_SYSTEM.into(),
            name: "_exit".into(),
            weak: false,
        });
        let slot = at(2, 8);
        if bound {
            request.fixups.push(Fixup::Bind {
                at: slot,
                import: 0,
                addend: 0,
            });
        } else {
            request.sections[slot.section].name = "__unused_got".into();
        }
        let text = layout(&request).unwrap().address(at(1, 0)).unwrap() - IMAGE_BASE;
        request.sections[1].content =
            SectionContent::Data(vec![0; (Arch::Arm64.page_size() - text - 128) as usize]);
        if prebuilt {
            request.imports.push(Import {
                dylib: LIB_SYSTEM.into(),
                name: "_write".into(),
                weak: false,
            });
            request.fixups.push(Fixup::Bind {
                at: at(2, 0),
                import: 1,
                addend: 0,
            });
            let slots = std::collections::BTreeMap::from(
                [("exit", slot), ("write", at(2, 0))].map(|(name, at)| {
                    (
                        name.into(),
                        ImportSlot {
                            at,
                            kind: NativeImportKind::Function,
                        },
                    )
                }),
            );
            a64_macho::append_import_stubs(&mut request, &slots).unwrap();
        }
        let old_got = layout(&request).unwrap().address(slot).unwrap();
        let file = a64_macho::link_entry(request, entry).unwrap().bytes;
        let (main, main_file) = section_address(&file, "__lc_main");
        let (stub, stub_file) = section_address(&file, "__stubs");
        let (got, got_file) = section_address(&file, "__got");
        let slot_offset = if bound { slot.offset } else { 0 };
        if bound {
            assert_ne!(old_got, got + slot_offset);
        }
        assert_eq!(
            branch_target(u32_at(&file, main_file + 28), main + 28),
            stub
        );
        assert_eq!(arm64_stub_target(&file, stub_file, stub), got + slot_offset);
        if prebuilt {
            assert_eq!(arm64_stub_target(&file, stub_file + 12, stub + 12), got);
        }
        assert_eq!(u64_at(&file, got_file + slot_offset as usize) >> 63, 1);
        let fixups = u32_at(&file, command(&file, 0x8000_0034) + 8) as usize;
        assert_eq!(u32_at(&file, fixups + 16), 1 + u32::from(prebuilt));
        assert_eq!(
            u32_at(&file, command(&file, 0x2) + 12),
            2 + u32::from(prebuilt)
        );
    }
}

fn arm64_stub_target(file: &[u8], off: usize, stub: u64) -> u64 {
    let adrp = u32_at(file, off);
    let ldr = u32_at(file, off + 4);
    let pages = ((((adrp >> 5) & 0x7ffff) << 2) | ((adrp >> 29) & 3)) as i32;
    let page = (stub & !0xfff)
        .checked_add_signed(i64::from((pages << 11) >> 11) * 4096)
        .unwrap();
    page + u64::from((ldr >> 10) & 0xfff) * 8
}

#[test]
fn arm64_darwin_entry_rejects_a_payload_outside_writable_data() {
    let (request, mut entry) = arm64_darwin_entry_fixture();
    entry.payload = entry.rt0_start;
    let error = crate::a64::macho::link_entry(request, entry).unwrap_err();
    assert!(
        error.contains("invalid arm64 Mach-O payload location"),
        "{error}"
    );
}

fn arm64_darwin_runtime_fixture() -> (MachORequest, crate::a64::macho::DarwinEntry) {
    use crate::{a64::macho::DarwinRuntime, runtime::entry::state};

    let (mut request, mut entry) = arm64_darwin_entry_fixture();
    request.sections[4].content = SectionContent::ZeroFill(128 + 4096 + state::BYTES);
    for (name, bytes) in [
        ("__gugu_maps", 32),
        ("__gugu_cfa", 16),
        ("__gugu_types", 24),
    ] {
        request.sections.push(Section {
            segment: Segment::DataConst,
            name: name.into(),
            align: 8,
            content: SectionContent::Data(vec![0x5a; bytes]),
        });
    }
    entry.runtime = Some(DarwinRuntime {
        entry: at(0, 8),
        runtime_state: at(4, 128 + 4096),
        coroutine_enter: at(0, 0),
        main_return: at(0, 4),
        fragment_base: at(0, 0),
        stackmap: at(5, 0),
        unwind: at(6, 0),
        gc_types: Some(at(7, 0)),
        gc_meta: None,
    });
    (request, entry)
}

#[test]
fn arm64_darwin_entry_writes_the_full_runtime_contract() {
    use crate::{
        a64::macho::link_entry,
        runtime::entry::{Rt0Payload, rt0_params},
    };
    use std::mem::offset_of;

    let (request, entry) = arm64_darwin_runtime_fixture();
    let file = link_entry(request, entry).unwrap().bytes;
    let rva = |name: &str, offset: u64| {
        let (address, _) = section_address(&file, name);
        address + offset - crate::macho::IMAGE_BASE
    };
    let (_, params_file) = section_address(&file, "__rt0_params");
    let (_, payload_file) = section_address(&file, "__data");
    for (word, expected) in [
        (rt0_params::ENTRY, rva("__text", 8)),
        (rt0_params::RUNTIME_STATE, rva("__bss", 128 + 4096)),
        (rt0_params::COROUTINE_ENTER, rva("__text", 0)),
    ] {
        assert_eq!(
            u64_at(&file, params_file + word * 8),
            expected,
            "word {word}"
        );
    }
    for (field, expected) in [
        (offset_of!(Rt0Payload, main_entry), rva("__text", 8)),
        (
            offset_of!(Rt0Payload, runtime_state),
            rva("__bss", 128 + 4096),
        ),
        (offset_of!(Rt0Payload, main_return), rva("__text", 4)),
        (offset_of!(Rt0Payload, stackmap), rva("__gugu_maps", 0)),
        (offset_of!(Rt0Payload, fragment_base), rva("__text", 0)),
        (offset_of!(Rt0Payload, stackmap_len), 32),
        (offset_of!(Rt0Payload, unwind), rva("__gugu_cfa", 0)),
        (offset_of!(Rt0Payload, unwind_len), 16),
        (offset_of!(Rt0Payload, gc_types), rva("__gugu_types", 0)),
        (offset_of!(Rt0Payload, gc_types_len), 24),
        (offset_of!(Rt0Payload, gc_meta), 0),
        (offset_of!(Rt0Payload, gc_meta_len), 0),
    ] {
        assert_eq!(
            u64_at(&file, payload_file + 8 + field),
            expected,
            "payload field {field}"
        );
    }
}

#[test]
fn arm64_darwin_entry_rejects_invalid_runtime_locations() {
    use crate::a64::macho::link_entry;

    for (mutate, name) in [
        // 状态区必须可写：__got 是 __DATA_CONST。
        (
            Box::new(|runtime: &mut crate::a64::macho::DarwinRuntime| {
                runtime.runtime_state = at(2, 0);
            }) as Box<dyn FnOnce(&mut crate::a64::macho::DarwinRuntime)>,
            "runtime_state",
        ),
        (Box::new(|runtime| runtime.entry = at(0, 12)), "entry"),
        (Box::new(|runtime| runtime.entry = at(0, 2)), "entry"),
        (
            Box::new(|runtime| runtime.fragment_base = at(0, 4)),
            "fragment_base",
        ),
        (
            Box::new(|runtime| runtime.gc_types = Some(at(7, 32))),
            "gc_types",
        ),
    ] {
        let (request, mut entry) = arm64_darwin_runtime_fixture();
        let mut runtime = entry.runtime.unwrap();
        mutate(&mut runtime);
        entry.runtime = Some(runtime);
        let error = link_entry(request, entry).unwrap_err();
        assert!(
            error.contains(&format!("invalid arm64 Mach-O {name} location")),
            "{name}: {error}"
        );
    }
}

fn section_address(file: &[u8], name: &str) -> (u64, usize) {
    for (_, _, _, _, _, at) in segments(file) {
        for i in 0..u32_at(file, at + 64) as usize {
            let s = at + 72 + 80 * i;
            if name16(&file[s..s + 16]) == name {
                return (u64_at(file, s + 32), u32_at(file, s + 48) as usize);
            }
        }
    }
    panic!("missing Mach-O section {name}");
}

fn branch_target(word: u32, pc: u64) -> u64 {
    assert_eq!(word >> 26, 0b100101);
    let displacement = (((word & 0x03ff_ffff) << 6) as i32) >> 4;
    pc.checked_add_signed(i64::from(displacement)).unwrap()
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn name16(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap().trim_end_matches('\0')
}

/// `(cmd, offset)`，偏移指向 load command 起点。
fn commands(file: &[u8]) -> Vec<(u32, usize)> {
    let ncmds = u32_at(file, 16);
    let mut at = 32;
    let mut out = Vec::new();
    for _ in 0..ncmds {
        out.push((u32_at(file, at), at));
        at += u32_at(file, at + 4) as usize;
    }
    assert_eq!(
        at,
        32 + u32_at(file, 20) as usize,
        "sizeofcmds matches commands"
    );
    out
}

fn command(file: &[u8], cmd: u32) -> usize {
    let found: Vec<_> = commands(file).into_iter().filter(|c| c.0 == cmd).collect();
    assert_eq!(found.len(), 1, "exactly one command {cmd:#x}");
    found[0].1
}

fn segments(file: &[u8]) -> Vec<(String, u64, u64, u64, u64, usize)> {
    commands(file)
        .into_iter()
        .filter(|c| c.0 == 0x19)
        .map(|(_, at)| {
            let name = name16(&file[at + 8..at + 24]).to_owned();
            let fields = [24, 32, 40, 48].map(|o| u64_at(file, at + o));
            (name, fields[0], fields[1], fields[2], fields[3], at)
        })
        .collect()
}

#[test]
fn header_and_segments_follow_architecture() {
    for (arch, cpu, subtype, page) in [
        (Arch::Arm64, 0x0100_000c, 0, 0x4000),
        (Arch::X86_64, 0x0100_0007, 3, 0x1000),
    ] {
        let image = link(&request(arch)).unwrap();
        let file = &image.bytes;
        assert_eq!(u32_at(file, 0), 0xfeed_facf);
        assert_eq!((u32_at(file, 4), u32_at(file, 8)), (cpu, subtype));
        assert_eq!(u32_at(file, 12), 2, "MH_EXECUTE");
        assert_ne!(u32_at(file, 24) & 0x0020_0000, 0, "MH_PIE");
        let names: Vec<_> = segments(file).iter().map(|s| s.0.clone()).collect();
        assert_eq!(
            names,
            [
                "__PAGEZERO",
                "__TEXT",
                "__DATA_CONST",
                "__DATA",
                "__LINKEDIT"
            ]
        );
        let segs = segments(file);
        assert_eq!((segs[0].1, segs[0].2, segs[0].4), (0, IMAGE_BASE, 0));
        assert_eq!(
            (segs[1].1, segs[1].3),
            (IMAGE_BASE, 0),
            "__TEXT maps the header"
        );
        for pair in segs[1..].windows(2) {
            assert_eq!(
                pair[0].1 + pair[0].2,
                pair[1].1,
                "{} is contiguous",
                pair[1].0
            );
            assert_eq!(pair[1].1 % page, 0);
            assert_eq!(pair[1].3 % page, 0);
        }
        let linkedit = segs.last().unwrap();
        assert_eq!(linkedit.3 + linkedit.4, file.len() as u64);
    }
}

#[test]
fn sections_carry_names_addresses_and_bytes() {
    let req = request(Arch::Arm64);
    let layout = layout(&req).unwrap();
    let file = link(&req).unwrap().bytes;
    let mut found = Vec::new();
    for (_, _, _, _, _, at) in segments(&file) {
        let nsects = u32_at(&file, at + 64) as usize;
        for i in 0..nsects {
            let s = at + 72 + 80 * i;
            found.push((
                name16(&file[s..s + 16]).to_owned(),
                name16(&file[s + 16..s + 32]).to_owned(),
                u64_at(&file, s + 32),
                u64_at(&file, s + 40),
                u32_at(&file, s + 48),
                u32_at(&file, s + 52),
                u32_at(&file, s + 64),
            ));
        }
    }
    assert_eq!(found.len(), req.sections.len());
    for (i, (section, seen)) in req.sections.iter().zip(&found).enumerate() {
        let placed = layout.sections[i];
        assert_eq!(seen.0, section.name);
        assert_eq!(seen.1, section.segment.name());
        assert_eq!((seen.2, seen.3), (placed.addr, section.content.size()));
        assert_eq!(1u64 << seen.5, section.align);
        assert_eq!(seen.2 % section.align, 0);
        match &section.content {
            SectionContent::ZeroFill(_) => assert_eq!((seen.4, seen.6), (0, 1)),
            SectionContent::Code(bytes) => {
                assert_eq!(seen.6, 0x8000_0400);
                assert_eq!(&file[seen.4 as usize..][..bytes.len()], &bytes[..]);
            }
            SectionContent::Data(_) if section.segment == Segment::Text => {
                assert_eq!(seen.6, 0);
                assert_eq!(&file[seen.4 as usize..][..3], b"hi\0");
            }
            SectionContent::Data(_) => assert_eq!(seen.6, 0),
        }
    }
    assert_eq!(
        layout
            .sections
            .iter()
            .map(|s| s.ordinal)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
}

#[test]
fn entry_dylibs_and_symbols_are_recorded() {
    let req = request(Arch::X86_64);
    let layout = layout(&req).unwrap();
    let file = link(&req).unwrap().bytes;
    let main = command(&file, 0x8000_0028);
    assert_eq!(
        u64_at(&file, main + 8),
        layout.address(req.entry).unwrap() - IMAGE_BASE
    );
    let dylinker = command(&file, 0xe);
    assert_eq!(
        name16(&file[dylinker + 12..dylinker + u32_at(&file, dylinker + 4) as usize]),
        "/usr/lib/dyld"
    );
    let dylibs: Vec<_> = commands(&file)
        .into_iter()
        .filter(|c| c.0 == 0xc)
        .map(|(_, at)| {
            let size = u32_at(&file, at + 4) as usize;
            name16(&file[at + u32_at(&file, at + 8) as usize..at + size]).to_owned()
        })
        .collect();
    assert_eq!(dylibs, [LIB_SYSTEM, "/usr/lib/libz.1.dylib"]);
    let symtab = command(&file, 0x2);
    let (symoff, nsyms, stroff) = (
        u32_at(&file, symtab + 8) as usize,
        u32_at(&file, symtab + 12),
        u32_at(&file, symtab + 16) as usize,
    );
    assert_eq!(nsyms, 1);
    let strx = u32_at(&file, symoff) as usize;
    assert_eq!(name16(&file[stroff + strx..stroff + strx + 6]), "_main");
    assert_eq!(
        (file[symoff + 4], file[symoff + 5]),
        (0x0e, 1),
        "N_SECT in __text"
    );
    assert_eq!(u64_at(&file, symoff + 8), layout.sections[0].addr);
    let build = command(&file, 0x32);
    assert_eq!(u32_at(&file, build + 8), 1, "PLATFORM_MACOS");
    let uuid = command(&file, 0x1b);
    assert_ne!(&file[uuid + 8..uuid + 24], &[0; 16]);
}

#[test]
fn chained_fixups_encode_binds_rebases_and_imports() {
    let req = request(Arch::Arm64);
    let layout = layout(&req).unwrap();
    let file = link(&req).unwrap().bytes;
    let cmd = command(&file, 0x8000_0034);
    let blob = &file[u32_at(&file, cmd + 8) as usize..][..u32_at(&file, cmd + 12) as usize];
    let (starts, imports, symbols, count, format) = (
        u32_at(blob, 4) as usize,
        u32_at(blob, 8) as usize,
        u32_at(blob, 12) as usize,
        u32_at(blob, 16),
        u32_at(blob, 20),
    );
    assert_eq!((u32_at(blob, 0), count, format), (0, 2, 1));
    let seg_count = u32_at(blob, starts) as usize;
    assert_eq!(seg_count, 5);
    let seg_offsets: Vec<_> = (0..seg_count)
        .map(|i| u32_at(blob, starts + 4 + 4 * i))
        .collect();
    assert_eq!((seg_offsets[0], seg_offsets[1], seg_offsets[4]), (0, 0, 0));
    for (index, segment) in [(2, Segment::DataConst), (3, Segment::Data)] {
        let s = starts + seg_offsets[index] as usize;
        assert_eq!(layout.segment_index(segment), Some(index));
        let placed = layout
            .segments
            .iter()
            .find(|p| p.segment == segment)
            .unwrap();
        assert_eq!(
            u16::from_le_bytes([blob[s + 4], blob[s + 5]]) as u64,
            Arch::Arm64.page_size()
        );
        assert_eq!(
            u16::from_le_bytes([blob[s + 6], blob[s + 7]]),
            6,
            "PTR_64_OFFSET"
        );
        assert_eq!(u64_at(blob, s + 8), placed.vmaddr - IMAGE_BASE);
        assert_eq!(
            u16::from_le_bytes([blob[s + 22], blob[s + 23]]),
            0,
            "chain starts at page 0"
        );
    }
    let name = |entry: u32| {
        let at = symbols + (entry >> 9) as usize;
        name16(&blob[at..at + 13])
            .split('\0')
            .next()
            .unwrap()
            .to_owned()
    };
    let (exit, zlib) = (u32_at(blob, imports), u32_at(blob, imports + 4));
    assert_eq!(
        (exit & 0xff, (exit >> 8) & 1, name(exit)),
        (1, 0, "_exit".to_owned())
    );
    assert_eq!(
        (zlib & 0xff, (zlib >> 8) & 1, name(zlib)),
        (2, 1, "_zlibVersion".to_owned())
    );

    let got = layout.sections[2].fileoff as usize;
    let first = u64_at(&file, got);
    assert_eq!(
        (first >> 63, first & 0xff_ffff, (first >> 51) & 0xfff),
        (1, 0, 2)
    );
    let second = u64_at(&file, got + 8);
    assert_eq!(
        (second >> 63, second & 0xff_ffff, (second >> 24) & 0xff),
        (1, 1, 4)
    );
    assert_eq!((second >> 51) & 0xfff, 0, "chain ends");
    let rebase = u64_at(&file, layout.sections[3].fileoff as usize);
    assert_eq!(rebase >> 51, 0, "rebase, chain ends");
    assert_eq!(rebase, layout.address(at(1, 1)).unwrap() - IMAGE_BASE);
}

#[test]
fn ad_hoc_signature_hashes_every_page() {
    for arch in [Arch::Arm64, Arch::X86_64] {
        let req = request(arch);
        let file = link(&req).unwrap().bytes;
        let cmd = command(&file, 0x1d);
        let (off, size) = (
            u32_at(&file, cmd + 8) as usize,
            u32_at(&file, cmd + 12) as usize,
        );
        assert_eq!(off + size, file.len());
        let sig = &file[off..];
        assert_eq!((be32(sig, 0), be32(sig, 8)), (0xfade_0cc0, 1));
        assert_eq!(be32(sig, 4) as usize, size);
        let cd = &sig[be32(sig, 16) as usize..];
        assert_eq!((be32(cd, 0), be32(cd, 8)), (0xfade_0c02, 0x2_0400));
        assert_eq!(be32(cd, 12), 0x2_0002, "adhoc | linker-signed");
        let (hash_off, ident_off, slots, limit) = (
            be32(cd, 16) as usize,
            be32(cd, 20) as usize,
            be32(cd, 28),
            be32(cd, 32) as usize,
        );
        assert_eq!(limit, off);
        assert_eq!(name16(&cd[ident_off..ident_off + 10]), "gugu.test");
        assert_eq!(&cd[36..40], &[32, 2, 0, 12]);
        let text = segments(&file)
            .into_iter()
            .find(|s| s.0 == "__TEXT")
            .unwrap();
        assert_eq!(u64::from_be_bytes(cd[64..72].try_into().unwrap()), text.3);
        assert_eq!(u64::from_be_bytes(cd[72..80].try_into().unwrap()), text.4);
        let pages: Vec<_> = file[..off].chunks(4096).collect();
        assert_eq!(pages.len(), slots as usize);
        for (i, page) in pages.into_iter().enumerate() {
            assert_eq!(
                &cd[hash_off + 32 * i..][..32],
                Sha256::digest(page).as_slice()
            );
        }
    }
}

#[test]
fn image_is_deterministic_and_identifier_sensitive() {
    let req = request(Arch::Arm64);
    let first = link(&req).unwrap();
    assert_eq!(first, link(&req).unwrap());
    let mut renamed = req.clone();
    renamed.identifier = "other".to_owned();
    assert_ne!(first.fingerprint, link(&renamed).unwrap().fingerprint);
}

#[test]
fn minimal_image_without_data_segments() {
    let mut req = request(Arch::Arm64);
    req.sections.truncate(1);
    req.imports.clear();
    req.fixups.clear();
    let file = link(&req).unwrap().bytes;
    let names: Vec<_> = segments(&file).iter().map(|s| s.0.clone()).collect();
    assert_eq!(names, ["__PAGEZERO", "__TEXT", "__LINKEDIT"]);
    assert_eq!(
        commands(&file).iter().filter(|c| c.0 == 0xc).count(),
        1,
        "libSystem only"
    );
}

#[test]
fn dylib_ordinals_stay_below_special_range() {
    let mut req = request(Arch::Arm64);
    let with_dylibs = |req: &mut MachORequest, count: usize| {
        req.imports = (1..count)
            .map(|i| Import {
                dylib: format!("/usr/lib/lib{i}.dylib"),
                name: format!("_f{i}"),
                weak: false,
            })
            .collect();
        req.fixups.retain(|f| matches!(f, Fixup::Rebase { .. }));
    };
    with_dylibs(&mut req, 239);
    link(&req).unwrap();
    with_dylibs(&mut req, 240);
    assert!(link(&req).unwrap_err().contains("more than 239 dylibs"));
}

#[test]
fn invalid_requests_are_rejected() {
    type Case = (&'static str, fn(&mut MachORequest));
    let cases: [Case; 9] = [
        ("entry", |r| r.entry = at(2, 0)),
        ("alignment", |r| r.sections[0].align = 3),
        ("16 bytes", |r| r.sections[0].name = "x".repeat(17)),
        ("__TEXT", |r| {
            r.sections[2].content = SectionContent::Code(vec![0; 16])
        }),
        ("zero-fill", |r| {
            r.sections[1].content = SectionContent::ZeroFill(4)
        }),
        ("8-byte", |r| {
            r.fixups[0] = Fixup::Bind {
                at: at(2, 4),
                import: 0,
                addend: 0,
            }
        }),
        ("duplicated", |r| {
            r.fixups[1] = Fixup::Rebase {
                at: at(2, 0),
                target: at(0, 0),
            }
        }),
        ("missing import", |r| {
            r.fixups[0] = Fixup::Bind {
                at: at(2, 0),
                import: 9,
                addend: 0,
            }
        }),
        ("data section", |r| {
            r.fixups[2] = Fixup::Rebase {
                at: at(0, 0),
                target: at(0, 0),
            }
        }),
    ];
    for (needle, mutate) in cases {
        let mut req = request(Arch::Arm64);
        mutate(&mut req);
        let err = link(&req).unwrap_err();
        assert!(err.contains(needle), "{needle:?} not in {err:?}");
    }
}
