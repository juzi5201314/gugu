use super::{Cli, Command};
use clap::Parser;

#[test]
fn run_separator_keeps_program_arguments_out_of_target_selection() {
    for (arguments, expected_target, expected_args) in [
        (vec!["gugu", "run"], None, vec![]),
        (vec!["gugu", "run", "--"], None, vec![]),
        (
            vec!["gugu", "run", "--", "a1", "a2"],
            None,
            vec!["a1", "a2"],
        ),
        (
            vec!["gugu", "run", "app", "--", "a1", "--format", "json"],
            Some("app"),
            vec!["a1", "--format", "json"],
        ),
        (
            vec!["gugu", "run", "--example", "demo", "--", "--", "--help"],
            None,
            vec!["--", "--help"],
        ),
    ] {
        let cli = Cli::try_parse_from(&arguments).expect("run 参数应能解析");
        assert!(cli.global.format.is_none());
        let Some(Command::Run { target_name, args }) = cli.command else {
            panic!("应解析为 run 命令");
        };
        assert_eq!(target_name.as_deref(), expected_target, "{arguments:?}");
        assert_eq!(args, expected_args, "{arguments:?}");
    }
}

#[cfg(unix)]
#[test]
fn run_separator_preserves_non_utf8_program_arguments() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let argument = OsString::from_vec(vec![0xff, b'a']);
    let cli = Cli::try_parse_from([
        OsString::from("gugu"),
        OsString::from("run"),
        OsString::from("--"),
        argument.clone(),
    ])
    .expect("程序实参不要求 UTF-8");
    let Some(Command::Run { target_name, args }) = cli.command else {
        panic!("应解析为 run 命令");
    };
    assert!(target_name.is_none());
    assert_eq!(args, [argument]);
}
