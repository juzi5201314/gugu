# CLI 命令

`gugu` 是唯一入口（`gugu-gc` 等内部工具按 -Z 开关暴露）。完整参数表与输出协议见[工具链与命令行](../spec/toolchain-cli.md)。

## 子命令速查

| 命令 | 用途 |
| --- | --- |
| `gugu new <path>` / `gugu init` | 建包骨架（init 不覆盖已有入口） |
| `gugu build` | 编译并物化到 `target/<目标>/` |
| `gugu check` | 只检查，不物化 |
| `gugu run [target] [-- args]` | 编译并执行（宿主目标） |
| `gugu test` / `gugu bench` | test/bench 域（宿主目标） |
| `gugu fmt` | 格式化（`--check` 只报差异） |
| `gugu doc [--open] [--no-deps]` | 生成 HTML API 文档到 `target/doc/` |
| `gugu clean [--cache|--registry|--all]` | 默认只删 `target/` |
| `gugu add` / `remove` / `update` / `tree` / `vendor` | 依赖管理 |
| `gugu package` / `publish` / `yank` / `login` | 发布流 |
| `gugu cache dir|clean|gc|verify` | action cache 管理 |
| `gugu explain <code>` | 解释诊断码/lint 名 |
| `gugu version` / `gugu help` | 版本与帮助 |

## 常用全局参数

`--format text|json|json-diagnostic-short`、`--offline`、`--locked`、`--frozen`、`--vendor`、`--require-signature`、`--deny-yanked`、`--target <目标>`、`-p owner/name`、`--workspace`、`--lib`/`--bin`/`--test`/`--bench`/`--example`/`--all-targets`、`--features`、`--strip`、`--permission` 与 `--read-allows` 等 build.gg 预授权、`--config`、`--cache-dir`、`--target-dir`、`-Z`。

优先级：命令行 > 环境变量 > workspace 配置 > 用户配置 > 内置默认。

## 输出协议

`--format json` 按行输出 JSONL 事件（诊断含 `code`/`severity`/span/notes）；`json-diagnostic-short` 只输出一条诊断。具体 schema 见[工具链与命令行](../spec/toolchain-cli.md)。
