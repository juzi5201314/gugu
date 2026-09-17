use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use clap::Parser;

use super::{
    Cli, Command, GlobalArgs, OutputFormat,
    config::{ConfigFile, ConfigValues},
    format_hint,
    output::{redact_secrets, sanitize_path},
};

fn global_from_values(mut raw: GlobalArgs, config: ConfigValues) -> Result<GlobalArgs, String> {
    raw.format = Some(raw.format.unwrap_or_default());
    raw.color = Some(raw.color.unwrap_or_default());
    raw.offline = raw.frozen || raw.offline;
    raw.locked = raw.frozen || raw.locked;
    raw.require_signature = raw.require_signature || config.require_signature.unwrap_or(false);
    raw.deny_yanked = raw.deny_yanked || config.deny_yanked.unwrap_or(false);
    raw.target = raw.target.or(config.target);
    raw.bin = raw.bin.map(|value| value.filter(|name| !name.is_empty()));
    raw.permission = raw.permission || config.permission.unwrap_or(false);
    raw.read_allows = if raw.read_allows.is_empty() {
        config.read_allows
    } else {
        raw.read_allows
    };
    raw.write_allows = if raw.write_allows.is_empty() {
        config.write_allows
    } else {
        raw.write_allows
    };
    raw.env_allows = if raw.env_allows.is_empty() {
        config.env_allows
    } else {
        raw.env_allows
    };
    raw.net_allows = if raw.net_allows.is_empty() {
        config.net_allows
    } else {
        raw.net_allows
    };
    raw.run_allows = if raw.run_allows.is_empty() {
        config.run_allows
    } else {
        raw.run_allows
    };
    raw.cache_dir = raw.cache_dir.or(config.cache_dir);
    raw.target_dir = raw.target_dir.or(config.target_dir);
    Ok(raw)
}

#[test]
fn version_text_output_includes_commit_host_and_llvm() {
    let lines = super::version_text_lines();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("gugu "));
    assert!(lines[0].contains("(commit "));
    assert!(lines[1].starts_with("host: "));
    assert_eq!(lines[2], "llvm: not-used");
}

#[test]
fn no_subcommand_and_version_are_registered() {
    assert!(
        Cli::try_parse_from(["gugu"])
            .expect("root help is handled at runtime")
            .command
            .is_none()
    );
    assert!(matches!(
        Cli::try_parse_from(["gugu", "version"])
            .expect("version command parses")
            .command,
        Some(Command::Version)
    ));
}

#[test]
fn version_flag_is_parsed_without_clap_early_exit() {
    let cli = Cli::try_parse_from(["gugu", "--version"]).expect("version flag parses");
    assert!(cli.version_flag);
    assert!(cli.command.is_none());
}

#[test]
fn cli_command_table_accepts_registered_commands() {
    let cases = [
        vec!["gugu", "new", "demo"],
        vec!["gugu", "new", "demo", "--bin"],
        vec!["gugu", "init"],
        vec!["gugu", "build"],
        vec!["gugu", "check"],
        vec!["gugu", "run"],
        vec!["gugu", "test"],
        vec!["gugu", "bench"],
        vec!["gugu", "fmt"],
        vec!["gugu", "doc"],
        vec!["gugu", "clean"],
        vec!["gugu", "add", "acme/json"],
        vec!["gugu", "remove", "acme/json"],
        vec!["gugu", "update"],
        vec!["gugu", "tree"],
        vec!["gugu", "vendor"],
        vec!["gugu", "package"],
        vec!["gugu", "publish"],
        vec!["gugu", "yank", "acme/json", "--version", "1.0.0"],
        vec!["gugu", "login", "public"],
        vec!["gugu", "cache", "dir"],
        vec!["gugu", "explain", "E0001"],
        vec!["gugu", "version"],
        vec!["gugu", "help", "build"],
    ];
    for arguments in cases {
        Cli::try_parse_from(arguments).expect("command table entry parses");
    }
}

#[test]
fn global_options_parse_before_the_subcommand() {
    let cli = Cli::try_parse_from([
        "gugu",
        "--format",
        "json",
        "--color",
        "never",
        "--offline",
        "--locked",
        "--frozen",
        "--vendor",
        "--require-signature",
        "--deny-yanked",
        "--target",
        "x86_64-linux",
        "--package",
        "acme/app",
        "--workspace",
        "--lib",
        "--strip",
        "--permission",
        "-A",
        "--read-allows",
        "src/**",
        "--write-allows",
        "out/**",
        "--env-allows",
        "HOME",
        "--net-allows",
        "registry.example",
        "--run-allows",
        "tool",
        "--config",
        "config.toml",
        "--cache-dir",
        "cache",
        "--target-dir",
        "target",
        "build",
    ])
    .expect("global options parse");
    assert_eq!(cli.global.format, Some(OutputFormat::Json));
    assert_eq!(cli.global.color, Some(super::ColorMode::Never));
    assert!(cli.global.offline);
    assert!(cli.global.locked);
    assert!(cli.global.frozen);
    assert_eq!(cli.global.target.as_deref(), Some("x86_64-linux"));
    assert_eq!(cli.global.package.as_deref(), Some("acme/app"));
    assert!(cli.global.workspace);
    assert!(cli.global.lib);
    assert!(cli.global.allow_all);
    assert!(matches!(cli.command, Some(Command::Build { file: None })));
}

#[test]
fn global_options_parse_after_the_subcommand() {
    let cli = Cli::try_parse_from([
        "gugu",
        "build",
        "--format",
        "json",
        "--target",
        "x86_64-windows",
        "--quiet",
    ])
    .expect("trailing global options parse");
    assert_eq!(cli.global.format, Some(OutputFormat::Json));
    assert_eq!(cli.global.target.as_deref(), Some("x86_64-windows"));
    assert!(cli.global.quiet);
}

#[test]
fn global_cli_values_override_config_values() {
    let mut config = ConfigValues::default();
    config.apply(toml::from_str(
        "[build]\ntarget = \"x86_64-windows\"\ntarget-dir = \"config-target\"\n[cache]\ndir = \"config-cache\"\n"
    ).expect("valid config"));
    let raw = GlobalArgs {
        target: Some("x86_64-linux".to_owned()),
        cache_dir: Some(PathBuf::from("cli-cache")),
        ..GlobalArgs::default()
    };
    let options = global_from_values(raw, config).expect("options resolve");
    assert_eq!(options.target.as_deref(), Some("x86_64-linux"));
    assert_eq!(options.cache_dir, Some(PathBuf::from("cli-cache")));
    assert_eq!(options.target_dir, Some(PathBuf::from("config-target")));
}

#[test]
fn config_toml_fields_map_and_preserve_lower_layers() {
    let config = toml::from_str::<ConfigFile>(
        "[build]\ntarget = \"x86_64-linux\"\n[permission]\nread-allows = [\"src/**\"]\n",
    )
    .expect("config TOML parses");
    let mut values = ConfigValues::default();
    values.apply(config);
    let second = toml::from_str::<ConfigFile>("[build]\ntarget = \"x86_64-windows\"\n")
        .expect("second config parses");
    values.apply(second);
    assert_eq!(values.target.as_deref(), Some("x86_64-windows"));
    assert_eq!(values.read_allows, [PathBuf::from("src/**")]);
}

#[test]
fn json_path_and_secret_redaction_are_not_absolute_or_sensitive() {
    let cwd = Path::new("/workspace/project");
    assert_eq!(
        sanitize_path(Path::new("/workspace/project/src/main.gg"), cwd),
        "src/main.gg"
    );
    let message = redact_secrets("token=abc123 password: \"hunter2\"");
    assert_eq!(message, "token=<redacted> password: <redacted>");
}

#[test]
fn output_format_values_are_closed() {
    assert_eq!(
        OutputFormat::parse("json").expect("json format"),
        OutputFormat::Json
    );
    assert!(OutputFormat::parse("yaml").is_err());
}

#[test]
fn format_hint_follows_explicit_format() {
    let arguments = ["gugu", "--format", "json", "build"].map(OsString::from);
    assert_eq!(format_hint(&arguments), OutputFormat::Json);
}

#[test]
fn resolve_global_reads_cli_precedence_without_environment() {
    let raw = GlobalArgs {
        format: Some(OutputFormat::Json),
        ..GlobalArgs::default()
    };
    let options = global_from_values(raw, ConfigValues::default()).expect("options resolve");
    assert_eq!(options.format, Some(OutputFormat::Json));
}

#[test]
fn single_file_mode_rejects_project_selectors() {
    let base = || GlobalArgs {
        format: Some(OutputFormat::Text),
        ..GlobalArgs::default()
    };

    // 纯单文件输入不触发任何冲突。
    assert!(super::single_file_mode_conflict(&base()).is_none());

    // 项目专属参数与单文件互斥。
    let mut with_package = base();
    with_package.package = Some("demo".to_owned());
    assert_eq!(
        super::single_file_mode_conflict(&with_package).as_deref(),
        Some("单文件编译模式不支持参数：-p/--package")
    );

    let mut with_workspace = base();
    with_workspace.workspace = true;
    assert_eq!(
        super::single_file_mode_conflict(&with_workspace).as_deref(),
        Some("单文件编译模式不支持参数：--workspace")
    );

    // 多个冲突参数按声明顺序聚合列出。
    let mut combined = base();
    combined.lib = true;
    combined.all_targets = true;
    assert_eq!(
        super::single_file_mode_conflict(&combined).as_deref(),
        Some("单文件编译模式不支持参数：--lib、--all-targets")
    );
}

#[test]
fn dump_internal_flags_parse_and_require_internal_gate() {
    let cli = Cli::try_parse_from([
        "gugu",
        "check",
        "-Zdump-gir",
        "-Zdump-lir",
        "-Zdump-runtime",
    ])
    .expect("内部选项可解析");
    assert_eq!(
        cli.global.z,
        vec![
            "dump-gir".to_owned(),
            "dump-lir".to_owned(),
            "dump-runtime".to_owned()
        ]
    );
    assert!(
        super::validate_internal_flags(&cli.global, false)
            .unwrap_err()
            .contains("未启用")
    );
    assert!(super::validate_internal_flags(&cli.global, true).is_ok());
    let mut unknown = cli.global.clone();
    unknown.z = vec!["dump-unknown".to_owned()];
    assert!(
        super::validate_internal_flags(&unknown, true)
            .unwrap_err()
            .contains("未知内部选项")
    );
}

#[test]
fn build_json_reports_barrier_contract_keys() {
    let source =
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }";
    let compilation =
        gugu_compiler::Compiler::new().compile(gugu_compiler::CompileRequest::single_file(
            "main.gg",
            source,
            gugu_compiler::TargetName::X86_64Linux,
        ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    let payload = super::output::image_plan_payload(plan);
    for key in [
        "turn-region-sites",
        "turn-region-publish-sites",
        "turn-region-reset-sites",
        "turn-region-promote-sites",
        "turn-region-transfer-sites",
        "turn-region-capacity-class-count",
        "turn-region-object-limit",
        "turn-region-max-bytes",
        "turn-region-total-bytes",
        "turn-region-contract-fingerprint",
        "turn-region-demand",
        "barrier-card-granularity-bytes",
        "barrier-card-mark-buffer-entries",
        "barrier-card-mark-stamp-entries",
        "barrier-flush-reason-count",
        "barrier-card-mark-batch-fields",
        "barrier-record-count",
        "barrier-contract-fingerprint",
        "barrier-demand",
        "local-heap-contract-fingerprint",
        "local-heap-runtime",
        "local-heap-trigger",
        "local-heap-demand",
        "shared-heap-contract-fingerprint",
        "shared-heap-demand",
        "shared-heap-profile",
        "shared-heap-profile-revision",
        "shared-heap-handle-tag",
        "shared-heap-slot-bytes",
        "shared-heap-payload-record-bytes",
        "shared-heap-forwarding-grace-steps",
        "shared-heap-state-count",
        "shared-heap-transition-count",
        "shared-heap-forward-fields",
        "shared-heap-records",
    ] {
        assert!(payload.get(key).is_some(), "JSON 缺少 {key}");
    }
    assert_eq!(payload["turn-region-sites"], 1);
    assert_eq!(payload["turn-region-publish-sites"], 1);
    assert_eq!(payload["turn-region-reset-sites"], 1);
    assert_eq!(payload["turn-region-transfer-sites"], 0);
    assert_eq!(payload["turn-region-object-limit"], 64);
    assert_eq!(payload["barrier-card-granularity-bytes"], 512);
    assert_eq!(payload["barrier-card-mark-buffer-entries"], 256);
    assert_eq!(payload["barrier-flush-reason-count"], 6);
    assert_eq!(payload["barrier-card-mark-batch-fields"], 13);
    assert_eq!(
        payload["barrier-demand"]["card-mark-sites"],
        payload["barrier-demand"]["edge-summary-sites"]
    );
    // LocalHeap 的 arena/block/line、位图、TLAB 与触发参数必须进入 JSON 且与 dump 同源。
    assert_eq!(
        payload["local-heap-runtime"]["arena-bytes"],
        2 * 1024 * 1024
    );
    assert_eq!(payload["local-heap-runtime"]["block-bytes"], 32 * 1024);
    assert_eq!(payload["local-heap-runtime"]["line-bytes"], 128);
    assert_eq!(payload["local-heap-runtime"]["tlab-span-bytes"], 256 * 1024);
    assert_eq!(payload["local-heap-runtime"]["bitmap-bytes"], 16 * 1024);
    assert_eq!(payload["local-heap-runtime"]["page-cover-entries"], 512);
    assert_eq!(
        payload["local-heap-runtime"]["records"]
            .as_array()
            .expect("记录布局数组")
            .len(),
        4
    );
    assert_eq!(
        payload["local-heap-trigger"]["minor-trigger-bytes"],
        256 * 1024
    );
    assert_eq!(payload["local-heap-trigger"]["tenure-age"], 2);
    // 真实编译的 managed 类型数来自冻结类型表，必须与 GC metadata 契约一致。
    assert!(
        payload["local-heap-demand"]["managed-types"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(
        payload["local-heap-demand"]["managed-types"],
        payload["gc-metadata-type-count"]
    );
    assert_eq!(
        payload["local-heap-demand"]["barrier-sites"],
        payload["barrier-demand"]["card-mark-sites"]
    );
    // pacing 契约与需求使用同一字段口径：JSON 键必须与 dump 的段一一对应。
    for key in [
        "pacing-contract-fingerprint",
        "pacing-profile",
        "pacing-profile-revision",
        "pacing-min-growth-budget",
        "pacing-assist-threshold",
        "pacing-assist-quantum",
        "pacing-mark-cost-per-byte",
        "pacing-gc-cpu-fraction",
        "pacing-gc-cpu-window-cost",
        "pacing-remark-cost-budget",
        "pacing-evacuation-pause-bytes",
        "pacing-evacuation-pause-roots",
        "pacing-evacuation-pause-fields",
        "pacing-pressure-enter-ratio",
        "pacing-pressure-clear-ratio",
        "pacing-credit-source-count",
        "pacing-pressure-poll-bytes",
        "pacing-owner-drain-items",
        "pacing-owner-drain-bytes",
        "pacing-owner-drain-interval-bytes",
        "pacing-demand",
        "mark-contract-fingerprint",
        "mark-runtime",
        "mark-cycle-states",
        "mark-conditions",
        "mark-snapshot-participants",
        "mark-credit-pool",
        "mark-mailbox-consumers",
        "mark-ticket-fields",
        "mark-records",
        "mark-demand",
        "edge-contract-fingerprint",
        "edge-runtime",
        "edge-demand",
        "edge-candidate-quantum",
        "edge-candidate-schema",
        "edge-phase-count",
        "edge-block-state-count",
        "edge-delta-field-count",
    ] {
        assert!(payload.get(key).is_some(), "JSON 缺少 {key}");
    }
    assert_eq!(payload["mark-conditions"], 7);
    assert_eq!(payload["mark-snapshot-participants"], 6);
    assert_eq!(payload["mark-mailbox-consumers"], 1);
    assert_eq!(payload["mark-ticket-fields"], 15);
    assert_eq!(payload["mark-records"], 5);
    // mark 需求与 GC metadata/barrier/LocalHeap 需求覆盖同一站点集合。
    assert_eq!(
        payload["mark-demand"]["root-sites"],
        payload["gc-metadata-demand"]["root_range_count"]
    );
    assert_eq!(
        payload["mark-demand"]["barrier-sites"],
        payload["barrier-demand"]["card-mark-sites"]
    );
    assert_eq!(
        payload["mark-demand"]["ticket-sites"],
        payload["shared-heap-demand"]["mark-sites"]
    );
    assert_eq!(payload["pacing-profile"], "mosaic-default");
    assert_eq!(payload["pacing-gc-cpu-fraction"], 25);
    assert_eq!(payload["pacing-credit-source-count"], 9);
    assert_eq!(payload["pacing-pressure-poll-bytes"], 1 << 20);
    assert_eq!(payload["pacing-owner-drain-items"], 64);
    assert_eq!(payload["pacing-owner-drain-bytes"], 1 << 16);
    assert_eq!(payload["pacing-owner-drain-interval-bytes"], 1 << 20);
    assert_eq!(payload["pacing-pressure-enter-ratio"], 85);
    assert_eq!(payload["pacing-pressure-clear-ratio"], 70);
    // pacing 需求与 barrier 需求覆盖同一屏障站点集合。
    assert_eq!(
        payload["pacing-demand"]["barrier-sites"],
        payload["barrier-demand"]["card-mark-sites"]
    );
    assert!(
        payload["pacing-demand"]["alloc-sites"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    let pacing_fingerprint = payload["pacing-contract-fingerprint"]
        .as_array()
        .expect("pacing 指纹是字节数组");
    assert_eq!(pacing_fingerprint.len(), 32);
    assert!(
        pacing_fingerprint
            .iter()
            .any(|byte| byte != &serde_json::json!(0))
    );
    let fingerprint = payload["barrier-contract-fingerprint"]
        .as_array()
        .expect("指纹是字节数组");
    assert_eq!(fingerprint.len(), 32);
    assert!(fingerprint.iter().any(|byte| byte != &serde_json::json!(0)));
}

/// sender 在 send 之后仍使用闭包时必须落 SharedHeap；JSON 契约键与 dump 同源。
#[test]
fn build_json_reports_shared_heap_contract_keys() {
    let source = "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n _ = closure()\n }";
    let compilation =
        gugu_compiler::Compiler::new().compile(gugu_compiler::CompileRequest::single_file(
            "main.gg",
            source,
            gugu_compiler::TargetName::X86_64Linux,
        ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    let payload = super::output::image_plan_payload(plan);
    for key in [
        "shared-heap-contract-fingerprint",
        "shared-heap-demand",
        "shared-heap-profile",
        "shared-heap-profile-revision",
        "shared-heap-handle-tag",
        "shared-heap-slot-bytes",
        "shared-heap-payload-record-bytes",
        "shared-heap-forwarding-grace-steps",
        "shared-heap-state-count",
        "shared-heap-transition-count",
        "shared-heap-forward-fields",
        "shared-heap-records",
    ] {
        assert!(payload.get(key).is_some(), "JSON 缺少 {key}");
    }
    let demand = &payload["shared-heap-demand"];
    // 分配、解析与 slot 上界是同一个站点集合：每个 fresh payload 恰解析一次。
    assert_eq!(demand["alloc-sites"], demand["resolve-sites"]);
    assert_eq!(demand["alloc-sites"], demand["handle-slots"]);
    assert!(demand["alloc-sites"].as_u64().unwrap_or(0) > 0);
    assert!(demand["access-begin-sites"].as_u64().unwrap_or(0) > 0);
    assert_eq!(demand["access-begin-sites"], demand["access-end-sites"]);
    assert_eq!(payload["shared-heap-profile"], "mosaic-shared-handle");
    assert_eq!(payload["shared-heap-profile-revision"], 1);
    assert_eq!(payload["shared-heap-handle-tag"], 10);
    assert_eq!(payload["shared-heap-slot-bytes"], 64);
    assert_eq!(payload["shared-heap-payload-record-bytes"], 32);
    assert_eq!(payload["shared-heap-forwarding-grace-steps"], 4);
    assert_eq!(payload["shared-heap-state-count"], 6);
    assert_eq!(payload["shared-heap-transition-count"], 8);
    assert_eq!(payload["shared-heap-forward-fields"], 16);
    assert_eq!(payload["shared-heap-records"], 2);
    // LocalHeap 不再为共享 guard/handle 保留容量字段。
    assert!(payload["local-heap-demand"].get("shared-sites").is_none());
    let fingerprint = payload["shared-heap-contract-fingerprint"]
        .as_array()
        .expect("指纹是字节数组");
    assert_eq!(fingerprint.len(), 32);
    assert!(fingerprint.iter().any(|byte| byte != &serde_json::json!(0)));
}
