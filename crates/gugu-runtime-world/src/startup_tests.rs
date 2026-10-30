//! 环境快照与启动配置解析的确定性测试。
//!
//! 全部用例固定输入、断言解析结果与 canonical 首错顺序；非法值都必须得到
//! `InvalidConfiguration` 的稳定错误，不允许部分生效。

use super::startup::{
    BacktraceMode, DiagnosticsFormat, EnvironmentSnapshot, GcTargetConfig, needs_emergency_report,
    parse, parse_backtrace_var, parse_diagnostics_var,
};

fn snapshot(entries: &[(&str, &str)]) -> EnvironmentSnapshot {
    EnvironmentSnapshot::fix(
        vec!["gugu".to_owned()],
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        "/work".to_owned(),
        4,
    )
}

fn config(entries: &[(&str, &str)]) -> super::startup::StartupConfig {
    parse(&snapshot(entries)).expect("配置可解析")
}

#[test]
fn missing_variables_use_documented_defaults() {
    let config = config(&[]);
    assert_eq!(config.parallelism(), 4);
    assert_eq!(config.gc_target(), GcTargetConfig::Automatic(100));
    assert_eq!(config.memory_limit(), None);
    assert_eq!(config.stack_max(), 1024 * 1024 * 1024);
    assert!(
        !config
            .trace()
            .is_open(super::startup_schema::TraceCategory::Gc)
    );
    assert_eq!(config.diagnostics(), DiagnosticsFormat::Text);
    assert_eq!(config.backtrace(), BacktraceMode::Triggering);
}

#[test]
fn parallelism_probe_failure_falls_back_to_one() {
    let fixed = EnvironmentSnapshot::fix(vec![], vec![], "/".to_owned(), 0);
    assert_eq!(fixed.host_parallelism(), 1);
    let config = parse(&fixed).expect("配置可解析");
    assert_eq!(config.parallelism(), 1);
}

#[test]
fn snapshot_env_is_sorted_and_fixed_once() {
    let fixed = snapshot(&[("B", "2"), ("A", "1"), ("C", "3")]);
    let names: Vec<&str> = fixed.env().iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(names, vec!["A", "B", "C"]);
    assert_eq!(fixed.argv(), &["gugu".to_owned()]);
    assert_eq!(fixed.working_directory(), "/work");
}

#[test]
fn procs_accepts_positive_decimal_only() {
    assert_eq!(config(&[("GUGU_RUNTIME_PROCS", "8")]).parallelism(), 8);
    let errors = parse(&snapshot(&[("GUGU_RUNTIME_PROCS", "0")])).expect_err("0 被拒绝");
    assert_eq!(errors[0].variable(), "GUGU_RUNTIME_PROCS");
    for bad in ["-1", "1.5", "0x10", "1 2", "", "99999999999999999999"] {
        let errors =
            parse(&snapshot(&[("GUGU_RUNTIME_PROCS", bad)])).expect_err("非法并行度被拒绝");
        assert_eq!(errors[0].variable(), "GUGU_RUNTIME_PROCS", "输入 {bad}");
    }
}

#[test]
fn byte_quantities_follow_fixed_grammar() {
    assert_eq!(
        config(&[("GUGU_RUNTIME_MEMORY_LIMIT", "512KiB")]).memory_limit(),
        Some(512 * 1024)
    );
    assert_eq!(
        config(&[("GUGU_RUNTIME_MEMORY_LIMIT", "2GiB")]).memory_limit(),
        Some(2 * 1024 * 1024 * 1024)
    );
    assert_eq!(
        config(&[("GUGU_RUNTIME_MEMORY_LIMIT", "4096")]).memory_limit(),
        Some(4096)
    );
    assert_eq!(
        config(&[("GUGU_RUNTIME_MEMORY_LIMIT", "off")]).memory_limit(),
        None
    );
    for bad in [
        "0",
        "12TB",
        "12kib",
        "12 KiB",
        "+12",
        " 12",
        "12 ",
        "1e3",
        "18446744073709551616TiB",
    ] {
        let errors =
            parse(&snapshot(&[("GUGU_RUNTIME_MEMORY_LIMIT", bad)])).expect_err("非法字节量被拒绝");
        assert_eq!(
            errors[0].variable(),
            "GUGU_RUNTIME_MEMORY_LIMIT",
            "输入 {bad}"
        );
    }
}

#[test]
fn stack_max_has_lower_and_upper_bound() {
    assert_eq!(
        config(&[("GUGU_RUNTIME_STACK_MAX", "64KiB")]).stack_max(),
        64 * 1024
    );
    assert_eq!(
        config(&[("GUGU_RUNTIME_STACK_MAX", "1TiB")]).stack_max(),
        1024u64 * 1024 * 1024 * 1024
    );
    let errors = parse(&snapshot(&[("GUGU_RUNTIME_STACK_MAX", "32KiB")])).expect_err("低于下界");
    assert_eq!(errors[0].variable(), "GUGU_RUNTIME_STACK_MAX");
    let errors = parse(&snapshot(&[("GUGU_RUNTIME_STACK_MAX", "off")])).expect_err("不接受 off");
    assert_eq!(errors[0].variable(), "GUGU_RUNTIME_STACK_MAX");
}

#[test]
fn gc_target_accepts_off_and_percent() {
    assert_eq!(
        config(&[("GUGU_RUNTIME_GC_TARGET", "off")]).gc_target(),
        GcTargetConfig::Off
    );
    assert_eq!(
        config(&[("GUGU_RUNTIME_GC_TARGET", "0")]).gc_target(),
        GcTargetConfig::Automatic(0)
    );
    let errors = parse(&snapshot(&[("GUGU_RUNTIME_GC_TARGET", "-5")])).expect_err("负数被拒绝");
    assert_eq!(errors[0].variable(), "GUGU_RUNTIME_GC_TARGET");
}

#[test]
fn trace_categories_merge_and_reject_unknown() {
    let selected = config(&[("GUGU_RUNTIME_TRACE", "panic,signal")]);
    assert!(
        selected
            .trace()
            .is_open(super::startup_schema::TraceCategory::Panic)
    );
    assert!(
        selected
            .trace()
            .is_open(super::startup_schema::TraceCategory::Signal)
    );
    assert!(
        !selected
            .trace()
            .is_open(super::startup_schema::TraceCategory::Scheduler)
    );
    let merged = config(&[("GUGU_RUNTIME_TRACE", "gc,gc")]);
    assert!(
        merged
            .trace()
            .is_open(super::startup_schema::TraceCategory::Gc)
    );
    for name in ["bogus", "scheduler,", ",panic", "GC"] {
        let errors = parse(&snapshot(&[("GUGU_RUNTIME_TRACE", name)])).expect_err("非法类别被拒绝");
        assert_eq!(errors[0].variable(), "GUGU_RUNTIME_TRACE", "输入 {name}");
    }
}

#[test]
fn trace_all_opens_every_category() {
    let config = config(&[("GUGU_RUNTIME_TRACE", "all")]);
    assert!(
        config
            .trace()
            .is_open(super::startup_schema::TraceCategory::Scheduler)
    );
    assert!(
        config
            .trace()
            .is_open(super::startup_schema::TraceCategory::Gc)
    );
    assert!(
        config
            .trace()
            .is_open(super::startup_schema::TraceCategory::Signal)
    );
    assert!(
        config
            .trace()
            .is_open(super::startup_schema::TraceCategory::Panic)
    );
}

#[test]
fn diagnostics_and_backtrace_accept_only_listed_values() {
    assert_eq!(
        config(&[("GUGU_RUNTIME_DIAGNOSTICS", "json")]).diagnostics(),
        DiagnosticsFormat::Json
    );
    assert_eq!(
        config(&[("GUGU_RUNTIME_DIAGNOSTICS", "both")]).diagnostics(),
        DiagnosticsFormat::Both
    );
    assert_eq!(
        config(&[("GUGU_BACKTRACE", "0")]).backtrace(),
        BacktraceMode::Off
    );
    assert_eq!(
        config(&[("GUGU_BACKTRACE", "full")]).backtrace(),
        BacktraceMode::Full
    );
    for (variable, value) in [
        ("GUGU_RUNTIME_DIAGNOSTICS", "yaml"),
        ("GUGU_RUNTIME_DIAGNOSTICS", ""),
        ("GUGU_BACKTRACE", "2"),
        ("GUGU_BACKTRACE", "yes"),
    ] {
        let errors = parse(&snapshot(&[(variable, value)])).expect_err("非法诊断配置被拒绝");
        assert_eq!(errors[0].variable(), variable, "输入 {value}");
    }
}

#[test]
fn errors_are_collected_in_canonical_variable_order() {
    let errors = parse(&snapshot(&[
        ("GUGU_BACKTRACE", "2"),
        ("GUGU_RUNTIME_STACK_MAX", "1KiB"),
        ("GUGU_RUNTIME_PROCS", "0"),
    ]))
    .expect_err("存在非法配置");
    let variables: Vec<&str> = errors.iter().map(|error| error.variable()).collect();
    assert_eq!(
        variables,
        vec![
            "GUGU_RUNTIME_PROCS",
            "GUGU_RUNTIME_STACK_MAX",
            "GUGU_BACKTRACE"
        ]
    );
}

#[test]
fn emergency_report_is_required_for_invalid_diagnostics_config() {
    assert!(needs_emergency_report(
        &parse(&snapshot(&[("GUGU_RUNTIME_DIAGNOSTICS", "yaml")])).expect_err("非法诊断配置")
    ));
    assert!(needs_emergency_report(
        &parse(&snapshot(&[("GUGU_BACKTRACE", "2")])).expect_err("非法回溯配置")
    ));
    assert!(!needs_emergency_report(
        &parse(&snapshot(&[("GUGU_RUNTIME_STACK_MAX", "1KiB")])).expect_err("非法栈上限")
    ));
    assert!(parse_diagnostics_var(&snapshot(&[("GUGU_RUNTIME_DIAGNOSTICS", "json")])).is_some());
    assert!(parse_diagnostics_var(&snapshot(&[("GUGU_RUNTIME_DIAGNOSTICS", "x")])).is_none());
    assert!(parse_diagnostics_var(&snapshot(&[])).is_none());
    assert!(parse_backtrace_var(&snapshot(&[("GUGU_BACKTRACE", "full")])).is_some());
    assert!(parse_backtrace_var(&snapshot(&[("GUGU_BACKTRACE", "2")])).is_none());
}

#[test]
fn valid_variables_still_apply_when_others_fail() {
    let errors = parse(&snapshot(&[
        ("GUGU_RUNTIME_PROCS", "0"),
        ("GUGU_RUNTIME_STACK_MAX", "2MiB"),
    ]))
    .expect_err("存在非法配置");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].variable(), "GUGU_RUNTIME_PROCS");
}
