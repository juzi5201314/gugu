# 部署与发布

本章把产物拿出去：双目标构建、strip、可重现性与 registry 发布。权威定义见[工具链与命令行](../spec/toolchain-cli.md)与[发布与生态](../spec/publishing-ecosystem.md)。

## 产物

```text
gugu build                       # target/<目标>/bin/<name>
gugu build --target x86_64-windows
gugu build --strip               # 最终镜像 strip：ELF 节表重写 / PE debug 目录清零
```

物化布局固定为 `target/<target>/{bin,lib,tests,benches,examples}`，构建日志在 `build-logs/` JSONL。生产镜像是自包含的 ELF/PE，不依赖系统 LLVM、汇编器或链接器。

## 可重现构建

同一 CompilerIdentity 与输入产生语义等价且可重现的结果。建议发布流水线用：

```text
gugu build --frozen --locked     # 锁文件冻结 + 离线
gugu cache verify                # 校验全局 action cache 完整性
```

缓存键、指纹输入与失效语义见[包、依赖与构建模型](../spec/packages-builds.md)。

## 发布到 registry

```text
gugu login <registry>            # token 写入本地凭据（0600）
gugu package                     # 生成归档并校验
gugu package --list              # 只列归档内容
gugu publish --dry-run           # 走完整校验不上传
gugu publish                     # 上传归档 + 签名
gugu yank owner/name             # 撤回已发布版本
```

归档遵守清单的 `include`/`exclude`，并默认排除凭据、临时文件与锁文件；校验覆盖依赖闭包与 checksum。签名要求、策略字段与 deny-yanked 语义见[发布与生态](../spec/publishing-ecosystem.md)。

## 运行时旋钮

进程启动时从环境读一次配置：`GUGU_RUNTIME_PROCS`（并行度）、`GUGU_RUNTIME_GC_TARGET`、`GUGU_RUNTIME_MEMORY_LIMIT`、`GUGU_RUNTIME_STACK_MAX`、`GUGU_RUNTIME_TRACE`、`GUGU_RUNTIME_DIAGNOSTICS`、`GUGU_BACKTRACE`。完整取值表见[运行时配置](../reference/runtime-config.md)与[运行时与运维语义](../spec/runtime.md)。
