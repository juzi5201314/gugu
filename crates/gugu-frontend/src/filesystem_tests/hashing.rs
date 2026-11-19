use super::printing::output;
use gugu_runtime_world::hash::{HashFamily, HashOutput, Hasher};

fn expected(hasher: &Hasher, seed: u64) -> String {
    let HashOutput::Bits64(value) = hasher.finish(HashFamily::FoldHashFast { seed }) else {
        unreachable!()
    };
    format!("{value}\n")
}

fn printed(source: &str) -> String {
    let bytes = output(source, &[])
        .into_iter()
        .flat_map(|(fd, bytes)| {
            assert_eq!(fd, 1);
            bytes
        })
        .collect::<Vec<_>>();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn seeded_builtin_tuple_and_user_hash_match_reference() {
    let mut integer = Hasher::new();
    integer.write_int(-42);
    let mut text = Hasher::new();
    text.write_str("ab中");
    let mut tuple = Hasher::new();
    tuple.write_int(-42);
    tuple.write_str("ab中");
    tuple.write_bool(true);
    let mut user = Hasher::new();
    user.write_int(-42);
    user.write_str("ab中");
    user.write_uint(9);
    assert_eq!(
        printed(
            r#"
            use std.io.{println}
            struct Key { number: int, name: string }
            impl Hash for Key {
                fn hash(self: &Self, hasher: &Hasher) {
                    self.number.hash(hasher)
                    self.name.hash(hasher)
                    hasher.write_uint(9)
                }
            }
            fn digest[T: Hash](key: T) uint {
                let h = Hasher::with_seed(123)
                key.hash(&h)
                h.finish()
            }
            fn main() {
                println(digest(-42))
                println(digest("ab中"))
                println(digest((-42, "ab中", true)))
                println(digest((Key { number: -42, name: "ab中" }, ())))
                println(digest((-42, "ab中", true)) == digest((-42, "ab中", true)))
            }
            "#,
        ),
        format!(
            "{}{}{}{}true\n",
            expected(&integer, 123),
            expected(&text, 123),
            expected(&tuple, 123),
            expected(&user, 123)
        )
    );
}

#[test]
fn finish_empty_repeated_and_continued_scalar_writes() {
    let mut model = Hasher::new();
    let empty = expected(&model, u64::MAX);
    model.write_uint(u64::MAX);
    model.write_bool(false);
    model.write_char('中');
    model.write_str("");
    model.write_bytes(&[0, 255, 17]);
    let first = expected(&model, u64::MAX);
    model.write_int(-7);
    let continued = expected(&model, u64::MAX);
    assert_eq!(
        printed(
            r#"
            use std.io.{println}
            use std.text.{Bytes}
            fn main() {
                let h = Hasher::with_seed(~uint(0))
                println(h.finish())
                h.write_uint(~uint(0))
                h.write_bool(false)
                h.write_char('中')
                h.write_str("")
                let bytes: [byte; 3] = [0, 255, 17]
                h.write_bytes(Bytes::copy_from(&bytes))
                println(h.finish())
                println(h.finish())
                h.write_int(-7)
                println(h.finish())
            }
            "#,
        ),
        format!("{empty}{first}{first}{continued}")
    );
}

#[test]
fn foldhash_short_and_long_boundaries_match_reference() {
    let lengths = [
        0, 1, 3, 4, 7, 8, 9, 15, 16, 17, 24, 31, 32, 56, 63, 64, 88, 95, 96, 120, 127, 128, 129,
        248, 256, 257, 345,
    ];
    let mut source = String::from(
        "use std.io.{println}\nfn main() {\nlet empty = Hasher::with_seed(0)\nprintln(empty.finish())\n",
    );
    let mut reference = expected(&Hasher::new(), 0);
    for (index, length) in lengths.into_iter().enumerate() {
        let text = (0..length)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect::<String>();
        source.push_str(&format!("let h{index} = Hasher::with_seed({index})\nh{index}.write_str(\"{text}\")\nprintln(h{index}.finish())\n"));
        let mut model = Hasher::new();
        model.write_str(&text);
        reference.push_str(&expected(&model, index as u64));
    }
    source.push_str("}\n");
    assert_eq!(printed(&source), reference);
}

#[test]
fn scalar_widths_and_algebraic_hash_feeding() {
    let mut reference = Hasher::new();
    for _ in 0..4 {
        reference.write_int(-1);
    }
    for value in [255, 65535, 4294967295, u64::MAX] {
        reference.write_uint(value);
    }
    for _ in 0..4 {
        reference.write_uint(u64::MAX);
    }
    reference.write_bool(true);
    reference.write_char('λ');
    reference.write_uint(0);
    reference.write_uint(1);
    reference.write_int(7);
    reference.write_uint(0);
    reference.write_str("ok");
    reference.write_uint(1);
    reference.write_bool(false);
    assert_eq!(
        printed(
            r#"
        use std.io.{println}
        fn feed[T: Hash](value: T, hasher: &Hasher) { (value, ()).hash(hasher) }
        fn main() {
            let h = Hasher::with_seed(17)
            feed((i8(-1), i16(-1), i32(-1), int(-1)), &h)
            feed((byte(255), ~u16(0), ~u32(0), ~uint(0)), &h)
            feed((i128(-1), ~u128(0)), &h)
            feed((true, 'λ'), &h)
            let absent: Option[int] = None
            let good: Result[string, bool] = Ok("ok")
            let bad: Result[string, bool] = Err(false)
            feed((absent, Some(7), good, bad), &h)
            println(h.finish())
        }
    "#
        ),
        expected(&reference, 17)
    );
}

#[test]
fn unsupported_builtin_hash_has_explicit_diagnostic() {
    let compiled = super::Compiler::shared().compile(super::CompileRequest::single_file(
        "main.gg",
        "fn main() { let h = Hasher::with_seed(0)\nlet values: [int; 2] = [1, 2]\nvalues.hash(&h) }",
        super::TargetName::X86_64Linux,
    ));
    assert!(!compiled.is_success());
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| { diagnostic.message().contains("暂不支持调用 Hash.hash") }),
        "{:?}",
        compiled.diagnostics().items()
    );
}
