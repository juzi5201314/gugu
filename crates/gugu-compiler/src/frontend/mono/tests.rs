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
    let reversed: Vec<(&str, &str)> = sources.iter().rev().map(|s| *s).collect();
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
    let summary_keys: Vec<String> = world
        .public_summaries
        .keys()
        .map(|key| key.clone())
        .collect();
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
    let mut empty = ActionInputs::new(b"c", "host", "host", "bin");
    assert_ne!(first.key(), empty.key());
}

#[test]
fn world_payload_has_no_session_local_ids() {
    let source = "pub fn api() int = 1\nfn main() { _ = api() }";
    let output = compile(&[("main.gg", source)], &crate::QueryEngine::new());
    let bytes = serde_json::to_vec(&output.mono).expect("world 序列化");
    // 规范编码不含 owner_index / DefId 字样字段。
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("owner_index"),
        "公共 world 不得携带 session-local 身份"
    );
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
