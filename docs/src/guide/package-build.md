# 包与构建

本章讲清单、依赖、workspace 与 `build.gg` 的日常用法；权威模型在[包、依赖与构建模型](../spec/packages-builds.md)与[发布与生态](../spec/publishing-ecosystem.md)。

## 清单

`gugu.toml` 描述包的名称、target、依赖与 feature。一个 package 至少一个 lib 或 bin target；`src/main.gg` 是 bin 入口、`src/lib.gg` 是 lib 入口。

## 依赖

```text
gugu add owner/name          # 解析版本、写入清单并更新锁文件
gugu remove owner/name
gugu update                  # 按清单约束重新解析
gugu update --precise 1.2.3  # 单包精确升降
gugu tree                    # 显示依赖图
```

解析产物写 `gugu.lock`；`--locked` 要求锁文件已最新、`--frozen` 等价 `--locked --offline`、`--offline` 只用本地缓存与 vendor。CI 上建议 `gugu build --frozen`。

## registry 依赖的获取

依赖从配置的 registry 解析：索引取元数据、归档入库到全局缓存、锁文件记录 checksum 与签名。`gugu vendor` 把依赖物化到 workspace `vendor/`，配合 `--vendor` 离线构建。发布侧见[发布与生态](../spec/publishing-ecosystem.md)。

## workspace

`gugu.toml` 用 `members`/`exclude` 声明 workspace；`-p owner/name` 选单包、`--workspace` 选全部。收集式命令（`test`/`bench`/`--all-targets`/`--lib`）跳过没有对应 target 的成员。

## build.gg 与权限门

包可以提供 `build.gg` 构建脚本生成源码模块（`emit_module` 的产物进编译闭包）。构建脚本的操作默认受权限门约束：

```text
gugu build --permission                      # 启用权限门
gugu build --read-allows 'assets/**'         # 预授权读
gugu build --write-allows 'generated/**'     # 预授权写
gugu build --env-allows 'CC'                 # 预授权环境变量
gugu build -A                                # 放行全部（慎用）
```

权限模型与门内授权语义见[包、依赖与构建模型](../spec/packages-builds.md)。

## 缓存

全局 action cache 在 `--cache-dir`（默认用户缓存目录）下内容寻址；`gugu cache dir|clean|gc|verify` 管理，`gugu clean` 只删 `target/`、不动全局缓存，`gugu clean --all` 全清。产物物化布局见[工具链与命令行](../spec/toolchain-cli.md)。
