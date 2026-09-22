use super::collections::*;
use super::hash::HashFamily;
use crate::{CompileRequest, Compiler, TargetName};

fn compile(source: &str) -> crate::Compilation {
    Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ))
}

#[test]
fn collection_views_and_handle_copies_lower_in_generic_gir() {
    let compilation = compile("fn main() {}");
    assert!(compilation.is_success());
    let dump = compilation.dump_gir().expect("GIR");
    let body = |owner: &str| {
        dump.split("body owner=")
            .find(|body| body.starts_with(&format!("{owner} ")))
            .unwrap_or_else(|| panic!("缺少 {owner}"))
            .to_owned()
    };
    for owner in ["peek", "walk"] {
        let body = body(owner);
        assert!(
            body.contains("ScopedViewBegin ScopedRead")
                && body.matches("ScopedViewEnd").count() == 2,
            "{owner} 必须在接收者的 ScopedRead view 内调用，且正常与展开路径都闭合：{body}"
        );
    }
    let share = body("share");
    assert!(
        !share.contains("cow_snapshot") && share.contains("= copy _1"),
        "集合复制只复制共享身份句柄：{share}"
    );
}

#[test]
fn std_collections_are_importable_and_require_stable_keys() {
    let stable = compile(
        "use std.collections.{HashMap, BTreeMap}\nfn probe(map: HashMap[int, string], tree: BTreeMap[string, int]) Option[int] {\n let key = 1\n tree.for_each_ref(fn(k: &string, v: &int) {})\n map.with_ref(&key, fn(v: &string) int = 0) }\nfn main() {}",
    );
    assert!(stable.is_success(), "{:?}", stable.diagnostics().items());
    let unstable = "use std.collections.{HashMap}\nstruct Foo { id: int }\nimpl Eq for Foo { fn eq(self: &Self, other: &Self) bool = true }\nimpl Hash for Foo { fn hash(self: &Self, hasher: &Hasher) {} }\nfn probe(map: HashMap[Foo, string]) Option[string] {\n let key = Foo { id: 1 }\n map.get(&key) }\nfn main() {}";
    assert!(
        !compile(unstable).is_success(),
        "缺少 StableHash 的键不能进入 HashMap"
    );
    let promised = unstable.replace("fn probe", "unsafe impl StableHash for Foo {}\nfn probe");
    assert!(compile(&promised).is_success());
    assert!(
        !compile("struct HashMap {}\nfn main() {}").is_success(),
        "用户不能声明预导入名"
    );
}

fn family(seed: u8) -> HashFamily {
    HashFamily::default_from_entropy([seed; 8])
}

fn filled(map: &Map<i64, String>, count: i64) {
    for key in 0..count {
        map.insert(key, format!("v{key}")).expect("无 view 时可写");
    }
}

#[test]
fn maps_are_shared_identity_handles_with_semantic_copies() {
    let map: Map<i64, String> = Map::hashed(family(1));
    let alias = map.clone();
    assert!(map.same_identity(&alias));
    assert_eq!(alias.insert(1, "one".to_owned()), Ok(None));
    assert_eq!(map.get(&1), Some("one".to_owned()));
    assert_eq!(map.insert(1, "uno".to_owned()), Ok(Some("one".to_owned())));
    assert_eq!(alias.len(), 1);
    assert_eq!(map.update(&1, |value| value + "!"), Ok(true));
    assert_eq!(map.update(&2, |value| value), Ok(false));
    assert_eq!(alias.get(&1), Some("uno!".to_owned()));
    assert_eq!(map.remove(&1), Ok(Some("uno!".to_owned())));
    assert_eq!(map.remove(&1), Ok(None));
}

#[test]
fn entry_only_passes_semantic_copies() {
    let map: Map<String, i64> = Map::ordered();
    assert_eq!(map.entry("a".to_owned()).or_insert(1), Ok(1));
    assert_eq!(map.entry("a".to_owned()).or_insert(9), Ok(1));
    let modified = map
        .entry("a".to_owned())
        .and_modify(|value| value + 10)
        .expect("无 view")
        .or_insert_with(|| unreachable!("键已存在"));
    assert_eq!(modified, Ok(11));
    assert_eq!(map.get(&"a".to_owned()), Some(11));
    let mut called = false;
    let fresh = map.entry("b".to_owned()).or_insert_with(|| {
        called = true;
        2
    });
    assert_eq!(fresh, Ok(2));
    assert!(called);
}

#[test]
fn scoped_views_reject_writes_until_the_callback_returns() {
    let map: Map<i64, String> = Map::hashed(family(1));
    filled(&map, 3);
    let alias = map.clone();
    let seen = map.with_ref(&1, |value| {
        assert_eq!(
            alias.insert(9, "x".to_owned()),
            Err(CollectionFault::WriteDuringView)
        );
        assert_eq!(alias.remove(&0), Err(CollectionFault::WriteDuringView));
        assert_eq!(
            alias.update(&1, |v| v),
            Err(CollectionFault::WriteDuringView)
        );
        value.len()
    });
    assert_eq!(seen, Some(2));
    assert_eq!(
        map.with_ref(&7, |_| unreachable!("缺失键不调用 callback")),
        None
    );
    let mut visited = 0;
    map.for_each_ref(|_, _| {
        visited += 1;
        assert_eq!(
            alias.insert(9, "x".to_owned()),
            Err(CollectionFault::WriteDuringView)
        );
    });
    assert_eq!(visited, 3);
    // view 结束后写入恢复。
    assert_eq!(map.insert(9, "x".to_owned()), Ok(None));
}

#[test]
fn iteration_snapshots_seal_backing_and_survive_later_writes() {
    let map: Map<i64, String> = Map::ordered();
    filled(&map, 3);
    assert!(!map.is_sealed());
    let mut iter = map.iter();
    assert!(map.is_sealed());
    assert_eq!(iter.next(), Some((0, "v0".to_owned())));
    let alias = map.clone();
    assert_eq!(
        alias.insert(1, "changed".to_owned()),
        Ok(Some("v1".to_owned()))
    );
    assert_eq!(alias.remove(&2), Ok(Some("v2".to_owned())));
    assert!(!map.is_sealed(), "第一次修改已分离 backing");
    assert_eq!(iter.next(), Some((1, "v1".to_owned())));
    assert_eq!(iter.next(), Some((2, "v2".to_owned())));
    assert_eq!(iter.next(), None);
    assert_eq!(
        map.iter().collect::<Vec<_>>(),
        vec![(0, "v0".to_owned()), (1, "changed".to_owned())]
    );
}

#[test]
fn ordered_maps_iterate_in_key_order_but_hashed_maps_follow_the_family() {
    let ordered: Map<i64, String> = Map::ordered();
    let first: Map<i64, String> = Map::hashed(family(1));
    let second: Map<i64, String> = Map::hashed(family(2));
    let secure: Map<i64, String> = Map::hashed(HashFamily::secure_from_entropy([5; 16]));
    for map in [&ordered, &first, &second, &secure] {
        for key in [5, 1, 9, 3, 7, 2, 8, 4, 6, 0] {
            map.insert(key, key.to_string()).expect("无 view");
        }
    }
    let keys = |map: &Map<i64, String>| map.iter().map(|(key, _)| key).collect::<Vec<_>>();
    assert_eq!(keys(&ordered), (0..10).collect::<Vec<_>>());
    assert_eq!(keys(&first), keys(&first), "同一表的两次迭代顺序一致");
    assert_ne!(keys(&first), keys(&second), "不同 seed 的实现序不同");
    assert_ne!(keys(&first), keys(&ordered), "实现序不是键序");
    let mut visited = Vec::new();
    secure.for_each_ref(|key, _| visited.push(*key));
    assert_eq!(
        visited,
        keys(&secure),
        "for_each_ref 与 iter 使用同一实现序"
    );
}

#[test]
fn small_maps_stay_inline_until_overflow() {
    let small: Map<i64, String> = Map::small(2, family(1));
    filled(&small, 2);
    assert!(small.is_inline());
    assert_eq!(
        small.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        vec![0, 1]
    );
    small.insert(2, "v2".to_owned()).expect("无 view");
    assert!(!small.is_inline());
    let hashed: Map<i64, String> = Map::hashed(family(1));
    filled(&hashed, 3);
    assert_eq!(
        small.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        hashed.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        "溢出后与同族 HashMap 的实现序相同"
    );
    assert_eq!(small.get(&1), Some("v1".to_owned()));
}

#[test]
fn sets_share_key_constraints_and_snapshot_semantics() {
    let set: Set<String> = Set::ordered();
    assert_eq!(set.insert("b".to_owned()), Ok(true));
    assert_eq!(set.insert("a".to_owned()), Ok(true));
    assert_eq!(set.insert("a".to_owned()), Ok(false));
    assert!(set.contains(&"a".to_owned()));
    assert_eq!(set.len(), 2);
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        vec!["a".to_owned(), "b".to_owned()]
    );
    let alias = set.clone();
    let mut seen = Vec::new();
    set.for_each_ref(|value| {
        seen.push(value.clone());
        assert_eq!(
            alias.insert("c".to_owned()),
            Err(CollectionFault::WriteDuringView)
        );
    });
    assert_eq!(seen, vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(set.remove(&"a".to_owned()), Ok(true));
    let hashed: Set<Vec<u8>> = Set::hashed(family(3));
    assert_eq!(hashed.insert(vec![1, 2]), Ok(true));
    assert!(hashed.contains(&vec![1, 2]));
}
