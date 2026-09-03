# Summary

[Gugu](introduction.md)

# 语言规范

- [概述与目标](spec/overview.md)
- [词法结构](spec/lexical.md)
- [格式化与代码风格](spec/format-style.md)
- [形式语法](spec/syntax.md)
- [类型系统](spec/types.md)
- [声明与模块](spec/declarations.md)
- [程序与编译模型](spec/program-model.md)
- [包、依赖与构建模型](spec/packages-builds.md)
- [发布与生态](spec/publishing-ecosystem.md)
- [工具链与命令行](spec/toolchain-cli.md)
- [表达式与语句](spec/expressions.md)
- [模式](spec/patterns.md)
- [函数与闭包](spec/functions.md)
- [接口、实现与特化](spec/traits.md)
- [值、句柄与传递](spec/passing.md)
- [内存与对象模型](spec/memory.md)
- [并发与调度](spec/concurrency.md)
- [编译期执行](spec/comptime.md)
- [unsafe 与 intrinsic](spec/unsafe.md)
- [平台与 ABI 参考](spec/platform-abi.md)
- [运行时与运维语义](spec/runtime.md)
- [标准库](spec/standard-library.md)
- [测试](spec/testing.md)

# 编译器内部

- [AST 与 HIR](internals/ast-hir.md)
- [comptime 与抽象分析](internals/comptime-analysis.md)
- [GIR 与 LIR](internals/gir-lir.md)
- [单态化与编译缓存](internals/monomorphization-cache.md)
- [栈图](internals/stack-maps.md)
- [GC 元数据](internals/gc-metadata.md)
- [内存所有权与消息通道](internals/memory-messaging.md)
- [调度器](internals/scheduler.md)
- [x86_64 后端](internals/backend.md)
