# ADR 0006：闭世界 TypeId 与窄 downcast

- 状态：已接受
- 日期：2026-08-30

## 上下文

闭世界 AOT 能在链接前枚举全部单态化类型。Rust 的 `TypeId` 是开世界哈希（dylib、碰撞、不能当数组下标）。Gugu 若照抄，等于丢掉闭世界最大的表示优势。名为 `any` 的渐进类型已被公理禁止；`dyn Trait` 已经是显式擦除。需要决定：要不要类型身份、要不要从擦除值里恢复具体类型。

## 决策

- 做 `TypeId`：编译器分配稠密 `u32` 编号（用户用 `as_int()` 取出），不是哈希。关键字构造器 `type_id[T]()` / `type_id_count()`，与 `size_of` 同形，避免值后 `[]` 当下标。
- 镜像写一张以编号为下标的类型表（名字、大小、对齐、GC 扫描描述符）。`downcast` 是整数比较。
- 不做名为 `any` 的类型。擦除走已有的 `dyn Any`。`Any` 是 lang trait（编译器按名字挂钩），只有 `type_of`（不能叫 `type_id`，那是关键字），不能有泛型方法。
- 做窄 downcast：只恢复当初放进盒子的具体 `T`。`is` / `downcast` / `downcast_copy` 是 `dyn Any` 的固有方法，不是 trait 方法。禁止从 `dyn Any` 猜 `dyn Print`。
- `!` 与 `MaybeUninit[T]` 没有 `TypeId`，语言写 `impl !Any`。用户不能手写 `Any` 的肯定或否定 impl。
- 盒子浅拷值表示。句柄进盒子只拷句柄字。`dyn Print` 再进 `dyn Any` 只能 downcast 回 `dyn Print`。

## 后果

- 异构容器、插件式回调表、诊断打印类型名，都不必引入开世界反射。
- 实现可以把载荷编号放进 vtable 或对象头；语言不把「GC 对象头索引 ≡ 语言 TypeId」写成同一件事。
- 重新编译可以重排编号；TypeId 不是 ABI、不是跨进程协议。
