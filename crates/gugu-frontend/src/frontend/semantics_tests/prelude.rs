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
