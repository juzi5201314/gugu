//! C 入口执行真实机器序列；只把 managed 调度执行替换为固定包读写。
use super::*;
use crate::lir::body::{Call, CallTarget, ForeignArgument, Type, ValueType};
use crate::lir::exports::{Export, Lane};
use crate::runtime::callback as q;

fn scalar_export() -> Export {
    let ty = ValueType::scalar(Type::I64);
    let base = crate::lir::exports::DATA_OFFSET;
    Export {
        name: "test_export".into(),
        adapter: [7; 32],
        call: Call {
            target: CallTarget::Instance([9; 32]),
            kind: crate::gir::body::CallKind::ForeignBridge,
            parameters: vec![ty; 2],
            results: vec![ty],
            may_unwind: false,
            may_suspend: false,
            may_allocate: false,
            captures_arguments: false,
            by_value: vec![],
            foreign_signature: None,
            foreign_arguments: (0..2)
                .map(|first| ForeignArgument {
                    first,
                    lanes: 1,
                    bytes: 8,
                    align: 8,
                    memory: false,
                })
                .collect(),
            sret: None,
            poll_free_leaf: false,
            scoped_arguments: Vec::new(),
        },
        arguments: vec![
            Lane {
                offset: base,
                bytes: 8,
                indirect: false,
            },
            Lane {
                offset: base + 16,
                bytes: 8,
                indirect: false,
            },
        ],
        results: vec![Lane {
            offset: base + 32,
            bytes: 8,
            indirect: false,
        }],
        result_offset: base + 32,
        result_bytes: 8,
        packet_bytes: base + 48,
    }
}

#[test]
fn c_export_large_aggregate_uses_target_sret_and_preserves_source() {
    let base = crate::lir::exports::DATA_OFFSET;
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let windows = target == TargetName::X86_64Windows;
        let pointer = ValueType::pointer(crate::lir::body::Provenance::Raw);
        let mut export = scalar_export();
        export.call.parameters = vec![pointer];
        export.call.results.clear();
        export.call.sret = Some((0, 24, [0; 32]));
        export.call.foreign_arguments = vec![ForeignArgument {
            first: 1,
            lanes: if windows { 1 } else { 3 },
            bytes: 24,
            align: 8,
            memory: true,
        }];
        export.arguments = vec![Lane {
            offset: crate::lir::exports::SRET_OFFSET,
            bytes: 8,
            indirect: false,
        }];
        if windows {
            export.call.parameters.push(pointer);
            export.call.by_value = vec![(1, [0; 32], 24)];
            export.arguments.push(Lane {
                offset: base,
                bytes: 24,
                indirect: true,
            });
        } else {
            for index in 0..3 {
                export.call.parameters.push(ValueType::scalar(Type::I64));
                export.arguments.push(Lane {
                    offset: base + index * 8,
                    bytes: 8,
                    indirect: false,
                });
            }
        }
        export.results.clear();
        export.result_offset = base + 32;
        export.result_bytes = 24;
        export.packet_bytes = base + 64;
        let mut machine = Machine::new(&[]);
        let source = if windows { 1536 } else { STACK + 8 };
        for (index, value) in [19, 23, 29].into_iter().enumerate() {
            machine.put(source + index as u64 * 8, value);
        }
        machine.put(2040, 0xcccc);
        machine.put(2072, 0xdddd);
        machine.set_reg(if windows { Rcx } else { Rdi }, 2048);
        if windows {
            machine.set_reg(Rdx, source);
        }
        machine.put(64 + q::ACCEPTING as u64, 1);
        machine.put(64 + q::PROCESSOR as u64, 1024);
        machine.managed = Some(|machine, symbol| {
            if *symbol == mangle::runtime_symbol("thread_id") {
                machine.set_reg(Rax, 91);
                return;
            }
            if *symbol == mangle::runtime_symbol("callback_control") {
                machine.set_reg(Rax, 64);
                return;
            }
            assert_eq!(*symbol, mangle::runtime_symbol("callback_queue"));
            let action = machine.get(Rcx);
            let node = machine.get(Rbx);
            let sequence = super::super::callback::queue().finish().sequence;
            assert_eq!(machine.run(&sequence), Stop::Return);
            assert_eq!(machine.get(Rax), 0);
            if action == q::SUBMIT as u64 {
                let base = crate::lir::exports::DATA_OFFSET;
                assert_eq!(machine.word(node + crate::lir::exports::SRET_OFFSET), 2048);
                for (index, value) in [19, 23, 29].into_iter().enumerate() {
                    assert_eq!(machine.word(node + base + index as u64 * 8), value);
                    machine.put(node + base + 32 + index as u64 * 8, value + 1);
                }
                let saved = machine.registers;
                machine.set_reg(Rax, 64);
                machine.set_reg(Rcx, q::TAKE as u64);
                assert_eq!(machine.run(&sequence), Stop::Return);
                machine.set_reg(Rax, 64);
                machine.set_reg(Rbx, node);
                machine.set_reg(Rcx, q::FINISH as u64);
                assert_eq!(machine.run(&sequence), Stop::Return);
                assert_eq!(machine.get(Rax), 0);
                machine.registers = saved;
            }
        });
        assert_eq!(
            machine.execute(super::super::exports::sequence(&export, target).unwrap()),
            Stop::Return
        );
        assert_eq!(machine.get(Rax), 2048);
        assert_eq!(machine.get(Rsp), STACK);
        for (index, value) in [19, 23, 29].into_iter().enumerate() {
            assert_eq!(machine.word(source + index as u64 * 8), value);
            assert_eq!(machine.word(2048 + index as u64 * 8), value + 1);
        }
        assert_eq!(machine.word(2040), 0xcccc);
        assert_eq!(machine.word(2072), 0xdddd);
    }
}

#[test]
fn c_export_waiter_runs_file_request_before_returning_to_c() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let windows = target == TargetName::X86_64Windows;
        let script = if windows {
            [("ReadFile", 1), ("Sleep", 0)]
        } else {
            [("syscall:0", 7), ("syscall:35", 0)]
        };
        let mut machine = Machine::new(&script);
        machine.put(48, u64::from(windows));
        machine.put(64 + q::ACCEPTING as u64, 1);
        machine.put(64 + q::PROCESSOR as u64, 1024);
        machine.managed = Some(|machine, symbol| {
            if *symbol == mangle::runtime_symbol("thread_id") {
                machine.set_reg(Rax, 91);
                return;
            }
            if *symbol == mangle::runtime_symbol("callback_control") {
                machine.set_reg(Rax, 64);
                return;
            }
            if *symbol == mangle::runtime_symbol("callback_file_worker") {
                let windows = machine.word(48) != 0;
                let request = machine.get(if windows { Rcx } else { Rdi });
                assert_eq!(request, machine.get(R12) + q::REQUEST as u64);
                let stack = machine.get(Rsp);
                machine.set_reg(Rsp, stack - 8); // 原生 call 的返回地址槽。
                let builder = if windows {
                    windows_worker()
                } else {
                    linux_file_worker(false)
                };
                assert_eq!(machine.run(&builder.finish().sequence), Stop::Return);
                assert_eq!(machine.get(Rsp), stack - 8);
                machine.set_reg(Rsp, stack);
                return;
            }
            assert_eq!(*symbol, mangle::runtime_symbol("callback_queue"));
            let action = machine.get(Rcx);
            let node = machine.get(Rbx);
            let sequence = super::super::callback::queue().finish().sequence;
            assert_eq!(machine.run(&sequence), Stop::Return);
            assert_eq!(machine.get(Rax), 0);
            if action == q::SUBMIT as u64 {
                let saved = machine.registers;
                machine.set_reg(Rax, 64);
                machine.set_reg(Rcx, q::TAKE as u64);
                assert_eq!(machine.run(&sequence), Stop::Return);
                assert_eq!(machine.get(Rax), node);
                let request = node + q::REQUEST as u64;
                machine.put(
                    request + OPERATION as u64,
                    if machine.word(48) != 0 { 2 } else { 0 },
                );
                machine.put(request + ARGUMENTS as u64, 44);
                machine.put(request + ARGUMENTS as u64 + 8, 512);
                machine.put(request + ARGUMENTS as u64 + 16, 16);
                machine.put(node + q::STATE as u64, q::REQUEST_READY as u64);
                machine.registers = saved;
            }
        });
        machine.after_host = Some(|machine, name| {
            if name != "Sleep" && name != "syscall:35" {
                return;
            }
            let node = machine.get(R12);
            assert_eq!(machine.word(node + q::STATE as u64), q::REQUEST_DONE as u64);
            assert_eq!(
                machine.word(node + q::REQUEST as u64 + VALUE as u64),
                if machine.word(48) != 0 { 1 } else { 7 }
            );
            let saved = machine.registers;
            machine.put(node + crate::lir::exports::DATA_OFFSET + 32, 42);
            machine.put(node + q::STATE as u64, q::RUNNING as u64);
            machine.set_reg(Rax, 64);
            machine.set_reg(Rbx, node);
            machine.set_reg(Rcx, q::FINISH as u64);
            assert_eq!(
                machine.run(&super::super::callback::queue().finish().sequence),
                Stop::Return
            );
            assert_eq!(machine.get(Rax), 0);
            machine.registers = saved;
        });
        assert_eq!(
            machine.execute(super::super::exports::sequence(&scalar_export(), target).unwrap()),
            Stop::Return
        );
        assert_eq!(machine.get(Rax), 42);
        assert_eq!(machine.get(Rsp), STACK);
        assert_eq!(machine.word(64 + q::EXTERNAL_THREADS as u64), 0);
        assert_eq!(machine.calls.len(), 2);
        assert_eq!(&machine.calls[0].1[..3], &[44, 512, 16]);
    }
}

#[test]
fn c_export_entry_preserves_native_context_and_returns_packet_result() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let mut machine = Machine::new(&[]);
        for (index, r) in [Rbp, Rbx, R12, R13, R14, R15, Rdi, Rsi]
            .into_iter()
            .enumerate()
        {
            machine.set_reg(r, 0x100 + index as u64);
        }
        let args = if target == TargetName::X86_64Linux {
            [Rdi, Rsi]
        } else {
            [Rcx, Rdx]
        };
        machine.set_reg(args[0], 17);
        machine.set_reg(args[1], 25);
        let saved = machine.registers;
        machine.put(64 + q::ACCEPTING as u64, 1);
        machine.put(64 + q::PROCESSOR as u64, 1024);
        machine.managed = Some(|machine, symbol| {
            if *symbol == mangle::runtime_symbol("thread_id") {
                machine.set_reg(Rax, 91);
                return;
            }
            if *symbol == mangle::runtime_symbol("callback_control") {
                machine.set_reg(Rax, 64);
                return;
            }
            assert_eq!(*symbol, mangle::runtime_symbol("callback_queue"));
            let action = machine.get(Rcx);
            let packet = machine.get(Rbx);
            assert_eq!(machine.get(Rsp) % 16, 0);
            let sequence = super::super::callback::queue().finish().sequence;
            assert_eq!(machine.run(&sequence), Stop::Return);
            assert_eq!(machine.get(Rax), 0);
            if action == q::SUBMIT as u64 {
                assert_eq!(machine.word(packet + q::ENTRY as u64), 0xfeed);
                assert_eq!(machine.word(packet + q::PACK as u64), packet);
                let base = crate::lir::exports::DATA_OFFSET;
                assert_eq!(machine.word(packet + base), 17);
                assert_eq!(machine.word(packet + base + 16), 25);
                let saved = machine.registers;
                machine.set_reg(Rax, 64);
                machine.set_reg(Rcx, q::TAKE as u64);
                assert_eq!(machine.run(&sequence), Stop::Return);
                assert_eq!(machine.get(Rax), packet);
                machine.put(packet + base + 32, 42);
                machine.set_reg(Rax, 64);
                machine.set_reg(Rbx, packet);
                machine.set_reg(Rcx, q::FINISH as u64);
                assert_eq!(machine.run(&sequence), Stop::Return);
                machine.registers = saved;
            }
        });
        let entry = super::super::exports::sequence(&scalar_export(), target).unwrap();
        assert_eq!(machine.execute(entry), Stop::Return);
        assert_eq!(machine.get(Rax), 42);
        assert_eq!(machine.get(Rsp), STACK);
        for r in [Rbp, Rbx, R12, R13, R14, R15, Rdi, Rsi] {
            assert_eq!(machine.get(r), saved[r as usize], "{target:?} {r:?}");
        }
        assert_eq!(machine.word(64 + q::REGISTERED as u64), 0);
    }
}
