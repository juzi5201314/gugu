# 发布与生态

本章规定 Gugu registry 的协议、package 归档与发布、SHA-256 校验、可选签名、版本撤回、离线构建、镜像、vendor 和供应链边界。package、workspace、依赖解析、feature、锁文件和构建任务的基础模型见[包、依赖与构建模型](packages-builds.md)；本章把其中涉及 registry 的约束收敛为可互操作的协议。

Gugu 的默认信任模型是 **HTTPS + package SHA-256 + 已提交的 `gugu.lock`**。签名是可选的附加证明，不在默认解析路径中替代校验和或锁文件；需要更强策略的组织可以要求受信任签名，但这属于用户配置的供应链门，不改变 package ID 和版本解析语义。

## 术语与不变量

- **registry**：为 `owner/name@version` 提供索引、归档下载和发布 API 的服务。registry 的规范身份是规范化后的索引 URL。
- **package ID**：`registry identity + owner/name + exact SemVer`。不同 registry 的同名同版本 package 永远是不同 package ID。
- **index record**：registry 为一个精确版本保存的 JSON 记录，包含依赖、feature、归档摘要和撤回状态。
- **package archive**：由 `gugu package` 生成、可被独立重新解析的 package 归档。归档不携带 workspace 锁、用户凭据或构建缓存。
- **package checksum**：对归档规范内容流计算的 SHA-256；它不是 URL、压缩时间戳或本地路径的摘要。
- **yank**：registry 侧把已发布版本标记为不供新的依赖解析选择。被 yank 的归档和索引记录仍可下载。
- **mirror**：替代某个 registry 下载位置的缓存服务；它不产生新的 package source 身份。
- **vendor**：由当前锁图物化的、可离线验证的依赖源码树。

以下不变量适用于所有 registry：

1. 已成功发布的 `owner/name@version` 不得被另一个归档、清单、依赖图或校验和覆盖。
2. registry 不得删除已发布版本；不再推荐使用时只能执行 yank。
3. 解析器、镜像和缓存都必须把 registry identity 纳入 package ID，不能按短名或 URL 的显示别名合并 package。
4. 归档在解包或进入编译缓存前必须通过 package checksum 验证；验证失败不得继续构建。
5. registry 服务端不执行归档中的 Gugu、`build.gg`、测试或任意脚本。

## Registry 配置与身份

### 配置模型

工具链从用户配置读取 registry 定义；workspace 清单只能引用 registry 名称或显式 source，不能写 token、私钥或其它凭据。一个配置可以写成：

```toml
[registry]
default = "public"
allowed = ["public", "internal"]
require-signature = false
deny-yanked = false

[registries.public]
index = "https://packages.example/index/"
api = "https://packages.example/api/v1"

[registries.internal]
index = "https://packages.example/internal/index/"
api = "https://packages.example/internal/api/v1"
mirror = "https://cache.example/internal/"
```

`registry.default` 是未显式写 `registry` 的 registry 依赖所使用的名称。实现可以把默认 registry 编译进工具链，并允许用户配置覆盖；其最终规范化 URL 仍必须显示在解析诊断和锁文件 source 中。`allowed` 非空时，所有 registry package 使用的 registry 名称必须在列表中；名称解析后的 identity 也必须与该 registry 定义一致。不在列表中的 source 在解析前失败。未配置 `allowed` 表示不额外限制 source，不表示允许不安全协议。`deny-yanked = true` 时，已锁定的 yanked package 也在解析完成后报告错误。

`registries.<name>.index` 是 registry 的规范身份来源，必须是 HTTPS URL，并以 `/` 结尾。URL 按 RFC 3986 解析：主机名转小写，移除默认 HTTPS 端口，路径中的未编码点段消除，片段和用户信息禁止，查询参数禁止。规范化后的完整 URL 写入 package ID 和 `gugu.lock`。同一服务的两个不同规范化 URL 是两个 source，不能由客户端自行判断为同一 registry。

`api` 用于发布和撤回；它必须与 `index` 属于同一 registry 配置。下载地址由 registry 的 `config.json` 提供，不能从清单任意指定。`mirror` 是可选的透明镜像，仅替换索引和归档的网络位置；镜像返回的 package checksum、package ID 和 yanked 状态必须与原 registry 一致。镜像失败不会自动转向另一个未配置的 registry。

`gugu.toml` 中的 registry 依赖可以写逻辑名：

```toml
[dependencies]
json = { package = "acme/json", version = "^1.2", registry = "public" }
```

也可以使用清单约定的默认 registry 省略 `registry`。清单不能把 HTTP、文件路径、任意下载 URL 或 registry token 写成依赖 source。path 和 Git 依赖的发布限制见[发布流程](#发布流程)。

### 凭据

`gugu login <registry>` 把 bearer token 写入用户配置目录；token 不得写入 workspace、`gugu.toml`、`gugu.lock`、归档、诊断事件或编译缓存。工具读取 token 时不得把完整 token 放入错误消息、命令回显、权限提示或 verbose 日志；非交互环境使用 `GUGU_REGISTRY_TOKEN` 时，同样禁止回显并优先使用进程环境而不是文件。

发布、yank 和需要私有索引的读取请求使用：

```text
Authorization: Bearer example-token
```

跨 origin 的 HTTP 重定向不得转发 Authorization header。未认证的公共索引可以不带 header；服务端返回 `401` 或 `403` 时，工具报告 registry 认证错误，不把它当作缺包或触发其它 source 回退。

## Registry Protocol v1

### 根配置

客户端先请求 `<index>/config.json`。响应必须是 UTF-8 JSON 对象，媒体类型为 `application/json`，内容至少包含：

```json
{
  "protocol": 1,
  "index": "https://packages.example/index/",
  "download": "https://packages.example/api/v1/download/{owner}/{name}/{version}",
  "api": "https://packages.example/api/v1",
  "auth_required": false
}
```

`protocol` 是正整数主协议号。客户端支持协议 `1` 时，遇到其它主版本必须停止并报告不兼容；本版本没有独立的 minor 协议号。`index`、`api` 和 download 模板的规范化规则与 registry 配置相同；服务端返回的 `index` 必须与客户端请求的规范身份相同，否则是 registry 配置安全错误。

download 模板中的 `{owner}`、`{name}` 和 `{version}` 在插入前按 URL path segment 编码。`version` 使用规范 SemVer 文本；模板不得把 checksum、token 或任意客户端路径作为隐式参数。服务端可以把下载请求重定向到 CDN，但重定向目标必须是 HTTPS，客户端不向新 origin 转发凭据，且最终内容仍需按记录 checksum 验证。

### 稀疏索引

对 `owner/name` 的索引请求为：

```text
GET <index>/<owner>/<name>
```

`owner` 和 `name` 已由清单规则限制为小写 ASCII 字母、数字和连字符，因此不能通过路径编码构造额外段。成功响应为 UTF-8 `application/x-ndjson`：每一行一个 JSON 对象，空行忽略；记录按 SemVer 规范排序，重复版本是协议错误。客户端可以使用 `ETag`、`If-None-Match`、`Last-Modified` 和 `304 Not Modified` 缓存，但缓存命中不能改变解析结果。

不存在的 package 返回 `404` 或 `410`；无权限返回 `401` 或 `403`；临时服务失败返回 `5xx`。`404`/`410` 只表示当前 registry 没有该 package，不会自动改用另一个 source。实现可以按明确配置的镜像顺序重试同一 source，但不得把网络异常解释成版本撤回。

每条记录的核心字段如下：

```json
{
  "schema": 1,
  "name": "acme/json",
  "vers": "1.2.3",
  "yanked": false,
  "cksum": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
  "deps": [
    {
      "name": "codec",
      "package": "acme/codec",
      "req": "^2.0",
      "registry": "public",
      "features": ["serde"],
      "default_features": true,
      "optional": false,
      "kind": "normal",
      "target": null
    }
  ],
  "features": {
    "default": ["codec"]
  },
  "metadata": {
    "license": "MIT",
    "repository": "https://example.com/acme/json"
  },
  "signatures": []
}
```

字段语义如下：

- `schema` 必须为 `1`。未知 schema 主版本不能按旧字段猜测。
- `name` 必须等于请求路径的 `owner/name`。
- `vers` 必须是合法 SemVer；同一 `name` 的规范版本文本只能出现一次。build metadata 保留在版本显示中，但不参与版本优先级比较；具有相同优先级的候选按规范版本文本稳定排序。
- `yanked` 是布尔值。它可以随授权的 yank/unyank 操作变化，不能导致其它字段变化。
- `cksum` 是 64 个小写十六进制字符，表示[package checksum](#package-sha-256)。发布后不可改变。
- `deps` 的每个字段按[依赖声明与别名](packages-builds.md)解释；`kind` 只能是 `normal`、`test` 或 `build`，发布归档中的依赖边不得引用本地绝对路径。`target` 为空表示所有目标，否则是规范 `cfg` 表达式文本。
- `features` 的键和值按清单 feature 规则解释；记录不能声明归档中不存在的 feature。
- `metadata` 只包含发布元数据，不参与依赖解析或 package checksum；服务端不得把 token、私钥、宿主路径或未声明环境写入其中。
- `signatures` 是可选签名列表，默认不会改变解析；格式见[可选签名](#可选签名)。

未知非核心字段在 `schema = 1` 下可以被客户端忽略，但不能覆盖上述核心字段；未知核心字段类型、重复 JSON 键、非法 UTF-8、非法 checksum、重复版本或记录与归档清单不一致，都是 registry 协议错误。索引记录中的 package 元数据与归档 `gugu.toml` 不一致时，客户端必须以归档清单重新验证，并拒绝不一致记录。

### HTTP 安全边界

registry、index、download 和 API 的外部 URL 必须使用 HTTPS。工具可以在显式测试配置中使用 loopback HTTP 服务，但该 source 不能写入可发布清单或被标记为公共 registry。TLS 证书验证使用宿主信任库；工具不提供跳过证书验证的 registry flag。

请求不能把清单中的任意字符串拼成 shell 命令。响应体按声明的媒体类型解析；超出工具配置的单响应大小、压缩炸弹、路径穿越、重复头或不支持的内容编码必须在写入缓存前失败。HTTP 重试只适用于幂等 GET 和明确相同 checksum 的幂等发布重试，不能重复提交未知内容。

## Package 归档与发布

### 归档内容

`gugu package` 生成一个自足的 package 归档。默认包含 `gugu.toml`、选中 target 源码、`build.gg`、清单声明资源、README、许可证和发布所需的文档；默认排除 `target/`、`vendor/`、VCS 元数据、`gugu.lock`、编辑器临时文件、系统文件、未声明生成物和凭据。`include` 替换普通候选集合，`exclude` 随后删减集合，但不能删掉清单、选中 target、build task 或声明资源。

归档路径是以 `/` 分隔的非绝对 UTF-8 相对路径，不能含空字节、`.`、`..`、反斜杠、重复分隔符或符号链接跳转。归档只包含普通文件；设备文件、FIFO、socket、目录硬链接和符号链接必须在打包时报告错误。路径按 UTF-8 字节序排序，文件内容按原始字节保留。

归档传输格式为 gzip 压缩的 tar，文件名为 `<name>-<version>.tar.gz`，媒体类型为 `application/gzip`。tar header 的路径、大小、类型、模式、owner、group、mtime、扩展 header 和 padding 使用规范固定值：普通文件模式为 `0644`，owner/group 为空，mtime 为 `0`，不写 PAX 时间戳、用户名、组名、设备号或归档文件名。实现必须拒绝会生成多个等价 header 的路径或元数据；gzip 不得携带文件名、注释或时间戳。registry 源站发布的归档字节在成功发布后保持不变；package checksum 则按下一节的规范内容流计算。透明镜像可以在验证逻辑内容和 checksum 后重新压缩，但不能改变已登记的文件集合或内容。

归档内的 `gugu.toml` 必须包含可发布的 `owner`、`name`、显式 SemVer 和 `publish = true`。归档不得包含另一个 package 根、嵌套 `gugu.lock` 或会通过相对路径逃逸的 build 输入。

### Package SHA-256

`cksum` 和锁文件中的 registry 摘要计算如下，避免压缩器版本、tar header 和宿主文件系统元数据影响结果。先按归档路径的 UTF-8 字节序排列所有普通文件，然后构造规范内容流：

```text
ASCII("gugu-package-v1\n")
for each file:
    U64BE(path_byte_length)
    path_utf8_bytes
    U64BE(file_byte_length)
    file_bytes
```

`U64BE` 是八字节大端无符号整数；长度超过 `uint64` 或内容流产生算术溢出是打包错误。对完整内容流计算 SHA-256，输出 64 个小写十六进制字符。目录本身、tar padding、gzip header、文件模式、owner、mtime 和本地绝对路径不进入 checksum。客户端必须在解压过程中先验证路径规则并计算内容流；摘要匹配后才可以把文件写入可执行的 build 输入或编译缓存。

`gugu.lock` 的 `checksum` 字段必须等于记录的 `cksum`。缓存可以另外记录传输归档的原始字节摘要，但该摘要不是 package ID，也不能替代 `checksum`。同一 package 内容由不同合法压缩流传输时，逻辑 checksum 相同；同一 URL 返回逻辑内容不同，即使压缩流能够解开，也必须报告 checksum 错误。

### 发布流程

`gugu publish` 对选中的 package 按以下顺序执行：

1. 解析清单、workspace、target、feature 和 lock 图；默认要求 lock 图可复现。
2. 生成归档并计算 package checksum。
3. 在隔离的归档视图中重新读取 `gugu.toml`，检查 target、依赖 source、feature、build 输入和文档路径。
4. 验证发布限制：path 依赖必须同时提供 registry package/version 回退；Git 依赖必须写不可变 `rev`，不能只写 branch 或 tag；归档不能引用本地绝对路径、未登记文件或凭据。
5. 使用 `--locked` 等价解析检查清单与锁图一致；若需要网络获取已锁定 package，允许下载但不得写入新的解析边。
6. 向目标 registry 的 API 上传归档；服务端重新解析清单、计算 checksum 并验证 package 名称、版本所有权和版本唯一性。
7. 只有服务端确认归档已持久化、索引记录可见且 checksum 固定后，命令才成功返回。

`gugu publish --dry-run` 执行前五步但不上传、不创建版本、不改变 registry 状态；它可以读取已配置的索引以检查名称和版本冲突。与 `--offline` 同用时不访问网络，只验证本地归档、锁和缓存，不能声称已检查远端冲突。

发布 API 请求使用归档字节作为 body，并携带：

```text
POST https://packages.example/api/v1/publish
Content-Type: application/gzip
X-Gugu-Package-SHA256: 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
Authorization: Bearer example-token
```

成功返回 `201 Created` 和 JSON `{ "name": "owner/name", "vers": "1.2.3", "cksum": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824" }`。相同 package ID 与相同 checksum 的重试可以返回 `200 OK` 并视为幂等成功；相同 package ID 与不同 checksum 必须返回 `409 Conflict`，客户端报告不可覆盖的发布冲突。清单或归档非法返回 `422 Unprocessable Content`；认证、授权、大小限制和服务暂时不可用分别使用 `401/403`、`413` 和 `5xx`。客户端不得在 `5xx` 后自动换版本或修改清单。

发布命令成功时输出 `publish-result` 事件，至少包含 registry identity、package ID、checksum 和是否为幂等重试；token、私钥和本地绝对路径不能出现在事件中。服务端不接受客户端直接提交索引记录，索引字段必须从已验证归档和服务端拥有权状态产生。

## 可选签名

### 默认行为

签名不替代 HTTPS、package checksum 或 `gugu.lock`。默认 `require-signature = false` 时，客户端仍必须验证 checksum；没有签名的记录不产生签名错误。若记录包含签名但签名未知、key 不在用户信任配置中或验证失败，客户端可以继续解析但必须以 warning 诊断其未验证状态。签名被验证为有效并不使不同 checksum 的归档可接受。

用户可以在用户配置中启用：

```toml
[registry]
require-signature = true

[registry.trusted-keys]
"66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925" = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
```

也可以用全局命令行参数 `--require-signature` 临时要求所有参与解析的 registry package 有受信任签名。严格模式下，缺少签名、算法不支持、key 不受信任、签名消息不匹配或签名验证失败都是解析错误；path package 和显式 Git package 不会因为该开关获得 registry 签名，若策略要求它们签名则必须由外部供应链工具另行阻止。

### 签名记录与消息

Protocol v1 只识别 `ed25519`。签名对象字段为：

```json
{
  "role": "publisher",
  "algorithm": "ed25519",
  "key_id": "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925",
  "signature": "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_"
}
```

上面的 `signature` 仅展示 Protocol v1 的 base64url 字段形状；它不是可用于通过验证的发布签名。实际发布记录必须由对应私钥对下述规范消息生成并由客户端验证。

`role` 只能是 `publisher` 或 `registry`。`key_id` 是对应原始 32 字节公钥的 SHA-256 小写十六进制摘要；用户信任配置中的公钥必须与 key ID 相符。`signature` 是 Ed25519 对下列 UTF-8 字节串的签名，字段按固定顺序、每行一个键值并以最终 LF 结束：

```text
gugu-signature-v1
registry=<canonical registry identity>
package=<owner/name>
version=<canonical SemVer>
checksum=<package checksum>
role=<publisher|registry>
```

同一 `role` 和 `key_id` 的重复签名记录是协议错误；不同 key 的多份签名可以同时存在。发布者签名由 registry 原样保存，registry 可以额外添加自己的 `role = registry` 签名。默认 registry 不承诺替发布者建立身份信任，也不提供公共 transparency database；信任根只来自用户配置或组织分发的受信任配置。

`gugu publish --sign <key>` 可以提交发布者签名；私钥只能从用户凭据存储或显式进程输入读取，不能从 package 清单读取，不能上传到 registry，也不能写日志。工具在收到记录后验证编码和签名范围；默认模式下无效签名给出 warning，严格模式下失败。已发布 checksum、package ID 和签名消息不可修改；新增签名只允许作为附加记录，不得删除已有签名。

密钥轮换通过新增 key 和新签名完成；旧 key 从用户信任配置移除后，旧版本在严格模式下会失败，但不会被 registry 自动删除或改写。密钥撤销不等同于版本 yank；组织需要同时撤销 key 和按风险决定是否 yank 受影响版本。

## 版本 Yank

### 语义

`gugu yank owner/name --version <version>` 请求 registry 把精确版本的 `yanked` 设为 `true`；`--undo` 恢复为 `false`。yank 只改变索引记录的撤回状态，不改变归档、checksum、依赖、feature、签名或版本号。撤回操作必须经过该 package owner 或 registry 管理权限认证。

解析器选择新版本时排除 `yanked = true`，包括精确版本要求；已有有效 `gugu.lock` 中已经锁定的 yanked package 仍可以下载、验证、构建和测试。显式更新锁图时会尝试移出 yanked 版本；若没有其它满足约束的版本，更新命令报告无解，不悄悄使用被撤回版本。`--locked` 不修改锁图，因此可以继续使用已锁定版本。

被 yank 的版本仍必须由 download URL 提供，缓存和 vendor 仍可使用它；服务器不能用 `404` 假装删除版本。客户端在解析已锁定 yanked 版本时可以发出 warning，但不能把 warning 变成构建失败，除非用户另有 `deny-yanked` 策略。Protocol v1 不采用 Go 式作者 `retract` 声明；版本撤回的唯一规范状态是 registry 的 yank。

### Yank API

请求格式为：

```text
POST https://packages.example/api/v1/yank
Content-Type: application/json
Authorization: Bearer example-token

{
  "name": "owner/name",
  "vers": "1.2.3",
  "yanked": true
}
```

成功返回 `200 OK` 和当前完整状态。不存在的 package/version 返回 `404`；无权限返回 `401/403`；状态已经等于请求值时仍返回幂等成功。客户端输出 `yank-result` 事件，包含 package ID、旧状态和新状态，不包含 token。

## 离线、锁定与镜像

### 模式组合

- `--locked`：解析必须完全使用现有 `gugu.lock`；缺锁、锁中 source/checksum 与索引不一致、清单要求改变或锁节点缺失时失败。允许通过 registry 下载锁中尚未缓存的精确 package。
- `--offline`：禁止工具链访问 registry、mirror、Git 和其它网络 source；只允许已通过 checksum 验证的全局缓存和显式 `vendor/`。缺少任何索引、归档或 Git commit 时立即报告 offline 缺失，不改变锁图。
- `--frozen`：同时启用 `--locked` 和 `--offline`；不自动启用 vendor。
- `--vendor`：只从当前 workspace 的 `vendor/` 读取 registry/Git 依赖，并验证 vendor manifest、source mapping、锁图和 checksum；vendor 不完整或不一致时失败，不回退网络。

`--offline` 对 build task 也生效：`std.build` 的网络入口必须拒绝请求；任务使用已允许的本地文件和进程仍按权限门执行。用户程序在 `gugu run` 中访问网络不属于工具链依赖下载，但 `gugu run --offline` 仍把该开关传递给工具链构建阶段，不改变程序自身的运行时网络语义。

### 镜像与缓存

镜像只服务于配置所绑定的 registry identity。镜像可以缓存 `config.json`、稀疏索引记录和归档，但必须透传或重新验证原始 checksum、yanked 状态和签名；镜像不能重写 package 元数据、替换下载 URL 为另一个 source、把未认证错误转换成成功或提供一个不同内容的同名版本。

工具在使用 registry 归档、解包树和 vendor 文件前都要验证 package checksum。全局缓存中的归档按 package ID 和 checksum 分桶；发现摘要不符、归档截断、路径非法或索引记录污染时隔离该 entry。`--offline` 下隔离后不能联网修复，命令直接失败。缓存命中与否、镜像是否参与和 HTTP 响应顺序不能改变锁图或程序语义。

锁文件记录 registry identity、owner/name、精确版本和 checksum。registry index 的 ETag、下载 URL、认证 token、镜像地址和本地缓存路径不进入锁文件，因此在不同机器上可以使用不同镜像而保持相同 package ID。镜像如果不能提供同一 checksum，必须被视为错误而不是产生新锁节点。

### Vendor

`gugu vendor` 从当前有效锁图生成 `vendor/`，只复制实际选择的 registry/Git package，并写入 source mapping、package ID、checksum 和生成工具版本。vendor 树不包含 path package、VCS 数据、其它 workspace 的 vendor、target 产物或凭据。生成过程按 package ID 排序并原子替换，已存在 vendor 与当前锁图不一致时要求显式重生成。

`--vendor` 下每个依赖必须能由 vendor mapping 唯一找到；mapping 中的 source、版本、checksum、清单内容和文件摘要必须一致。vendor 中的 `build.gg` 仍是依赖自己的 build task，执行时使用当前 host/target 规则和权限门；vendor 本身不是 sandbox，也不是对依赖代码的安全背书。

## 供应链策略

Gugu 的默认生态策略固定以下边界：

- registry source 必须显式可识别，package ID 不按短名合并，避免把不同 registry 的同名 package 混成一个依赖。
- registry package 的版本归档不可覆盖，撤回使用 yank，不能通过删除下载文件制造历史缺口。
- 每个 registry 节点都有 SHA-256；锁文件和缓存都验证该摘要。首版不依赖公共 checksum transparency database，也不把 DNS、HTTP 地址或镜像身份当作内容证明。
- 签名可选；严格签名策略必须由用户配置受信任公钥，工具不能把“有签名”自动解释成“可信”。
- 发布 package 不得携带 token、私钥、workspace lock、绝对路径、VCS 凭据、构建缓存或未声明生成物。
- 发布归档中的 Git 依赖必须锁定 `rev`；branch、tag 和随远端默认分支移动的引用只能用于本地开发，不能进入发布归档。
- build task 不是 registry 服务端的 post-install 脚本；它只在消费者本地构建时执行，并继承[包、依赖与构建模型](packages-builds.md)和[运行时与运维语义](runtime.md)规定的权限、host/target 与缓存规则。
- `--permission` 是 advisory 权限检查，不是操作系统沙箱；`--offline`、`--vendor` 和签名要求也不能把不可信的 build task 变成绝对安全代码。
- registry API、镜像和依赖缓存不得执行归档中的程序来发现元数据；所有发布字段从清单和归档静态验证得到。

组织可以在用户配置和 CI 中增加 registry allowlist、`--frozen`、`--vendor`、`--require-signature`、禁止 Git source 或 `deny-yanked`，但这些属于调用方策略。策略失败必须发生在目标代码编译前，不能通过修改 fixture、清单解析结果或 package 内容绕过。

## 错误与事件

以下错误类别是稳定的 registry/发布诊断名：

| 名称 | 条件 |
|------|------|
| `registry-config` | `config.json` 缺字段、协议版本不支持、身份 URL 不一致或协议字段类型错误 |
| `registry-auth` | token 缺失、无效或权限不足 |
| `registry-not-found` | registry 没有请求的 package/version，且不是已锁定归档缺失 |
| `registry-protocol` | 索引记录、HTTP 媒体类型、重复版本或核心字段违反 Protocol v1 |
| `package-conflict` | 已存在 package ID 的 checksum 与待发布归档不同 |
| `package-checksum` | 归档内容流、缓存、vendor 或 lock checksum 不一致 |
| `package-invalid` | 归档路径、清单、依赖 source、文件集合或发布元数据非法 |
| `package-yanked` | 新解析试图选择 yanked 版本，或严格策略拒绝已锁定 yanked 版本 |
| `signature-unverified` | 默认模式下已提供但不受信任或验证失败的签名 warning |
| `signature-required` | 严格模式下没有有效受信任签名 |
| `offline-missing` | 离线模式缺少已验证索引、归档、Git commit 或 vendor 文件 |
| `source-not-allowed` | registry identity 不在 allowlist，或 source 类型违反策略 |

发布和撤回成功分别产生 `publish-result` 与 `yank-result`；依赖解析事件 `package-resolve` 至少包含 package ID、source、version、checksum、yanked 状态和是否来自 cache/vendor。所有事件都不得输出凭据、私钥和宿主绝对路径。

## 设计参考

本章的协议分层、归档发布、撤回和供应链边界参考以下官方资料；这些资料用于解释设计取舍，不覆盖 Gugu 已固定的 package ID、SHA-256、锁文件和离线语义：

- [Cargo Registries](https://doc.rust-lang.org/cargo/reference/registries.html)
- [Cargo Registry Index](https://doc.rust-lang.org/cargo/reference/registry-index.html)
- [Cargo Publishing](https://doc.rust-lang.org/cargo/reference/publishing.html)
- [Cargo Yank](https://doc.rust-lang.org/cargo/commands/cargo-yank.html)
- [Go Modules Reference：Module proxy protocol](https://go.dev/ref/mod#goproxy-protocol)
- [Go Modules Reference：Authenticating modules](https://go.dev/ref/mod#authenticating)
- [Go Modules Reference：Go checksum database](https://go.dev/ref/mod#checksum-database)
- [Go Modules Reference：Vendoring](https://go.dev/ref/mod#vendoring)
- [Go Modules Reference：Private modules](https://go.dev/ref/mod#private-modules)
