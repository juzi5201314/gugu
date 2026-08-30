# 项目协作约定

## Rust 与测试

- 使用 `cargo nextest run --workspace` 运行测试，不使用 `cargo test`；当前 workspace 尚无测试时，可追加 `--no-tests=pass` 让验证命令成功结束。
- 测试可以采用 fixtures 形式；固定输入放在测试专用的 `fixtures/` 目录中，并让测试明确声明使用的 fixture。
- 涉及 workspace 的命令优先使用 `--workspace`，例如 `cargo fmt --all`、`cargo build --workspace` 与 `cargo nextest run --workspace`。
## 文档目录约定

- 根目录 `book.toml` 是 mdBook 配置；书籍源目录固定为 `docs/src`，构建输出固定为 `target/book`，不要把构建产物写入 `docs/`。
- `docs/src/spec/` 存放语言规范，`docs/src/guide/` 存放教程，`docs/src/reference/` 存放参考文档，`docs/src/internals/` 存放实现说明，`docs/adr/` 存放架构决策记录。
- 读取文档前先查看 `docs/` 目录结构与 `docs/src/SUMMARY.md`（若存在），再读取相关章节和邻近文档；复用已有术语与结构，避免重复主题。
- 修改已有文档使用内置编辑工具；新建具体文档必须由任务明确要求，并在 `SUMMARY.md` 存在时同步维护其导航项。
- 规范、教程、参考文档和实现说明分别归档；不要把实现细节写进规范章节，也不要把尚未决定的提案写成现行规范。
- 当前目录只预建目录；不要为了填充目录自动创建空文档、README 或索引页。
