use super::*;
use crate::x64::fs_native::process;

fn call(machine: &mut Machine, target: TargetName, op: u64) {
    machine.set_reg(Rax, op);
    machine.set_reg(Rbx, REQUEST);
    machine.put(REQUEST, op);
    assert_eq!(machine.execute(process::test_call(target)), Stop::Return);
    assert_eq!(machine.get(Rsp), STACK);
    assert_eq!(machine.get(R12), 0x1234);
}
#[test]
fn linux_wait_decodes_exit_and_signal_without_blocking() {
    for (status, kind, value) in [(7 << 8, 1, 7), (9, 2, 9)] {
        let mut machine = Machine::new(&[("syscall:61", 123)]);
        machine.put(REQUEST + 24, status);
        machine.put(REQUEST + 8, 123);
        machine.after_host = Some(|m, _| {
            m.put(REQUEST + 16, m.word(REQUEST + 24));
        });
        call(&mut machine, TargetName::X86_64Linux, 0);
        assert_eq!(&machine.calls[0].1[..4], &[123, REQUEST + 16, 1, 0]);
        assert_eq!(machine.word(REQUEST + 72), kind);
        assert_eq!(machine.word(REQUEST + 16), value);
        assert_eq!(machine.word(REQUEST + 80), 0);
    }
    let mut pending = Machine::new(&[("syscall:61", 0)]);
    call(&mut pending, TargetName::X86_64Linux, 0);
    assert_eq!(pending.word(REQUEST + 72), 0);
}
#[test]
fn windows_collects_exit_259_after_signalled_handle() {
    let mut machine = Machine::new(&[("WaitForSingleObject", 0), ("GetExitCodeProcess", 1)]);
    machine.put(REQUEST + 8, 0x123456789);
    machine.after_host = Some(|m, name| {
        if name == "GetExitCodeProcess" {
            m.put(REQUEST + 16, 259);
        }
    });
    call(&mut machine, TargetName::X86_64Windows, 0);
    assert_eq!(&machine.calls[0].1[..2], &[0x123456789, 0]);
    assert_eq!(machine.word(REQUEST + 72), 1);
    assert_eq!(machine.word(REQUEST + 16), 259);
    let mut pending = Machine::new(&[("WaitForSingleObject", 258)]);
    call(&mut pending, TargetName::X86_64Windows, 0);
    assert_eq!(pending.word(REQUEST + 72), 0);
    assert_eq!(pending.calls.len(), 1);
}
#[test]
fn linux_pipe_write_preserves_preexisting_sigpipe() {
    for old_pending in [0, 4096] {
        let mut script = vec![
            ("syscall:14", 0),
            ("syscall:127", 0),
            ("syscall:1", 32u64.wrapping_neg()),
        ];
        if old_pending == 0 {
            script.push(("syscall:128", 13));
        }
        script.push(("syscall:14", 0));
        let mut machine = Machine::new(&script);
        machine.put(REQUEST + 8, 17);
        machine.put(REQUEST + 16, 1024);
        machine.put(REQUEST + 24, 31);
        machine.put(REQUEST + 48, old_pending);
        machine.after_host = Some(|m, name| {
            let args = m.calls.last().unwrap().1.clone();
            if name == "syscall:127" {
                m.put(args[0], m.word(REQUEST + 48));
            }
            if name == "syscall:14" && args[0] == 0 {
                m.put(args[2], 0x400);
            }
            if name == "syscall:14" && args[0] == 2 {
                assert_eq!(m.word(args[1]), 0x400);
            }
        });
        call(&mut machine, TargetName::X86_64Linux, 4);
        assert_eq!(&machine.calls[2].1[..3], &[17, 1024, 31]);
        assert_eq!(machine.word(REQUEST + 80), 32);
        assert_eq!(machine.word(REQUEST + 72), 0);
    }
}
#[test]
fn windows_overlapped_io_keeps_wide_handles_and_error_status() {
    for (op, api, status) in [
        (3, "ReadFile", 997),
        (4, "WriteFile", 109),
        (5, "GetOverlappedResult", 996),
        (6, "CancelIoEx", 1168),
    ] {
        let mut machine = Machine::new(&[(api, 0), ("GetLastError", status)]);
        machine.put(REQUEST + 8, 0x123456789);
        machine.put(REQUEST + 16, 2048);
        machine.put(REQUEST + 24, 64);
        machine.put(REQUEST + 32, 4096);
        call(&mut machine, TargetName::X86_64Windows, op);
        assert_eq!(machine.calls[0].1[0], 0x123456789);
        assert_eq!(machine.word(REQUEST + 80), status);
        if op <= 4 {
            assert_eq!(machine.calls[0].1[4], 4096);
        } else {
            assert_eq!(machine.calls[0].1[1], 4096);
        }
    }
}
#[test]
fn linux_spawn_exec_error_is_reaped_and_returned() {
    let mut machine = Machine::new(&[
        ("syscall:293", 0),
        ("syscall:57", 123),
        ("syscall:3", 0),
        ("syscall:0", 4),
        ("syscall:3", 0),
        ("syscall:61", 123),
    ]);
    machine.put(REQUEST, 0);
    machine.put(REQUEST + 8, 1024);
    machine.after_host = Some(|m, name| {
        let args = m.calls.last().unwrap().1.clone();
        if name == "syscall:293" {
            m.put(args[0], 11 | (12 << 32));
        }
        if name == "syscall:0" {
            m.put(args[1], 2);
        }
    });
    assert_eq!(
        machine.execute(process::test_worker(TargetName::X86_64Linux)),
        Stop::Return
    );
    assert_eq!(machine.word(REQUEST + 80), 2);
    assert_eq!(machine.word(1024 + 72), 2);
    assert_eq!(machine.word(1024 + 56), 0);
    assert_eq!(&machine.calls[5].1[..4], &[123, 0, 0, 0]);
    assert_eq!(machine.get(Rsp), STACK);
}
#[test]
fn linux_stdio_dup_is_cloexec_and_avoids_standard_descriptors() {
    let mut machine = Machine::new(&[("syscall:72", 11)]);
    machine.put(REQUEST, 3);
    machine.put(REQUEST + 8, 0);
    assert_eq!(
        machine.execute(process::test_worker(TargetName::X86_64Linux)),
        Stop::Return
    );
    assert_eq!(&machine.calls[0].1[..3], &[0, 1030, 3]);
    assert_eq!(machine.word(REQUEST + 72), 11);
    assert_eq!(machine.get(Rsp), STACK);
}

#[test]
fn windows_spawn_limits_inheritance_and_releases_attribute_storage_on_every_exit() {
    for failure in 0..5 {
        let mut script = vec![
            ("InitializeProcThreadAttributeList", 0),
            ("GetProcessHeap", 900),
            ("HeapAlloc", if failure == 1 { 0 } else { 4096 }),
        ];
        if failure != 1 {
            script.push(("InitializeProcThreadAttributeList", u64::from(failure != 2)));
            if failure == 2 {
                script.push(("GetLastError", 5));
            } else {
                script.push(("UpdateProcThreadAttribute", u64::from(failure != 3)));
                if failure == 3 {
                    script.push(("GetLastError", 5));
                } else {
                    script.push(("CreateProcessW", u64::from(failure != 4)));
                    script.push(if failure == 4 {
                        ("GetLastError", 5)
                    } else {
                        ("CloseHandle", 1)
                    });
                }
                script.push(("DeleteProcThreadAttributeList", 0));
            }
            script.push(("HeapFree", 1));
        }
        let mut m = Machine::new(&script);
        m.put(REQUEST + 8, 1024);
        for (index, value) in [2048, 2200, 2400, 2600, 71, 72, 73].into_iter().enumerate() {
            m.put(1024 + u64::try_from(index).unwrap() * 8, value);
        }
        m.after_host = Some(|m, name| {
            let args = m.calls.last().unwrap().1.clone();
            match name {
                "InitializeProcThreadAttributeList" if args[0] == 0 => m.put(args[3], 128),
                "HeapAlloc" => assert_eq!(&args[..3], &[900, 0, 128]),
                "UpdateProcThreadAttribute" => {
                    assert_eq!(&args[..3], &[4096, 0, 0x20002]);
                    assert_eq!(args[4], 24);
                    assert_eq!(
                        [m.word(args[3]), m.word(args[3] + 8), m.word(args[3] + 16)],
                        [71, 72, 73]
                    );
                }
                "CreateProcessW" => {
                    assert_eq!(&args[..8], &[2048, 2200, 0, 0, 1, 0x80400, 2400, 2600]);
                    assert_eq!(m.word(args[8]), 112);
                    assert_eq!(m.word(args[8] + 104), 4096);
                    m.put(args[9], 0x123456789);
                    m.put(args[9] + 8, 0x234567890);
                    m.put(args[9] + 16, 123);
                }
                "CloseHandle" => assert_eq!(args[0], 0x234567890),
                "DeleteProcThreadAttributeList" => assert_eq!(args[0], 4096),
                "HeapFree" => assert_eq!(&args[..3], &[900, 0, 4096]),
                _ => (),
            }
        });
        assert_eq!(
            m.execute(process::test_worker(TargetName::X86_64Windows)),
            Stop::Return
        );
        assert_eq!(
            m.word(REQUEST + 80),
            if failure == 1 {
                8
            } else if failure != 0 {
                5
            } else {
                0
            }
        );
        assert_eq!(
            m.word(1024 + 56),
            if failure == 0 { 0x123456789 } else { 0 }
        );
        assert_eq!(m.word(1024 + 64), if failure == 0 { 123 } else { 0 });
        assert_eq!(m.get(Rsp), STACK);
        assert_eq!(m.get(R12), 0x1234);
    }
}

#[test]
fn windows_pipe_creation_collects_pending_connect_before_releasing_stack() {
    let scripts = [
        vec![("CreateNamedPipeW", u64::MAX), ("GetLastError", 5)],
        vec![
            ("CreateNamedPipeW", 71),
            ("ConnectNamedPipe", 0),
            ("GetLastError", 5),
            ("CloseHandle", 1),
        ],
        vec![
            ("CreateNamedPipeW", 71),
            ("ConnectNamedPipe", 0),
            ("GetLastError", 997),
            ("CreateFileW", u64::MAX),
            ("GetLastError", 5),
            ("CancelIoEx", 1),
            ("GetOverlappedResult", 0),
            ("CloseHandle", 1),
        ],
        vec![
            ("CreateNamedPipeW", 71),
            ("ConnectNamedPipe", 0),
            ("GetLastError", 997),
            ("CreateFileW", 72),
            ("GetOverlappedResult", 0),
            ("GetLastError", 5),
            ("CloseHandle", 1),
            ("CloseHandle", 1),
        ],
        vec![
            ("CreateNamedPipeW", 71),
            ("ConnectNamedPipe", 0),
            ("GetLastError", 997),
            ("CreateFileW", 72),
            ("GetOverlappedResult", 1),
        ],
    ];
    for (index, script) in scripts.iter().enumerate() {
        let mut m = Machine::new(script);
        m.put(REQUEST, 1);
        m.put(REQUEST + 16, 1024);
        m.after_host = Some(|m, name| {
            let args = &m.calls.last().unwrap().1;
            if name == "CreateNamedPipeW" {
                assert_eq!(&args[..4], &[1024, 0x40080002, 8, 1]);
                assert_eq!(args[7], 0);
            }
            if name == "GetOverlappedResult" {
                assert_eq!(args[0], 71);
                assert_eq!(args[3], 1);
            }
        });
        assert_eq!(
            m.execute(process::test_worker(TargetName::X86_64Windows)),
            Stop::Return
        );
        assert_eq!(m.word(REQUEST + 80), if index == 4 { 0 } else { 5 });
        assert_eq!(m.word(REQUEST + 72), if index == 4 { 71 } else { 0 });
        let closed: Vec<_> = m
            .calls
            .iter()
            .filter(|(name, _)| name == "CloseHandle")
            .map(|(_, args)| args[0])
            .collect();
        assert_eq!(
            closed,
            match index {
                1 | 2 => vec![71],
                3 => vec![72, 71],
                _ => vec![],
            }
        );
        assert_eq!(m.get(Rsp), STACK);
    }
}

#[test]
fn linux_spawn_exec_error_is_reported_before_pid_publication_and_reaps_child() {
    let mut m = Machine::new(&[
        ("syscall:293", 0),
        ("syscall:57", 79),
        ("syscall:3", 0),
        ("syscall:0", 4),
        ("syscall:3", 0),
        ("syscall:61", 79),
    ]);
    m.put(REQUEST + 8, 1024);
    m.after_host = Some(|m, name| {
        let args = m.calls.last().unwrap().1.clone();
        match name {
            "syscall:293" => {
                m.memory[usize::try_from(args[0]).unwrap()..][..8]
                    .copy_from_slice(&[71i32.to_le_bytes(), 72i32.to_le_bytes()].concat());
            }
            "syscall:0" => {
                assert_eq!(args[0], 71);
                let at = usize::try_from(args[1]).unwrap();
                m.memory[at..at + 4].copy_from_slice(&2i32.to_le_bytes());
            }
            "syscall:61" => {
                assert_eq!(args[0], 79);
                assert_eq!(args[2], 0);
            }
            _ => (),
        }
    });
    assert_eq!(
        m.execute(process::test_worker(TargetName::X86_64Linux)),
        Stop::Return
    );
    assert_eq!(m.word(REQUEST + 80), 2);
    assert_eq!((m.word(1024 + 56), m.word(1024 + 64)), (0, 0));
    assert_eq!(
        m.calls
            .iter()
            .filter(|(name, _)| name == "syscall:3")
            .map(|(_, args)| args[0])
            .collect::<Vec<_>>(),
        [72, 71]
    );
    assert_eq!(m.get(Rsp), STACK);
}
