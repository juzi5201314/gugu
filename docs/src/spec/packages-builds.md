# 包、依赖与构建模型

本章规定 Gugu 项目的清单、workspace、package、target、依赖解析、feature、锁文件、构建任务、缓存与 vendor 模型。package 归档、registry 协议、发布、校验和、签名、yank、离线和供应链策略见[发布与生态](publishing-ecosystem.md)。语言模块和可见性见[声明与模块](declarations.md)，闭世界编译和目标镜像见[程序与编译模型](program-model.md)。

Gugu 的项目模型参考 Cargo，依赖下载与编译缓存参考 Go，但不照搬 Rust edition、多 profile、任意 shell build script 或 Go 的模块路径导入。规范中的路径都先按所在清单目录解析，再规范化为不含 `.`、`..` 和符号链接歧义的绝对路径；写入清单或锁文件时必须使用 `/` 分隔的相对路径或规范 URL。

## 术语与层级

层级固定为：

```text
workspace
└── package
    ├── lib / bin / test / bench / example target
    ├── staticlib / cdylib 导出产物
    └── .gg module
```

- **workspace**：共同解析、构建和测试的一组 package，共享一个 `gugu.lock`、一个 workspace `target/` 视图和根级配置。
- **package**：由一个 `gugu.toml` 描述的发布与依赖单元。package 可以有多个 target。
- **target**：一次闭世界编译的入口及产物种类。每个 bin/test/bench/example/C 导出 target 分别形成自己的闭世界。
- **module**：一个 `.gg` 文件。模块路径相对其 target 源码根解析。
- **package ID**：`source + 规范包名 + 精确版本`。path package 的 source 是规范绝对路径，Git package 的 source 包含规范仓库 URL 与锁定 commit，registry package 的 source 是 registry 身份。
- **host graph**：为当前宿主平台编译并执行 `build.gg` 的依赖图。
- **target graph**：为目标名编译进普通程序或测试的依赖图。

外部 package 只有被当前 package 直接声明为依赖后才能在源码中使用；传递依赖不会自动进入名称解析作用域。

## 清单发现与格式

清单文件名固定为小写 `gugu.toml`，锁文件名固定为小写 `gugu.lock`。二者使用 TOML。工具从当前目录向父目录查找最近的 `gugu.toml`；到文件系统根仍未找到时，除显式接收单文件的底层编译命令外，项目命令失败。

`gugu.toml` 不记录 edition、语言版本、最低工具链版本或清单格式版本。源码始终按当前编译器理解的语言规范解释；旧 package 需要自行迁移。编译器完整构建身份仍进入编译缓存 key，但不写入锁文件。清单中未知的核心表或字段是错误；外部工具数据只能放在 `[metadata.<tool>]` 下，核心工具保留但不解释该内容。

一个普通 package 的清单可以写成：

```toml
[package]
owner = "acme"
name = "demo"
version = "0.1.0"
description = "示例程序"
license = "MIT"
repository = "https://example.com/acme/demo"
publish = true
default-run = "demo"
include = ["src/**", "assets/**"]
exclude = ["assets/raw/**"]

[dependencies]
"acme/json" = "1.2"
http = { package = "net/http-client", version = "^2.1", features = ["tls"] }

[test-dependencies]
check = { package = "tools/check", version = "0.4" }

[features]
default = ["cli"]
cli = []
tls = ["http/tls"]

[lib]
path = "src/lib.gg"
artifacts = ["gugu"]

[[bin]]
name = "demo"
path = "src/main.gg"
required-features = ["cli"]
```

`package.name` 是当前 package 的短名；`package.owner` 与短名组成 registry 规范名 `owner/name`。发布到 registry 时二者都必须存在；只供本地 path 使用且 `publish = false` 的 package 可以省略 owner。`package.version` 使用 SemVer 2.0.0；本地未写时为 `0.0.0`，发布时必须显式填写。

owner 和短名分别只能含小写 ASCII 字母、数字和 `-`，长度为 1–64，不能以 `-` 开头或结尾。registry 在自身范围内保证 `owner/name` 唯一。依赖别名必须是合法 Gugu 标识符；未显式写别名时，规范包名最后一段把 `-` 换成 `_` 得到默认别名，冲突时必须显式改名。

`authors`、`description`、`license`、`repository`、`homepage`、`documentation`、`readme`、`keywords`、`categories` 和 `[metadata.<tool>]` 是发布元数据，不影响编译缓存，除非 build.gg 显式读取并登记相应清单输入。`publish = false` 禁止发布。

未写字段时采用以下默认：`publish = true`；所有 `auto-* = true`；lib 的 `artifacts = ["gugu"]`；target 的 `required-features = []`；bench 的 `harness = true`；依赖的 `default-features = true`、`optional = false`。只有一个 bin 时它是隐式 default-run；存在多个 bin 且未写 `default-run` 时，无 target 参数的 run 命令报歧义。没有 owner 的 package 只能作为 path package，不能发布。

## Workspace

根清单可以同时是根 package 和 workspace，也可以只有 `[workspace]` 而成为虚拟 workspace：

```toml
[workspace]
members = ["packages/*", "tools/codegen"]
exclude = ["packages/legacy"]
default-members = ["packages/app"]

[workspace.package]
owner = "acme"
license = "MIT"
repository = "https://example.com/acme/mono"

[workspace.dependencies]
json = { package = "acme/json", version = "1.2" }

[workspace.lints]
unused = "warn"
```

成员只来自 `workspace.members` 展开后减去 `exclude` 的结果。glob 展开按规范相对路径排序；同一路径匹配多次只算一个成员。path 依赖不会自动成为 workspace 成员，workspace 外的 `gugu.toml` 也不会因位于子目录而自动加入。成员路径必须位于 workspace 根之下且最终指向恰好一个清单。

成员可用 `{ workspace = true }` 继承 `[workspace.package]` 字段、`[workspace.dependencies]` 条目和 `[workspace.lints]`。成员不能覆盖已经继承的 source、版本约束或 lint 级别后再声称继承；需要不同值时必须完全写出本地条目。依赖 feature 可以在继承条目上追加，不能删除根条目启用的 feature。

workspace 共享根目录的 `gugu.lock` 和 `target/`。在根执行命令时，若有 `default-members` 就选择它；没有该字段且根清单同时是 package，就只选择根 package；虚拟 workspace 默认选择全部成员。`-p <owner/name>` 或 `--workspace` 覆盖默认选择。package 名称或 target 名称产生歧义时必须要求更完整的选择参数。

根级 `[patch.<registry>]`、workspace 依赖和 lint 只在 workspace 根生效；成员中的对应根级表是错误。语言没有 profile 表。

## 默认源码布局与 Target

在没有显式 target 表或未关闭相应自动发现时，工具按以下路径建立 target：

| 路径 | target |
|------|--------|
| `src/lib.gg` | 唯一普通 lib |
| `src/main.gg` | 名为 package 短名的默认 bin |
| `src/bin/<name>.gg`、`src/bin/<name>/main.gg` | 命名 bin |
| `tests/<name>.gg`、`tests/<name>/main.gg` | 独立 test target |
| `benches/<name>.gg`、`benches/<name>/main.gg` | 独立 bench target |
| `examples/<name>.gg`、`examples/<name>/main.gg` | 独立 example target |
| package 根 `build.gg` | 唯一 host build task |

同名的文件形式与目录形式同时存在是错误。`auto-lib`、`auto-bins`、`auto-tests`、`auto-benches`、`auto-examples` 和 `auto-build` 可以分别关闭自动发现。显式 target 路径相对 package 根；两个 target 不能拥有同一 target 名与种类。

`[lib]` 最多一个，默认名是 package 短名把 `-` 换成 `_`。`artifacts = ["gugu"]` 表示它作为其它 Gugu package 的普通依赖入口；还可以包含 `staticlib`、`cdylib`，把该 lib 中的 `pub extern "C"` 导出写成平台静态库或共享库。`staticlib` / `cdylib` 只稳定 C ABI，不形成可在运行时加载新 Gugu 类型的动态库。

`[[bin]]`、`[[test]]`、`[[bench]]`、`[[example]]` 至少接受 `name`、`path`、`required-features`。Test target 始终使用语言内建测试 harness。Bench target 默认 `harness = true` 并收集 `#[bench]`；可以显式写 `harness = false`，此时它是必须提供 main 的普通 benchmark 可执行程序。Example 是普通可执行入口，只有显式选择或相应工具命令才运行。

普通 bin/example 和 `harness = false` 的 bench 必须提供合法 main；test 与默认 bench 不调用用户 main。bin/test/bench/example 可以使用同 package lib 的公开 API，但不能借此访问 lib 的模块私有项。内联 `#[test]` / `#[bench]` 仍可以访问其所在模块的私有项。具体 harness 见[测试](testing.md)。

## 依赖声明与别名

依赖有三种语义范围：

- `[dependencies]`：普通 target graph，可传递进入依赖闭世界。
- `[test-dependencies]`：只进入 test、bench 和 example 的 test graph，不传播给当前 package 的消费者。
- `[build.dependencies]`：只为宿主平台编译 `build.gg`，不进入任何 target graph 或最终镜像。

目标条件依赖写在 `[target.'cfg(...)'.dependencies]` 和 `[target.'cfg(...)'.test-dependencies]`；build task 的宿主条件依赖写在 `[build.target.'cfg(...)'.dependencies]`。前两者按目标名求值，最后一项按宿主目标名求值。条件中的 cfg 名称必须来自规范目标键，不能由同一 package 的 build.gg 反向改变依赖解析。

Registry 依赖：

```toml
[dependencies]
"acme/json" = "1.2"
json = { package = "acme/json", version = "~1.2", registry = "private" }
codec_v2 = { package = "acme/codec", version = "^2", default-features = false, features = ["simd"] }
```

Path 依赖：

```toml
[dependencies]
core = { path = "../core" }
core_dev = { package = "acme/core", version = "1.4", path = "../core" }
```

Git 依赖：

```toml
[dependencies]
parser = { git = "https://example.com/acme/parser.git", rev = "8f3c1a2b4d5e6f708192a3b4c5d6e7f8091a2b3c" }
parser_dev = { git = "https://example.com/acme/parser.git", branch = "next" }
```

`version`、`path`、`git` 是 source 选择；`path`/`git` 可以同时带 registry `package + version` 作为发布回退。`git` 的 `rev`、`tag`、`branch` 最多写一个；锁文件总是记录解析后的完整 commit 与规范 tree hash。发布归档允许 Git 依赖，但必须显式写不可变 `rev`；tag、branch 和裸仓库默认分支在发布时是错误。Path 依赖发布时必须同时存在 registry package/version 回退，打包后移除本地 path 覆盖。

同一个 package ID 可以被多个别名引用，但只解析和编译一次。依赖图中同一 owner/name 的不兼容版本可以并存；源码若要直接使用多个版本，必须给每个版本不同别名。别名只改变名称解析入口，不改变 package ID、类型身份或锁文件节点。

package 依赖图、host build graph 和 target graph 都禁止循环。测试依赖不能反向成为普通依赖，build 依赖不能引用正在构建的 target 产物。

## SemVer Resolver

版本要求采用 Cargo 风格的 SemVer：

- `1.2.3` 等价 caret `^1.2.3`。
- 支持 `^`、`~`、`>=`/`>`/`<=`/`<`、逗号交集、`=精确版本` 和 `*`/`1.*` wildcard。
- `0.x` 的兼容边界按最左侧非零分量确定。
- 预发布版本只有要求显式包含兼容预发布标识时才作为候选。
- build metadata 不参与版本排序或兼容判断。

Resolver 对每个 source + owner/name 优先选择满足全部兼容约束的最高非撤回版本，并尽量统一兼容要求。若同一兼容范围的要求无法统一则报错；互不兼容的版本范围可以得到多个 package ID。选择顺序、回溯顺序和 registry 返回顺序不能改变最终解析图。

Registry 的 yanked 版本不参与新解析；若已存在于有效锁文件中则可以继续使用，显式 update 会尝试移出。精确版本要求也不能绕过新解析的 yanked 限制。具体 yank API、警告和严格策略见[发布与生态](publishing-ecosystem.md)。

workspace 根可用 `[patch.<registry>]` 将某个 registry package 临时替换为 path、Git 或另一 registry source。patch 参与整个 workspace 解析并写入锁；成员不能定义 patch。不提供 `replace` 或 resolver 版本开关。

## Feature

Feature 是 package 自己定义的、只能增加能力的命名集合：

```toml
[features]
default = ["json"]
json = ["dep:serde"]
full = ["json", "http/tls"]

[dependencies]
serde = { package = "data/serde", version = "1", optional = true }
http = { package = "net/http", version = "2", default-features = false }
```

Feature 名只能含小写 ASCII 字母、数字、`_`、`-`，不能以分隔符开头或结尾。`default` 是普通保留 feature；依赖未写 `default-features = false` 时自动启用。`dep:alias` 启用可选依赖而不把该依赖名隐式暴露成同名 feature；未被任何 `dep:` 使用的 optional 依赖自动提供同名 feature。`alias/feature` 启用依赖 feature。

Feature 必须 additive：启用 feature 不能删除 API、禁用依赖或改变同一 API 的类型；互斥 feature 组合是 package 设计错误，resolver 不提供“最后一个获胜”。源码用 `#[cfg(feature = "name")]` 检查当前 package feature。

同一 package ID 分三域统一 feature：目标普通依赖域、目标 test/bench/example 域、宿主 build.gg 域。每个域内对所有选择 target 取并集；三域之间不泄漏，不同目标名之间也不统一。feature 集与目标名进入编译缓存 key。

## 锁文件

workspace 使用根目录的一个 `gugu.lock`。普通解析命令采用 Cargo 行为：锁不存在时创建；清单约束变化、选择 target/feature 需要新节点或锁中节点不再合法时，普通 build/check/test 可以自动更新。`--locked` 要求命令不得改变锁，`--offline` 禁止网络，`--frozen` 同时等价于 locked + offline。命令行参数、输出格式与退出码见[工具链与命令行](toolchain-cli.md)。

含 bin、staticlib 或 cdylib 的应用 workspace 应把锁文件提交版本控制。只有普通 lib 的 workspace 可以不提交，但本地构建仍可生成锁；发布归档默认不包含锁。该规则是发布与协作约定，不改变 resolver 算法。

锁文件稳定记录：

- package ID、精确版本和 source；
- registry package 的 package checksum（见[发布与生态](publishing-ecosystem.md)）；
- Git commit 与规范 tree hash；
- 已解析的依赖边及其 normal/test/build 范围；
- target 条件；
- 三个 feature 统一域的结果；
- patch 后实际 source。

节点与边按 package ID 和规范字段顺序稳定排序。锁不记录编译器版本、宿主绝对路径、workspace path 内容哈希、构建缓存位置或 strip 状态。Path source 写 workspace 相对规范路径；移出 workspace 的 path 写相对当前 package 的规范路径，不能写机器绝对路径。

锁内容与清单、package checksum、Git tree 或 package 自身清单不一致时是解析错误。工具不能在 `--locked` 下悄悄改正。

## 单一编译流水线

Gugu 没有 dev/release/test/bench profile，没有自定义 profile，也没有 `--release`。所有普通 target 使用同一套稳定默认优化流水线：编译器执行常规优化、生成运行时所需源位置和行号信息，并按当前编译器固定策略选择内联、LTO 等内部参数。清单不能覆盖依赖的优化级别或 codegen 配置。

test 与 bench 是正交 target/harness 模式，不是 profile。实现还可以提供 race、coverage 等插桩，但它们不能成为源码 cfg 或改变用户 API。源码只能观察 `cfg(test)` 和 `cfg(bench)`；test/bench 与任何插桩选择都进入编译缓存 key。

`--strip` 是布尔参数，只对已经生成的最终 ELF/PE/静态库执行镜像后处理；它删除调试节、非导出符号和其它不可观察元数据，不触发重新解析、类型检查、单态化或 codegen。C 导出、展开信息、GC 栈图、`std.src` 和 panic 位置等语言可观察或运行时必需数据不能删除。strip 前后共享同一编译缓存，只产生两个末端镜像 action。

运行时整数语义不因构建命令改变：加、减、乘、整数负号、有符号左移和显式整数变窄都按目标位宽二进制补码环绕或截断。除零仍 panic；`MIN / -1` 结果为 `MIN`，余数为 0。负移位量 panic；非负移位量先按位宽取模。comptime 溢出仍是编译错误。

## 单一 build.gg

package 根存在 `build.gg` 时，它是唯一 build task。没有该文件且未显式声明 build 路径时，不存在 build task。build.gg 为宿主目标名编译，使用 `[build.dependencies]`，在任何 target 源码解析前执行；它必须提供：

```text
fn main() Result[(), string]
```

返回 `Err`、panic 或违反输出协议都会使 package 构建失败。build task 可以使用完整 Gugu、unsafe、FFI 和宿主标准库；权限门不是安全沙箱，低层代码可以绕过它。

`std.build` 提供类型化构建接口，至少包括：

```text
struct RunOptions {
    cwd: string
    env: Vec[(string, string)]
    inherit_env: bool
}

struct ProcessOutput {
    code: int
    stdout: Vec[byte]
    stderr: Vec[byte]
}

fn out_dir() string
fn host() string
fn target() string
fn features() &[string]
fn rerun_if_changed(path_or_glob: string)
fn rerun_if_env_changed(name: string)
fn rerun_always()
fn emit_module(name: string, path: string)
fn define_cfg(name: string)
fn define_cfg_value(name: string, value: string)
fn link_library(kind: string, name: string, path: string)
fn link_search(path: string)
fn warning(message: string)
fn error(message: string) !
fn run(executable: string, args: &[string], options: RunOptions) ProcessOutput
```

构建信息通过这些类型化 API 传递，不解析 stdout 魔法指令。`emit_module` 的文件必须位于 `out_dir()` 下，并以保留根 `generated.<name>` 加入当前 package；同名用户模块或重复 emit 是错误。`link_library` 的 kind 只能是 `"static"` 或 `"dynamic"`，最终仍由 Gugu 编译器写 ELF/PE，不调用系统链接器。`define_cfg*` 不能覆盖内置 cfg、feature 或另一不同值，并进入缓存 key。`run` 捕获 stdout/stderr，进程非零退出不会自动 panic，由 build.gg 根据 `code` 决定；无法启动进程则 panic。

build.gg 首次构建必定执行。执行期间调用 `rerun_if_changed` / `rerun_if_env_changed` 记录下一次重跑条件；任一匹配输入变化、build.gg/清单/build 依赖变化、宿主/目标/feature 变化或 `rerun_always` 都使任务失效。一次成功执行没有登记任何 rerun 条件时，默认 package 归档范围内任一文件变化都重跑，与 Cargo build.rs 一致。

普通文件读取、run、网络、时间或随机不会自动成为重跑条件；任务作者必须用 rerun API 声明能代表外部结果的文件/环境，或使用 `rerun_always()`。漏声明导致旧生成物被复用是 build task 的错误。未失效时直接复用记录的生成文件与构建元数据，不执行 build.gg，也不发生权限询问。

`run` 直接执行规范化 executable + argv，不经过 shell。调用记录 executable、argv、cwd 和显式环境供权限门显示，但不会自动追踪子进程读取的文件、网络或其后代进程。获准的进程及后代拥有当前用户的完整宿主权限。

build.gg 不能直接调用 `std.process.Command` 或 `ShellCommand`；外部进程只能通过 `std.build.run` 启动，使 executable、argv、cwd、环境、权限请求和重跑声明都进入同一 action 记录。需要 shell 时，build task 必须把 shell 本身作为 executable、把脚本作为显式 argv；不存在不透明的 build shell 捷径。

## 构建任务权限门

权限门默认关闭；关闭时 build.gg 的文件、环境、网络和进程操作全部放行，行为与普通本地程序相同。`--permission` 开启规则检查：

- 已有匹配的永久授权则放行；
- 交互 TTY 中没有匹配项时显示 package 身份、build.gg 内容哈希、操作、路径/主机或 executable+argv，并询问；
- 同意后永久记录，不提供“仅本次”或“本次会话”；
- 非交互环境中没有匹配项立即失败；
- `-A` 显式允许全部操作；
- `--read-allows`、`--write-allows`、`--env-allows`、`--net-allows`、`--run-allows` 可以预先提供精确或 glob 规则。

永久授权绑定到 package source、owner/name、version、registry 归档 checksum 或 Git tree/path 身份、build.gg 内容哈希和规范化操作。版本、source、checksum、build.gg 或被批准操作变化后不匹配旧授权，必须重新确认。拒绝只使当前构建失败，不写永久 deny。

`--run-allows` 同时匹配规范化 executable 路径 glob 与 argv 各位置的 glob；规则缺少 argv 部分时只授权 executable，不限制参数。Windows 路径比较使用规范大小写与分隔符，Unix 按字节区分大小写。TUI 始终显示最终解析路径和 argv 数组，不把参数拼成 shell 字符串。

该机制完全是 advisory 规则检查，不是 OS sandbox：build.gg 可以通过 unsafe、FFI、直接 syscall、已获准进程或修改工具状态绕过；`-A` 也允许写任意宿主位置，包括缓存。工具必须在界面和文档中明确这一点，不能把 `--permission` 描述为执行不可信依赖的安全边界。

永久授权放在用户配置目录，不写 workspace、锁文件或 package 归档。授权与否不进入编译缓存 key；它只决定本次需要执行 build.gg 时某个操作能否发生。

## 缓存与 Target 视图

工具规范两个全局缓存，内部目录布局不是稳定 API：

1. **依赖源码缓存**：保存 registry 元数据、不可变归档、解包源码、Git commit/tree 与校验信息。多个 workspace 和并发进程共享。
2. **编译 action cache**：保存可复用的解析、类型检查、生成物、目标代码或完整镜像 action。规范不规定内部按模块、package、单态化还是镜像分层。

编译 action key 必须完整覆盖会改变输出的输入：编译器构建身份、宿主与目标名、target 种类、test/bench/插桩模式、feature 域、完整锁图、所有可达源码与 `embed_file`、源码宏展开闭包与生成文本、宏预算属性、comptime capability registry、冻结 type universe 与被消费的 late 常量、跨 package 公共分析摘要对象键、build.gg 记录输入和输出内容、cfg、native 链接元数据。公共分析摘要的生产 action 必须因实现输入变化而重算；若重算后的可消费摘要相同，其内容对象键保持不变，依赖 action 不继续失效。绝对 workspace 路径若不影响可观察输出不得进入 key；`std.src.file` 使用 package 相对规范路径以允许跨目录命中。

Linux 默认使用 `$XDG_CACHE_HOME/gugu`（未设置时 `~/.cache/gugu`）、`$XDG_CONFIG_HOME/gugu`、`$XDG_DATA_HOME/gugu`；Windows 使用对应 LocalAppData/RoamingAppData/Known Folder。`GUGU_CACHE_DIR`、`GUGU_CONFIG_DIR`、`GUGU_DATA_DIR` 可以覆盖。首版只规范本地缓存，不定义远程缓存协议。

依赖源码与归档默认不会自动淘汰，只由显式 cache clean/gc 删除，以保证离线可用。编译 action cache 使用可配置容量和最近使用 LRU 自动回收；实现必须保证正在读取或写入的 entry 不被回收。并发写以临时文件、内容哈希验证和原子发布完成；损坏或摘要不符的 entry 必须隔离并重新构建。

workspace `target/` 只呈现用户产物、build.gg 生成物和可读日志，不是唯一缓存来源：

```text
target/
├── <target>/
│   ├── bin/
│   ├── lib/
│   ├── tests/
│   ├── benches/
│   └── examples/
├── generated/
└── build-logs/
```

工具可以用硬链接、reflink 或复制从全局缓存物化 target 视图，行为不可观察。`GUGU_TARGET_DIR` 或命令行可以覆盖 workspace target 目录。`gugu clean` 默认只清当前 workspace target；显式 cache 子命令清全局依赖或编译缓存。

## Registry、校验与镜像

Registry 依赖使用稀疏 HTTPS 索引；每条版本记录至少提供 owner/name、SemVer、yanked 状态、package checksum、依赖要求、feature 和发布元数据。registry source identity 是 package ID 的一部分，不同 registry 的同名同版本 package 不能合并。协议版本、索引记录字段、发布 API、不可变归档和可选签名见[发布与生态](publishing-ecosystem.md)。

锁文件记录精确 registry source、版本和 package checksum。工具在解包、缓存命中、编译输入和 vendor 物化前验证 checksum；记录不符时隔离损坏 entry，`--offline` 下直接失败。首版不依赖公共 checksum transparency database。

依赖源可以配置默认 registry、alternate registry、镜像与私有 registry。镜像只替换同一 source 的网络位置，不能改变 source identity、版本、yanked 状态、签名或 checksum。配置、allowlist、认证和重定向规则见[发布与生态](publishing-ecosystem.md)。

Git source 规范化 URL，锁定 commit 与 tree hash。发布 package 的 Git 依赖必须指定不可变 `rev`；私有 Git 认证由用户环境或凭据配置提供，不能写入锁文件、package 归档或可导出的构建缓存元数据。

## Vendor 与离线

`gugu vendor` 严格从当前锁图生成 workspace `vendor/`，复制实际选中的 registry/Git package，并写校验元数据和 source 映射。vendor 树默认只读，不包含 path package、其它 package 的 vendor、VCS 元数据或构建缓存。

存在 `vendor/` 不会自动改变解析。只有 `--vendor` 或 workspace 本地配置显式启用时才从 vendor 读取；vendor 节点、checksum、清单或锁图不一致时失败，不回退网络。`--offline` 禁止 registry、镜像、Git 和其它工具链网络访问，只允许已校验全局缓存或显式 vendor；完整模式组合和 `std.build` 网络边界见[发布与生态](publishing-ecosystem.md)。

`--frozen` 等价于同时启用 `--locked` 与 `--offline`，但不自动启用 vendor。

## 打包与发布

`gugu package` 生成发布归档、文件清单和 package checksum；默认排除 target、vendor、VCS 数据、`gugu.lock`、临时文件、未声明生成物和凭据。归档清单、路径安全、规范内容流和 checksum 算法见[发布与生态](publishing-ecosystem.md)。

`gugu publish` 在隔离归档视图中重新验证清单、target、依赖 source、Git rev、feature、build 输入和文档，然后向 registry API 上传不可覆盖的 package。`gugu yank` 只改变 registry 的 yanked 状态，不删除归档或改变 checksum；已有有效锁图仍可使用被 yank 版本。发布、签名和撤回的完整 HTTP 契约见[发布与生态](publishing-ecosystem.md)。

## 错误与确定性

下列情况必须在构建目标代码前失败：清单 TOML 错误、未知核心字段、workspace 成员冲突、target 重名、依赖循环、SemVer 无解、锁不一致、checksum 不符、缺少 feature、build.gg 失败、权限门拒绝、vendor 不完整、offline 缺包、发布归档不自足。

同一编译器身份、相同 target/插桩/feature、相同锁图、相同规范源码和相同 build 输出必须产生语义等价产物。Registry 返回顺序、workspace glob 枚举顺序、缓存是否命中、target 物化方法、权限授权存储顺序和本机绝对路径不能改变解析图或程序语义。
