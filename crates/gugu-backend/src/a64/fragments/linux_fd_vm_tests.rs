use std::collections::BTreeMap;

use super::*;
use crate::a64::reg::RegKind;

pub(super) const REQUEST: u64 = 0x1000;
const STACK: u64 = 0x8000;

pub(super) struct Vm {
    pub registers: [u64; 32],
    memory: BTreeMap<u64, u8>,
    equal: bool,
    nonnegative: bool,
}

impl Vm {
    pub fn new(operation: u64, arguments: [u64; 6]) -> Self {
        let mut vm = Self {
            registers: [0; 32],
            memory: BTreeMap::new(),
            equal: false,
            nonnegative: false,
        };
        vm.registers[0] = REQUEST;
        vm.registers[31] = STACK;
        for i in 19..31 {
            vm.registers[i] = 0x10000 + i as u64;
        }
        vm.put(REQUEST + 8, operation, 8);
        vm.put(REQUEST + 72, 999, 8);
        for (i, value) in arguments.into_iter().enumerate() {
            vm.put(REQUEST + 16 + i as u64 * 8, value, 8);
        }
        vm
    }

    pub fn put(&mut self, address: u64, value: u64, size: usize) {
        for (i, byte) in value.to_le_bytes()[..size].iter().enumerate() {
            self.memory.insert(address + i as u64, *byte);
        }
    }

    pub fn read(&self, address: u64, size: usize) -> u64 {
        (0..size).fold(0, |value, i| {
            value | (u64::from(*self.memory.get(&(address + i as u64)).unwrap_or(&0)) << (i * 8))
        })
    }

    fn get(&self, operand: &Operand) -> u64 {
        let Operand::Reg(register) = operand else {
            panic!("not a register")
        };
        match register.kind {
            RegKind::Gpr(n) => self.registers[n as usize],
            RegKind::Sp => self.registers[31],
            RegKind::Zero => 0,
            _ => panic!("not a GPR"),
        }
    }

    fn set(&mut self, operand: &Operand, value: u64) {
        let Operand::Reg(register) = operand else {
            panic!("not a register")
        };
        let value = if register.width == Width::W {
            value & 0xffff_ffff
        } else {
            value
        };
        match register.kind {
            RegKind::Gpr(n) => self.registers[n as usize] = value,
            RegKind::Sp => self.registers[31] = value,
            RegKind::Zero => {}
            _ => panic!("not a GPR"),
        }
    }

    pub fn run(&mut self, code: &[Inst], mut syscall: impl FnMut(&mut Self)) {
        assert_eq!(self.run_result(code, &mut syscall), 0);
    }

    pub fn run_result(&mut self, code: &[Inst], mut syscall: impl FnMut(&mut Self)) -> u64 {
        let mut pc = 0;
        let saved = self.registers[19..31].to_vec();
        for _ in 0..2048 {
            let instruction = &code[pc / 4];
            let args = &instruction.operands;
            let mut next = pc + 4;
            match form(instruction) {
                "b" => next = jump(pc, &args[0]),
                "b.cond" if self.condition(&args[1]) => next = jump(pc, &args[0]),
                "b.cond" => {}
                "svc" => syscall(self),
                "adr" => self.set(&args[0], jump(pc, &args[1]) as u64),
                "ret" => {
                    assert_eq!(self.registers[19..31], saved);
                    assert_eq!(self.registers[31], STACK);
                    return self.registers[0];
                }
                key if key.starts_with("ldr") || key.starts_with("str") => self.transfer(key, args),
                key => self.calculate(key, args),
            }
            pc = next;
        }
        panic!("fd dispatcher did not return")
    }

    fn condition(&self, operand: &Operand) -> bool {
        match operand {
            Operand::Condition(0) => self.equal,
            Operand::Condition(1) => !self.equal,
            Operand::Condition(10) => self.nonnegative,
            Operand::Condition(13) => !self.nonnegative || self.equal,
            _ => panic!("unknown condition"),
        }
    }

    fn transfer(&mut self, key: &str, args: &[Operand]) {
        let Operand::Address(Address { base, ref mode }) = args[1] else {
            panic!("address")
        };
        let (AddressMode::Unsigned(offset) | AddressMode::Unscaled(offset)) = mode else {
            panic!("address mode")
        };
        let address = self.get(&reg(base)).wrapping_add(*offset as u64);
        let size = if key.contains(".W.") || key.starts_with("ldrsw.") {
            4
        } else {
            8
        };
        if key.starts_with("ldr") {
            let value = self.read(address, size);
            self.set(
                &args[0],
                if key.starts_with("ldrsw.") {
                    value as i32 as i64 as u64
                } else {
                    value
                },
            );
        } else {
            self.put(address, self.get(&args[0]), size);
        }
    }

    fn calculate(&mut self, key: &str, args: &[Operand]) {
        match key {
            "add.X.imm" => self.set(&args[0], self.get(&args[1]).wrapping_add(addend(&args[2]))),
            "sub.X.imm" => self.set(&args[0], self.get(&args[1]).wrapping_sub(addend(&args[2]))),
            "sub.X.shift" => self.set(
                &args[0],
                self.get(&args[1]).wrapping_sub(self.get(&args[2])),
            ),
            "subs.X.imm" => {
                let (left, right) = (self.get(&args[1]), addend(&args[2]));
                self.equal = left == right;
                self.nonnegative = (left as i64) >= right as i64;
                self.set(&args[0], left.wrapping_sub(right));
            }
            "movz.X" | "movn.X" => {
                let Operand::Wide { imm16, shift } = args[1] else {
                    panic!("wide")
                };
                let value = u64::from(imm16) << shift;
                self.set(&args[0], if key == "movn.X" { !value } else { value });
            }
            "ubfm.X" | "bfm.X" | "sbfm.X" => self.bitfield(key, args),
            _ => panic!("unsupported fd instruction {key}"),
        }
    }

    fn bitfield(&mut self, key: &str, args: &[Operand]) {
        let (Operand::Imm(r), Operand::Imm(s)) = (&args[2], &args[3]) else {
            panic!("bitfield")
        };
        let source = self.get(&args[1]);
        let value = if key != "bfm.X" {
            let width = s - r + 1;
            let value = (source >> r) & ((1 << width) - 1);
            if key == "sbfm.X" {
                ((value << (64 - width)) as i64 >> (64 - width)) as u64
            } else {
                value
            }
        } else {
            let mask = (1 << (s + 1)) - 1;
            (self.get(&args[0]) & !(mask << (64 - r))) | ((source & mask) << (64 - r))
        };
        self.set(&args[0], value);
    }
}

fn addend(operand: &Operand) -> u64 {
    let Operand::AddImm {
        imm12,
        shift12: false,
    } = operand
    else {
        panic!("addend")
    };
    u64::from(*imm12)
}

fn jump(pc: usize, operand: &Operand) -> usize {
    let Operand::Target(Target::Offset(delta)) = operand else {
        panic!("jump")
    };
    (pc as i64 + delta) as usize
}
