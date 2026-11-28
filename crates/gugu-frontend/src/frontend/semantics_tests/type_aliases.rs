use super::accepts;

#[test]
fn generic_type_aliases_transparency_and_error_contracts() {
    assert!(accepts(
        "type Table[K] = Vec[K]\n\
         type Nested[T] = Table[Table[T]]\n\
         type Again[T] = Table[T]\n\
         type Ints = Again[int]\n\
         type Plain = int\n\
         struct Row { values: Table[int] }\n\
         fn identity[T](xs: Table[T]) Vec[T] { xs }\n\
         fn nested(xs: Nested[int]) Vec[Vec[int]] { xs }\n\
         fn plain(x: Plain) int { x }\n\
         fn main() { let xs: Ints = Vec[int]::new()\n\
         let row = Row { values: identity(xs) }\n\
         let ys: Table[int] = row.values\n _ = ys\n\
         let zs: Vec[Vec[int]] = nested(Vec[Vec[int]]::new())\n _ = zs\n\
         _ = plain(1) }"
    ));
    assert!(!accepts(
        "type Table[K] = Vec[K]\nfn f(x: Table) {}\nfn main() {}"
    ));
    assert!(!accepts(
        "type Table[K] = Vec[K]\nfn f(x: Table[int, bool]) {}\nfn main() {}"
    ));
    assert!(!accepts(
        "type Table[K] = Vec[K]\nfn f(x: Table[int]) Table[bool] { x }\nfn main() {}"
    ));
    assert!(!accepts("type A[T] = B[T]\ntype B[T] = A[T]\nfn main() {}"));
}
