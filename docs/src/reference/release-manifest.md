# 发布清单 {#release-manifest}

本页登记 gugu 首个生产发布（0.1.0）的版本、编译器身份、运行时 tuning profile、迁移说明与产物清单，由阶段 79 发布门禁核验。

## 版本 {#version}

`gugu --version` 输出固定四行：`gugu <semver> (commit <hash> <date>)`、`host:`、`unicode:`、`llvm:`。

- 工具链语义版本 `0.1.0`（workspace crate 统一版本）。
- Unicode 数据版本 `18.0.0`（编码、大小写映射与 case fold 共用此版本，见 [工具链与命令行](../spec/toolchain-cli.md)）。
- `llvm: not-used`：生产镜像由内置 x64 后端直写 ELF/PE，不依赖系统 LLVM、汇编器或链接器；构建输入与产物都不经过外部工具链。

## CompilerIdentity {#compiler-identity}

编译器构建身份是所有 action key 与 artifact 指纹的输入段，由六段组成：

```text
gugu-compiler-0.1.0 unicode-18.0.0 unicode-tables-normalization-<表版本>
segmentation-<表版本> runtime-artifact-schema-4 core-revision-1
```

规范化与分段表版本取自依赖 crate 的实际表版本（不必等于 Unicode 目标版本），`runtime-artifact-schema` 对应 `GUGURT01` artifact schema，`core-revision` 对应编译器核心 GBC schema。任何一段变化都会使既有缓存与镜像指纹失效，升级依赖必须带来新身份（机制见 [单态化与缓存](../internals/monomorphization-cache.md)）。

## Runtime tuning profile {#runtime-tuning}

镜像运行时可调参数全部经由 `GUGU_RUNTIME_*` 环境变量声明，取值域、默认值与语义在[运行时配置](runtime-config.md)登记；本清单不重复定义。发布基线 profile：不改设任何变量即默认 profile（并行度=可用核、GC 堆目标 100%、内存软上限关闭、协程逻辑栈 1GiB、trace 与诊断默认）。

## 迁移说明 {#migration}

0.1.0 是首个生产版本，不存在从早期发布迁移的路径。锁文件、action cache、registry 归档与镜像都在本次版本内新建；`runtime-artifact-schema` 或 `core-revision` 未来变化时，失效处理由缓存指纹与镜像验证负责，见 [包、依赖与构建模型](../spec/packages-builds.md)。

## 发布产物清单 {#artifacts}

| 产物 | 位置/形式 | 说明 |
| ---- | --------- | ---- |
| `gugu` CLI | 单静态可执行 | 全部子命令，无外部工具链依赖 |
| 目标镜像 | `target/<target>/` ELF（linux）/PE（windows） | 内置 x64 后端直写，自包含无系统链接器产物 |
| `GUGURT01` runtime artifact | 编译输入闭包 | schema v4，含 source manifest 与 prebuilt 段 |
| action cache | `--cache-dir` 内容寻址 | `actions/v1`，见[包与构建](../guide/package-build.md) |
| 规范/教程/参考文档 | mdBook `target/book/` | `docs/src` 构建产物，不随工具链分发 |
| 锁文件 `gugu.lock` | 包根 | 记录解析版本、checksum 与签名 |
