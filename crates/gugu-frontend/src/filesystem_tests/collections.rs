use super::printing::output_with_heap;

fn printed(source: &str) -> String {
    String::from_utf8(
        output_with_heap(source, &[], 1048576)
            .into_iter()
            .flat_map(|(fd, bytes)| {
                assert_eq!(fd, 1);
                bytes
            })
            .collect(),
    )
    .unwrap()
}

const CHECK: &str = r#"
    use std.io.{println}
    use std.collections.{HashMap, HashSet, BTreeMap, BTreeSet}
    #[track_caller]
    fn check(ok: bool) { if !ok { panic("collection assertion") } }
    fn is_none[T](value: Option[T]) bool { match value { None => true, Some(value) => false } }
    fn same(value: Option[int], expected: int) bool {
        match value {
            None => panic(f"expected {expected}, got None"),
            Some(value) => { if value != expected { panic(f"expected {expected}, got {value}") }
                true },
        }
    }
"#;

#[test]
fn tuple_and_variant_patterns_test_every_field() {
    assert_eq!(
        printed(
            r#"
            use std.io.{println}
            enum Item { Triple(int, int, int), Empty }
            fn tuple(value: (int, int, int)) bool {
                match value { (1, 2, 3) => true, _ => false }
            }
            fn item(value: Item) bool {
                match value { Item::Triple(1, 2, 3) => true, _ => false }
            }
            fn main() {
                println(tuple((1, 2, 3)), tuple((0, 2, 3)), tuple((1, 0, 3)), tuple((1, 2, 0)))
                println(item(Item::Triple(1, 2, 3)), item(Item::Triple(0, 2, 3)),
                    item(Item::Triple(1, 0, 3)), item(Item::Triple(1, 2, 0)), item(Item::Empty))
                println(match () { () => true })
            }
            "#,
        ),
        "truefalsefalsefalse\ntruefalsefalsefalsefalse\ntrue\n"
    );
}

#[test]
fn generic_key_comparisons_dispatch_eq_and_ord() {
    assert_eq!(
        printed(&format!(
            r#"{CHECK}
            struct Key {{ id: int, ignored: int }}
            impl Eq for Key {{ fn eq(self: &Self, rhs: &Self) bool = self.id == rhs.id }}
            impl Ord for Key {{ fn cmp(self: &Self, rhs: &Self) int = self.id - rhs.id }}
            unsafe impl StableOrd for Key {{}}
            fn equal[K: Eq](a: K, b: K) bool = a == b
            fn unequal[K: Eq](a: K, b: K) bool = a != b
            fn less[K: Ord](a: K, b: K) bool = a < b
            fn main() {{
                let one = Key {{ id: 1, ignored: 100 }}
                let alias = Key {{ id: 1, ignored: 0 }}
                let two = Key {{ id: 2, ignored: 0 }}
                check(equal(one, alias) && !unequal(one, alias) && unequal(one, two))
                check(!less(one, alias) && less(one, two) && !less(two, one))
                check(equal(9, 9) && unequal(9, 10) && less(9, 10))
                check(equal("one", "one") && unequal("one", "two") && less("a", "b"))
                let tree = BTreeMap[Key, int]::new()
                check(is_none(tree.insert(two, 2)) && is_none(tree.insert(one, 1)))
                check(same(tree.insert(alias, 11), 1) && tree.len() == 2)
                let expected = 1
                for (key, value) in tree {{
                    check(key.id == expected)
                    expected = expected + 1
                }}
                check(expected == 3 && same(tree.get(&alias), 11))
                println("ok")
            }}"#
        )),
        "ok\n"
    );
}

#[test]
fn hash_map_empty_core_and_string_keys() {
    assert_eq!(
        printed(&format!(
            r#"{CHECK}
            fn main() {{
                let map = HashMap[string, int]::new()
                let absent = "absent"
                let one = "one"
                let two = "two"
                check(map.is_empty() && map.len() == 0)
                check(is_none(map.get(&absent)) && is_none(map.remove(&absent)))
                check(is_none(map.iter().next()))
                check(is_none(map.insert("one", 1)))
                check(is_none(map.insert("two", 2)))
                check(same(map.insert("one", 11), 1))
                check(same(map.get(&one), 11) && map.len() == 2)
                let alias = map
                check(same(alias.remove(&two), 2))
                check(!map.contains_key(&two) && map.len() == 1)
                check(same(map.remove(&one), 11) && alias.is_empty())
                check(is_none(map.iter().next()))
                println("ok")
            }}"#
        )),
        "ok\n"
    );
}

#[test]
fn hash_trie_levels_aliases_and_snapshot_pairs() {
    assert_eq!(
        printed(&format!(
            r#"{CHECK}
            fn main() {{
                let map = HashMap[int, int]::new()
                let zero = 0
                let last = 127
                let five = 5
                let missing = 129
                for i in 0..128 {{ check(is_none(map.insert(i, i * 3))) }}
                check(map.len() == 128)
                let saved = map.iter()
                let alias = map
                check(same(alias.remove(&zero), 0))
                check(same(alias.remove(&last), 381))
                _ = alias.insert(200, 600)
                _ = alias.insert(5, -1)
                let seen: [bool; 128] = [false; 128]
                let count = 0
                while let Some((key, value)) = saved.next() {{
                    check(key >= 0 && key < 128 && value == key * 3)
                    check(!seen[key])
                    seen[key] = true
                    count = count + 1
                }}
                check(count == 128 && is_none(saved.next()))
                for i in 0..128 {{ check(seen[i]) }}
                let current = 0
                for (key, value) in map {{
                    check(same(map.get(&key), value))
                    check(key != 0 && key != 127)
                    current = current + 1
                }}
                check(current == 127 && same(map.get(&five), -1))
                check(is_none(map.get(&missing)) && is_none(map.get(&zero)))
                println("ok")
            }}"#
        )),
        "ok\n"
    );
}

#[test]
fn hash_collisions_compare_semantic_keys_and_keep_snapshots() {
    assert_eq!(
        printed(&format!(
            r#"{CHECK}
            struct Key {{ id: int, ignored: int }}
            impl Eq for Key {{ fn eq(self: &Self, other: &Self) bool = self.id == other.id }}
            impl Hash for Key {{ fn hash(self: &Self, h: &Hasher) {{ h.write_int(42) }} }}
            unsafe impl StableHash for Key {{}}
            fn key(id: int) Key = Key {{ id: id, ignored: 0 }}
            fn main() {{
                let map = HashMap[Key, int]::new()
                let k0 = key(0)
                let k1 = key(1)
                let k2 = key(2)
                let k3 = key(3)
                let k4 = key(4)
                let k5 = key(5)
                let k7 = key(7)
                let k9 = key(9)
                let revised = Key {{ id: 3, ignored: 99 }}
                for i in 0..6 {{ check(is_none(map.insert(key(i), i * 10))) }}
                for i in 0..6 {{ let k = key(i)
                    match map.get(&k) {{
                        None => panic(f"missing collision key {{i}}"),
                        Some(value) => check(value == i * 10),
                    }}
                }}
                let saved = map.iter()
                check(same(map.insert(revised, 33), 30))
                check(map.len() == 6 && same(map.get(&k3), 33))
                check(is_none(map.get(&k9)) && is_none(map.remove(&k9)))
                check(same(map.remove(&k0), 0))
                check(same(map.remove(&k3), 33))
                check(same(map.remove(&k5), 50))
                let seen: [bool; 6] = [false; 6]
                let count = 0
                while let Some((key, value)) = saved.next() {{
                    check(!seen[key.id] && value == key.id * 10)
                    seen[key.id] = true
                    count = count + 1
                }}
                check(count == 6 && map.len() == 3)
                check(same(map.remove(&k1), 10))
                check(same(map.remove(&k2), 20))
                check(same(map.remove(&k4), 40))
                check(map.is_empty() && is_none(map.iter().next()))
                check(is_none(map.insert(key(7), 70)) && same(map.get(&k7), 70))
                println("ok")
            }}"#
        )),
        "ok\n"
    );
}

#[test]
fn maps_views_update_and_entry_semantics() {
    let operations = r#"
        let missing = 99
        let one = 1
        let two = 2
        let three = 3
        check(is_none(map.with_ref(&missing, fn(v: &int) int { panic("missing callback") })))
        map.for_each_ref(fn(k: &int, v: &int) { panic("empty callback") })
        check(!map.update(&missing, fn(v: int) int { panic("missing update") }))
        check(map.entry(1).and_modify(fn(v: int) int = v + 1).or_insert(10) == 10)
        check(map.entry(1).or_insert_with(fn() int { panic("occupied default") }) == 10)
        check(map.entry(2).or_insert_with(fn() int = 20) == 20)
        check(map.entry(1).and_modify(fn(v: int) int = v + 2).or_insert(0) == 12)
        check(map.update(&two, fn(v: int) int = v + 3))
        check(same(map.with_ref(&one, fn(v: &int) int = *v + 1), 13))
        let sum = 0
        let visits = 0
        map.for_each_ref(fn(k: &int, v: &int) {
            sum = sum + *v
            visits = visits + 1
            check(same(map.with_ref(k, fn(v: &int) int = *v), *v))
        })
        check(sum == 35 && visits == 2)
        check(is_none(map.insert(3, 30)))
        check(same(map.remove(&three), 30))
        println("ok")
    "#;
    assert_eq!(
        printed(&format!(
            "{CHECK}\nfn main() {{ let map = HashMap[int, int]::new()\n{operations}\nlet map = BTreeMap[int, int]::new()\n{operations}\n}}"
        )),
        "ok\nok\n"
    );
}

#[test]
fn avl_balancing_order_removal_and_snapshot_pairs() {
    assert_eq!(
        printed(&format!(
            r#"{CHECK}
            fn main() {{
                let map = BTreeMap[int, int]::new()
                let one = 1
                let fifty = 50
                check(map.is_empty() && is_none(map.get(&one)) && is_none(map.remove(&one)))
                check(is_none(map.iter().next()))
                for i in 0..96 {{ let key = (i * 37) % 96
                    check(is_none(map.insert(key, key * 3))) }}
                check(map.len() == 96 && same(map.insert(50, 150), 150))
                let saved = map.iter()
                let alias = map
                for i in 0..48 {{ let key = i * 2
                    check(same(alias.remove(&key), i * 6)) }}
                check(map.len() == 48 && !map.contains_key(&fifty))
                let expected = 0
                while let Some((key, value)) = saved.next() {{
                    check(key == expected && value == key * 3)
                    expected = expected + 1
                }}
                check(expected == 96 && is_none(saved.next()))
                expected = 1
                for (key, value) in map {{
                    check(key == expected && value == key * 3)
                    expected = expected + 2
                }}
                check(expected == 97)
                expected = 1
                map.for_each_ref(fn(k: &int, v: &int) {{
                    check(*k == expected && *v == *k * 3)
                    expected = expected + 2
                }})
                check(expected == 97)
                for i in 0..48 {{ let key = 95 - i * 2
                    check(same(map.remove(&key), key * 3)) }}
                check(map.is_empty() && is_none(map.iter().next()))
                for i in 0..32 {{ _ = map.insert(i, i) }}
                for i in 0..32 {{ check(same(map.remove(&i), i)) }}
                check(map.is_empty())
                println("ok")
            }}"#
        )),
        "ok\n"
    );
}

#[test]
fn sets_identity_snapshots_order_and_views() {
    let operations = r#"
        let one = 1
        let two = 2
        let nine = 9
        check(set.is_empty() && !set.contains(&nine) && !set.remove(&nine))
        check(is_none(set.iter().next()))
        set.for_each_ref(fn(k: &int) { panic("empty set callback") })
        check(set.insert(3) && set.insert(1) && set.insert(2))
        check(!set.insert(3) && set.len() == 3)
        let saved = set.iter()
        let alias = set
        check(alias.remove(&two) && !alias.remove(&two))
        check(!set.contains(&two) && set.contains(&one) && set.len() == 2)
        let sum = 0
        while let Some(key) = saved.next() { sum = sum + key }
        check(sum == 6 && is_none(saved.next()))
        sum = 0
        set.for_each_ref(fn(k: &int) { sum = sum + *k })
        check(sum == 4 && set.insert(5))
    "#;
    assert_eq!(
        printed(&format!(
            "{CHECK}\nfn main() {{ let set = HashSet[int]::new()\n{operations}\nlet set = BTreeSet[int]::new()\n{operations}\nlet expected = 1\nfor key in set {{ check(key == expected)\nexpected = expected + 2 }}\ncheck(expected == 7)\nprintln(\"ok\")\n}}"
        )),
        "ok\n"
    );
}

#[test]
#[should_panic(expected = "Runtime(Panic): map write during scoped view")]
fn map_alias_write_in_view_panics() {
    printed(&format!(
        "{CHECK}\nfn main() {{ let map = HashMap[int, int]::new()\nlet one = 1\n_ = map.insert(1, 10)\nlet alias = map\n_ = map.with_ref(&one, fn(v: &int) int {{ _ = alias.insert(2, 20)\n*v }}) }}"
    ));
}

#[test]
#[should_panic(expected = "Runtime(Panic): map write during scoped view")]
fn ordered_entry_write_in_view_panics() {
    printed(&format!(
        "{CHECK}\nfn main() {{ let map = BTreeMap[int, int]::new()\n_ = map.insert(1, 10)\nlet entry = map.entry(2)\nmap.for_each_ref(fn(k: &int, v: &int) {{ _ = entry.or_insert(20) }}) }}"
    ));
}

#[test]
fn collections_e2e_fixture_matches_stdout() {
    assert_eq!(
        printed(include_str!("../../../../ci/e2e/fixtures/collections.gg")),
        include_str!("../../../../ci/e2e/fixtures/collections.stdout")
    );
}
