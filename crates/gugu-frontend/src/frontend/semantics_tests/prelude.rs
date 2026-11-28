use super::accepts;

#[test]
fn source_prelude_implicit_and_explicit_use_resolve() {
    assert!(accepts(
        "fn take(v: Vec[int], m: HashMap[int, int], s: HashSet[int], p: Path, d: Duration) int {\n _ = m\n _ = s\n _ = p\n _ = d\n v.len() }\nfn pipe[R: Read, W: Write](r: &R, w: &W) {}\nfn main() {\n let v = Vec[int]::new()\n _ = v\n let w: Vec[string] = Vec::new()\n _ = w }"
    ));
    assert!(accepts(
        "use std.vec.{Vec}\nuse std.collections.{HashMap}\nuse std.io.{Read}\nfn peek(m: HashMap[int, int], key: int) Option[int] = m.get(&key)\nfn pull[R: Read](r: &R) {}\nfn main() {\n let v = Vec[int]::new()\n _ = v }"
    ));
}

#[test]
fn qualified_std_paths_resolve_types_functions_and_reexports() {
    assert!(accepts(
        "fn bench(b: &std.test.Bencher) { b.iter(fn() {}) }\n\
         fn main() {\n let m: std.collections.BTreeMap[string, int] = std.collections.BTreeMap::new()\n\
         _ = m.insert(\"a\", 1)\n std.io.println(\"ok\") }"
    ));
}

#[test]
fn qualified_std_paths_preserve_privacy_and_diagnostic_spans() {
    for path in [
        "std.test.Missing",
        "std.prelude.Option",
        "std.runtime.platform.RawPtr",
    ] {
        let source = format!("fn check(value: &{path}) {{}}\nfn main() {{}}");
        let compilation =
            crate::Compiler::shared().compile_sema_for_test(crate::CompileRequest::single_file(
                "main.gg",
                source.clone(),
                crate::TargetName::X86_64Linux,
            ));
        assert!(!compilation.is_success(), "{path}");
        assert!(
            compilation.diagnostics().items().iter().any(|error| {
                error.span().is_some_and(|span| {
                    let start = span.start() as usize;
                    let end = span.end() as usize;
                    source[start..end].contains(path)
                })
            }),
            "{:?}",
            compilation.diagnostics().items()
        );
    }
}

#[test]
fn collection_associated_functions_infer_keys_from_later_calls_and_context() {
    assert!(accepts(
        "use std.collections.{BTreeMap, BTreeSet}\nfn main() {\n\
         let map = HashMap::new()\n _ = map.insert(\"a\", 1)\n\
         let set = HashSet::new()\n _ = set.insert(1)\n\
         let tree = BTreeMap::new()\n _ = tree.insert(\"a\", 1)\n\
         let sorted = BTreeSet::new()\n _ = sorted.insert(1)\n\
         let typed: HashMap[string, int] = HashMap::new()\n _ = typed\n\
         let explicit = HashMap[string, int]::new()\n _ = explicit }"
    ));
    assert!(!accepts(
        "fn main() { let map = HashMap::new()\n _ = map.insert(1.5, 1) }"
    ));
}

#[test]
fn inferred_collection_iteration_defers_key_bounds_until_numeric_types_settle() {
    assert!(accepts(
        "fn main() {\n\
         let set = HashSet::new()\n _ = set.insert(1)\n\
         for value in set { let widened: u64 = value\n _ = widened }\n\
         let map = std.collections.BTreeMap::new()\n _ = map.insert(2, 3)\n\
         for (key, value) in map { let widened: u64 = key\n _ = widened\n _ = value }\n\
         let secure = std.collections.SecureHashSet::new()\n _ = secure.insert(4)\n\
         for value in secure { _ = value } }"
    ));
    assert!(!accepts(
        "fn main() { let set = HashSet::new()\n _ = set.insert(1.5)\n\
         for value in set { _ = value } }"
    ));
}
