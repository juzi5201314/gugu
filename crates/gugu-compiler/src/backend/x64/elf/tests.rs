//! ELF64 static PIE 的进程内正确性：头、段权限、重定位与归档，不启动进程。

use std::collections::BTreeMap;

use super::archive::write_archive;
use super::link::{
    LinkCode, LinkReloc, LinkRelocKind, LinkRequest, apply_relative, link, pc_disp,
    reject_writable_executable,
};
use super::runtime;
use super::{BootOffsets, ElfImage};

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

fn code(symbol: &str, bytes: &[u8], relocs: Vec<LinkReloc>) -> LinkCode {
    LinkCode {
        symbol: symbol.to_owned(),
        bytes: bytes.to_vec(),
        included: true,
        relocs,
    }
}

fn sections() -> [(&'static str, &'static [u8]); 3] {
    [
        (".gugu.stackmap", &[1, 2, 3, 4]),
        (".gugu.unwind", &[5, 6]),
        (".gugu.meta", &[7]),
    ]
}

fn link_main(
    codes: &[LinkCode],
    interp: Option<&str>,
    archives: &[&[u8]],
) -> Result<ElfImage, String> {
    let owned = sections();
    link(&LinkRequest {
        codes,
        consts: &[],
        sections: &owned,
        entry: "main",
        interpreter: interp,
        archives,
        boot: boot(),
        emit_runtime: true,
    })
    .map_err(|error| error.message().to_owned())
}

fn ret_main() -> Vec<LinkCode> {
    vec![code("main", &[0xc3], Vec::new())]
}

fn phdrs(image: &[u8]) -> Vec<(u32, u32, u64)> {
    let count = u16::from_le_bytes([image[56], image[57]]) as usize;
    (0..count)
        .map(|index| {
            let at = 64 + index * 56;
            let kind = u32::from_le_bytes(image[at..at + 4].try_into().expect("kind"));
            let flags = u32::from_le_bytes(image[at + 4..at + 8].try_into().expect("flags"));
            let align = u64::from_le_bytes(image[at + 48..at + 56].try_into().expect("align"));
            (kind, flags, align)
        })
        .collect()
}

#[test]
fn static_pie_has_dyn_header_relro_and_no_interp() {
    let image = link_main(&ret_main(), Some("/lib64/ld-linux-x86-64.so.2"), &[]).expect("链接");
    let bytes = &image.bytes;
    assert!(bytes.starts_with(&[0x7f, b'E', b'L', b'F', 2, 1, 1]));
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 3);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 62);
    assert_eq!(image.elf_type, 3);
    assert_eq!(image.schema, 1);
    assert_eq!(image.load_segments, 4);
    assert_eq!(image.relative_relocs, 1);
    assert!(image.interp.is_empty());
    assert_ne!(image.entry, 0x1000);
    assert_eq!(bytes[image.entry as usize], 0x48);
    assert_eq!(bytes[0x1000], 0xc3);
    let headers = phdrs(bytes);
    assert!(headers.iter().all(|header| header.0 != 3));
    let stack = headers
        .iter()
        .find(|header| header.0 == 0x6474_e551)
        .expect("GNU_STACK");
    assert_eq!(stack.1 & 1, 0);
    assert_eq!(stack.2, 1);
    assert!(headers.iter().any(|header| header.0 == 0x6474_e552));
    for name in [
        ".text",
        ".rodata",
        ".data.rel.ro",
        ".gugu.stackmap",
        ".gugu.unwind",
        ".gugu.meta",
    ] {
        assert!(
            bytes
                .windows(name.len())
                .any(|window| window == name.as_bytes()),
            "{name}"
        );
    }
    let again = link_main(&ret_main(), Some("/lib64/ld-linux-x86-64.so.2"), &[]).expect("重放");
    assert_eq!(again.bytes, image.bytes);
    assert_eq!(again.fingerprint, image.fingerprint);
}

#[test]
fn relative_reloc_applies_bias_once() {
    let image = link_main(&ret_main(), None, &[]).expect("链接");
    let mut bytes = image.bytes.clone();
    let table = usize::try_from(image.reloc_table).expect("表偏移");
    apply_relative(0x400000, &mut bytes, table).expect("应用");
    let offset = u64::from_le_bytes(bytes[table + 8..table + 16].try_into().expect("offset"));
    let addend = u64::from_le_bytes(bytes[table + 16..table + 24].try_into().expect("addend"));
    assert_eq!(addend, image.sentinel_vaddr);
    let slot = usize::try_from(offset).expect("槽");
    let value = u64::from_le_bytes(bytes[slot..slot + 8].try_into().expect("槽值"));
    assert_eq!(value, 0x400000 + image.sentinel_vaddr);
    let error = apply_relative(0x400000, &mut bytes, table).expect_err("重复");
    assert!(error.message().contains("重复"));
}

#[test]
fn included_functions_follow_symbol_order() {
    let codes = vec![
        code("b_fn", &[0x91, 0xc3], Vec::new()),
        code("a_fn", &[0x90, 0xc3], Vec::new()),
        code("main", &[0xc3], Vec::new()),
    ];
    let image = link_main(&codes, None, &[]).expect("链接");
    assert_eq!(image.bytes[0x1000], 0x90);
    assert_eq!(image.bytes[0x1010], 0x91);
}

#[test]
fn link_rejects_malformed_inputs() {
    let missing = link_main(&[], None, &[]);
    assert!(missing.expect_err("缺入口").contains("入口"));
    let dup = vec![
        code("main", &[0xc3], Vec::new()),
        code("main", &[0xc3], Vec::new()),
    ];
    assert!(
        link_main(&dup, None, &[])
            .expect_err("重复")
            .contains("重复")
    );
    let past = vec![code(
        "main",
        &[0xc3],
        vec![LinkReloc {
            offset: 4,
            kind: LinkRelocKind::PcRel32,
            target: "main".to_owned(),
            addend: 0,
        }],
    )];
    assert!(
        link_main(&past, None, &[])
            .expect_err("越界")
            .contains("越界")
    );
    let again = vec![code(
        "main",
        &[0xe8, 0, 0, 0, 0],
        vec![
            LinkReloc {
                offset: 1,
                kind: LinkRelocKind::PcRel32,
                target: "main".to_owned(),
                addend: 0,
            },
            LinkReloc {
                offset: 1,
                kind: LinkRelocKind::PcRel32,
                target: "main".to_owned(),
                addend: 0,
            },
        ],
    )];
    assert!(
        link_main(&again, None, &[])
            .expect_err("重定位重复")
            .contains("重复")
    );
    let absolute = vec![code(
        "main",
        &[0, 0, 0, 0, 0, 0, 0, 0],
        vec![LinkReloc {
            offset: 0,
            kind: LinkRelocKind::Abs64,
            target: "main".to_owned(),
            addend: 0,
        }],
    )];
    assert!(
        link_main(&absolute, None, &[])
            .expect_err("绝对")
            .contains("不可写")
    );
    let unknown = vec![code(
        "main",
        &[0xe8, 0, 0, 0, 0],
        vec![LinkReloc {
            offset: 1,
            kind: LinkRelocKind::PcRel32,
            target: "__gugu_runtime_deadbeef".to_owned(),
            addend: 0,
        }],
    )];
    assert!(
        link_main(&unknown, None, &[])
            .expect_err("未知")
            .contains("未知运行时符号")
    );
    let cold = vec![code(
        "main",
        &[0xe8, 0, 0, 0, 0],
        vec![LinkReloc {
            offset: 1,
            kind: LinkRelocKind::PcRel32,
            target: "cold:2147483649".to_owned(),
            addend: 0,
        }],
    )];
    assert!(link_main(&cold, None, &[]).is_ok());
}

#[test]
fn dynamic_import_uses_exact_interpreter_and_archive_does_not() {
    let call = vec![code(
        "main",
        &[0xe8, 0, 0, 0, 0],
        vec![LinkReloc {
            offset: 1,
            kind: LinkRelocKind::PcRel32,
            target: "puts".to_owned(),
            addend: 0,
        }],
    )];
    let path = "/lib64/ld-linux-x86-64.so.2";
    let dynamic = link_main(&call, Some(path), &[]).expect("动态");
    assert_eq!(dynamic.interp, path);
    let interp = phdrs(&dynamic.bytes)
        .into_iter()
        .find(|header| header.0 == 3)
        .expect("PT_INTERP");
    assert_eq!(interp.2, 1);
    assert!(
        link_main(&call, None, &[])
            .expect_err("无解释器")
            .contains("解释器")
    );
    let mut members = BTreeMap::new();
    members.insert("puts".to_owned(), vec![0xc3]);
    let archive = write_archive(&members);
    let linked = link_main(&call, Some(path), &[&archive]).expect("归档");
    assert!(linked.interp.is_empty());
    let extracted = super::archive::extract(&archive).expect("抽取");
    assert_eq!(extracted["puts"], vec![0xc3]);
}

#[test]
fn permission_and_displacement_helpers_reject_overflow() {
    let error = reject_writable_executable(1 | 2).expect_err("W+X");
    assert!(error.message().contains("可写可执行"));
    assert!(reject_writable_executable(4 | 1).is_ok());
    assert!(pc_disp(0, i64::from(i32::MAX) + 8).is_err());
}

#[test]
fn runtime_entries_issue_syscalls() {
    let bodies = runtime::named_bodies(boot());
    for name in [
        "morestack_or_poll",
        "panic",
        "gc_alloc_slow",
        "channel_new",
        "spawn",
    ] {
        let body = bodies.iter().find(|item| item.0 == name).expect(name);
        assert!(body.1.len() > 16, "{name}");
        assert!(body.1.windows(2).any(|pair| pair == [0x0f, 0x05]), "{name}");
        assert_ne!(body.1.first().copied(), Some(0xc3), "{name}");
    }
    for name in ["channel_send", "channel_receive", "memset", "memmove"] {
        let body = bodies.iter().find(|item| item.0 == name).expect(name);
        assert!(body.1.len() > 8, "{name}");
        assert_ne!(body.1, vec![0xc3], "{name}");
    }
    assert_eq!(runtime::trap(), vec![0x0f, 0x0b]);
}

#[test]
fn runtime_catalog_covers_switch_platform_and_channels() {
    let bodies = runtime::named_bodies(boot());
    let switch = bodies
        .iter()
        .find(|item| item.0 == "coroutine_switch")
        .expect("coroutine_switch");
    let fixed = crate::runtime::ContextSwitchCode::fixed();
    assert_eq!(switch.1, fixed.bytes);
    for name in [
        "platform_reserve_aligned",
        "platform_entropy",
        "channel_try_send",
        "channel_try_recv",
        "channel_close",
        "wide_div",
        "concat",
        "utf8_boundary",
        "gc_region_promote",
        "sched_park",
        "join_wait",
        "value_transfer",
        "format",
    ] {
        let body = bodies.iter().find(|item| item.0 == name).expect(name);
        assert_ne!(body.1, [0xc3], "{name}");
    }
}
