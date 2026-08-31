# 工具链与命令行

本章规范 Gugu 工具链的命令行契约：可执行入口、子命令、参数、输出格式、退出码、配置文件与环境变量。语言语义见其它章节；本章只规范用户与工具链之间的接口。

## 可执行入口

工具链只有一个可执行文件 `gugu`。所有功能都通过子命令暴露；不存在 `guguc`、`gugu-fmt` 等独立可执行文件。`gugu` 是单一静态可执行文件，不依赖运行时安装器、组件管理器或外部插件。

`gugu` 无子命令时等价于 `gugu help`。`gugu --version` 与 `gugu version` 等价。

## 全局参数

以下参数对所有子命令有效，除非子命令明确禁止：

| 参数 | 含义 |
| --- | --- |
| `--format <text\|json\|json-diagnostic-short>` | 输出格式，默认 `text` |
| `--color <auto\|always\|never>` | 颜色输出，默认 `auto` |
| `-q`、`--quiet` | 只输出错误与最终结果 |
| `-v`、`--verbose` | 输出详细进度与缓存命中信息 |
| `--offline` | 禁止网络访问 |
| `--locked` | 要求锁文件已是最新，否则失败 |
| `--frozen` | 等价于 `--locked --offline` |
| `--vendor` | 从 workspace `vendor/` 读取依赖 |
| `--target <target>` | 目标名，见[平台与 ABI 参考](platform-abi.md)，默认宿主 |
| `-p <owner/name>`、`--package <owner/name>` | 选择 package |
| `--workspace` | 选择整个 workspace |
| `--lib` | 选择 lib target |
| `--bin <name>` | 选择 bin target |
| `--test <name>` | 选择 test target |
| `--bench <name>` | 选择 bench target |
| `--example <name>` | 选择 example target |
| `--all-targets` | 选择所有 target |
| `--features <list>` | 启用 feature，逗号分隔 |
| `--no-default-features` | 禁用默认 feature |
| `--all-features` | 启用所有 feature |
| `--strip` | 对最终镜像执行 strip |
| `--permission` | 启用 build.gg 权限门 |
| `-A` | 允许 build.gg 全部操作 |
| `--read-allows <glob>` | 预授权读路径 |
| `--write-allows <glob>` | 预授权写路径 |
| `--env-allows <glob>` | 预授权环境变量 |
| `--net-allows <glob>` | 预授权网络主机 |
| `--run-allows <glob>` | 预授权进程执行 |
| `--config <path>` | 追加配置文件 |
| `--cache-dir <path>` | 覆盖缓存目录 |
| `--target-dir <path>` | 覆盖 target 目录 |

参数优先级：命令行参数 > 环境变量 > workspace 本地配置 > 用户配置 > 内置默认。

## 子命令

### `gugu new <path>`

创建新 package 或 workspace。`--lib` 创建 lib package，`--bin` 创建 bin package（默认），`--workspace` 创建 workspace 根。生成的 `gugu.toml` 只含必要字段。

### `gugu init`

在当前目录初始化 package 或 workspace，语义同 `new`，但不创建目录。

### `gugu build`

编译选中 target。无 target 选择器时，默认选择当前 package 的 lib 与所有 bin；在 workspace 根且无 `-p` 时，选择 `default-members` 或全部成员。

`gugu build <file.gg>` 是单文件编译入口：绕过 package 模型，直接把该文件作为闭世界根编译。此时 `-p`、`--workspace`、`--features`、`--lib`、`--bin`、`--test`、`--bench`、`--example`、`--all-targets` 非法。`--target`、`--strip`、`--offline` 仍有效。

### `gugu check`

只执行解析、类型检查与代码生成前的全部检查，不生成最终镜像。比 `build` 快，用于 IDE 与快速反馈。target 选择规则同 `build`。

### `gugu run [target] [args...]`

编译并运行 bin target。无 target 时按 `default-run` 或唯一 bin 选择；多个 bin 且无 `default-run` 是错误。`--` 之后的参数原样传给程序。

### `gugu test`

编译并运行 test target。支持 `--lib`、`--test <name>`、`--all-targets`。`-- <args>` 传给测试 harness。

### `gugu bench`

编译并运行 bench target。target 选择规则同 `test`。

### `gugu fmt`

格式化源码。`--check` 只检查不写入，差异时退出码非 0。`--all` 格式化整个 workspace。

### `gugu doc`

生成文档。`--open` 在浏览器打开。`--no-deps` 只生成当前 package。

### `gugu clean`

删除 workspace `target/`。`--cache` 删除全局编译缓存；`--registry` 删除 registry 缓存；`--all` 删除全部缓存。

### `gugu add <package>`

向 `gugu.toml` 添加依赖。`--dev` 加入 `[test-dependencies]`，`--build` 加入 `[build.dependencies]`。`--features`、`--default-features`、`--optional`、`--path`、`--git`、`--rev`、`--branch`、`--tag` 控制依赖属性。

### `gugu remove <package>`

从 `gugu.toml` 移除依赖。

### `gugu update`

更新锁文件。`-p <owner/name>` 只更新指定 package；`--precise <version>` 指定精确版本。

### `gugu tree`

打印依赖树。`--depth <n>` 限制深度；`--duplicates` 只显示重复版本；`--invert <package>` 显示反向依赖。

### `gugu vendor`

生成 `vendor/` 目录，见[包、依赖与构建模型](packages-builds.md)。

### `gugu package`

生成发布归档，见[包、依赖与构建模型](packages-builds.md)。

### `gugu publish`

发布到 registry。`--dry-run` 只验证不上传；`--token <token>` 提供凭据；`--registry <name>` 选择 registry。

### `gugu login`

保存 registry 凭据到用户配置。`--registry <name>` 选择 registry。

### `gugu cache`

缓存管理子命令：

- `gugu cache clean`：清空编译缓存
- `gugu cache gc`：按 LRU 回收编译缓存
- `gugu cache verify`：校验缓存完整性
- `gugu cache dir`：打印缓存目录路径

### `gugu explain <code>`

解释诊断码或 lint 名。

### `gugu version`

打印版本信息，见下文。

### `gugu help [command]`

打印帮助。`gugu <command> --help` 等价。

## 单文件编译模式

`gugu build <file.gg>` 与 `gugu check <file.gg>` 进入单文件模式：

- 文件必须是合法模块根，可以引用同目录或子目录的其它 `.gg` 文件，形成闭世界。
- 不读取 `gugu.toml`，不解析依赖，不启用 feature，不执行 build.gg。
- 输出到当前目录或 `--target-dir` 指定目录。
- 诊断仍使用统一格式。

该模式用于脚本、示例与底层调试，不用于多 package 项目。

## 输出格式

### text

默认格式，面向人读。诊断包含文件、行列、严重级别、消息与可选建议。颜色遵循 `--color`。

### json

NDJSON，每行一个 JSON 对象。信封：

```json
{ "reason": "compiler-diagnostic", "payload": { ... } }
```

`reason` 是稳定字符串枚举，至少包括：

- `compiler-diagnostic`：编译诊断
- `compiler-artifact`：生成物路径
- `build-start` / `build-finish`：构建阶段
- `test-start` / `test-result` / `test-finish`：测试事件
- `bench-result`：bench 测量
- `package-resolve`：依赖解析结果

`payload` 字段按 reason 定义，字段可增不可删。诊断 payload 至少含 `file`、`line`、`column`、`severity`、`code`、`message`、`suggestion`。

### json-diagnostic-short

只输出诊断，每条一行，省略 `build-start` 等进度事件。用于 IDE 快速采集。

## 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 编译错误、测试失败、目标操作失败 |
| 2 | 命令行用法错误：未知参数、歧义 target、找不到清单、非法参数组合 |
| 101 | 工具链内部错误（panic、断言失败、不可恢复 I/O） |

脚本可以用退出码区分「代码有错」与「参数有错」。

## 配置文件

配置文件为 TOML，路径按优先级：

1. `--config <path>` 显式指定
2. workspace 根 `.gugu/config.toml`
3. 用户配置目录 `config.toml`（见[包、依赖与构建模型](packages-builds.md)的目录规则）

后加载的覆盖先加载的；同一文件内后出现的键覆盖先出现的。

支持的键：

```toml
[build]
target = "x86_64-linux"
jobs = 8

[cache]
dir = "/path/to/cache"
max-size = "10GiB"

[registry]
default = "https://registry.example.com"
token = "..."  # 不推荐，优先用 gugu login

[permission]
enabled = true
read-allows = ["/usr/include/**"]
```

环境变量覆盖配置文件：`GUGU_BUILD_TARGET`、`GUGU_CACHE_DIR`、`GUGU_REGISTRY_DEFAULT` 等，命名规则为 `GUGU_` + 配置路径大写、点转下划线。

## 环境变量

| 变量 | 含义 |
| --- | --- |
| `GUGU_CACHE_DIR` | 覆盖缓存根目录 |
| `GUGU_CONFIG_DIR` | 覆盖配置目录 |
| `GUGU_DATA_DIR` | 覆盖数据目录 |
| `GUGU_TARGET_DIR` | 覆盖 workspace target 目录 |
| `GUGU_BUILD_TARGET` | 默认目标名 |
| `GUGU_REGISTRY_DEFAULT` | 默认 registry URL |
| `GUGU_OFFLINE` | 非空等价 `--offline` |
| `GUGU_LOCKED` | 非空等价 `--locked` |
| `GUGU_FORMAT` | 默认输出格式 |
| `GUGU_COLOR` | 默认颜色策略 |

`GUGU_RUNTIME_*`、`GUGU_BACKTRACE` 和运行时诊断变量由已编译程序的 rt0 读取，不是工具链配置，不参与依赖解析或编译缓存 key。工具链只负责在 `gugu run` 中传递它们；具体变量和优先级见[运行时与运维语义](runtime.md)。

## `gugu version` 输出

```text
gugu 0.1.0 (commit 8f3c2a1 2026-08-31)
host: x86_64-linux
llvm: 19.1.0
```

`--format json` 时：

```json
{
  "version": "0.1.0",
  "commit": "8f3c2a1",
  "commit-date": "2026-08-31",
  "host": "x86_64-linux",
  "llvm": "19.1.0"
}
```

版本字符串与 commit 共同构成编译器构建身份，进入编译缓存 key。

## 与语言规范的交叉引用

- 诊断位置格式见[概述](overview.md)。
- lint 级别与 `#[allow]` 等属性见[词法结构](lexical.md)。
- target 种类、feature、锁文件、缓存、vendor、发布见[包、依赖与构建模型](packages-builds.md)。
- 目标名与平台 ABI 见[平台与 ABI 参考](platform-abi.md)。
- 测试 harness 与 bench 见[测试](testing.md)。
- `std.env.args`、`std.env.vars` 与运行时控制环境见[标准库](standard-library.md)和[运行时与运维语义](runtime.md)。
