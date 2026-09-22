//! EarlyConst 域、capability registry 与受限脚本解释器的验收测试。
use super::comptime::{self, EarlyConstTable};
use super::model::{Model, Ty};
use super::tests::{accepts, frontend};
use crate::frontend::ast::ItemKind;
use crate::{CompileRequest, Compiler, DiagnosticCode, QueryEngine, TargetName};

fn diagnostics_of(source: &str) -> Vec<DiagnosticCode> {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    compilation
        .diagnostics()
        .items()
        .iter()
        .map(|diagnostic| diagnostic.code())
        .collect()
}

#[test]
fn early_const_query_evaluates_items_and_repeat_counts() {
    let queries = QueryEngine::new();
    let source = "const N: int = 4 * 2\nconst LABEL: string = \"gu\" + \"gu\"\n\
        fn main() { let a = [0; N]\n _ = a\n _ = LABEL }";
    let output = frontend(&[("main.gg", source)], &queries).expect("示例通过早期 comptime");
    let table: &EarlyConstTable = &output.semantics.early_constants;
    let item_values: Vec<_> = table
        .constants
        .iter()
        .filter(|entry| entry.key.expr == u32::MAX)
        .map(|entry| entry.value.clone())
        .collect();
    use comptime::eval::ConstantValue;
    assert!(item_values.contains(&ConstantValue::Int(8)));
    assert!(item_values.contains(&ConstantValue::String("gugu".to_owned())));
    // 冷/热 query 产出相同的规范表。
    let second = frontend(&[("main.gg", source)], &queries).expect("第二次命中 query 缓存");
    assert_eq!(&second.semantics.early_constants, table);
    assert!(table.verify().is_ok());
}

#[test]
fn comptime_fstrings_apply_static_format_codes() {
    let source = "const HEX: string = f\"{255:#06x}\"\nconst BIN: string = f\"{5:b}\"\n\
        const FLT: string = f\"{1.5:+.2}\"\nconst EXP: string = f\"{1234.5:.1e}\"\n\
        const AB: string = \"ab\"\nconst PAD: string = f\"{AB:>4}\"\n\
        const ARR: [int; 2] = [1, 2]\nconst DBG: string = f\"{ARR:?}\"\n\
        const TUP: (int, bool) = (1, true)\nconst PRETTY: string = f\"{TUP:#?}\"\n\
        const CUT: string = f\"{AB:.1}\"\n\
        fn width(n: int) string = f\"{7:n$}\"\nconst DYN: string = width(3)\nfn main() {}";
    let output = frontend(&[("main.gg", source)], &QueryEngine::new()).expect("格式码在编译期求值");
    let values: Vec<_> = output
        .semantics
        .early_constants
        .constants
        .iter()
        .filter(|entry| entry.key.expr == u32::MAX)
        .map(|entry| entry.value.clone())
        .collect();
    use comptime::eval::ConstantValue;
    for expected in [
        "0x00ff",
        "101",
        "+1.50",
        "1.2e3",
        "  ab",
        "[1, 2]",
        "(\n    1,\n    true,\n)",
        "a",
        "  7",
    ] {
        assert!(
            values.contains(&ConstantValue::String(expected.to_owned())),
            "缺少 {expected:?}：{values:?}"
        );
    }
}

#[test]
fn comptime_format_faults_are_diagnostics() {
    // 负的动态计数是 comptime panic。
    let codes = diagnostics_of(
        "fn width(n: int) string = f\"{7:n$}\"\nconst S: string = width(-1)\nfn main() {}",
    );
    assert!(codes.contains(&DiagnosticCode::ComptimePanic));
    // 标志与值不兼容在求值时就失败，不等待类型检查。
    let codes = diagnostics_of("const S: string = f\"{true:+}\"\nfn main() {}");
    assert!(
        codes.contains(&DiagnosticCode::InvalidExpression)
            || codes.contains(&DiagnosticCode::InvalidType)
    );
    // 类型检查器对运行时 f-string 应用同一套兼容规则。
    assert!(!accepts("fn main() { let s = f\"{true:+}\"\n _ = s }"));
    assert!(!accepts("fn main() { let s = f\"{1:.2}\"\n _ = s }"));
    assert!(!accepts(
        "fn main() { let t = \"x\"\n let s = f\"{t:#}\"\n _ = s }"
    ));
    assert!(!accepts("fn main() { let s = f\"{1.5:#}\"\n _ = s }"));
    assert!(accepts("fn main() { let s = f\"{1.5:+08.2}\"\n _ = s }"));
    assert!(accepts(
        "fn main() { let t = \"abc\"\n let s = f\"{t:.2}\"\n _ = s }"
    ));
    assert!(accepts("fn main() { let s = f\"{255:#x}\"\n _ = s }"));
}

#[test]
fn unregistered_std_call_fails_before_evaluation() {
    let codes = diagnostics_of("const X: int = std.io.println(\"x\")\nfn main() {}");
    assert!(codes.contains(&DiagnosticCode::ComptimeCapability));
}

#[test]
fn registered_capability_rejects_wrong_domain() {
    let codes = diagnostics_of("const X: int = std.syntax.parse_source(\"1\")\nfn main() {}");
    assert!(codes.contains(&DiagnosticCode::ComptimeCapability));
}

#[test]
fn comptime_panic_is_compile_error() {
    let codes = diagnostics_of("const X: int = panic(\"boom\")\nfn main() {}");
    assert!(codes.contains(&DiagnosticCode::ComptimePanic));
}

#[test]
fn budget_exhaustion_is_deterministic() {
    let output = frontend(
        &[(
            "main.gg",
            "const N: int = 2 * 10 + 1\nconst S: string = \"s\"\nfn main() {}",
        )],
        &QueryEngine::new(),
    )
    .expect("基线编译成功");
    let mut model = Model::new(&output.modules, &output.names).unwrap();
    model.set_eval_profile(comptime::eval::EvalProfile {
        fuel: 1,
        heap_bytes: 1 << 20,
        depth: 128,
    });
    // fuel 耗尽：任何一个子表达式都无法完成求值。
    let item = output.modules[0]
        .arena
        .items
        .iter()
        .find(|item| matches!(item.kind, ItemKind::Const { .. }))
        .unwrap();
    let ItemKind::Const {
        value: Some(value), ..
    } = item.kind
    else {
        unreachable!("已过滤 const 项")
    };
    let error = model
        .eval_early_const(0, value, &Ty::int())
        .expect_err("fuel 上限必须拒绝求值");
    assert_eq!(error.code(), DiagnosticCode::ComptimeBudget);
    // heap 耗尽：零字节配额下任何 string 构造都失败。
    model.set_eval_profile(comptime::eval::EvalProfile {
        fuel: 1_000_000,
        heap_bytes: 0,
        depth: 128,
    });
    let ItemKind::Const {
        value: Some(value), ..
    } = output.modules[0].arena.items[1].kind
    else {
        unreachable!("第二项是 string 常量")
    };
    let error = model
        .eval_early_const(0, value, &Ty::String)
        .expect_err("heap 上限必须拒绝分配");
    assert_eq!(error.code(), DiagnosticCode::ComptimeBudget);
}

#[test]
fn comptime_blocks_force_early_evaluation() {
    assert!(accepts(
        "fn main() { let x = comptime { let a = 2\n a * 3 }\n _ = x }"
    ));
    // 运行时实参无法在编译期求值，`comptime` 强制立即失败。
    assert!(!accepts(
        "fn f(x: int) int { comptime x }\nfn main() { _ = f(1) }"
    ));
    assert!(accepts("fn main() { let x = comptime 1 + 2\n _ = x }"));
}

#[test]
fn comptime_value_parameters_require_early_constants() {
    assert!(accepts(
        "fn repeat(comptime n: int) int = n * 2\nfn main() { _ = repeat(3) }"
    ));
    let codes = diagnostics_of(
        "fn repeat(comptime n: int) int = n * 2\nfn main() { let x = 1\n _ = repeat(x) }",
    );
    assert!(codes.contains(&DiagnosticCode::ComptimeCapability));
}

#[test]
fn user_functions_are_interpreted_with_capability_propagation() {
    assert!(accepts(
        "fn add(a: int, b: int) int = a + b\nconst N: int = add(2, 3)\nfn main() { let a = [0; N]\n _ = a }"
    ));
    // 用户函数内的未登记 std 调用沿调用链在求值前拒绝。
    let codes =
        diagnostics_of("fn bad() int = std.io.println(\"x\")\nconst N: int = bad()\nfn main() {}");
    assert!(codes.contains(&DiagnosticCode::ComptimeCapability));
    // 循环与局部绑定是解释器语言核心。
    assert!(accepts(
        "fn sum(n: int) int { let total = 0\n for i in 0..n { total += i }\n total }\nconst N: int = sum(4)\nfn main() { let a = [0; N]\n _ = a }"
    ));
}

#[test]
fn comptime_calls_use_lexical_scope() {
    let output = frontend(
        &[("main.gg", "const X: int = 7\nfn read() int = X\nfn outer() int { let X = 2\n read() }\nconst N: int = outer()\nfn main() { _ = N }")],
        &QueryEngine::new(),
    ).expect("常量函数使用定义处名称");
    let value = output
        .semantics
        .early_constants
        .constants
        .iter()
        .find(|entry| entry.key.item == 3 && entry.key.expr == u32::MAX)
        .unwrap();
    assert_eq!(value.value, comptime::eval::ConstantValue::Int(7));
}

#[test]
fn comptime_match_compares_numeric_literals() {
    let output = frontend(
        &[("main.gg", "fn choose(n: int) int { match n { 2 => 9, _ => 4 } }\nconst N: int = choose(2)\nfn main() { _ = N }")],
        &QueryEngine::new(),
    ).expect("整数模式可以在编译期匹配");
    let value = output
        .semantics
        .early_constants
        .constants
        .iter()
        .find(|entry| entry.key.item == 1 && entry.key.expr == u32::MAX)
        .unwrap();
    assert_eq!(value.value, comptime::eval::ConstantValue::Int(9));
}

#[test]
fn comptime_heap_accounts_for_aggregate_payloads() {
    let output = frontend(
        &[("main.gg", "const A: [string; 4] = [\"abcdefgh\"; 4]\nconst B: string = (\"abcd\" + \"efgh\") + \"ijkl\"\nfn main() {}")],
        &QueryEngine::new(),
    ).expect("普通预算下聚合可求值");
    let mut model = Model::new(&output.modules, &output.names).unwrap();
    model.set_eval_profile(comptime::eval::EvalProfile {
        fuel: 1000,
        heap_bytes: 16,
        depth: 128,
    });
    for item in &output.modules[0].arena.items[..2] {
        let ItemKind::Const {
            value: Some(value), ..
        } = item.kind
        else {
            unreachable!()
        };
        let error = model
            .eval_early_const(0, value, &Ty::int())
            .expect_err("复制的嵌套负载和拼接结果必须计入堆预算");
        assert_eq!(error.code(), DiagnosticCode::ComptimeBudget);
    }
}

#[test]
fn cyclic_and_unevaluable_initializers_still_fail_at_use_sites() {
    assert!(!accepts(
        "const A: int = B\nconst B: int = A\nfn main() { let a = [0; A]\n _ = a }"
    ));
    // 未被使用的不可求值构造留给惰性路径，不影响其它常量进入表。
    assert!(accepts("const X: int = 1\nfn main() {}"));
}

#[test]
fn action_key_is_deterministic_and_input_sensitive() {
    let compiler = Compiler::new();
    let source = "const N: int = 4\nfn main() { let a = [0; N]\n _ = a }";
    let first = compiler.compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    let second = compiler.compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    let changed = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "const N: int = 5\nfn main() { let a = [0; N]\n _ = a }",
        TargetName::X86_64Linux,
    ));
    let first_key = first.action_key().expect("成功编译产生 action key");
    assert_eq!(
        first_key,
        second.action_key().expect("相同输入得到相同 key")
    );
    assert_ne!(first_key, changed.action_key().expect("输入变化改变 key"));
}

#[test]
fn comptime_loop_preserves_break_value() {
    let output = frontend(
        &[("main.gg", "fn answer() int { let x = loop { break 7 }\n x }\nconst N: int = answer()\nfn main() { _ = N }")],
        &QueryEngine::new(),
    ).expect("有值 break 形成循环结果");
    let value = output
        .semantics
        .early_constants
        .constants
        .iter()
        .find(|entry| entry.key.item == 1 && entry.key.expr == u32::MAX)
        .unwrap();
    assert_eq!(value.value, comptime::eval::ConstantValue::Int(7));
}
