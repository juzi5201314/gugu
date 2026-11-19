# 并发与取消

Gugu 用有栈协程做并发：`async` 创建协程得到 `Join[T]`，`chan[T]` 传值，`select` 等多路等待，`CancelToken` 做协作式取消。语义权威在[并发与调度](../spec/concurrency.md)与[标准库 · 取消](../spec/standard-library.md)。

## 创建协程与等待

```gugu
fn worker(n: int) int = n * 2

fn main() {
    let job = async worker(21)
    match job.wait() {
        Ok(result) => {
            if result != 42 { panic("wait") }
        }
        Err(_) => panic("child panicked")
    }
}
```

`async 调用` 或 `async { 块 }` 立即返回 `Join[T]`，不等待。`wait()` 阻塞到子协程发布最终结果，返回 `Result[T, Panic]`——子协程的 panic 作为 `Err` 交付给等待者而不炸进程；`wait()` 之后，子协程的写入对等待者可见。丢弃 `Join` 不会取消子协程。

## channel 传值

```gugu
fn producer(jobs: chan[int]) {
    for i in 0..8 {
        jobs.send(i)
    }
    jobs.close()
}

fn consumer(jobs: chan[int]) int {
    let seen = 0
    loop {
        match jobs.recv() {
            Ok(_) => { seen += 1 }
            Err(_) => return seen
        }
    }
}

fn main() {
    let jobs = chan[int](4)
    async producer(jobs)
    if consumer(jobs) != 8 { panic("count") }
}
```

- `chan[T](n)` 是缓冲 channel；`send` 阻塞到送出，`recv` 返回 `Result[T, ChanClosed]`。
- `close()` 之后 `recv` 收尽缓冲返回 `Err(ChanClosed)`；对已关闭 channel 再 `send`/`close` 是 panic。
- `try_send`/`try_recv` 不阻塞，错误类型区分 `Full`/`Empty`/`Closed`。

## select 多路等待

```gugu
fn main() {
    let first: chan[int] = chan[int](1)
    let second: chan[int] = chan[int](1)
    select {
        first.send(1) => { _ = 1 }
        let got = second.recv() => { _ = got }
        _ => { _ = 0 }
    }
}
```

分支可以是 `send`、`recv` 和 `Join.wait()`；`_` 分支是默认、不阻塞。就绪分支随机公平选择，选中即原子提交——未选中的分支不发送、不接收。

## 协作式取消

取消是显式、幂等、协作式的：`CancelSource` 发 `CancelToken`，阻塞 I/O、sleep、`Join.wait` 的 cancel 变体在 safepoint 观察它，返回 `Cancelled` 而不是 panic：

```gugu
use std.sync.{CancelSource}

fn main() {
    let source = CancelSource::new()
    let token = source.token()
    source.cancel()
    match token.check() {
        Err(_) => { _ = 0 }
        Ok(_) => panic("must be cancelled")
    }
}
```

阻塞操作的 cancel 变体签名见[标准库 · 取消](../spec/standard-library.md)。

## 易错点

- 协程间可见性只由 channel、`Join.wait()`、同步原语建立；两个独立 wake 的观察顺序不是保证。
- `#[os_thread_local]` 是 OS 线程槽、`#[coroutine_local]` 是协程槽；并行度变化与外调都会改变二者的使用时机，见[声明与模块](../spec/declarations.md)。
- `select` 的分支表达式在进入时只求值一次；把有副作用的调用写进分支载荷时要注意它已执行但可能未选中。
