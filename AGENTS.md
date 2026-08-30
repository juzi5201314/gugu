# 项目协作约定

## Rust 与测试

- 使用 `cargo nextest run --workspace` 运行测试，不使用 `cargo test`；当前 workspace 尚无测试时，可追加 `--no-tests=pass` 让验证命令成功结束。
- 测试可以采用 fixtures 形式；固定输入放在测试专用的 `fixtures/` 目录中，并让测试明确声明使用的 fixture。
- 涉及 workspace 的命令优先使用 `--workspace`，例如 `cargo fmt --all`、`cargo build --workspace` 与 `cargo nextest run --workspace`。
