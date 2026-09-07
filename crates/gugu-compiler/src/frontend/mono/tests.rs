//! 阶段 24 单测：实例闭合、根、稳定性、预算与公共摘要接入。

use crate::frontend::FrontendOutput;
use crate::frontend::mono::{MonoWorldV1, digest_of};

fn compile(sources: &[(&str, &str)], queries: &crate::QueryEngine) -> FrontendOutput {
    let mut map = crate::SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| crate::SourceSnapshot::from_str(path, source).expect("快照"))
            .collect(),
    )
    .expect("源映射");
    let cfg = crate::frontend::cfg::CfgContext::new(
        crate::TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut map,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/mono@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        queries,
    )
    .expect("前端检查通过")
}

fn compile_failing(sources: &[(&str, &str)]) -> Vec<crate::Diagnostic> {
    let mut map = crate::SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| crate::SourceSnapshot::from_str(path, source).expect("快照"))
            .collect(),
    )
    .expect("源映射");
    let cfg = crate::frontend::cfg::CfgContext::new(
        crate::TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut map,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/mono@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        &crate::QueryEngine::new(),
    )
    .expect_err("前端必须失败")
}

/// 按符号名收集闭合实例的 callee 数。
fn instance_map(world: &MonoWorldV1) -> std::collections::BTreeMap<&str, Vec<&str>> {
    let mut map = std::collections::BTreeMap::new();
    for instance in &world.instances {
        let callees: Vec<&str> = world
            .instances
            .iter()
            .filter(|candidate| instance.callees.contains(&digest_of(&candidate.mono_key)))
            .map(|candidate| candidate.symbol.as_str())
            .collect();
        map.insert(instance.symbol.as_str(), callees);
    }
    map
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn symbols(world: &MonoWorldV1) -> Vec<&str> {
    world
        .instances
        .iter()
        .map(|instance| instance.symbol.as_str())
        .collect()
}

#[test]
fn call_chain_closes_and_dead_code_is_excluded() {
    let source = "fn leaf() {}\nfn mid() { leaf() }\nfn dead() {}\nfn main() { mid() }";
    let queries = crate::QueryEngine::new();
    let output = compile(&[("main.gg", source)], &queries);
    let map = instance_map(&output.mono);
    assert_eq!(map["main"], vec!["mid"]);
    assert_eq!(map["mid"], vec!["leaf"]);
    assert!(!map.contains_key("dead"), "不可达函数不得进入实例图");
    // 冷/热 query 一致：同一 engine 二次编译得到相同实例图。
    let warm = compile(&[("main.gg", source)], &queries);
    assert_eq!(output.mono, warm.mono, "冷热闭合必须一致");
}

#[test]
fn cfg_pruned_items_never_enter_instance_graph() {
    let source = "#[cfg(os = \"windows\")] fn windows_only() {}\nfn main() { }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let symbols = symbols(&output.mono);
    assert!(!symbols.contains(&"windows_only"), "cfg 裁掉的定义不可达");
}

#[test]
fn generic_id_instantiates_per_type_argument() {
    let source = "fn id[T](value: T) T = value\nfn main() { _ = id(1)\n _ = id(\"a\") }";
    let queries = crate::QueryEngine::new();
    let output = compile(&[("main.gg", source)], &queries);
    // 同一符号两个实例：按 key 排序，实参不同即不同 MonoKey。
    let ids: Vec<_> = output
        .mono
        .instances
        .iter()
        .filter(|instance| instance.symbol == "id")
        .map(|instance| instance.mono_key.clone())
        .collect();
    assert_eq!(ids.len(), 2, "int/string 实参各成一个实例");
    assert_ne!(ids[0], ids[1]);
    // 实例按 key 摘要排序：instances 摘要序列单调不减。
    let digests: Vec<_> = output
        .mono
        .instances
        .iter()
        .map(|instance| digest_of(&instance.mono_key))
        .collect();
    let mut sorted = digests.clone();
    sorted.sort();
    assert_eq!(digests, sorted, "实例必须按 key 摘要排序");
}

#[test]
fn self_recursive_generic_converges_with_same_key() {
    let source = "fn r[T](value: T) T = r(value)\nfn main() { _ = r(1) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let instances: Vec<_> = output
        .mono
        .instances
        .iter()
        .filter(|instance| instance.symbol == "r")
        .collect();
    assert_eq!(instances.len(), 1, "同 key 递归只形成一个实例（图环）");
    assert!(
        instances[0]
            .callees
            .contains(&digest_of(&instances[0].mono_key))
    );
}

#[test]
fn strictly_growing_generic_chain_reports_mono_divergence() {
    // wrap[T] 以 [T; 1] 递归调用自身：类型结构严格增长，命中 E0052。
    let source = "fn wrap[T](value: T) { wrap([value]) }\nfn main() { wrap(1) }";
    let errors = compile_failing(&[("main.gg", source)]);
    assert!(
        errors
            .iter()
            .any(|error| { error.code() == crate::DiagnosticCode::MonoDivergence }),
        "严格增长链必须报 E0052：{errors:?}"
    );
}

#[test]
fn used_and_export_functions_are_roots_without_callers() {
    let source = "#[used]\nfn keep() {}\n#[export_name = \"gugu_custom\"]\npub extern \"C\" fn shipped() {}\nfn main() {}";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let symbols = symbols(&output.mono);
    assert!(symbols.contains(&"keep"), "#[used] 保持可达");
    assert!(symbols.contains(&"shipped"), "导出保持可达");
}

#[test]
fn static_initializers_are_root_instances() {
    let source = "static GIFT: int = compute()\nfn compute() int = 7\nfn main() { _ = GIFT }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let map = instance_map(&output.mono);
    assert!(
        map.contains_key("GIFT"),
        "static 初始化器必须成为根实例：{:?}",
        map.keys().collect::<Vec<_>>()
    );
}

#[test]
fn harness_test_items_become_roots() {
    let source = "#[test]\nfn case_one() {}\nfn main() {}";
    let mut map = crate::SourceMap::new(vec![
        crate::SourceSnapshot::from_str("main.gg", source).expect("快照"),
    ])
    .expect("源映射");
    let cfg = crate::frontend::cfg::CfgContext::new(
        crate::TargetName::X86_64Linux,
        [],
        [],
        true,
        false,
        Default::default(),
    );
    let output = crate::frontend::bootstrap(
        crate::frontend::SourceInput::Sources {
            source_map: &mut map,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/mono@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        &crate::QueryEngine::new(),
    )
    .expect("前端检查通过");
    let symbols = symbols(&output.mono);
    assert!(symbols.contains(&"case_one"), "harness 下 #[test] 是根");
}

#[test]
fn dyn_erasure_adds_vtable_roots_and_reaches_impl_methods() {
    let source = "trait Shape { fn area(self) int }\nstruct Square { side: int }\nimpl Shape for Square { fn area(self) int = self.side }\nfn measure(shape: dyn Shape) int = shape.area()\nfn main() { let s = Square { side: 2 }\n _ = measure(s) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let world = &output.mono;
    // Square 的擦除点物化 vtable 根；impl 方法实例可达。
    let symbols = symbols(world);
    assert!(symbols.contains(&"area"), "impl 方法经 vtable 根可达");
    assert!(
        !world.metadata_roots.is_empty(),
        "擦除点必须登记 dyn metadata 根"
    );
}

#[test]
fn type_id_queries_register_metadata_roots() {
    let source = "fn main() { let id: TypeId = type_id[int]()\n _ = id.name() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert!(
        !output.mono.metadata_roots.is_empty(),
        "type_id[int] 必须进入 metadata 根"
    );
}

#[test]
fn trait_operator_dispatch_reaches_method_instance() {
    let source = "struct Point { x: int }\nimpl Add[Point] for Point { type Output = Point\n fn add(self, rhs: Point) Point = Point { x: self.x + rhs.x } }\nfn main() { let a = Point { x: 1 }\n _ = a + a }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let symbols = symbols(&output.mono);
    assert!(
        symbols.contains(&"add"),
        "运算符 trait 派发必须可达 impl 方法"
    );
}

#[test]
fn multi_module_input_order_does_not_change_graph() {
    let sources: &[(&str, &str)] = &[
        ("main.gg", "use helper.{side}\nfn main() { _ = side() }"),
        ("helper.gg", "pub fn side() int = 1"),
    ];
    let first = compile(sources, &crate::QueryEngine::new());
    let reversed: Vec<(&str, &str)> = sources.iter().rev().copied().collect();
    let second = compile(&reversed, &crate::QueryEngine::new());
    assert_eq!(
        first.mono.graph_fingerprint, second.mono.graph_fingerprint,
        "源文件顺序不得改变实例图"
    );
}

#[test]
fn public_summaries_cover_public_functions_only() {
    let source =
        "pub fn api() int = 1\nfn internal() int = 2\nfn main() { _ = api()\n _ = internal() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let world = &output.mono;
    let public_symbols: Vec<_> = world
        .instances
        .iter()
        .filter(|instance| instance.public && instance.symbol == "api")
        .collect();
    let internal_public: Vec<_> = world
        .instances
        .iter()
        .filter(|instance| instance.public && instance.symbol == "internal")
        .collect();
    assert_eq!(public_symbols.len(), 1);
    assert!(internal_public.is_empty(), "私有函数不得产生公共摘要对象");
    let api_keys: Vec<String> = public_symbols
        .iter()
        .map(|instance| hex(&digest_of(&instance.mono_key)))
        .collect();
    let summary_keys: Vec<String> = world.public_summaries.keys().cloned().collect();
    assert!(!summary_keys.is_empty(), "公共函数必须有摘要对象");
    let matched = api_keys
        .iter()
        .filter(|key| summary_keys.contains(key))
        .count();
    assert_eq!(matched, 1, "api 恰有一个公共摘要：{summary_keys:?}");
}

#[test]
fn public_summaries_change_action_key() {
    use crate::project::ActionInputs;
    let mut first = ActionInputs::new(b"c", "host", "host", "bin");
    first.add_public_summary("abc", [1u8; 32]);
    let mut second = ActionInputs::new(b"c", "host", "host", "bin");
    second.add_public_summary("abc", [2u8; 32]);
    assert_ne!(first.key(), second.key());
    let empty = ActionInputs::new(b"c", "host", "host", "bin");
    assert_ne!(first.key(), empty.key());
}

#[test]
fn analysis_world_records_instances_by_mono_key() {
    let source = "fn leaf() int = 1\nfn main() { _ = leaf() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert_eq!(
        output.analysis.instances.len(),
        output.mono.instances.len(),
        "每个闭合实例必须有一个摘要投影"
    );
    for instance in &output.mono.instances {
        assert!(
            output
                .analysis
                .instances
                .iter()
                .any(|record| record.mono_key == instance.mono_key),
            "world 缺少实例 {} 的摘要",
            instance.symbol
        );
    }
}

#[test]
fn image_plan_reports_mono_counts() {
    let compilation = crate::Compiler::new().compile(crate::CompileRequest::single_file(
        "main.gg",
        "fn helper() {}\nfn main() { helper() }",
        crate::TargetName::X86_64Linux,
    ));
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.mono_instance_count(), 2, "main + helper 两个实例");
    assert_eq!(plan.mono_root_count(), 1, "入口是唯一根");
    assert_ne!(plan.mono_graph_fingerprint(), [0u8; 32]);
}

#[test]
fn empty_package_has_empty_mono_world() {
    let compilation = crate::Compiler::new().compile(crate::CompileRequest::empty_package(
        crate::TargetName::X86_64Linux,
    ));
    assert!(compilation.is_success());
    assert!(compilation.image_plan().is_none());
}

#[test]
fn function_values_retain_their_callable_instances() {
    let source = "fn leaf() {}\nfn main() { let f: fn() = leaf\n f() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert_eq!(instance_map(&output.mono)["main"], vec!["leaf"]);
}

#[test]
fn nested_closure_owns_its_call_edges() {
    let source = "fn leaf() {}\nfn main() { let f = fn() { leaf() }\n f() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let closure = output
        .mono
        .instances
        .iter()
        .find(|instance| instance.kind == super::keys::MonoKind::Closure)
        .expect("闭包实例");
    let leaf = output
        .mono
        .instances
        .iter()
        .find(|instance| instance.symbol == "leaf")
        .unwrap();
    assert_eq!(closure.callees, vec![digest_of(&leaf.mono_key)]);
}

#[test]
fn async_and_local_static_bodies_are_reachable_instances() {
    let source =
        "fn init() int = 7\nfn main() { let job = async { static S: int = init()\n S }\n _ = job }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let task = output
        .mono
        .instances
        .iter()
        .find(|instance| instance.kind == super::keys::MonoKind::Async)
        .expect("协程 body 必须进入实例图");
    let initializer = output
        .mono
        .instances
        .iter()
        .find(|instance| instance.kind == super::keys::MonoKind::StaticInit)
        .expect("局部 static 初始化器必须进入实例图");
    let init = output
        .mono
        .instances
        .iter()
        .find(|instance| instance.symbol == "init")
        .unwrap();
    assert!(task.callees.contains(&digest_of(&initializer.mono_key)));
    assert_eq!(initializer.callees, vec![digest_of(&init.mono_key)]);
}

#[test]
fn one_interface_retains_vtables_for_each_concrete_type() {
    let source = "trait Shape { fn area(self) int }\nstruct A {}\nstruct B {}\nimpl Shape for A { fn area(self) int = 1 }\nimpl Shape for B { fn area(self) int = 2 }\nfn measure(shape: dyn Shape) int = shape.area()\nfn main() { _ = measure(A {})\n _ = measure(B {}) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let main = output
        .mono
        .instances
        .iter()
        .find(|instance| instance.symbol == "main")
        .unwrap();
    assert_eq!(main.vtable_roots.len(), 2);
    assert_ne!(
        main.vtable_roots[0].self_type,
        main.vtable_roots[1].self_type
    );
}

#[test]
fn generic_trait_methods_bind_method_arguments_after_impl_selection() {
    let source = "trait Identity { fn identity[T](self, value: T) T }\nstruct Point {}\nimpl Identity for Point { fn identity[U](self, value: U) U = value }\nfn call[X: Identity](x: X) int = x.identity::[int](1)\nfn main() { _ = call(Point {}) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert_eq!(instance_map(&output.mono)["call"], vec!["identity"]);
}

#[test]
fn concrete_instances_reselect_specialized_impls() {
    let source = "trait Value { fn value(self) int }\nstruct Box[T] { value: T }\nfn generic() int = 1\nfn specialized() int = 2\nimpl Value for Box[T] { fn value(self) int = generic() }\nimpl Value for Box[int] { fn value(self) int = specialized() }\nfn read[T](value: Box[T]) int = value.value()\nfn main() { _ = read(Box { value: 1 }) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let reachable = symbols(&output.mono);
    assert!(reachable.contains(&"specialized"));
    assert!(!reachable.contains(&"generic"));
}

#[test]
fn comptime_value_arguments_distinguish_instances() {
    let source = "fn repeat(comptime n: int) int = n * 2\nfn main() { _ = repeat(1)\n _ = repeat(2)\n _ = repeat(1) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert_eq!(
        output
            .mono
            .instances
            .iter()
            .filter(|instance| instance.symbol == "repeat")
            .count(),
        2
    );
}

#[test]
fn late_reflection_in_generic_body_uses_only_concrete_roots() {
    let source =
        "fn late[T](value: T) { _ = type_id[T]()\n _ = type_id_count() }\nfn main() { late(1) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert_eq!(
        output
            .mono
            .instances
            .iter()
            .filter(|instance| instance.symbol == "late")
            .count(),
        1
    );
}

#[test]
fn changed_source_with_shared_queries_matches_a_fresh_world() {
    let queries = crate::QueryEngine::new();
    let before = "fn a() {}\nfn b() {}\nfn main() { a() }";
    let after = "fn a() {}\nfn b() {}\nfn main() { b() }";
    let _ = compile(&[("main.gg", before)], &queries);
    let warm = compile(&[("main.gg", after)], &queries);
    let cold = compile(&[("main.gg", after)], &crate::QueryEngine::new());
    assert_eq!(instance_map(&warm.mono)["main"], vec!["b"]);
    assert_eq!(warm.mono, cold.mono);
}

#[test]
fn public_summary_changes_with_effects_but_not_private_definition_ids() {
    let source = "pub fn api() int = 1\nfn main() { _ = api() }";
    let changed = "fn unrelated() {}\npub fn api() int = 1\nfn main() { _ = api() }";
    let first = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let second = compile(&[("main.gg", changed)], &crate::QueryEngine::new());
    assert_eq!(first.mono.public_summaries, second.mono.public_summaries);
    let queries = crate::QueryEngine::new();
    let _ = compile(&[("main.gg", source)], &queries);
    let panicking = "pub fn api() int = panic(\"stop\")\nfn main() { _ = api() }";
    let warm = compile(&[("main.gg", panicking)], &queries);
    let cold = compile(&[("main.gg", panicking)], &crate::QueryEngine::new());
    assert_ne!(first.mono.public_summaries, cold.mono.public_summaries);
    assert_eq!(warm.mono.public_summaries, cold.mono.public_summaries);
}

#[test]
fn fragment_fingerprints_follow_body_and_instance_not_arena_ids() {
    let source = "pub fn api() int = 1\nfn main() { _ = api() }";
    let changed = "pub fn api() int = 2\nfn main() { _ = api() }";
    let first = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let second = compile(&[("main.gg", changed)], &crate::QueryEngine::new());
    let fingerprint = |output: &FrontendOutput| {
        output
            .mono
            .instances
            .iter()
            .find(|instance| instance.symbol == "api")
            .unwrap()
            .fragment_input_fingerprint
    };
    assert_ne!(fingerprint(&first), fingerprint(&second));
    assert_eq!(
        first.mono.public_summaries, second.mono.public_summaries,
        "实现常量改变但接口效果未变，公共摘要内容身份应保持不变"
    );
}

fn summary<'a>(
    output: &'a FrontendOutput,
    symbol: &str,
) -> &'a crate::frontend::analysis::FunctionSummary {
    let key = &output
        .mono
        .instances
        .iter()
        .find(|instance| instance.symbol == symbol)
        .unwrap()
        .mono_key;
    &output
        .analysis
        .instances
        .iter()
        .find(|instance| &instance.mono_key == key)
        .unwrap()
        .summary
}

#[test]
fn operator_dispatch_propagates_callee_effects_to_public_summary() {
    let source = "struct Point { x: int }\nimpl Add[Point] for Point { type Output = Point\n fn add(self, rhs: Point) Point = panic(\"stop\") }\npub fn api() { let p = Point { x: 1 }\n _ = p + p }\nfn main() { api() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert!(summary(&output, "api").may_panic);
}

#[test]
fn public_summaries_preserve_hidden_state_effects() {
    let source = "static SECRET: int = 1\npub fn read() int = SECRET\npub fn write() { SECRET = 2 }\nfn main() { _ = read()\n write() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert!(summary(&output, "read").reads_hidden_state);
    assert!(summary(&output, "write").writes_hidden_state);
}

#[test]
fn each_generic_instance_consumes_its_own_selected_callee_summary() {
    let source = "trait Value { fn value(self) int }\nstruct Box[T] { value: T }\nimpl Value for Box[T] { fn value(self) int = panic(\"stop\") }\nimpl Value for Box[int] { fn value(self) int = 1 }\nfn read[T](value: Box[T]) int = value.value()\nfn main() { _ = read(Box { value: 1 })\n _ = read(Box { value: true }) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let mut effects: Vec<_> = output
        .mono
        .instances
        .iter()
        .filter(|instance| instance.symbol == "read")
        .map(|instance| {
            output
                .analysis
                .instances
                .iter()
                .find(|record| record.mono_key == instance.mono_key)
                .unwrap()
                .summary
                .may_panic
        })
        .collect();
    effects.sort();
    assert_eq!(effects, vec![false, true]);
}

#[test]
fn exported_c_abi_changes_instance_identity() {
    let gugu = compile(
        &[(
            "main.gg",
            "#[used]\npub fn api(value: int) int = value\nfn main() {}",
        )],
        &crate::QueryEngine::new(),
    );
    let c = compile(
        &[(
            "main.gg",
            "#[used]\npub extern \"C\" fn api(value: int) int = value\nfn main() {}",
        )],
        &crate::QueryEngine::new(),
    );
    let key = |output: &FrontendOutput| {
        output
            .mono
            .instances
            .iter()
            .find(|instance| instance.symbol == "api")
            .unwrap()
            .mono_key
            .clone()
    };
    assert_ne!(key(&gugu), key(&c));
}

#[test]
fn public_parameter_effects_do_not_truncate_after_sixty_four() {
    let parameters = (0..65)
        .map(|index| format!("p{index}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!("#[used]\npub fn api({parameters}) int = p64\nfn main() {{}}");
    let unused = format!("#[used]\npub fn api({parameters}) int = 0\nfn main() {{}}");
    let read = compile(&[("main.gg", &source)], &crate::QueryEngine::new());
    let unused = compile(&[("main.gg", &unused)], &crate::QueryEngine::new());
    assert_eq!(summary(&read, "api").read_params, vec![64]);
    assert_ne!(read.mono.public_summaries, unused.mono.public_summaries);
}

#[test]
fn metadata_roots_change_closed_world_fingerprint() {
    let first = compile(
        &[("main.gg", "fn main() { _ = type_id[int]() }")],
        &crate::QueryEngine::new(),
    );
    let second = compile(
        &[("main.gg", "fn main() { _ = type_id[bool]() }")],
        &crate::QueryEngine::new(),
    );
    assert_ne!(first.mono.metadata_roots, second.mono.metadata_roots);
    assert_ne!(first.mono.graph_fingerprint, second.mono.graph_fingerprint);
}

#[test]
fn packs_and_callable_bounds_preserve_all_concrete_edges() {
    let source = "fn leaf() {}\nfn consume[Ts...](...args: Ts) { leaf() }\nfn apply[T, U, F: Fn(T) U](f: F, value: T) U = f(value)\nfn inc(value: int) int = value + 1\nfn main() { consume()\n consume(1, true)\n _ = apply(inc, 1) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    assert_eq!(
        output
            .mono
            .instances
            .iter()
            .filter(|instance| instance.symbol == "consume")
            .count(),
        2
    );
    let map = instance_map(&output.mono);
    assert_eq!(map["consume"], vec!["leaf"]);
    assert_eq!(map["apply"], vec!["inc"]);
}

#[test]
fn caller_type_parameter_does_not_rebind_callee_parameter() {
    let source = "fn inner[T](value: T) T = value\nfn outer[T](value: T) { _ = inner(value)\n _ = inner(false) }\nfn main() { outer(1) }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let inner: Vec<_> = output
        .mono
        .instances
        .iter()
        .filter(|instance| instance.symbol == "inner")
        .collect();
    assert_eq!(inner.len(), 2);
    assert_ne!(
        inner[0].signature_and_abi_fingerprint,
        inner[1].signature_and_abi_fingerprint
    );
    assert_eq!(instance_map(&output.mono)["outer"], vec!["inner", "inner"]);
}
