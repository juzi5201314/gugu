use super::{Value, universe::Shape};
use crate::frontend::{self, FrontendOutput};

fn compile(
    source: &str,
    queries: &crate::QueryEngine,
) -> Result<FrontendOutput, Vec<crate::Diagnostic>> {
    let mut sources = crate::SourceMap::new(vec![
        crate::SourceSnapshot::from_str("main.gg", source).unwrap(),
    ])
    .unwrap();
    let cfg = frontend::cfg::CfgContext::target_only(crate::TargetName::X86_64Linux);
    frontend::bootstrap(
        frontend::SourceInput::Sources {
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/late@1",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        queries,
    )
}

fn constant<'a>(output: &'a FrontendOutput, name: &str) -> &'a Value {
    let instance = output
        .mono
        .instances
        .iter()
        .find(|i| i.symbol == name || i.symbol.ends_with(&format!("::{name}")))
        .unwrap_or_else(|| {
            panic!(
                "常量 {name} 缺少实例：{:?}",
                output
                    .mono
                    .instances
                    .iter()
                    .map(|i| &i.symbol)
                    .collect::<Vec<_>>()
            )
        });
    let digest = frontend::mono::digest_of(&instance.mono_key);
    let definition = output
        .hir
        .module()
        .owners
        .iter()
        .find(|o| {
            output.hir.module().definitions[o.definition.index()].key == instance.mono_key[..32]
        })
        .unwrap();
    &output
        .mono
        .late
        .results
        .iter()
        .find(|result| result.key.instance == digest && result.key.expression == definition.body.0)
        .unwrap()
        .value
}

#[test]
fn late_count_transitive_calls_and_cache() {
    let source = "const N: int = type_id_count()\nfn add(x: int) int { if N > 0 { x + N } else { 0 } }\nconst ANSWER: int = add(3)\nfn main() { _ = ANSWER }";
    let queries = crate::QueryEngine::new();
    let output = compile(source, &queries).unwrap();
    assert_eq!(
        constant(&output, "ANSWER"),
        &Value::Integer(output.mono.universe.records.len() as u128 + 3)
    );
    let warm = compile(source, &queries).unwrap();
    assert_eq!(output.mono, warm.mono);
}

#[test]
fn type_ids_aliases_hidden_types_and_exclusions() {
    let output = compile("use std.mem.{MaybeUninit}\ntype Alias = int\nstruct Wrap(int)\nconst SAME: bool = comptime { type_id[int]() == type_id[Alias]() }\nconst DISTINCT: bool = type_id[int]().as_int() != type_id[Wrap]().as_int()\nfn main() { let raw: MaybeUninit[int] = MaybeUninit::uninit()\n _ = raw\n _ = DISTINCT }", &crate::QueryEngine::new()).unwrap();
    let universe = &output.mono.universe;
    assert_eq!(
        universe.records.iter().filter(|r| r.name == "int").count(),
        1
    );
    assert!(
        !universe
            .records
            .iter()
            .any(|r| r.name == "!" || r.name.starts_with("MaybeUninit"))
    );
    assert_eq!(constant(&output, "DISTINCT"), &Value::Bool(true));
    for (id, record) in universe.records.iter().enumerate() {
        assert_eq!(universe.type_id(&record.key), Some(id as u32));
    }
}

#[test]
fn late_values_cannot_feed_early_shapes_or_macros() {
    for source in [
        "const N: int = type_id_count()\nfn main() { let a: [int; N]\n _ = 0 }",
        "fn n() int { type_id_count() }\nfn main() { _ = [0; n()] }",
        "fn f(comptime n: int) {}\nfn main() { f(type_id_count()) }",
        "comptime source { if type_id_count() > 0 { std.syntax.parse_source(\"fn main() {}\") } else { std.syntax.parse_source(\"fn main() {}\") } }",
    ] {
        let errors = compile(source, &crate::QueryEngine::new()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.code() == crate::DiagnosticCode::LateComptime),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn late_aggregates_have_fixed_shape_and_forbidden_leaves_fail() {
    let output = compile("const VALUES: (int, [int; 2]) = (type_id_count(), [type_id_count(); 2])\nfn main() { _ = VALUES }", &crate::QueryEngine::new()).unwrap();
    let n = Value::Integer(output.mono.universe.records.len() as u128);
    assert_eq!(
        constant(&output, "VALUES"),
        &Value::Aggregate(vec![n.clone(), Value::Aggregate(vec![n.clone(), n])])
    );
    let errors = compile("const TEXT: string = if type_id_count() > 0 { \"yes\" } else { \"no\" }\nfn main() { _ = TEXT }", &crate::QueryEngine::new()).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.code() == crate::DiagnosticCode::LateComptime)
    );
}

#[test]
fn metadata_fields_and_vtable_references_are_closed() {
    let source = "struct Inner { value: byte }\nstruct Outer { inner: Inner }\ntrait Get { fn get(self) int }\nimpl Get for Outer { fn get(self) int { 1 } }\nfn main() { let x: dyn Get = Outer { inner: Inner { value: 2 } }\n _ = x }";
    let output = compile(source, &crate::QueryEngine::new()).unwrap();
    let universe = &output.mono.universe;
    assert!(universe.records.iter().any(|r| r.name == "byte"));
    let outer = universe
        .records
        .iter()
        .find(|r| r.name.ends_with("Outer"))
        .unwrap();
    assert!(matches!(&outer.shape, Shape::Struct(fields) if fields.len() == 1));
    assert!(
        universe
            .vtables
            .iter()
            .any(|v| universe.records[v.concrete as usize].key == outer.key)
    );
    let mut corrupted = universe.clone();
    corrupted.records[0].children.push([0; 32]);
    corrupted.fingerprint = corrupted.fingerprint();
    assert!(corrupted.verify().is_err());
}

#[test]
fn late_execution_failure_keeps_diagnostics() {
    let errors = compile("const N: int = comptime { if type_id_count() > 0 { panic(\"late\") }\n 1 }\nfn main() { _ = N }", &crate::QueryEngine::new()).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.code() == crate::DiagnosticCode::ComptimePanic && e.span().is_some())
    );
}

#[test]
fn generic_late_results_use_each_concrete_instance() {
    let output = compile("fn number[T](value: T) int { comptime { type_id[T]().as_int() } }\nconst A: int = number(1)\nconst B: int = number(true)\nfn main() { _ = (A, B) }", &crate::QueryEngine::new()).unwrap();
    assert_ne!(constant(&output, "A"), constant(&output, "B"));
    let records = &output.mono.universe.records;
    let int_id = records.iter().position(|r| r.name == "int").unwrap();
    let bool_id = records.iter().position(|r| r.name == "bool").unwrap();
    assert_eq!(constant(&output, "A"), &Value::Integer(int_id as u128));
    assert_eq!(constant(&output, "B"), &Value::Integer(bool_id as u128));
}

#[test]
fn opaque_aliases_share_hidden_type_identity() {
    let output = compile("type First = impl Clone\ntype Second = impl Clone\nfn first() First = 1\nfn second() Second = 2\nconst SAME: bool = type_id[First]().as_int() == type_id[Second]().as_int()\nfn main() { _ = (first(), second(), SAME) }", &crate::QueryEngine::new()).unwrap();
    assert_eq!(constant(&output, "SAME"), &Value::Bool(true));
    assert!(
        !output
            .mono
            .universe
            .records
            .iter()
            .any(|record| record.name == "impl Trait")
    );
}

#[test]
fn early_symbolic_names_and_equality_remain_available_to_macros() {
    let output = compile("const NAME: string = type_id[byte]().name()\nconst SAME: bool = type_id[int]() == type_id[i64]()\ncomptime source { if SAME && NAME == \"byte\" { std.syntax.parse_source(\"fn main() {}\") } else { panic(\"wrong type identity\") } }", &crate::QueryEngine::new()).unwrap();
    assert!(output.hir.module().entry.is_some());
}

#[test]
fn late_closure_includes_types_from_both_branches() {
    let source = "struct Left {}\nstruct Right {}\nfn choose() TypeId { if type_id_count() > 0 { type_id[Left]() } else { type_id[Right]() } }\nconst CHOICE: TypeId = choose()\nfn main() { _ = CHOICE }";
    let output = compile(source, &crate::QueryEngine::new()).unwrap();
    let records = &output.mono.universe.records;
    assert!(records.iter().any(|r| r.name.ends_with("Right")));
    let left = records.iter().find(|r| r.name.ends_with("Left")).unwrap();
    assert_eq!(constant(&output, "CHOICE"), &Value::Type(left.key));
}

#[test]
fn late_integer_widths_and_local_aggregate_updates() {
    let source = "const HIGH: u128 = if type_id_count() > 0 { 340282366920938463463374607431768211455 } else { 0 }\nconst NEG: i8 = if type_id_count() > 0 { -7 } else { 0 }\nconst ARRAY: [int; 2] = comptime { let a = [0; 2]\n for i in 0..2 { a[i] = type_id_count() + i }\n a }\nfn main() { _ = (HIGH, NEG, ARRAY) }";
    let output = compile(source, &crate::QueryEngine::new()).unwrap();
    assert_eq!(constant(&output, "HIGH"), &Value::Integer(u128::MAX));
    assert_eq!(constant(&output, "NEG"), &Value::Integer(249));
    let n = output.mono.universe.records.len() as u128;
    assert_eq!(
        constant(&output, "ARRAY"),
        &Value::Aggregate(vec![Value::Integer(n), Value::Integer(n + 1)])
    );
}

#[test]
fn unreachable_late_foreign_call_is_rejected_before_execution() {
    let source = "extern \"C\" fn external() int\nconst N: int = comptime { if type_id_count() > 0 { 1 } else { unsafe { external() } } }\nfn main() { _ = N }";
    let errors = compile(source, &crate::QueryEngine::new()).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.code() == crate::DiagnosticCode::LateComptime)
    );
}
