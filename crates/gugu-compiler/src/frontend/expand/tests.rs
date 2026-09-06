//! 阶段 22：源码宏展开测试（五个 slot、嵌套轮次、失败边界与缓存一致性）。

use crate::{CompileRequest, Compiler, DiagnosticCode, TargetName};

fn compile(source: &str) -> crate::Compilation {
    Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ))
}

fn accepts(source: &str) -> bool {
    let compilation = compile(source);
    if !compilation.is_success() {
        for diagnostic in compilation.diagnostics().items() {
            eprintln!("{diagnostic:?}");
        }
    }
    compilation.is_success()
}

fn rejects_with(source: &str, code: DiagnosticCode) -> crate::Compilation {
    let compilation = compile(source);
    assert!(
        !compilation.is_success(),
        "源码应被 `{code}` 拒绝：{:?}",
        compilation.diagnostics().items()
    );
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code() == code),
        "诊断应包含 `{code}`：{:?}",
        compilation.diagnostics().items()
    );
    compilation
}

#[test]
fn expression_slot_macro_expands_into_initializer() {
    // spec/comptime.md 的示例：表达式 slot 宏生成 `1 + 2`。
    assert!(accepts(
        "const b: int = 2\nfn main() { let value = comptime source {\n    std.syntax.parse_source(f\"1 + {b}\")\n}\n _ = value }"
    ));
}

#[test]
fn expression_slot_macro_registers_expansion_record() {
    let compilation = compile(
        "fn main() { let value = comptime source {\n    std.syntax.parse_source(\"1 + 2\")\n}\n _ = value }",
    );
    assert!(compilation.is_success());
    assert_eq!(compilation.source_map().expansions().len(), 1);
    let expansion = &compilation.source_map().expansions()[0];
    assert_eq!(
        expansion.fragment_kind(),
        crate::source::SourceSlot::Expression
    );
    assert_eq!(expansion.parent(), crate::source::ExpansionId::ROOT);
}

#[test]
fn item_slot_macro_generates_functions() {
    assert!(accepts(
        "comptime source {\n    std.syntax.parse_items(\"fn helper() int { 7 }\")\n}\nfn main() { _ = helper() }"
    ));
}

#[test]
fn statement_slot_macro_expands_statements_and_tail() {
    // 语句 slot：宏位于块尾，生成 `let x = 5` 语句与 `x + 1` 尾表达式。
    // 片段尾表达式成为内层块的块尾，块值再绑定到 v。
    assert!(accepts(
        "fn main() { let v: int = { comptime source {\n    std.syntax.parse_source(\"let x = 5\\n x + 1\")\n} }\n _ = v }"
    ));
}

#[test]
fn statement_slot_macro_in_block_middle_without_tail() {
    // 宏在块中部：生成语句（无尾表达式），块的尾表达式来自后续源码。
    assert!(accepts(
        "fn main() { comptime source {\n    std.syntax.parse_source(\"let x = 5\")\n}\n _ = x }"
    ));
}

#[test]
fn type_slot_macro_expands_into_type_position() {
    assert!(accepts(
        "fn main() { let pair: comptime source {\n    std.syntax.parse_type(\"(int, bool)\")\n} = (1, true)\n _ = pair }"
    ));
}

#[test]
fn pattern_slot_macro_expands_into_match_arm() {
    assert!(accepts(
        "fn pick(pair: (int, int)) int { match pair { comptime source {\n    std.syntax.parse_pattern(\"(a, b)\")\n} => a + b } }\nfn main() { _ = pick((1, 2)) }"
    ));
}

#[test]
fn nested_macro_registers_parent_chain_over_two_rounds() {
    // 外层宏生成包含内层宏的源码；第二轮展开内层宏。
    let source = "comptime source {\n    std.syntax.parse_items(\"comptime source {\\n    std.syntax.parse_items(\\\"fn inner() int { 9 }\\\")\\n}\\nfn outer() int { inner() }\")\n}\nfn main() { _ = outer() }";
    let compilation = compile(source);
    assert!(compilation.is_success());
    let expansions = compilation.source_map().expansions();
    assert_eq!(expansions.len(), 2, "两轮各注册一个展开");
    assert_eq!(expansions[1].parent().as_u32(), 1, "内层展开挂在第一轮之下");
}

#[test]
fn generated_items_respect_cfg_pruning() {
    // 生成片段中的 cfg(false) item 必须被裁掉，不进入名称解析。
    assert!(accepts(
        "comptime source {\n    std.syntax.parse_items(\"#[cfg(false)]\\nfn dead() int { 1 }\\nfn live() int { 2 }\")\n}\nfn main() { _ = live() }"
    ));
    rejects_with(
        "comptime source {\n    std.syntax.parse_items(\"#[cfg(false)]\\nfn dead() int { 1 }\")\n}\nfn main() { _ = dead() }",
        DiagnosticCode::InvalidType,
    );
}

#[test]
fn type_error_in_generated_code_anchors_at_macro_call() {
    // 生成代码类型错误：主诊断 span 锚定在宏调用点（原始源码），附注携带展开链。
    let compilation = compile(
        "fn main() { let x = comptime source {\n    std.syntax.parse_source(\"\\\"text\\\" + 1\")\n}\n _ = x }",
    );
    assert!(!compilation.is_success());
    let diagnostics = compilation.diagnostics().items();
    let has_anchored = diagnostics.iter().any(|diagnostic| {
        diagnostic
            .span()
            .is_some_and(|span| span.expansion() == crate::source::ExpansionId::ROOT)
    });
    assert!(has_anchored, "诊断必须锚定原始源码：{diagnostics:?}");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.severity() == crate::Severity::Note
                && diagnostic
                    .span()
                    .is_some_and(|span| span.expansion() != crate::source::ExpansionId::ROOT)
        }),
        "附注必须指向生成文本：{diagnostics:?}"
    );
}

#[test]
fn uncaught_syntax_error_becomes_macro_boundary_error() {
    // 解析失败且脚本未捕获：Err 传播到宏边界，编译不展开该宏。
    rejects_with(
        "fn main() { let x = comptime source {\n    std.syntax.parse_source(\"1 +\")\n}\n _ = x }",
        DiagnosticCode::MacroBoundaryError,
    );
}

#[test]
fn script_catches_syntax_error_and_falls_back() {
    // 脚本用 match 捕获 Err(SyntaxError)，回退到候选文本。
    assert!(accepts(
        "fn main() { let x = comptime source {\n    match std.syntax.parse_source(\"1 +\") {\n        Ok(fragment) => fragment\n        Err(_) => std.syntax.parse_source(\"2\")\n    }\n}\n _ = x }"
    ));
}

#[test]
fn script_try_operator_propagates_to_boundary() {
    // 顶层 `?` 把 Err 传出脚本，成为宏边界错误。
    rejects_with(
        "fn main() { let x = comptime source {\n    std.syntax.parse_source(\"1 +\")?\n}\n _ = x }",
        DiagnosticCode::MacroBoundaryError,
    );
}

#[test]
fn fragment_kind_mismatch_is_expansion_error() {
    // 表达式 slot 使用 parse_items：类别不匹配是展开错误（E0050），不是类型错误。
    rejects_with(
        "fn main() { let x = comptime source {\n    std.syntax.parse_items(\"fn f() int { 1 }\")\n}\n _ = x }",
        DiagnosticCode::ExpansionFragmentMismatch,
    );
}

#[test]
fn parse_source_uses_insertion_slot() {
    // parse_source 按插入上下文（表达式）解析；语句文本不构成合法单表达式片段，
    // 解析闸门失败以 Err 传给脚本，未捕获即到宏边界。
    rejects_with(
        "fn main() { let x = comptime source {\n    std.syntax.parse_source(\"let a = 1\")\n}\n _ = x }",
        DiagnosticCode::MacroBoundaryError,
    );
}

#[test]
fn self_regenerating_macro_reports_cycle() {
    // 自再生宏：生成文本包含与祖先完全相同的脚本文本，第二轮即命中 cycle。
    let compilation = compile(
        "const SELF: string = \"comptime source {\\n    std.syntax.parse_items(SELF)\\n}\"\ncomptime source {\n    std.syntax.parse_items(SELF)\n}\nfn main() {}",
    );
    assert!(!compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code() == DiagnosticCode::ExpansionCycle),
        "必须报告展开循环：{:?}",
        compilation.diagnostics().items()
    );
}

#[test]
fn expansion_limit_attribute_bounds_depth() {
    // expansion_limit = 2：第三个嵌套深度被拒。
    let compilation = compile(
        "#![comptime(expansion_limit = 2)]\ncomptime source {\n    std.syntax.parse_items(\"comptime source {\\n    std.syntax.parse_items(\\\"comptime source {}\\\")\\n}\\nfn f() int { 1 }\")\n}\nfn main() {}",
    );
    assert!(!compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code() == DiagnosticCode::ExpansionLimit),
        "必须报告深度超限：{:?}",
        compilation.diagnostics().items()
    );
}

#[test]
fn invalid_expansion_limit_values_are_rejected() {
    for source in [
        "#![comptime(expansion_limit = 0)]\nfn main() {}",
        "#![comptime(expansion_limit = 99999)]\nfn main() {}",
    ] {
        let compilation = compile(source);
        assert!(!compilation.is_success(), "`{source}` 必须被拒绝");
        assert!(
            compilation
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic.code() == DiagnosticCode::ExpansionLimit),
            "expansion_limit 非法值必须报 E0049：{:?}",
            compilation.diagnostics().items()
        );
    }
}

#[test]
fn macro_in_early_const_domain_is_rejected() {
    // std.syntax.parse_* 只能在 SourceExpand 域调用：普通 comptime 常量中调用
    // 在求值前返回 E0045（阶段 21 语义）。
    let compilation = compile(
        "const c: int = comptime {\n    let r = std.syntax.parse_source(\"1\")\n    1\n}\nfn main() { _ = c }",
    );
    assert!(!compilation.is_success());
    assert!(
        compilation
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.code() == DiagnosticCode::ComptimeCapability),
        "错域调用必须报 E0045：{:?}",
        compilation.diagnostics().items()
    );
}

#[test]
fn cold_and_warm_compilations_agree() {
    let source = "comptime source {\n    std.syntax.parse_items(\"fn gen() int { 3 }\")\n}\nfn main() { _ = gen() }";
    let cold = compile(source);
    let warm = compile(source);
    assert!(cold.is_success());
    assert!(warm.is_success());
    assert_eq!(
        cold.action_key().map(|key| key.hex()),
        warm.action_key().map(|key| key.hex())
    );
    assert_eq!(
        cold.source_map().expansions().len(),
        warm.source_map().expansions().len()
    );
    assert_eq!(cold.is_success(), warm.is_success());
}

#[test]
fn action_key_is_sensitive_to_generated_text() {
    let base = "comptime source {\n    std.syntax.parse_items(\"fn gen() int { 3 }\")\n}\nfn main() { _ = gen() }";
    let changed = "comptime source {\n    std.syntax.parse_items(\"fn gen() int { 4 }\")\n}\nfn main() { _ = gen() }";
    let first = compile(base);
    let second = compile(changed);
    assert!(first.is_success());
    assert!(second.is_success());
    assert_ne!(
        first.action_key().map(|key| key.hex()),
        second.action_key().map(|key| key.hex()),
        "宏脚本变化必须改变 action key"
    );
}

#[test]
fn macro_free_sources_keep_baseline_pipeline() {
    // 无宏源码：展开驱动器直通，快照表不含生成文件，主链行为不变。
    let compilation = compile("fn main() { let x = 1\n _ = x }");
    assert!(compilation.is_success());
    assert_eq!(compilation.source_map().expansions().len(), 0);
    assert_eq!(compilation.source_map().snapshots().len(), 1);
}

#[test]
fn dual_target_macro_smoke() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            "comptime source {\n    std.syntax.parse_items(\"fn gen() int { 5 }\")\n}\nfn main() { _ = gen() }",
            target,
        ));
        assert!(compilation.is_success(), "{target} 必须接受源码宏");
    }
}

#[test]
fn query_cache_returns_same_expansion_data() {
    // 相同生成文本复用 ParseSource/ExpandSourceMacro 缓存：两次编译展开记录一致。
    let source = "fn main() { _ = comptime source {\n    std.syntax.parse_source(\"40 + 2\")\n} }";
    let first = compile(source);
    let second = compile(source);
    assert!(first.is_success() && second.is_success());
    let extract = |compilation: &crate::Compilation| {
        compilation
            .source_map()
            .expansions()
            .iter()
            .map(|expansion| (expansion.source_hash(), expansion.fragment_kind()))
            .collect::<Vec<_>>()
    };
    assert_eq!(extract(&first), extract(&second));
}
