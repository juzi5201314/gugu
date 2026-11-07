//! 解释生产指令序列，只有 syscall/IAT 使用固定脚本；不执行宿主线程或文件操作。
use super::*;
use crate::x64::{inst::Sequence, reg::Reg, table::FORMS};
use std::collections::VecDeque;

mod callback;
mod exports;
#[path = "protocol_tests/foreign.rs"]
mod foreign_tests;
mod gc_entry;
#[path = "protocol_tests/net.rs"]
mod network;

const REQUEST: u64 = 256;
const STACK: u64 = 8192 + 8;
#[derive(Debug, PartialEq)]
enum Stop {
    Return,
    Exit,
    Worker,
}
struct HostCall {
    name: &'static str,
    result: u64,
}
struct Machine {
    registers: [u64; 16],
    floats: [u64; 16],
    native: Option<fn(&mut Machine)>,
    after_host: Option<fn(&mut Machine, &str)>,
    memory: Vec<u8>,
    script: VecDeque<HostCall>,
    managed: Option<fn(&mut Machine, &Symbol)>,
    calls: Vec<(String, Vec<u64>)>,
}
impl Machine {
    fn new(script: &[(&'static str, u64)]) -> Self {
        let mut machine = Self {
            registers: [0; 16],
            floats: [0; 16],
            native: None,
            after_host: None,
            memory: vec![0; 16384],
            script: script
                .iter()
                .map(|&(name, result)| HostCall { name, result })
                .collect(),
            managed: None,
            calls: Vec::new(),
        };
        machine.set_reg(Rsp, STACK);
        machine.set_reg(Rax, REQUEST);
        machine.set_reg(Rcx, REQUEST);
        machine.set_reg(Rdi, REQUEST);
        machine.set_reg(R12, 0x1234);
        machine
    }
    fn get(&self, r: Gpr) -> u64 {
        self.registers[r as usize]
    }
    fn set_reg(&mut self, r: Gpr, value: u64) {
        self.registers[r as usize] = value;
    }
    fn word(&self, address: u64) -> u64 {
        u64::from_le_bytes(
            self.memory[address as usize..address as usize + 8]
                .try_into()
                .unwrap(),
        )
    }
    fn put(&mut self, address: u64, value: u64) {
        self.memory[address as usize..address as usize + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn address(&self, operand: &Operand) -> u64 {
        let Operand::Mem(mem) = operand else {
            panic!("预期基址操作数")
        };
        assert!(mem.index.is_none());
        let Some(Reg::Gpr(base)) = mem.base else {
            panic!("预期物理基址")
        };
        self.get(base).wrapping_add_signed(i64::from(mem.disp))
    }
    fn value(&self, operand: &Operand) -> u64 {
        match operand {
            Operand::Reg(Reg::Gpr(r)) => self.get(*r),
            Operand::Reg(Reg::Xmm(r)) => self.floats[*r as usize],
            Operand::Imm(value) => *value,
            Operand::Mem(_) => self.word(self.address(operand)),
            _ => panic!("未登记操作数 {operand:?}"),
        }
    }
    fn store(&mut self, operand: &Operand, value: u64) {
        match operand {
            Operand::Reg(Reg::Gpr(r)) => self.set_reg(*r, value),
            Operand::Reg(Reg::Xmm(r)) => self.floats[*r as usize] = value,
            Operand::Mem(_) => self.put(self.address(operand), value),
            _ => panic!("未登记写入 {operand:?}"),
        }
    }
    fn host(&mut self, name: &str, args: Vec<u64>, windows: bool) -> bool {
        let call = self.script.pop_front().expect("脚本外调用");
        assert_eq!(name, call.name);
        self.calls.push((name.into(), args));
        if windows {
            assert_eq!(self.get(Rsp) % 16, 0, "Win64 调用前栈对齐");
            for r in [Rcx, Rdx, R8, R9, R10, R11] {
                self.set_reg(r, 0xbad);
            }
        } else {
            self.set_reg(Rcx, 0xbad);
            self.set_reg(R11, 0xbad);
        }
        self.set_reg(Rax, call.result);
        if let Some(after_host) = self.after_host {
            after_host(self, name);
        }
        name == "ExitProcess" || name == "syscall:60" || name == "syscall:231"
    }
    fn run(&mut self, sequence: &Sequence) -> Stop {
        let mut pc = 0;
        let mut comparison = (0, 0);
        for _ in 0..1024 {
            let inst = &sequence.instructions[pc];
            let form = &FORMS[inst.form.index()];
            let ops = &inst.operands;
            pc += 1;
            match form.mnemonic {
                "xchg" => {
                    let (a, b) = (self.value(&ops[0]), self.value(&ops[1]));
                    self.store(&ops[0], b);
                    self.store(&ops[1], a);
                }
                "movzx" => {
                    let value = self.value(&ops[0])
                        & match form.operands[0] {
                            K::Rm8 => 0xff,
                            K::Rm16 => 0xffff,
                            _ => panic!("未登记 movzx 源宽度"),
                        };
                    self.store(&ops[1], value);
                }
                "mov" | "movsd" => {
                    let dst = form
                        .access
                        .iter()
                        .position(|a| *a == Access::Write)
                        .unwrap();
                    let mut value = self.value(&ops[1 - dst]);
                    if matches!(form.operands[dst], K::R32 | K::Rm32) {
                        value &= 0xffffffff;
                    }
                    self.store(&ops[dst], value);
                }
                "lea" => {
                    let address = match &ops[0] {
                        Operand::Mem(_) => self.address(&ops[0]),
                        Operand::Rip(_, _) => 0xfeed,
                        other => panic!("未登记 LEA 操作数 {other:?}"),
                    };
                    self.store(&ops[1], address);
                }
                "push" => {
                    let value = self.value(&ops[0]);
                    self.set_reg(Rsp, self.get(Rsp) - 8);
                    self.put(self.get(Rsp), value);
                }
                "pop" => {
                    let value = self.word(self.get(Rsp));
                    self.store(&ops[0], value);
                    self.set_reg(Rsp, self.get(Rsp) + 8);
                }
                "add" | "sub" | "and" | "or" | "shl" | "shr" => {
                    let (a, b) = (self.value(&ops[0]), self.value(&ops[1]));
                    let value = match form.mnemonic {
                        "add" => a.wrapping_add(b),
                        "sub" => a.wrapping_sub(b),
                        "or" => a | b,
                        "shl" => a.wrapping_shl((b & 63) as u32),
                        "shr" => a.wrapping_shr((b & 63) as u32),
                        _ => a & b,
                    };
                    self.store(&ops[0], value);
                }
                "imul" => self.store(
                    &ops[1],
                    self.value(&ops[0]).wrapping_mul(self.value(&ops[2])),
                ),
                "neg" => self.store(&ops[0], self.value(&ops[0]).wrapping_neg()),
                "test" => comparison = (self.value(&ops[0]) & self.value(&ops[1]), 0),
                "cmp" => comparison = (self.value(&ops[0]), self.value(&ops[1])),
                "jmp" | "je" | "jne" | "jl" | "jge" | "jae" => {
                    let (a, b) = comparison;
                    let take = match form.mnemonic {
                        "je" => a == b,
                        "jne" => a != b,
                        "jl" => (a as i64) < (b as i64),
                        "jae" => a >= b,
                        "jge" => (a as i64) >= (b as i64),
                        _ => true,
                    };
                    if take {
                        match ops[0] {
                            Operand::Label(label) => {
                                pc = sequence
                                    .labels
                                    .iter()
                                    .find(|entry| entry.label == label)
                                    .unwrap()
                                    .at as usize
                            }
                            Operand::Reloc(RelocTarget::Lir(ref symbol), _)
                                if *symbol == mangle::glue_symbol("memcpy") =>
                            {
                                self.managed.expect("未登记 memcpy 边界")(self, symbol);
                                return Stop::Return;
                            }
                            Operand::Reloc(_, _) => return Stop::Worker,
                            _ => panic!("未知跳转"),
                        }
                    }
                }
                "call" => {
                    if matches!(&ops[0], Operand::Mem(_)) {
                        self.native.expect("未登记 native 调用")(self);
                        continue;
                    }
                    if let Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32) = &ops[0] {
                        self.managed.expect("未登记 managed 调用")(self, symbol);
                        continue;
                    }
                    let Operand::Rip(RelocTarget::Lir(Symbol::External { name, .. }), _) = &ops[0]
                    else {
                        panic!("非 IAT 调用")
                    };
                    let name = name.strip_prefix("__gugu_iat_").unwrap();
                    let mut args = [Rcx, Rdx, R8, R9].map(|r| self.get(r)).to_vec();
                    args.extend((0..4).map(|index| self.word(self.get(Rsp) + 32 + index * 8)));
                    if self.host(name, args, true) {
                        return Stop::Exit;
                    }
                }
                "syscall" => {
                    let name = format!("syscall:{}", self.get(Rax));
                    let args = [Rdi, Rsi, Rdx, R10, R8, R9].map(|r| self.get(r)).to_vec();
                    let clone = self.get(Rax) == 56;
                    let stack = self.get(Rsi);
                    if self.host(&name, args, false) {
                        return Stop::Exit;
                    }
                    if clone && self.get(Rax) == 0 {
                        self.set_reg(Rsp, stack);
                    }
                }
                "ret" => return Stop::Return,
                other => panic!("未登记机器操作 {other}"),
            }
        }
        panic!("固定机器片段未在预算内结束")
    }
    fn execute(&mut self, builder: Builder) -> Stop {
        let sequence = builder.finish().sequence;
        assert!(!encode::assemble(&sequence).unwrap().bytes.is_empty());
        let stop = self.run(&sequence);
        assert!(self.script.is_empty(), "未消费的宿主调用");
        stop
    }
}

#[test]
fn clock_fragments_return_monotonic_nanoseconds() {
    let mut linux = Machine::new(&[("syscall:228", 0)]);
    linux.put(STACK - 16, 123);
    linux.put(STACK - 8, 456);
    assert_eq!(linux.execute(linux_clock()), Stop::Return);
    assert_eq!(&linux.calls[0].1[..2], &[7, STACK - 16]);
    assert_eq!(linux.get(Rax), 123_000_000_456);
    assert_eq!(linux.get(Rsp), STACK);
    let mut windows = Machine::new(&[("GetTickCount64", 1234)]);
    assert_eq!(windows.execute(windows_clock()), Stop::Return);
    assert_eq!(windows.get(Rax), 1_234_000_000);
    assert_eq!(windows.get(Rsp), STACK);
}

#[test]
fn region_fast_allocation_records_exact_payload_bytes() {
    use crate::runtime::{
        gc_metadata_contract::GC_BLOCK_BYTES, processor::PROCESSOR_REGION_PAYLOAD_BYTES_OFFSET,
    };
    let mut machine = Machine::new(&[]);
    machine.memory.resize(131072, 0);
    machine.set_reg(R15, 4096);
    machine.set_reg(R11, u64::from(GC_BLOCK_BYTES) + 80);
    machine.set_reg(R10, 7);
    let mut builder = Builder::new();
    crate::x64::lower::alloc::record_region_payload(&mut builder);
    ret(&mut builder);
    assert_eq!(machine.execute(builder), Stop::Return);
    assert_eq!(machine.word(u64::from(GC_BLOCK_BYTES) * 2 + 128), 1 << 4);
    assert_eq!(machine.word(u64::from(GC_BLOCK_BYTES) * 2 + 96), 7);
    assert_eq!(machine.word(u64::from(GC_BLOCK_BYTES) * 2 + 40), 1);
    assert_eq!(
        machine.word(4096 + PROCESSOR_REGION_PAYLOAD_BYTES_OFFSET as u64),
        7
    );
    assert_eq!(machine.get(R11), u64::from(GC_BLOCK_BYTES) + 80);
    assert_eq!(machine.get(R10), 7);
    let source = include_str!("../../../../gugu-frontend/resources/runtime/heap.gg");
    for (name, value) in [
        (
            "REGION_CONTROL_BYTES",
            crate::runtime::region_schema::REGION_CONTROL_BYTES,
        ),
        (
            "REGION_OBJECT_START_OFFSET",
            crate::runtime::region_schema::REGION_OBJECT_START_OFFSET,
        ),
        (
            "REGION_NEXT_OFFSET",
            crate::runtime::region_schema::REGION_NEXT_OFFSET,
        ),
        (
            "REGION_MARK_EPOCH_OFFSET",
            crate::runtime::region_schema::REGION_MARK_EPOCH_OFFSET,
        ),
        (
            "REGION_PIN_COUNT_OFFSET",
            crate::runtime::region_schema::REGION_PIN_COUNT_OFFSET,
        ),
        (
            "COORDINATOR_REGION_HEAD_OFFSET",
            crate::runtime::region_schema::COORDINATOR_REGION_HEAD_OFFSET,
        ),
        (
            "REGION_OBJECTS_OFFSET",
            crate::runtime::region_schema::REGION_OBJECTS_OFFSET,
        ),
        (
            "REGION_PAYLOAD_BYTES_OFFSET",
            crate::runtime::region_schema::REGION_PAYLOAD_BYTES_OFFSET,
        ),
    ] {
        assert!(
            source
                .lines()
                .any(|line| line == format!("pub const {name}: uint = {value}"))
        );
    }
}

#[test]
fn poll_lowering_checks_pressure_and_reaches_safepoint_slow() {
    for (flags, pressure, ready, used, limit, calls) in [
        (0, 0, 1, 101, 100, 0),
        (0, 1, 0, 101, 100, 0),
        (0, 1, 2, 101, 100, 0),
        (0, 2, 1, 101, 100, 0),
        (0, 1, 1, 100, 100, 0),
        (0, 1, 1, 99, 100, 0),
        (0, 1, 1, 101, 100, 0),
        (1, 1, 1, 101, 100, 1),
        (1, 0, 0, 0, 0, 1),
    ] {
        let mut machine = Machine::new(&[]);
        machine.set_reg(R15, 4096);
        for (offset, value) in [
            (0, flags),
            (3888, pressure),
            (3912, ready),
            (3928, used),
            (3952, limit),
        ] {
            machine.put(4096 + offset, value);
        }
        machine.managed = Some(|machine, symbol| {
            assert_eq!(*symbol, mangle::runtime_symbol("safepoint_slow"));
            machine.calls.push(("safepoint_slow".into(), vec![]));
        });
        let mut builder = Builder::new();
        crate::x64::lower::alloc::safepoint_poll(
            crate::x64::lower::LowerCtx::probe(TargetName::X86_64Linux),
            &mut builder,
        )
        .unwrap();
        ret(&mut builder);
        assert_eq!(machine.execute(builder), Stop::Return);
        assert_eq!(machine.calls.len(), calls);
        assert_eq!(machine.get(Rsp), STACK);
    }
}

#[test]
fn allocation_bitmap_sequence_preserves_bits_across_blocks() {
    use crate::runtime::gc_metadata_contract::GC_ARENA_BYTES;
    use crate::runtime::local_heap_schema::{HEAP_GRANULE_BYTES, HEAP_OBJECT_START_OFFSET};
    let mut m = Machine::new(&[]);
    let arena = GC_ARENA_BYTES;
    m.memory.resize((arena * 2 + 65536) as usize, 0);
    for offset in [0, 63 * 16, 64 * 16, 32768, GC_ARENA_BYTES - 32] {
        let payload = arena + offset + 16;
        let granule = offset / u64::from(HEAP_GRANULE_BYTES);
        let slot = arena * 2 + HEAP_OBJECT_START_OFFSET + granule / 64 * 8;
        m.put(slot, 2);
        m.set_reg(R11, payload);
        let mut builder = Builder::new();
        crate::x64::lower::alloc::mark_object_start(&mut builder);
        ret(&mut builder);
        assert_eq!(m.execute(builder), Stop::Return);
        assert_eq!(m.get(R11), payload);
        assert_eq!(m.word(slot), 2 | 1 << (granule % 64));
    }
    let source = include_str!("../../../../gugu-frontend/resources/runtime/heap.gg");
    assert!(source.lines().any(|line| line
        == format!("pub const ARENA_OBJECT_START_OFFSET: uint = {HEAP_OBJECT_START_OFFSET}")));
}

#[test]
fn native_start_preserves_request_and_reports_creation_failure() {
    for result in [0, 123, (-11i64) as u64] {
        let mut m = Machine::new(&[("syscall:56", result)]);
        m.set_reg(Rbx, 12000);
        let stop = m.execute(linux_start());
        assert_eq!(m.calls[0].1[..5], [0x350f00, 11984, REQUEST, REQUEST, 0]);
        assert_eq!(m.word(11984), REQUEST);
        if result == 0 {
            assert_eq!(stop, Stop::Worker);
            assert_eq!(m.get(Rdi), REQUEST);
        } else {
            assert_eq!(stop, Stop::Return);
            assert_eq!(m.get(Rax), if result == 123 { 0 } else { 11 });
        }
        assert_eq!(
            m.word(REQUEST + ERROR as u64),
            if result == (-11i64) as u64 { 11 } else { 0 }
        );
    }
    for handle in [0, 77] {
        let mut calls = vec![("CreateThread", handle)];
        if handle == 0 {
            calls.push(("GetLastError", 0xbad00000008));
        }
        let mut m = Machine::new(&calls);
        assert_eq!(m.execute(windows_start()), Stop::Return);
        assert_eq!(m.calls[0].1[..6], [0, 65536, 0xfeed, REQUEST, 0, 0]);
        assert_eq!(m.word(REQUEST + THREAD_HANDLE as u64), handle);
        assert_eq!(m.get(Rax), if handle == 0 { 8 } else { 0 });
        assert_eq!(m.get(Rsp), STACK);
        assert_eq!(m.get(R12), 0x1234);
    }
}

#[test]
fn linux_worker_publishes_result_before_thread_exit_and_poll_waits_for_tid() {
    for result in [3, (-28i64) as u64] {
        let mut m = Machine::new(&[("syscall:1", result), ("syscall:60", 0)]);
        m.put(REQUEST, 42);
        m.put(REQUEST + OPERATION as u64, 1);
        for index in 0..6 {
            m.put(REQUEST + ARGUMENTS as u64 + index * 8, 100 + index);
        }
        assert_eq!(m.execute(linux_worker()), Stop::Exit);
        assert_eq!(m.calls[0].1, [100, 101, 102, 103, 104, 105]);
        assert_eq!(m.word(REQUEST + VALUE as u64), result);
        assert_eq!(
            m.word(REQUEST + ERROR as u64),
            if result == 3 { 0 } else { 28 }
        );
        assert_eq!(m.word(REQUEST), 42, "线程代码不能伪造 CHILD_CLEARTID");
        m.set_reg(Rax, REQUEST);
        assert_eq!(m.execute(linux_poll()), Stop::Return);
        assert_eq!(m.get(Rax), 0);
        m.put(REQUEST, 0); // 内核已确认退出。
        m.set_reg(Rax, REQUEST);
        m.execute(linux_poll());
        assert_eq!(m.get(Rax), 1);
    }
}

#[test]
fn windows_worker_preserves_argument_lanes_and_normalizes_results() {
    for &(opcode, name, sentinel) in WINDOWS_CALLS {
        for failed in [false, true] {
            let result = if sentinel == -1 {
                if failed { u64::MAX } else { 0x1234567800000042 }
            } else if name == "CreateSymbolicLinkW" {
                0x12345678ffffff00 | u64::from(!failed)
            } else {
                0x1234567800000000 | u64::from(!failed)
            };
            let mut calls = vec![(name, result)];
            if failed {
                calls.push(("GetLastError", 0xbad00000005));
            }
            let mut m = Machine::new(&calls);
            m.put(REQUEST + OPERATION as u64, opcode as u64);
            for index in 0..8 {
                let offset = if index < 6 {
                    ARGUMENTS as u64 + index * 8
                } else {
                    EXTRA_ARGUMENTS as u64 + (index - 6) * 8
                };
                m.put(REQUEST + offset, 100 + index);
            }
            assert_eq!(m.execute(windows_worker()), Stop::Return);
            assert_eq!(
                m.calls[0].1,
                [100, 101, 102, 103, 104, 105, 106, 107],
                "{name}"
            );
            let normalized = if sentinel == -1 {
                result
            } else {
                u64::from(!failed)
            };
            assert_eq!(m.word(REQUEST + VALUE as u64), normalized, "{name}");
            assert_eq!(
                m.word(REQUEST + ERROR as u64),
                if failed { 5 } else { 0 },
                "{name}"
            );
            assert_eq!(m.get(Rax), 0);
            assert_eq!(m.get(R12), 0x1234);
            assert_eq!(m.get(Rsp), STACK);
        }
    }
}

#[test]
fn unknown_native_operations_do_not_call_file_apis() {
    let mut linux = Machine::new(&[("syscall:60", 0)]);
    linux.put(REQUEST + OPERATION as u64, 999);
    assert_eq!(linux.execute(linux_worker()), Stop::Exit);
    assert_eq!(linux.word(REQUEST + ERROR as u64), 22);
    let mut windows = Machine::new(&[]);
    windows.put(REQUEST + OPERATION as u64, 999);
    assert_eq!(windows.execute(windows_worker()), Stop::Return);
    assert_eq!(windows.word(REQUEST + ERROR as u64), 87);
}

#[test]
fn windows_poll_distinguishes_timeout_completion_and_native_failure() {
    for (wait, close, expected) in [
        (258, None, Stop::Return),
        (0, Some(1), Stop::Return),
        (0xdeadbeef00000102, None, Stop::Return),
        (0xdeadbeef00000000, Some(0xdeadbeef00000001), Stop::Return),
        (0xffffffff, None, Stop::Exit),
        (0, Some(0), Stop::Exit),
        (0, Some(0xdeadbeef00000000), Stop::Exit),
    ] {
        let mut calls = vec![("WaitForSingleObject", wait)];
        if let Some(close) = close {
            calls.push(("CloseHandle", close));
        }
        if expected == Stop::Exit {
            calls.push(("ExitProcess", 0));
        }
        let mut machine = Machine::new(&calls);
        machine.put(REQUEST + THREAD_HANDLE as u64, 77);
        machine.put(REQUEST + bridge::NATIVE_REQUEST_OWNER as u64, 128);
        assert_eq!(machine.execute(windows_poll()), expected);
        assert_eq!(&machine.calls[0].1[..2], &[77, 0]);
        let success = wait as u32 == 0 && close.is_some_and(|value| value as u32 != 0);
        assert_eq!(
            machine.word(REQUEST + THREAD_HANDLE as u64),
            if success { 0 } else { 77 }
        );
        assert_eq!(
            machine.word(REQUEST + bridge::NATIVE_REQUEST_OWNER as u64),
            128
        );
        if expected == Stop::Return {
            assert_eq!(machine.get(Rax), u64::from(success));
            assert_eq!(machine.get(Rsp), STACK);
            assert_eq!(machine.get(R12), 0x1234);
        } else {
            assert_eq!(machine.calls.last().unwrap().1[0], 2);
        }
    }
}
