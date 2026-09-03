use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use gugu_compiler::{Compilation, CompileRequest, Compiler, TargetName};

#[derive(Debug, Parser)]
#[command(name = "gugu", version, about = "Gugu 编译器 bootstrap")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 编译一个单文件入口或空 package。
    Build {
        /// 单文件入口；省略时使用空 package。
        file: Option<PathBuf>,
        /// 目标名称。
        #[arg(long)]
        target: Option<String>,
    },
    /// 执行编译检查但不准备镜像计划。
    Check {
        /// 单文件入口；省略时使用空 package。
        file: Option<PathBuf>,
        /// 目标名称。
        #[arg(long)]
        target: Option<String>,
    },
    /// 打印版本。
    Version,
}

fn main() {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        let mut command = Cli::command();
        if let Err(error) = command.print_help() {
            eprintln!("无法输出帮助：{error}");
            std::process::exit(101);
        }
        println!();
        return;
    };

    let exit_code = match command {
        Command::Build { file, target } => run_compile(file, target, false),
        Command::Check { file, target } => run_compile(file, target, true),
        Command::Version => {
            println!("gugu {}", env!("CARGO_PKG_VERSION"));
            0
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn run_compile(file: Option<PathBuf>, target: Option<String>, check_only: bool) -> i32 {
    let target = match target {
        Some(target) => match TargetName::parse(&target) {
            Ok(target) => target,
            Err(error) => {
                eprintln!("error: {error}");
                return 2;
            }
        },
        None => match TargetName::host() {
            Some(target) => target,
            None => {
                eprintln!("error: 当前宿主不是已登记的 Gugu 目标");
                return 2;
            }
        },
    };

    let request = match file {
        Some(file) => CompileRequest::single_file_path(file, target),
        None => CompileRequest::empty_package(target),
    };
    let compilation = Compiler::new().compile(request);
    print_compilation(&compilation, check_only);
    i32::from(!compilation.is_success())
}

fn print_compilation(compilation: &Compilation, check_only: bool) {
    for node in compilation.action_graph().nodes() {
        println!(
            "action {:02} {:<16} {}",
            node.id(),
            node.kind(),
            node.status()
        );
    }
    for diagnostic in compilation.diagnostics().items() {
        eprintln!("{}", diagnostic.render_text());
    }
    if let Some(plan) = compilation.image_plan() {
        if check_only {
            println!("check succeeded: {}", plan.target());
        } else {
            println!(
                "bootstrap plan ready: target={}, entry={}, image not emitted",
                plan.target(),
                plan.entry()
            );
        }
    }
}
