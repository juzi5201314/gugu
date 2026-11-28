use super::{
    CompileRequest, Compiler, TargetName,
    harness::Harness,
    printing::{capture, output_with_heap},
};

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

fn run_printed_with(harness: &mut Harness<'_>, memory: &mut Vec<u8>, entry: &str) -> String {
    harness.reset_for_next_call(memory);
    harness.invoke("main.gg", entry, &[], memory);
    String::from_utf8(
        harness
            .calls
            .iter()
            .flat_map(|(fd, bytes)| {
                assert_eq!(*fd, 1);
                bytes.iter().map(|byte| *byte as u8)
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

fn aggregate_key_fn(
    name: &str,
    ty: &str,
    low: &str,
    middle: &str,
    high: &str,
    fresh: &str,
) -> String {
    format!(
        r#"
        fn {name}() {{
            let low: {ty} = {low}
            let middle: {ty} = {middle}
            let high: {ty} = {high}
            let fresh: {ty} = {fresh}
            let map = HashMap[{ty}, int]::new()
            check(is_none(map.insert(high, 30)))
            check(is_none(map.insert(low, 10)))
            check(is_none(map.insert(middle, 20)))
            check(same(map.get(&fresh), 10))
            check(same(map.insert(fresh, 11), 10) && map.len() == 3)
            let tree = BTreeMap[{ty}, int]::new()
            check(is_none(tree.insert(high, 30)))
            check(is_none(tree.insert(middle, 20)))
            check(is_none(tree.insert(low, 10)))
            check(same(tree.get(&fresh), 10))
            check(same(tree.insert(fresh, 11), 10) && tree.len() == 3)
            let index = 0
            for (key, value) in tree {{
                match index {{
                    0 => check(equal(key, low) && value == 11),
                    1 => check(equal(key, middle) && value == 20),
                    _ => check(equal(key, high) && value == 30),
                }}
                index = index + 1
            }}
            check(index == 3 && equal(low, fresh) && !equal(low, high))
            check(ordered(low, middle) && ordered(middle, high))
            check(!ordered(low, fresh) && !ordered(high, middle))
            check(low == fresh && low != high && low < middle && middle < high)
            check(same(map.remove(&fresh), 11) && map.len() == 2)
            check(same(tree.remove(&fresh), 11) && tree.len() == 2)
            println("ok")
        }}
"#
    )
}

#[test]
fn collections_aggregate_keys_and_structural_comparisons() {
    let mut source = String::from(CHECK);
    source.push_str(
        r#"
        fn equal[T: Eq](a: T, b: T) bool = a == b
        fn unequal[T: Eq](a: T, b: T) bool = a != b
        fn less[T: Ord](a: T, b: T) bool = a < b
        fn ordered[T: Ord](a: T, b: T) bool = a < b && a <= b && b > a && b >= a
        fn compare[T: Eq + Ord](a: T, b: T) int {
            if a == b { 0 } else if a < b { -1 } else { 1 }
        }
"#,
    );
    source.push_str(&aggregate_key_fn(
        "case_large_tuples",
        "(int, int, int)",
        "(1, 2, 3)",
        "(1, 9, 9)",
        "(2, 1, 0)",
        "(1, 2, 3)",
    ));
    source.push_str(&aggregate_key_fn(
        "case_small_tuples",
        "(int, int)",
        "(-2, 50)",
        "(1, 2)",
        "(1, 9)",
        "(-2, 50)",
    ));
    source.push_str(&aggregate_key_fn(
        "case_string_tuples",
        "(string, int)",
        r#"("ant", 1)"#,
        r#"("ant", 2)"#,
        r#"("bat", 0)"#,
        r#"("a" + "nt", 1)"#,
    ));
    source.push_str(&aggregate_key_fn(
        "case_nested_tuples",
        "((string, int), (int, int, int))",
        r#"(("ant", -1), (1, 2, 3))"#,
        r#"(("ant", -1), (1, 3, 0))"#,
        r#"(("bat", -5), (0, 0, 0))"#,
        r#"(("a" + "nt", -1), (1, 2, 3))"#,
    ));
    source.push_str(
        r#"
        fn case_nan_equality() {
            let nan = unsafe { std.mem.transmute[uint, float](0x7ff8000000000000) }
            check(nan != nan && nan != 1.0 && !(nan == 1.0))
            check(!([nan] == [1.0]) && [nan] != [1.0])
            check(!([nan] == [nan]) && [nan] != [nan])
            check(!((nan, 0) == (1.0, 0)) && (nan, 0) != (1.0, 0))
            check(!((nan, 0) == (nan, 0)) && (nan, 0) != (nan, 0))
            check([1.0] == [1.0] && (1.0, 2) == (1.0, 2))
            let negative_zero = unsafe { std.mem.transmute[uint, float](0x8000000000000000) }
            check([0.0] == [negative_zero] && !([0.0] != [negative_zero]))
            check(!([(nan, 1)] == [(nan, 1)]) && [(nan, 1)] != [(1.0, 1)])
            let empty: [float; 0] = []
            check(empty == empty && !(empty != empty))
            let narrow = unsafe { std.mem.transmute[u32, f32](0x7fc00000) }
            let one: f32 = 1.0
            check(!([narrow] == [one]) && [narrow] != [one])
            check(!((narrow, 0) == (narrow, 0)) && (narrow, 0) != (narrow, 0))
            println("ok")
        }

        fn case_nan_ordering() {
            let nan = unsafe { std.mem.transmute[uint, float](0x7ff8000000000000) }
            check(!([nan] < [1.0]) && !([nan] > [1.0]) && !([nan] <= [1.0]) && !([nan] >= [1.0]))
            check(!([nan] < [nan]) && !([nan] > [nan]) && !([nan] <= [nan]))
            check(!((nan, 0) < (1.0, 1)) && !((nan, 1) > (1.0, 0)) && !((nan, 0) <= (1.0, 1)))
            check(!((nan, 0) < (nan, 1)) && !((nan, 1) > (nan, 0)) && !((nan, 0) <= (nan, 0)))
            check(!((1.0, nan) < (1.0, 2.0)) && !((1.0, nan) <= (1.0, 2.0)))
            check((0, nan) < (1, nan) && (1, nan) > (0, nan))
            check([1.0] <= [1.0] && [1.0] < [2.0] && [2.0] > [1.0])
            check(!([(nan, 0)] < [(1.0, 1)]) && !([(nan, 0)] >= [(1.0, 1)]))
            let empty: [float; 0] = []
            check(empty <= empty && empty >= empty && !(empty < empty))
            let narrow = unsafe { std.mem.transmute[u32, f32](0x7fc00000) }
            let one: f32 = 1.0
            check(!([narrow] < [one]) && !([narrow] > [one]) && !([narrow] <= [one]))
            check(!((narrow, 0) < (narrow, 1)) && !((narrow, 0) <= (narrow, 0)))
            println("ok")
        }

        fn case_empty_arrays_and_variants() {
            check(equal((), ()) && compare((), ()) == 0)
            let empty: [int; 0] = [0; 0]
            check(equal(empty, empty) && compare(empty, empty) == 0)
            check(equal([(1, 2), (3, 4)], [(1, 2), (3, 4)]))
            check(compare([(1, 2), (3, 4)], [(1, 2), (3, 5)]) < 0)
            check(compare([(-1, 9), (8, 9)], [(0, 0), (0, 0)]) < 0)
            let none: Option[(string, int)] = None
            let some: Option[(string, int)] = Some(("ant", -1))
            let same_some: Option[(string, int)] = Some(("a" + "nt", -1))
            let later: Option[(string, int)] = Some(("ant", 1))
            check(equal(none, none) && !equal(none, some))
            check(equal(some, same_some) && compare(some, same_some) == 0)
            check(compare(none, some) > 0 && compare(some, none) < 0)
            check(compare(some, later) < 0)
            let ok: Result[(int, int), string] = Ok((1, 2))
            let same_ok: Result[(int, int), string] = Ok((1, 2))
            let error: Result[(int, int), string] = Err("ant")
            let same_error: Result[(int, int), string] = Err("a" + "nt")
            check(equal(ok, same_ok) && equal(error, same_error) && !equal(ok, error))
            check(compare(ok, error) < 0 && compare(error, ok) > 0)
            println("ok")
        }

        enum Item { Triple(int, int, int), Empty }
        fn tuple_match(value: (int, int, int)) bool {
            match value { (1, 2, 3) => true, _ => false }
        }
        fn item_match(value: Item) bool {
            match value { Item::Triple(1, 2, 3) => true, _ => false }
        }
        fn case_tuple_and_variant_patterns() {
            println(tuple_match((1, 2, 3)), tuple_match((0, 2, 3)), tuple_match((1, 0, 3)), tuple_match((1, 2, 0)))
            println(item_match(Item::Triple(1, 2, 3)), item_match(Item::Triple(0, 2, 3)),
                item_match(Item::Triple(1, 0, 3)), item_match(Item::Triple(1, 2, 0)), item_match(Item::Empty))
            println(match () { () => true })
        }

        fn case_array_rest_patterns() {
            let arr = [1, 2, 3, 4]
            println(match arr { [.., 4] => true, _ => false })
            println(match arr { [1, .., 3, 4] => true, _ => false })
            println(match arr { [.., last] => last, _ => 0 })
            println(match arr { [first, .., before, last] => first * 100 + before * 10 + last, _ => 0 })
        }

        struct OrdKey { id: int, ignored: int }
        impl Eq for OrdKey { fn eq(self: &Self, rhs: &Self) bool = self.id == rhs.id }
        impl Ord for OrdKey { fn cmp(self: &Self, rhs: &Self) int = self.id - rhs.id }
        unsafe impl StableOrd for OrdKey {}
        fn case_generic_key_comparisons() {
            let one = OrdKey { id: 1, ignored: 100 }
            let alias = OrdKey { id: 1, ignored: 0 }
            let two = OrdKey { id: 2, ignored: 0 }
            check(equal(one, alias) && !unequal(one, alias) && unequal(one, two))
            check(!less(one, alias) && less(one, two) && !less(two, one))
            check(equal(9, 9) && unequal(9, 10) && less(9, 10))
            check(equal("one", "one") && unequal("one", "two") && less("a", "b"))
            let tree = BTreeMap[OrdKey, int]::new()
            check(is_none(tree.insert(two, 2)) && is_none(tree.insert(one, 1)))
            check(same(tree.insert(alias, 11), 1) && tree.len() == 2)
            let expected = 1
            for (key, value) in tree {
                check(key.id == expected)
                expected = expected + 1
            }
            check(expected == 3 && same(tree.get(&alias), 11))
            println("ok")
        }

        fn main() {
            case_large_tuples()
            case_small_tuples()
            case_string_tuples()
            case_nested_tuples()
            case_nan_equality()
            case_nan_ordering()
            case_empty_arrays_and_variants()
            case_tuple_and_variant_patterns()
            case_array_rest_patterns()
            case_generic_key_comparisons()
        }
"#,
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
    let mut harness = Harness::new(&compiled);
    harness.machine_hook = Some(capture);
    let mut memory = vec![0; 1048576];
    for entry in [
        "case_large_tuples",
        "case_small_tuples",
        "case_string_tuples",
        "case_nested_tuples",
        "case_nan_equality",
        "case_nan_ordering",
        "case_empty_arrays_and_variants",
        "case_generic_key_comparisons",
    ] {
        assert_eq!(
            run_printed_with(&mut harness, &mut memory, entry),
            "ok\n",
            "{entry}"
        );
    }
    assert_eq!(
        run_printed_with(&mut harness, &mut memory, "case_tuple_and_variant_patterns"),
        "truefalsefalsefalse\ntruefalsefalsefalsefalse\ntrue\n"
    );
    assert_eq!(
        run_printed_with(&mut harness, &mut memory, "case_array_rest_patterns"),
        "true\ntrue\n4\n134\n"
    );
}

#[test]
fn collections_maps_sets_and_scoped_views() {
    let map_operations = r#"
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
    let set_operations = r#"
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
    let source = format!(
        r#"{CHECK}
        fn case_hash_map_string_keys() {{
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
        }}

        fn case_hash_trie_snapshots() {{
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
        }}

        struct CollideKey {{ id: int, ignored: int }}
        impl Eq for CollideKey {{ fn eq(self: &Self, other: &Self) bool = self.id == other.id }}
        impl Hash for CollideKey {{ fn hash(self: &Self, h: &Hasher) {{ h.write_int(42) }} }}
        unsafe impl StableHash for CollideKey {{}}
        fn collide_key(id: int) CollideKey = CollideKey {{ id: id, ignored: 0 }}
        fn case_hash_collisions() {{
            let map = HashMap[CollideKey, int]::new()
            let k0 = collide_key(0)
            let k1 = collide_key(1)
            let k2 = collide_key(2)
            let k3 = collide_key(3)
            let k4 = collide_key(4)
            let k5 = collide_key(5)
            let k7 = collide_key(7)
            let k9 = collide_key(9)
            let revised = CollideKey {{ id: 3, ignored: 99 }}
            for i in 0..6 {{ check(is_none(map.insert(collide_key(i), i * 10))) }}
            for i in 0..6 {{ let k = collide_key(i)
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
            check(is_none(map.insert(collide_key(7), 70)) && same(map.get(&k7), 70))
            println("ok")
        }}

        fn case_maps_views_and_entries() {{
            let map = HashMap[int, int]::new()
            {map_operations}
            let map = BTreeMap[int, int]::new()
            {map_operations}
        }}

        fn case_avl_balancing() {{
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
        }}

        fn case_sets() {{
            let set = HashSet[int]::new()
            {set_operations}
            let set = BTreeSet[int]::new()
            {set_operations}
            let expected = 1
            for key in set {{ check(key == expected)
                expected = expected + 2 }}
            check(expected == 7)
            println("ok")
        }}

        fn case_map_alias_write_in_view_panic() {{
            let map = HashMap[int, int]::new()
            let one = 1
            _ = map.insert(1, 10)
            let alias = map
            _ = map.with_ref(&one, fn(v: &int) int {{ _ = alias.insert(2, 20)
                *v }})
        }}

        fn case_ordered_entry_write_in_view_panic() {{
            let map = BTreeMap[int, int]::new()
            _ = map.insert(1, 10)
            let entry = map.entry(2)
            map.for_each_ref(fn(k: &int, v: &int) {{ _ = entry.or_insert(20) }})
        }}

        fn main() {{
            case_hash_map_string_keys()
            case_hash_trie_snapshots()
            case_hash_collisions()
            case_maps_views_and_entries()
            case_avl_balancing()
            case_sets()
            case_map_alias_write_in_view_panic()
            case_ordered_entry_write_in_view_panic()
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

    let mut harness = Harness::new(&compiled);
    harness.machine_hook = Some(capture);
    let mut memory = vec![0; 1048576];
    for entry in [
        "case_hash_map_string_keys",
        "case_hash_trie_snapshots",
        "case_hash_collisions",
        "case_avl_balancing",
        "case_sets",
    ] {
        assert_eq!(
            run_printed_with(&mut harness, &mut memory, entry),
            "ok\n",
            "{entry}"
        );
    }
    assert_eq!(
        run_printed_with(&mut harness, &mut memory, "case_maps_views_and_entries"),
        "ok\nok\n"
    );

    for entry in [
        "case_map_alias_write_in_view_panic",
        "case_ordered_entry_write_in_view_panic",
    ] {
        let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_printed_with(&mut harness, &mut memory, entry);
        }))
        .expect_err(&format!("{entry} 必须触发 scoped view 写入 panic"));
        let message = error
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| error.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("Runtime(Panic): map write during scoped view"),
            "{entry}: {message}"
        );
    }
}

#[test]
fn collections_e2e_fixture_matches_stdout() {
    assert_eq!(
        printed(include_str!("../../../../ci/e2e/fixtures/collections.gg")),
        include_str!("../../../../ci/e2e/fixtures/collections.stdout")
    );
}
