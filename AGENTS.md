# 项目协作约定

## Rust 与测试

- 使用 `cargo nextest run --workspace` 运行测试，不使用 `cargo test`；当前 workspace 尚无测试时，可追加 `--no-tests=pass` 让验证命令成功结束。
- 测试可以采用 fixtures 形式；固定输入放在测试专用的 `fixtures/` 目录中，并让测试明确声明使用的 fixture。
- 涉及 workspace 的命令优先使用 `--workspace`，例如 `cargo fmt --all`、`cargo build --workspace` 与 `cargo nextest run --workspace`。
## 代码与规范同步

- 任何源代码变更都必须在同一变更中同步更新 `docs/src/spec/` 的对应规范；只改代码、不改规范的变更不算完成。
- 规范、实现与相关测试必须保持一致；代码行为没有对应规范章节时，先补充相关 `docs/src/spec/` 文档，再完成实现变更。
- 提交前必须检查代码变更与规范变更是否成对出现，并确认 `docs/src/SUMMARY.md` 的导航仍然覆盖新增规范章节。

## 文档提交跟踪

- `docs/.commit` 只保存一个完整的 Git commit SHA，表示最近一次修改文档正文、导航或 ADR 的已提交 commit；尚无已提交文档变更时，初始化为建立跟踪时的 `HEAD`，作为文档基线。
- 文档内容变更提交完成后，必须把该提交的 SHA 写入 `docs/.commit`，再提交一次只更新跟踪标记的变更；只更新 `docs/.commit` 的提交不计为文档内容更新。
- 修改 `docs/` 前先核对 `docs/.commit` 与 `git log -- docs`，确保文档更新基线可追踪；代码与规范同步变更时也必须遵守这项记录流程。

## 文档目录约定

- 根目录 `book.toml` 是 mdBook 配置；书籍源目录固定为 `docs/src`，构建输出固定为 `target/book`，不要把构建产物写入 `docs/`。
- `docs/src/spec/` 存放语言规范，`docs/src/guide/` 存放教程，`docs/src/reference/` 存放参考文档，`docs/src/internals/` 存放实现说明，`docs/adr/` 存放架构决策记录。
- 读取文档前先查看 `docs/` 目录结构与 `docs/src/SUMMARY.md`（若存在），再读取相关章节和邻近文档；复用已有术语与结构，避免重复主题。
- 修改已有文档使用内置编辑工具；新建具体文档必须由任务明确要求，并在 `SUMMARY.md` 存在时同步维护其导航项。
- 规范、教程、参考文档和实现说明分别归档；不要把实现细节写进规范章节，也不要把尚未决定的提案写成现行规范。
- 当前目录只预建目录；不要为了填充目录自动创建空文档、README 或索引页。
