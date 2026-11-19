use super::{CompileRequest, Compiler, TargetName, printing::output};

const CLONE_IMPL: &str = r#"
impl[T: Clone, comptime N: int] Clone for [T; N] {
    fn clone(self: &Self) [T; N] {
        let out: [T; N]
        for i in 0..N { out[i] = self[i].clone() }
        out
    }
}
"#;

#[test]
fn array_clone_fallback_selects_each_nested_element() {
    let source = r#"
use std.io.{println}
impl[T: Clone] Clone for [T; 2] {
    fn clone(self: &Self) Self { println("inner")
        [self[1].clone(), self[0].clone()] }
}
fn main() {
    let values = [["a", "b"], ["c", "d"], ["e", "f"]]
    let copied = values.clone()
    println(copied[0][0])
    println(copied[2][1])
    let empty: [[string; 2]; 0] = []
    _ = empty.clone()
}
"#;
    let bytes: Vec<_> = output(source, &[])
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(bytes, b"inner\ninner\ninner\nb\ne\n");
}

#[test]
fn array_clone_fallback_supports_managed_source_element_impls() {
    let source = r#"
use std.io.{println}
struct Item { text: string }
impl Clone for Item { fn clone(self: &Self) Self { Item { text: self.text + "!" } } }
fn main() {
    let values = [Item { text: "a" }, Item { text: "b" }]
    let copied = values.clone()
    println(copied[1].text)
}
"#;
    let bytes: Vec<_> = output(source, &[])
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(bytes, b"b!\n");
}

#[test]
fn array_clone_fallback_propagates_element_panic() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        r#"
impl[T: Clone] Clone for [T; 2] { fn clone(self: &Self) Self { panic("inner") } }
fn main() { let values = [["a", "b"], ["c", "d"], ["e", "f"]]
    _ = values.clone() }
"#,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let mut harness = super::harness::Harness::new(&compiled);
    harness.root_frames = Some(Default::default());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.invoke("main.gg", "main", &[], &mut vec![0; 262144])
    }));
    assert!(
        result
            .unwrap_err()
            .is::<super::root_trace::InterpretedPanic>()
    );
}

#[test]
fn array_impl_lengths_use_distinct_const_argument_identities() {
    let source = "trait Length { fn length(self: &Self) int }\nimpl[T, comptime N: int] Length for [T; N] { fn length(self: &Self) int { N } }\nfn main() { _ = [1, 2].length()\n _ = [1, 2, 3].length()\n _ = [1, 2].length() }";
    let queries = crate::QueryEngine::new();
    let compile = || {
        let mut sources = crate::SourceMap::new(vec![
            crate::SourceSnapshot::from_str("main.gg", source).unwrap(),
        ])
        .unwrap();
        crate::frontend::bootstrap(
            crate::frontend::SourceInput::Sources {
                source_map: &mut sources,
                entry: "main.gg",
                source_root: "",
                package_identity: "tests/array-impls@1",
                require_main: true,
                cfg: &crate::frontend::cfg::CfgContext::target_only(TargetName::X86_64Linux),
                external_packages: &Default::default(),
                embed: None,
            },
            &queries,
            crate::runtime::RuntimeResources::new(
                crate::testing::provider()
                    .artifact(TargetName::X86_64Linux)
                    .unwrap(),
            ),
        )
        .unwrap()
    };
    let output = compile();
    let instances: Vec<_> = output
        .mono
        .instances
        .iter()
        .filter(|instance| instance.symbol == "length")
        .collect();
    assert_eq!(instances.len(), 2);
    assert_eq!(instances[0].const_arguments.len(), 1);
    assert_eq!(instances[1].const_arguments.len(), 1);
    assert_ne!(instances[0].const_arguments, instances[1].const_arguments);
    assert_ne!(instances[0].mono_key, instances[1].mono_key);
    assert_eq!(output.mono, compile().mono);
}

#[test]
fn array_const_impls_clone_two_lengths_and_managed_elements() {
    let source = format!(
        r#"
use std.io.{{println}}
{CLONE_IMPL}
struct Item {{ text: string }}
impl Clone for Item {{ fn clone(self: &Self) Self {{ Item {{ text: self.text + "!" }} }} }}
fn main() {{
    let a = [Item {{ text: "a" }}, Item {{ text: "b" }}].clone()
    let b = [Item {{ text: "c" }}, Item {{ text: "d" }}, Item {{ text: "e" }}].clone()
    let empty: [Item; 0] = []
    _ = empty.clone()
    println(a[1].text)
    println(b[2].text)
}}
"#
    );
    let bytes: Vec<_> = output(&source, &[])
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(bytes, b"b!\ne!\n");
}

#[test]
fn array_const_impls_builtin_clone_remains_a_fallback() {
    let bytes: Vec<_> = output(
        r#"
use std.io.{println}
fn main() {
    let source: [int; 2] = [1, 2]
    let numbers = source.clone()
    let words = ["a", "b"].clone()
    let empty: [string; 0] = []
    let cloned = empty.clone()
    println(numbers[1])
    println(words[1])
    println(cloned.len())
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(bytes, b"2\nb\n0\n");
}

#[test]
fn array_const_impls_select_length_and_specialize() {
    let bytes: Vec<_> = output(
        r#"
use std.io.{println}
trait Length { fn length(self: &Self) int }
impl[T, comptime N: int] Length for [T; N] { fn length(self: &Self) int { N } }
impl[T] Length for [T; 4] { fn length(self: &Self) int { 40 } }
fn main() { println([1, 2].length())
    println([1, 2, 3].length())
    println([1, 2, 3, 4].length()) }
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(bytes, b"2\n3\n40\n");
}

fn rejects(source: &str, message: &str) {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(!compiled.is_success());
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains(message)),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn array_const_impls_reject_symbolic_concrete_cross_overlap() {
    rejects(
        "trait Mark {}\nimpl[T, comptime N: int] Mark for [(T, int); N] {}\nimpl[U] Mark for [(int, U); 4] {}\nfn main() {}",
        "重叠",
    );
}

#[test]
fn array_const_impls_reject_symbolic_symbolic_cross_overlap() {
    rejects(
        "trait Mark {}\nimpl[T, comptime N: int] Mark for [(T, int); N] {}\nimpl[U, comptime M: int] Mark for [(int, U); M] {}\nfn main() {}",
        "重叠",
    );
}

#[test]
fn array_const_impls_reject_alpha_equivalent_duplicates() {
    rejects(
        "trait Mark {}\nimpl[T, comptime N: int] Mark for [T; N] {}\nimpl[U, comptime M: int] Mark for [U; M] {}\nfn main() {}",
        "重叠",
    );
}

#[test]
fn array_const_impls_keep_repeated_length_constraints_consistent() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file("main.gg", "trait Mark {}\nimpl[comptime N: int] Mark for [[int; N]; N] {}\nimpl Mark for [[int; 3]; 4] {}\nfn main() {}", TargetName::X86_64Linux));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn array_const_impls_trait_value_parameters_remain_unsupported() {
    rejects(
        "trait Count[comptime N: int] {}\nfn main() {}",
        "trait 的 comptime 泛型参数尚未实现",
    );
}

#[test]
fn array_const_impls_empty_managed_array_type_is_valid_without_impl() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "struct Item { text: string }\nfn main() { let empty: [Item; 0] = []\n _ = empty }",
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
}
