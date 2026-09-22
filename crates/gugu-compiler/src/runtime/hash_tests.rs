use super::hash::*;

#[test]
fn hasher_feeds_semantic_fields_prefix_free() {
    let mut split = Hasher::new();
    split.write_str("ab");
    split.write_str("c");
    let mut joined = Hasher::new();
    joined.write_str("abc");
    assert_ne!(split.input(), joined.input(), "长度前缀区分字段边界");
    let mut pair = Hasher::new();
    ("ab".to_owned(), 1i64).feed(&mut pair);
    let mut manual = Hasher::new();
    manual.write_str("ab");
    manual.write_int(1);
    assert_eq!(pair.input(), manual.input(), "元组按语义顺序馈送");
}

#[test]
fn xxhash3_is_a_persistent_known_answer_hash() {
    assert!(HashFamily::XxHash3_64.is_persistent());
    assert!(HashFamily::XxHash3_128.is_persistent());
    assert_eq!(
        HashFamily::XxHash3_64.finish(b""),
        HashOutput::Bits64(0x2D06_8005_38D3_94C2)
    );
    assert_eq!(
        HashFamily::XxHash3_128.finish(b""),
        HashOutput::Bits128(0x99AA_06D3_0147_98D8_6001_C324_468D_497F)
    );
    // 相同字节在两次独立计算里相同；不同输入不同。
    assert_eq!(
        HashFamily::XxHash3_64.finish(b"gugu"),
        HashFamily::XxHash3_64.finish(b"gugu")
    );
    assert_ne!(
        HashFamily::XxHash3_64.finish(b"gugu"),
        HashFamily::XxHash3_64.finish(b"gugv")
    );
}

#[test]
fn default_and_secure_families_depend_on_process_entropy() {
    let first = HashFamily::default_from_entropy([1; 8]);
    let second = HashFamily::default_from_entropy([2; 8]);
    assert!(!first.is_persistent());
    assert_eq!(first.finish(b"key"), first.finish(b"key"));
    assert_ne!(first.finish(b"key"), second.finish(b"key"));
    let secure = HashFamily::secure_from_entropy([7; 16]);
    let other = HashFamily::secure_from_entropy([8; 16]);
    assert!(matches!(secure, HashFamily::SipHash13 { .. }));
    assert!(!secure.is_persistent());
    assert_eq!(secure.finish(b"key"), secure.finish(b"key"));
    assert_ne!(secure.finish(b"key"), other.finish(b"key"));
    assert_ne!(secure.finish(b"key"), first.finish(b"key"));
}

#[test]
fn stable_keys_hash_equal_values_identically() {
    let family = HashFamily::default_from_entropy([3; 8]);
    assert_eq!(
        "text".to_owned().stable_hash(family),
        String::from("text").stable_hash(family)
    );
    assert_eq!([1i64, 2].stable_hash(family), [1i64, 2].stable_hash(family));
    assert_ne!(1i64.stable_hash(family), 2i64.stable_hash(family));
    assert_eq!(
        (true, 'x').stable_hash(HashFamily::XxHash3_64),
        (true, 'x').stable_hash(HashFamily::XxHash3_64)
    );
    // string 的 hash 按原始 UTF-8 字节：规范等价但字节不同的文本不同。
    assert_ne!(
        "\u{e9}".to_owned().stable_hash(HashFamily::XxHash3_64),
        "e\u{301}".to_owned().stable_hash(HashFamily::XxHash3_64)
    );
}
