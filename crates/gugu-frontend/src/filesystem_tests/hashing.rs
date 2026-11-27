use super::{CompileRequest, Compiler, TargetName, harness::Harness, printing::capture};
use gugu_runtime_world::hash::{HashFamily, HashOutput, Hasher};

fn expected(hasher: &Hasher, seed: u64) -> String {
    let HashOutput::Bits64(value) = hasher.finish(HashFamily::FoldHashFast { seed }) else {
        unreachable!()
    };
    format!("{value}\n")
}

fn run_entry(compiled: &super::Compilation, entry: &str) -> String {
    let mut harness = Harness::new(compiled);
    harness.machine_hook = Some(capture);
    harness.invoke("main.gg", entry, &[], &mut vec![0; 262144]);
    let bytes = harness
        .calls
        .into_iter()
        .flat_map(|(fd, bytes)| {
            assert_eq!(fd, 1);
            bytes.into_iter().map(|byte| byte as u8)
        })
        .collect::<Vec<_>>();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn hashing_runtime_contracts() {
    let lengths = [
        0, 1, 3, 4, 7, 8, 9, 15, 16, 17, 24, 31, 32, 56, 63, 64, 88, 95, 96, 120, 127, 128, 129,
        248, 256, 257, 345,
    ];
    let mut boundary_body =
        String::from("    let empty = Hasher::with_seed(0)\n    println(empty.finish())\n");
    let mut boundary_reference = expected(&Hasher::new(), 0);
    for (index, length) in lengths.into_iter().enumerate() {
        let text = (0..length)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect::<String>();
        boundary_body.push_str(&format!(
            "    let h{index} = Hasher::with_seed({index})\n    h{index}.write_str(\"{text}\")\n    println(h{index}.finish())\n"
        ));
        let mut model = Hasher::new();
        model.write_str(&text);
        boundary_reference.push_str(&expected(&model, index as u64));
    }

    let source = format!(
        r#"
use std.io.{{println}}
use std.text.{{Bytes}}

struct Key {{ number: int, name: string }}
impl Hash for Key {{
    fn hash(self: &Self, hasher: &Hasher) {{
        self.number.hash(hasher)
        self.name.hash(hasher)
        hasher.write_uint(9)
    }}
}}
fn digest_123[T: Hash](key: T) uint {{
    let h = Hasher::with_seed(123)
    key.hash(&h)
    h.finish()
}}
fn seeded_case() {{
    println(digest_123(-42))
    println(digest_123("ab中"))
    println(digest_123((-42, "ab中", true)))
    println(digest_123((Key {{ number: -42, name: "ab中" }}, ())))
    println(digest_123((-42, "ab中", true)) == digest_123((-42, "ab中", true)))
}}

fn finish_case() {{
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
}}

fn boundary_case() {{
{boundary_body}}}

fn feed[T: Hash](value: T, hasher: &Hasher) {{ (value, ()).hash(hasher) }}
fn scalar_case() {{
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
}}

struct Wrapper[T] {{ value: T }}
impl Wrapper[Hasher] {{
    fn digest(self: &Self) uint = self.value.finish()
}}
struct Opaque {{ id: int }}
impl Hash for (Opaque, int) {{
    fn hash(self: &Self, hasher: &Hasher) {{ hasher.write_int(99) }}
}}
fn digest_5[T: Hash](value: T) uint {{
    let h = Hasher::with_seed(5)
    value.hash(&h)
    h.finish()
}}
fn prelude_case() {{
    let h = Hasher::with_seed(5)
    h.write_int(99)
    let wrapped = Wrapper {{ value: h }}
    println(wrapped.digest() == digest_5((Opaque {{ id: 1 }}, 2)))
}}

fn main() {{
    seeded_case()
    finish_case()
    boundary_case()
    scalar_case()
    prelude_case()
}}
"#
    );
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );

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
        run_entry(&compiled, "seeded_case"),
        format!(
            "{}{}{}{}true\n",
            expected(&integer, 123),
            expected(&text, 123),
            expected(&tuple, 123),
            expected(&user, 123)
        )
    );

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
        run_entry(&compiled, "finish_case"),
        format!("{empty}{first}{first}{continued}")
    );

    assert_eq!(run_entry(&compiled, "boundary_case"), boundary_reference);

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
        run_entry(&compiled, "scalar_case"),
        expected(&reference, 17)
    );
    assert_eq!(run_entry(&compiled, "prelude_case"), "true\n");
}

#[test]
fn unsupported_builtin_hash_has_explicit_diagnostic() {
    let compiled = Compiler::shared().compile_frontend_for_test(CompileRequest::single_file(
        "main.gg",
        "fn main() { let h = Hasher::with_seed(0)\nlet values: [int; 2] = [1, 2]\nvalues.hash(&h) }",
        TargetName::X86_64Linux,
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
