use super::text::{
    CowBytes, Norm, TextFault, UNICODE_VERSION, byte_at, case_fold, char_at, graphemes, lines,
    normalize, scalar_boundary, to_lowercase, to_uppercase, unicode_tables_agree, utf8_decode,
    utf8_decode_lossy, utf16_decode, utf16_decode_lossy, utf16_encode, words,
};

#[test]
fn strict_utf8_rejects_the_first_illegal_byte() {
    assert_eq!(utf8_decode(b"ok").as_deref().ok(), Some("ok"));
    assert_eq!(utf8_decode(&[0x61, 0x80, 0x62]), Err(1));
    assert_eq!(utf8_decode(&[0xc0, 0x80]), Err(0));
}

#[test]
fn lossy_utf8_replaces_each_maximal_subpart() {
    let text = utf8_decode_lossy(&[0x61, 0xf1, 0x80, 0x80, 0xe1, 0x80, 0xc2, 0x62]);
    assert_eq!(text, "a\u{FFFD}\u{FFFD}\u{FFFD}b");
    assert_eq!(utf8_decode_lossy(&[0x80]), "\u{FFFD}");
    assert_eq!(utf8_decode_lossy(&[0xc2]), "\u{FFFD}");
}

#[test]
fn utf16_roundtrip_and_lone_surrogates() {
    let units = utf16_encode("a\u{1F600}b");
    assert_eq!(utf16_decode(&units).as_deref().ok(), Some("a\u{1F600}b"));
    assert_eq!(utf16_decode(&[0xD800]), Err(0));
    assert_eq!(utf16_decode_lossy(&[0xD800, 0x61]), "\u{FFFD}a");
    assert_eq!(utf16_decode(&[0xDC00, 0xD800]), Err(0));
}

#[test]
fn negative_and_scalar_boundary_are_distinct_faults() {
    assert_eq!(byte_at("é", -1), Err(TextFault::Negative));
    assert_eq!(byte_at("é", 0), Ok(Some(0xc3)));
    assert_eq!(byte_at("é", 2), Ok(None));
    assert_eq!(scalar_boundary("é", 1, true), Err(TextFault::Boundary));
    assert_eq!(scalar_boundary("é", 2, true), Ok(2));
    assert_eq!(scalar_boundary("é", 3, true), Err(TextFault::OutOfRange));
    assert_eq!(scalar_boundary("ab", -4, false), Err(TextFault::Negative));
    assert_eq!(char_at("aé", 1), Ok(Some('é')));
    assert_eq!(char_at("aé", 2), Ok(None));
    assert_eq!(char_at("a", -1), Err(TextFault::Negative));
}

#[test]
fn copy_seals_and_mutation_detaches() {
    let mut original = CowBytes::new();
    original.push(b'g');
    let mut copy = original.share();
    assert!(original.is_sealed());
    assert_eq!(original.backing(), copy.backing());
    copy.push(b'!');
    assert_eq!(original.as_bytes(), b"g");
    assert_eq!(copy.as_bytes(), b"g!");
    assert_ne!(original.backing(), copy.backing());
}

#[test]
fn freeze_thaw_and_split_keep_snapshots_stable() {
    let buffer = CowBytes::with_capacity(-1);
    assert_eq!(buffer.err(), Some(TextFault::Negative));
    let mut buffer = CowBytes::with_capacity(4).expect("非负容量");
    buffer.push(b'a');
    buffer.push(b'b');
    buffer.push(b'c');
    let frozen = buffer.freeze();
    assert_eq!(frozen.as_bytes(), b"abc");
    assert!(buffer.is_sealed());
    let mut thawed = frozen.thaw();
    assert_eq!(thawed.backing(), frozen.backing());
    thawed.push(b'z');
    assert_eq!(frozen.as_bytes(), b"abc");
    assert_ne!(thawed.backing(), frozen.backing());
    let mut buffer = CowBytes::new();
    buffer.push(b'a');
    buffer.push(b'b');
    buffer.push(b'c');
    assert_eq!(buffer.split_to(-1).err(), Some(TextFault::Negative));
    assert_eq!(buffer.split_to(4).err(), Some(TextFault::OutOfRange));
    let prefix = buffer.split_to(1).expect("范围内切分");
    assert_eq!(prefix.as_bytes(), b"a");
    assert_eq!(buffer.as_bytes(), b"bc");
    assert!(prefix.is_sealed());
    buffer.push(b'd');
    assert_eq!(prefix.as_bytes(), b"a");
    assert_eq!(buffer.as_bytes(), b"bcd");
}

#[test]
fn unicode_encoding_version_changes_compiler_identity() {
    use crate::project::ActionInputs;
    let version = UNICODE_VERSION;
    assert_eq!(version, "17.0.0");
    assert!(unicode_tables_agree());
    let current = crate::compiler_identity();
    assert!(current.contains(version));
    let same = ActionInputs::new(current.clone(), "host", "host", "bin");
    let other = ActionInputs::new(current.replace(version, "15.1.0"), "host", "host", "bin");
    assert_ne!(same.key(), other.key());
}

#[test]
fn case_normalization_and_segmentation_follow_unicode_17() {
    assert!(unicode_tables_agree());
    assert_eq!(to_lowercase("İ"), "i\u{0307}");
    assert_eq!(to_uppercase("ß"), "SS");
    assert_eq!(case_fold("Straße"), "strasse");
    assert_eq!(normalize("e\u{0301}", Norm::Nfc), "é");
    assert_eq!(normalize("é", Norm::Nfd), "e\u{0301}");
    assert_eq!(normalize("ﬁ", Norm::Nfkc), "fi");
    assert_eq!(normalize("ﬁ", Norm::Nfkd), "fi");
    assert_eq!(graphemes("a\r\nb"), ["a", "\r\n", "b"]);
    assert_eq!(words("a b"), ["a", " ", "b"]);
    assert_eq!(lines("a\nb"), ["a\n", "b"]);
    assert_eq!(lines("a\r\nb"), ["a\r\n", "b"]);
    assert_eq!(lines("a\u{2028}b"), ["a\u{2028}", "b"]);
}

#[test]
fn bytes_and_byte_buffer_copies_seal_in_generic_gir() {
    let compilation = crate::Compiler::new().compile(crate::CompileRequest::single_file(
        "main.gg",
        "fn main() {}",
        crate::TargetName::X86_64Linux,
    ));
    assert!(compilation.is_success());
    let dump = compilation.dump_gir().expect("GIR");
    for owner in ["share_bytes", "share_buffer"] {
        let body = dump
            .split("body owner=")
            .find(|body| body.starts_with(owner))
            .unwrap_or_else(|| panic!("缺少 {owner}"));
        assert!(
            body.contains("cow_snapshot"),
            "{owner} 复制必须封存：{body}"
        );
    }
}
