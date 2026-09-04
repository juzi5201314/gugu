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
