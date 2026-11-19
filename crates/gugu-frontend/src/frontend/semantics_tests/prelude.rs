use super::accepts;

#[test]
fn source_prelude_names_resolve_without_use() {
    assert!(accepts(
        "fn take(v: Vec[int], m: HashMap[int, int], s: HashSet[int], p: Path, d: Duration) int {\n _ = m\n _ = s\n _ = p\n _ = d\n v.len() }\nfn pipe[R: Read, W: Write](r: &R, w: &W) {}\nfn main() {\n let v = Vec[int]::new()\n _ = v\n let w: Vec[string] = Vec::new()\n _ = w }"
    ));
}

#[test]
fn explicit_use_of_prelude_names_still_resolves() {
    assert!(accepts(
        "use std.vec.{Vec}\nuse std.collections.{HashMap}\nuse std.io.{Read}\nfn peek(m: HashMap[int, int], key: int) Option[int] = m.get(&key)\nfn pull[R: Read](r: &R) {}\nfn main() {\n let v = Vec[int]::new()\n _ = v }"
    ));
}
