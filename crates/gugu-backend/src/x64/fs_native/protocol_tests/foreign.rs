//! 固定 C 边界脚本执行生产 worker 指令；不建立真实宿主线程。
use super::*;
use crate::runtime::foreign_packet as packet;
const PACKET: u64 = 1024;
const ERRNO: u64 = 512;

fn c_runtime(machine: &mut Machine, symbol: &Symbol) {
    let Symbol::External { name, .. } = symbol else {
        panic!("预期 C 运行时符号")
    };
    assert_eq!(machine.get(Rsp) % 16, 0);
    let args = [Rdi, Rsi, Rdx, Rcx, R8, R9]
        .map(|r| machine.get(r))
        .to_vec();
    machine.host(name, args, false);
    for r in [Rdi, Rsi, Rdx, Rcx, R8, R9, R10, R11] {
        machine.set_reg(r, 0xbad);
    }
    machine.floats.fill(0xbad);
}

#[test]
fn native_packet_narrow_results_discard_undefined_high_bits() {
    use crate::lir::body::Type;
    for (ty, expected) in [
        (Type::I8, 0x81),
        (Type::I16, 0x7781),
        (Type::I32, 0x44557781),
    ] {
        let mut machine = Machine::new(&[]);
        machine.put(STACK + 128, 0xbadc0ffe44557781);
        let mut builder = Builder::new();
        crate::x64::lower::load_stack(&mut builder, 128, gpr(Rax), ty).unwrap();
        ret(&mut builder);
        assert_eq!(machine.execute(builder), Stop::Return);
        assert_eq!(machine.get(Rax), expected);
    }
}

#[test]
fn stack_copy_tail_target_is_the_registered_memory_glue() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let blobs = crate::x64::fragments::registered_fragments(target);
        let copy = blobs
            .iter()
            .find(|blob| {
                blob.entries
                    .iter()
                    .any(|entry| entry.glue == "stack_copy_raw")
            })
            .unwrap();
        let memcpy = blobs
            .iter()
            .find(|blob| blob.entries.iter().any(|entry| entry.glue == "memcpy"))
            .unwrap();
        let symbol = match memcpy.domain {
            FragmentDomain::Runtime => mangle::runtime_symbol("memcpy"),
            FragmentDomain::Glue => mangle::glue_symbol("memcpy"),
        };
        assert_eq!(copy.relocations.len(), 1);
        assert_eq!(copy.relocations[0].target, RelocTarget::Lir(symbol));
    }
}

#[test]
fn high_relative_stack_primitives_use_current_bounds_without_growing() {
    for high in [4000, 4512] {
        let mut machine = Machine::new(&[]);
        machine.set_reg(R14, REQUEST);
        machine.put(REQUEST + 80, high);
        machine.set_reg(Rax, 96);
        machine.put(high - 96, 0x123456789abcdef0);
        assert_eq!(machine.execute(stack::read_word()), Stop::Return);
        assert_eq!(machine.get(Rax), 0x123456789abcdef0);
        assert_eq!(machine.get(Rsp), STACK);
        for to_stack in [0, 1] {
            for length in [0, 1, 7, 8, 17] {
                let mut machine = Machine::new(&[]);
                machine.set_reg(R14, REQUEST);
                machine.put(REQUEST + 80, high);
                machine.set_reg(Rax, 96);
                machine.set_reg(Rbx, 1024);
                machine.set_reg(Rcx, length);
                machine.set_reg(Rdx, to_stack);
                let (source, destination) = if to_stack == 0 {
                    (high - 96, 1024)
                } else {
                    (1024, high - 96)
                };
                machine.memory[source as usize..(source + length) as usize].fill(0x5a);
                machine.memory[destination as usize - 1..(destination + length + 1) as usize]
                    .fill(0xcc);
                machine.managed = Some(|machine, _| {
                    assert_eq!(machine.get(Rsp), STACK, "复制原语不增长协程栈");
                    let source = machine.get(Rbx) as usize;
                    let destination = machine.get(Rax) as usize;
                    let length = machine.get(Rcx) as usize;
                    machine
                        .memory
                        .copy_within(source..source + length, destination);
                });
                assert_eq!(machine.execute(stack::copy_raw()), Stop::Return);
                assert!(
                    machine.memory[destination as usize..(destination + length) as usize]
                        .iter()
                        .all(|byte| *byte == 0x5a)
                );
                assert_eq!(machine.memory[destination as usize - 1], 0xcc);
                assert_eq!(machine.memory[(destination + length) as usize], 0xcc);
            }
        }
    }
}

#[test]
fn pthread_creation_and_join_preserve_credit_until_native_exit() {
    for status in [0, 11] {
        let mut machine = Machine::new(&[
            ("pthread_attr_init", 0),
            ("pthread_attr_setstack", 0),
            ("pthread_create", status),
            ("pthread_attr_destroy", 0),
        ]);
        machine.set_reg(Rbx, 131072);
        machine.managed = Some(c_runtime);
        machine.put(REQUEST + OPERATION as u64, u64::MAX);
        machine.put(REQUEST + THREAD_HANDLE as u64, 123);
        assert_eq!(machine.execute(linux_start()), Stop::Return);
        assert_eq!(&machine.calls[1].1[..3], &[STACK - 88, 65536, 65536]);
        assert_eq!(
            &machine.calls[2].1[..4],
            &[REQUEST + THREAD_HANDLE as u64, STACK - 88, 0xfeed, REQUEST]
        );
        assert_eq!(machine.get(Rax), status);
        assert_eq!(machine.word(REQUEST + ERROR as u64), status);
        assert_eq!(machine.get(Rsp), STACK);
        assert_eq!(machine.get(R12), 0x1234);
    }
    for (status, completed) in [(16, false), (0, true)] {
        let mut machine = Machine::new(&[("pthread_tryjoin_np", status)]);
        machine.managed = Some(c_runtime);
        machine.put(REQUEST + OPERATION as u64, u64::MAX);
        machine.put(REQUEST + THREAD_HANDLE as u64, 123);
        machine.put(REQUEST + bridge::NATIVE_REQUEST_OWNER as u64, 777);
        assert_eq!(machine.execute(linux_poll()), Stop::Return);
        assert_eq!(&machine.calls[0].1[..2], &[123, 0]);
        assert_eq!(machine.get(Rax), u64::from(completed));
        assert_eq!(
            machine.word(REQUEST + THREAD_HANDLE as u64),
            if completed { 0 } else { 123 }
        );
        assert_eq!(
            machine.word(REQUEST + bridge::NATIVE_REQUEST_OWNER as u64),
            777
        );
    }
    let mut failed = Machine::new(&[("pthread_tryjoin_np", 3), ("syscall:231", 0)]);
    failed.managed = Some(c_runtime);
    failed.put(REQUEST + OPERATION as u64, u64::MAX);
    failed.put(REQUEST + THREAD_HANDLE as u64, 123);
    assert_eq!(failed.execute(linux_poll()), Stop::Exit);
    assert_eq!(failed.word(REQUEST + THREAD_HANDLE as u64), 123);
}

fn native(machine: &mut Machine) {
    assert_eq!(machine.get(Rsp) % 16, 0);
    let windows = machine.word(PACKET + packet::FOREIGN_PACKET_TARGET as u64) == 2;
    let registers: &[Gpr] = if windows {
        &[Rcx, Rdx, R8, R9]
    } else {
        &[Rdi, Rsi, Rdx, Rcx, R8, R9]
    };
    for (index, &r) in registers.iter().enumerate() {
        assert_eq!(machine.get(r), 100 + index as u64);
    }
    let float_registers = if windows { 4 } else { 8 };
    for (index, &value) in machine.floats[..float_registers].iter().enumerate() {
        assert_eq!(value, 0x3ff0000000000000 + index as u64);
    }
    let bytes = machine.word(PACKET + packet::FOREIGN_PACKET_STACK_BYTES as u64);
    for offset in (0..bytes).step_by(8) {
        assert_eq!(machine.word(machine.get(Rsp) + offset), 500 + offset);
    }
    assert_eq!(machine.word(REQUEST), 91, "native 调用期发布真实线程身份");
    machine.set_reg(Rax, 0x123456789abcdef0);
    machine.set_reg(Rdx, 0x9876543210123456);
    machine.floats[0] = 0x7ff8000000000001;
    machine.floats[1] = 0x8000000000000000;
    machine.put(ERRNO, 0xbad0000001c);
    machine.calls.push(("native".into(), vec![]));
}

#[test]
fn foreign_worker_copies_abi_lanes_and_captures_error_before_return() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        for stack_bytes in [0, 32, 48] {
            let windows = target == TargetName::X86_64Windows;
            if windows && stack_bytes == 0 {
                continue;
            }
            let script = if windows {
                ("GetLastError", 0xbad00000005)
            } else {
                ("__errno_location", ERRNO)
            };
            let mut machine = Machine::new(&[
                if windows {
                    ("GetCurrentThreadId", 91)
                } else {
                    ("syscall:186", 91)
                },
                script,
            ]);
            machine.managed = Some(c_runtime);
            machine.native = Some(native);
            machine.put(REQUEST + ARGUMENTS as u64, PACKET);
            machine.put(PACKET, if windows { 2 } else { 1 });
            machine.put(PACKET + 8, stack_bytes);
            for index in 0..6 {
                machine.put(PACKET + 16 + index * 8, 100 + index);
            }
            for index in 0..8 {
                machine.put(PACKET + 64 + index * 8, 0x3ff0000000000000 + index);
            }
            for offset in (0..stack_bytes).step_by(8) {
                machine.put(
                    PACKET + packet::FOREIGN_PACKET_BYTES as u64 + offset,
                    500 + offset,
                );
            }
            for r in [Rbp, R13, R14, R15] {
                machine.set_reg(r, 0x100 + r as u64);
            }
            assert_eq!(machine.execute(foreign::worker(target)), Stop::Return);
            assert_eq!(machine.get(Rsp), STACK);
            assert_eq!(machine.get(R12), 0x1234);
            for r in [Rbp, R13, R14, R15] {
                assert_eq!(machine.get(r), 0x100 + r as u64);
            }
            for (index, value) in [
                0x123456789abcdef0,
                0x9876543210123456,
                0x7ff8000000000001,
                0x8000000000000000,
            ]
            .into_iter()
            .enumerate()
            {
                assert_eq!(machine.word(PACKET + 128 + index as u64 * 8), value);
            }
            assert_eq!(machine.word(PACKET + 160), if windows { 5 } else { 28 });
            assert_eq!(machine.word(REQUEST + ERROR as u64), 0);
            assert_eq!(machine.word(REQUEST), 0, "退出前撤销可借用线程身份");
        }
    }
}
