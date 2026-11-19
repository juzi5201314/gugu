use super::*;
use inst::{AddressMode, Operand, Target};
use reg::{Reg, RegKind, Width};
use std::collections::BTreeMap;

fn get(reg: Reg, registers: &[u64; 32]) -> u64 {
    let value = match reg.kind {
        RegKind::Gpr(id) => registers[id as usize],
        RegKind::Sp => registers[31],
        RegKind::Zero => 0,
        _ => panic!("not a GPR"),
    };
    if reg.width == Width::W {
        value & u64::from(u32::MAX)
    } else {
        value
    }
}
fn put(reg: Reg, value: u64, registers: &mut [u64; 32]) {
    let value = if reg.width == Width::W {
        value & u64::from(u32::MAX)
    } else {
        value
    };
    match reg.kind {
        RegKind::Gpr(id) => registers[id as usize] = value,
        RegKind::Sp => registers[31] = value,
        RegKind::Zero => {}
        _ => panic!("not a GPR"),
    }
}
fn rr(operand: &Operand) -> Reg {
    let Operand::Reg(reg) = operand else {
        panic!("expected register")
    };
    *reg
}

pub(super) fn run(fragment: &codegen::LeafFragment, args: &[u64]) -> [u64; 2] {
    run_linked(fragment, &[], args)
}

pub(super) fn link(fragments: &[&codegen::LeafFragment]) -> Result<Vec<inst::Inst>, String> {
    let mut symbols = BTreeMap::new();
    let mut instructions = Vec::new();
    for fragment in fragments {
        if symbols
            .insert(fragment.symbol.clone(), instructions.len() * 4)
            .is_some()
        {
            return Err("duplicate fragment symbol".into());
        }
        instructions.extend(decoded(fragment));
    }
    let mut base = 0;
    for fragment in fragments {
        for fixup in &fragment.relocations {
            let relocation = &fixup.relocation;
            let target = *symbols
                .get(&relocation.target)
                .ok_or("unknown fragment symbol")?;
            let pc = base + usize::try_from(fixup.offset).map_err(|_| "invalid offset")?;
            if !pc.is_multiple_of(4) || fixup.offset >= fragment.bytes.len() as u64 {
                return Err("invalid relocation offset".into());
            }
            let key = match relocation.kind {
                inst::RelocKind::Call26 => "bl",
                inst::RelocKind::Branch26 => "b",
                _ => return Err("unsupported test relocation".into()),
            };
            let displacement = target as i64 + relocation.addend - pc as i64;
            let patched =
                machine::inst(key, vec![Operand::Target(Target::Offset(displacement))]).unwrap();
            let encoded = encode::encode(&patched).map_err(|error| error.to_string())?;
            instructions[pc / 4] = decode::decode(encoded.word).unwrap();
        }
        base += fragment.bytes.len();
    }
    Ok(instructions)
}

pub(super) fn run_linked(
    fragment: &codegen::LeafFragment,
    callees: &[&codegen::LeafFragment],
    args: &[u64],
) -> [u64; 2] {
    execute(fragment, callees, args).0
}

pub(super) fn execute(
    fragment: &codegen::LeafFragment,
    callees: &[&codegen::LeafFragment],
    args: &[u64],
) -> ([u64; 2], BTreeMap<u64, u8>) {
    execute_inner(fragment, callees, args, None)
}

pub(super) fn execute_inner(
    fragment: &codegen::LeafFragment,
    callees: &[&codegen::LeafFragment],
    args: &[u64],
    mut runtime: Option<&mut super::runtime_vm::Harness>,
) -> ([u64; 2], BTreeMap<u64, u8>) {
    let fragments = std::iter::once(fragment)
        .chain(callees.iter().copied())
        .collect::<Vec<_>>();
    let instructions = link(&fragments).unwrap();
    let mut registers = [0_u64; 32];
    const INITIAL_SP: u64 = 1 << 40;
    registers[31] = INITIAL_SP;
    for (id, value) in registers.iter_mut().enumerate().take(31).skip(19) {
        *value = 1000 + id as u64;
    }
    registers[30] = u64::MAX;
    if let Some(runtime) = runtime.as_mut() {
        runtime.setup(&mut registers);
    }
    let saved = registers;
    let mut calls = Vec::new();
    let mut memory = BTreeMap::new();
    for (index, value) in args.iter().enumerate() {
        if index < 8 {
            registers[index] = *value;
        } else {
            for byte in 0..8 {
                memory.insert(
                    registers[31] + (index as u64 - 8) * 8 + byte,
                    (value >> (byte * 8)) as u8,
                );
            }
        }
    }
    if let Some(runtime) = runtime.as_mut() {
        runtime.initialize(&registers, &mut memory);
    }
    let mut flags = (false, false, false, false);
    let condition = |code: u8, (n, z, c, v): (bool, bool, bool, bool)| -> bool {
        match code {
            0 => z,
            1 => !z,
            2 => c,
            3 => !c,
            4 => n,
            5 => !n,
            6 => v,
            7 => !v,
            8 => c && !z,
            9 => !c || z,
            10 => n == v,
            11 => n != v,
            12 => !z && n == v,
            13 => z || n != v,
            14 => true,
            _ => false,
        }
    };
    let mut pc = 0_usize;
    for _ in 0..1000 {
        let inst = &instructions[pc / 4];
        let form = table::get(inst.form).unwrap();
        let key = form.key;
        let op = &inst.operands;
        assert_eq!(registers[31] % 16, 0, "SP alignment");
        if key == "ret" {
            let target = get(rr(&op[0]), &registers);
            if let Some((return_pc, before)) = calls.pop() {
                let before: [u64; 32] = before;
                assert_eq!(target, return_pc);
                assert_eq!(registers[31], before[31]);
                assert_eq!(&registers[19..30], &before[19..30]);
                pc = target as usize;
                continue;
            }
            assert_eq!(target, u64::MAX);
            for id in 19..31 {
                if runtime.is_some() && matches!(id, 27 | 28) {
                    continue;
                }
                assert_eq!(registers[id], saved[id], "callee-saved x{id}");
            }
            assert_eq!(
                registers[31],
                runtime.as_ref().map_or(INITIAL_SP, |r| r.expected_sp)
            );
            return ([registers[0], registers[1]], memory);
        }
        let mut next = pc + 4;
        if key == "bl" {
            if let Some(runtime) = runtime.as_mut()
                && runtime.call(fragment, pc, &mut registers, &mut memory, args.len())
            {
                pc += 4;
                continue;
            }
            calls.push(((pc + 4) as u64, registers));
            registers[30] = (pc + 4) as u64;
        }
        let read = |index: usize| get(rr(&op[index]), &registers);
        if key == "bl" || key == "b" || key == "b.cond" || key.starts_with("cb") {
            let offset = op
                .iter()
                .find_map(|o| match o {
                    Operand::Target(Target::Offset(n)) => Some(*n),
                    _ => None,
                })
                .unwrap();
            let taken = if key == "b" || key == "bl" {
                true
            } else if key == "b.cond" {
                let Operand::Condition(c) = op[1] else {
                    panic!()
                };
                condition(c, flags)
            } else if key.starts_with("cbnz") {
                read(0) != 0
            } else {
                read(0) == 0
            };
            if taken {
                next = (pc as i64 + offset) as usize;
            }
        } else if key.starts_with("movz") || key.starts_with("movn") || key.starts_with("movk") {
            let Operand::Wide { imm16, shift } = op[1] else {
                panic!()
            };
            let immediate = u64::from(imm16) << shift;
            let value = if key.starts_with("movn") {
                !immediate
            } else if key.starts_with("movk") {
                (read(0) & !(0xffff_u64 << shift)) | immediate
            } else {
                immediate
            };
            put(rr(&op[0]), value, &mut registers);
        } else if key.starts_with("ldr") || key.starts_with("ldar") || key.starts_with("str") {
            let Operand::Address(address) = &op[1] else {
                panic!()
            };
            let displacement = match address.mode {
                AddressMode::Unsigned(offset) => offset,
                AddressMode::Base => 0,
                _ => panic!(),
            };
            let at = get(address.base, &registers).wrapping_add_signed(displacement);
            let bytes = if key.starts_with("ldrb") || key.starts_with("strb") {
                1
            } else if key.starts_with("ldrh") || key.starts_with("strh") {
                2
            } else if rr(&op[0]).width == Width::W {
                4
            } else {
                8
            };
            if key.starts_with("ldr") || key.starts_with("ldar") {
                if let Some(runtime) = runtime.as_mut() {
                    runtime.load(fragment, at, &registers, &memory);
                }
                let value = (0..bytes).fold(0_u64, |value, byte| {
                    value | (u64::from(*memory.get(&(at + byte)).unwrap_or(&0)) << (byte * 8))
                });
                put(rr(&op[0]), value, &mut registers);
            } else {
                let value = read(0);
                for byte in 0..bytes {
                    memory.insert(at + byte, (value >> (byte * 8)) as u8);
                }
            }
        } else {
            let a = read(1);
            let b = match op.get(2) {
                Some(Operand::Reg(reg)) => get(*reg, &registers),
                Some(Operand::AddImm { imm12, shift12 }) => {
                    u64::from(*imm12) << if *shift12 { 12 } else { 0 }
                }
                Some(Operand::Bits(bits)) => *bits,
                Some(Operand::Imm(bits)) => *bits as u64,
                _ => 0,
            };
            let value = if key.starts_with("ubfm") || key.starts_with("sbfm") {
                let Operand::Imm(last) = op[3] else { panic!() };
                let width = last as u32 + 1;
                if key.starts_with("sbfm") {
                    (((a << (64 - width)) as i64) >> (64 - width)) as u64
                } else {
                    a & (u64::MAX >> (64 - width))
                }
            } else if key.starts_with("adds.")
                || key.starts_with("subs.")
                || key.starts_with("add.")
                || key.starts_with("sub.")
            {
                let subtract = key.starts_with("sub");
                let value = if subtract {
                    a.wrapping_sub(b)
                } else {
                    a.wrapping_add(b)
                };
                if key.starts_with("adds") || key.starts_with("subs") {
                    let carry = if subtract {
                        a >= b
                    } else {
                        u128::from(a) + u128::from(b) > u128::from(u64::MAX)
                    };
                    let overflow = if subtract {
                        ((a ^ b) & (a ^ value)) >> 63 != 0
                    } else {
                        (!(a ^ b) & (a ^ value)) >> 63 != 0
                    };
                    flags = ((value as i64) < 0, value == 0, carry, overflow);
                }
                value
            } else if key.starts_with("and.") {
                a & b
            } else if key.starts_with("orr.") {
                a | b
            } else if key.starts_with("orn.") {
                a | !b
            } else if key.starts_with("eor.") {
                a ^ b
            } else if key == "madd.X" {
                a.wrapping_mul(b).wrapping_add(read(3))
            } else if key == "umulh" {
                ((u128::from(a) * u128::from(b)) >> 64) as u64
            } else if key == "udiv.X" {
                a / b
            } else if key == "sdiv.X" {
                (a as i64).wrapping_div(b as i64) as u64
            } else if key == "lslv.X" {
                a.wrapping_shl(b as u32)
            } else if key == "lsrv.X" {
                a.wrapping_shr(b as u32)
            } else if key == "asrv.X" {
                (a as i64).wrapping_shr(b as u32) as u64
            } else if key == "csinc.X" || key == "csel.X" {
                let Operand::Condition(c) = op[3] else {
                    panic!()
                };
                if condition(c, flags) {
                    a
                } else {
                    b + u64::from(key == "csinc.X")
                }
            } else {
                panic!("unhandled {}", inst.text().unwrap());
            };
            put(rr(&op[0]), value, &mut registers);
        }
        pc = next;
    }
    panic!("leaf fixture did not return within deterministic bound")
}
