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
}
