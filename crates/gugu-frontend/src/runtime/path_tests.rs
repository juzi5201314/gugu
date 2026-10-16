use super::hash::Hasher;
use super::io::{INVALID_INPUT, UNSUPPORTED};
use super::path::{OsString, PathError, PathTarget, PathValue};

fn linux(value: &str) -> PathValue {
    PathValue::from_string(value, PathTarget::Linux).expect("Linux 路径")
}

fn windows(value: &str) -> PathValue {
    PathValue::from_string(value, PathTarget::Windows).expect("Windows 路径")
}

#[test]
fn linux_bytes_round_trip_and_lossy_replacement() {
    let text = OsString::from_bytes(&[0xFF, 0x2F, 0x61], PathTarget::Linux).expect("原始字节");
    assert_eq!(text.to_bytes(), [0xFF, 0x2F, 0x61]);
    let error = text.to_string().expect_err("非法 UTF-8");
    assert_eq!(error.offset(), 0);
    assert!(text.to_string_lossy().contains('\u{FFFD}'));
}

#[test]
fn windows_rejects_lone_surrogate_units() {
    let error = OsString::from_units(&[0xD800], PathTarget::Windows).expect_err("代理项");
    assert!(matches!(error, PathError::Unicode(error) if error.offset() == 0));
    assert!(matches!(
        OsString::from_units(&[0xD800], PathTarget::Linux),
        Err(PathError::Io(error)) if error.kind_code() == UNSUPPORTED
    ));
}

#[test]
fn join_parent_extension_and_absoluteness_follow_native_rules() {
    assert!(linux("/a").is_absolute());
    assert!(linux("a").is_relative());
    assert_eq!(
        linux("a").join(&linux("b")).to_string().expect("拼接"),
        "a/b"
    );
    assert_eq!(linux("/").parent(), None);
    assert_eq!(
        linux("a/b.txt")
            .extension()
            .expect("扩展名")
            .to_string()
            .expect("文本"),
        "txt"
    );
    assert!(windows(r"C:\").is_absolute());
    assert!(windows("C:/a").is_absolute());
    assert!(windows(r"\\server").is_absolute());
    assert!(windows("a").is_relative());
    assert_eq!(windows(r"C:\").parent(), None);
    assert_eq!(
        windows(r"a\b")
            .join(&windows(r"C:\c"))
            .to_string()
            .expect("绝对 child"),
        r"C:\c"
    );
}

#[test]
fn equality_does_not_collapse_distinct_components() {
    assert_ne!(linux("a/b"), linux("a/c"));
    let mut left = Hasher::new();
    let mut right = Hasher::new();
    linux("a/b").as_os().feed(&mut left);
    linux("a/c").as_os().feed(&mut right);
    assert_ne!(left.input(), right.input());
}

#[test]
fn linux_string_rejects_nul_and_windows_bytes_require_utf8() {
    let error = OsString::from_string("a\0b", PathTarget::Linux).expect_err("NUL");
    assert_eq!(error.kind_code(), INVALID_INPUT);
    let error = OsString::from_bytes(&[0xFF], PathTarget::Windows).expect_err("Windows UTF-8");
    assert_eq!(error.offset(), 0);
}

#[test]
fn components_keep_empty_and_dot_segments() {
    let parts = linux("a//./..").components();
    let text = parts
        .iter()
        .map(|part| part.to_string().expect("组件"))
        .collect::<Vec<_>>();
    assert_eq!(text, ["a", "", ".", ".."]);
}
