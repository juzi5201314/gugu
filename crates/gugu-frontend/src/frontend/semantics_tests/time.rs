use super::{accept_cases, accepts};

accept_cases! {
    std_time_instant_facade_00: true,
        "use std.time.{Duration, Instant}\nfn main() { let a = Instant::now()\n let b = Instant::now()\n _ = a.elapsed()\n _ = b.checked_add(Duration::from_secs(1)) }";
    std_time_system_time_facade_01: true,
        "use std.time.{SystemTime}\nfn main() { let a = SystemTime::now()\n _ = a.duration_since(a) }";
    std_time_checked_ops_02: true,
        "use std.time.{Duration, Instant}\nfn main() { let a = Instant::now()\n let d = Duration::from_millis(1)\n _ = a.checked_add(d)\n _ = a.checked_sub(d) }";
    std_time_error_result_03: true,
        "use std.time.{Error, SystemTime}\nfn main() { let a = SystemTime::now()\n match a.duration_since(a) { Ok(_) => (), Err(e) => match e { Error::ClockWentBackward => () } } }";
    // `Instant.nanos` 不公开：外部不能构造或读取单调时刻的字面值。
    std_time_instant_field_private_04: false,
        "use std.time.{Instant}\nfn main() { _ = Instant { nanos: 0 } }";
    std_time_sleep_05: true,
        "use std.time.{Duration, sleep}\nfn main() { sleep(Duration::from_secs(0))\n sleep(Duration::from_millis(10)) }";
    std_time_sleep_cancel_06: true,
        "use std.time.{Duration, sleep_cancel}\nuse std.sync.{CancelSource, Cancelled}\nfn main() { let source = CancelSource::new()\n let token = source.token()\n match sleep_cancel(Duration::from_millis(1), &token) { Ok(_) => (), Err(e) => match e { Cancelled => () } } }";
    std_sync_cancel_check_07: true,
        "use std.sync.{CancelSource, Cancelled}\nfn main() { let source = CancelSource::new()\n let token = source.token()\n source.cancel()\n if !token.is_cancelled() { panic(\"x\") }\n match token.check() { Ok(_) => (), Err(e) => match e { Cancelled => () } } }";
    std_time_timeout_08: true,
        "use std.time.{Duration, timeout, TimedOut}\nuse std.sync.{CancelToken}\nfn op(cancel: &CancelToken) int { 7 }\nfn main() { let r: Result[int, TimedOut] = timeout(Duration::from_millis(10), op)\n match r { Ok(_) => (), Err(e) => match e { TimedOut => () } } }";
    std_time_timeout_closure_09: true,
        "use std.time.{Duration, timeout}\nuse std.sync.{CancelToken}\nfn main() { let op = fn(cancel: &CancelToken) int = 1\n _ = timeout(Duration::from_secs(1), op) }";
    // `operation` 必须接受 `&CancelToken`；普通无参函数不是合法实参。
    std_time_timeout_rejects_token_free_op_10: false,
        "use std.time.{Duration, timeout}\nfn op() int { 1 }\nfn main() { _ = timeout(Duration::from_secs(1), op) }";
}
