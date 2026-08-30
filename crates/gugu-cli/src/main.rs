use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "gugu-cli", version, about = "gugu 命令行工具")]
struct Cli;

fn main() {
    let _cli = Cli::parse();
    println!("Hello, world!");
}
