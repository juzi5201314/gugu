use super::*;
use crate::x64::fs_native::net;

#[test]
fn linux_socket_calls_preserve_arguments_and_native_errno() {
    for (op, syscall, returned, value, error) in [
        (0, "syscall:41", 7, 7, 0),
        (2, "syscall:49", (-98i64) as u64, 0, 98),
        (4, "syscall:288", (-11i64) as u64, 0, 11),
        (10, "syscall:232", 2, 2, 0),
        (21, "syscall:42", (-115i64) as u64, 0, 115),
        (22, "syscall:48", 0, 0, 0),
        (23, "syscall:55", 0, 0, 0),
        (24, "syscall:45", 3, 3, 0),
        (25, "syscall:44", (-90i64) as u64, 0, 90),
    ] {
        let mut machine = Machine::new(&[(syscall, returned)]);
        machine.set_reg(Rax, op);
        machine.set_reg(Rbx, REQUEST);
        for i in 0..6 {
            machine.put(REQUEST + i * 8, 100 + i);
        }
        assert_eq!(machine.execute(net::linux()), Stop::Return);
        assert_eq!(machine.calls[0].1, [100, 101, 102, 103, 104, 105]);
        assert_eq!(machine.word(REQUEST + 72), value);
        assert_eq!(machine.word(REQUEST + 80), error);
        assert_eq!(machine.get(R12), 0x1234);
        assert_eq!(machine.get(Rsp), STACK);
    }
}

#[test]
fn winsock_errors_use_wsa_status_and_preserve_wide_socket_handles() {
    for (op, script, value, error) in [
        (0, vec![("socket", 0x123456789)], 0x123456789, 0),
        (
            0,
            vec![("socket", u64::MAX), ("WSAGetLastError", 10024)],
            u64::MAX,
            10024,
        ),
        (
            2,
            vec![("bind", u64::MAX), ("WSAGetLastError", 10048)],
            u32::MAX as u64,
            10048,
        ),
        (12, vec![("WSAStartup", 10093)], 10093, 10093),
        (
            21,
            vec![("connect", u64::MAX), ("WSAGetLastError", 10035)],
            u32::MAX as u64,
            10035,
        ),
        (22, vec![("shutdown", 0)], 0, 0),
        (23, vec![("getsockopt", 0)], 0, 0),
        (
            26,
            vec![("WSARecvFrom", u64::MAX), ("WSAGetLastError", 997)],
            u32::MAX as u64,
            997,
        ),
        (27, vec![("WSASendTo", 0)], 0, 0),
        (28, vec![("WSARecv", 0)], 0, 0),
        (29, vec![("WSASend", 0)], 0, 0),
        (
            10,
            vec![("GetQueuedCompletionStatus", 0), ("GetLastError", 258)],
            0,
            258,
        ),
    ] {
        let mut machine = Machine::new(&script);
        machine.set_reg(Rax, op);
        machine.set_reg(Rbx, REQUEST);
        for i in 0..9 {
            machine.put(REQUEST + i * 8, 200 + i);
        }
        assert_eq!(machine.execute(net::windows()), Stop::Return);
        assert_eq!(
            &machine.calls[0].1[..8],
            &[200, 201, 202, 203, 204, 205, 206, 207]
        );
        assert_eq!(machine.word(REQUEST + 72), value);
        assert_eq!(machine.word(REQUEST + 80), error);
        assert_eq!(machine.get(R12), 0x1234);
        assert_eq!(machine.get(Rsp), STACK);
    }
}
