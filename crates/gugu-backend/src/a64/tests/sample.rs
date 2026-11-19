//! 每个描述符都有一个规范样本，边界样本独立追加。

use crate::a64::{decode, inst::*, reg::*, table::*};

pub fn sample(id: FormId) -> Inst {
    let form = get(id).unwrap();
    let operands = form
        .operands
        .iter()
        .enumerate()
        .map(|(i, slot)| match slot.kind {
            Kind::Reg { .. } | Kind::ExtendedReg(_) => {
                decode::unpack(slot.kind, (i + 1) as u32).unwrap()
            }
            Kind::Imm { scale, .. } => Operand::Imm(
                if matches!(form.mnemonic, "dmb" | "dsb" | "isb" | "clrex") {
                    15
                } else {
                    3 << scale
                },
            ),
            Kind::AddImm => Operand::AddImm {
                imm12: 123,
                shift12: true,
            },
            Kind::Logical(width) => Operand::Bits(if width == 64 {
                0x5555555555555555
            } else {
                0x55555555
            }),
            Kind::Wide(_) => Operand::Wide {
                imm16: 0x1234,
                shift: 16,
            },
            Kind::TestBit(high) => Operand::Imm(if high { 35 } else { 3 }),
            Kind::Shift { .. } => Operand::Shift(Shift::Lsl, 3),
            Kind::Extend(_) => Operand::Extend(Extend::Uxtw, 2),
            Kind::Condition => Operand::Condition(0),
            Kind::Address { mode, size } => Operand::Address(Address {
                base: Reg::x(10),
                mode: match mode {
                    AddrMode::Unsigned | AddrMode::Pair => AddressMode::Unsigned(3 << size),
                    AddrMode::Unscaled => AddressMode::Unscaled(-3),
                    AddrMode::Pre => AddressMode::Pre(-3),
                    AddrMode::Post => AddressMode::Post(-3),
                    AddrMode::PairPre => AddressMode::Pre(-3 << size),
                    AddrMode::PairPost => AddressMode::Post(-3 << size),
                    AddrMode::Base => AddressMode::Base,
                    AddrMode::Register => AddressMode::Register {
                        index: Reg::w(11),
                        extend: Extend::Sxtw,
                        scaled: true,
                    },
                },
            }),
            Kind::Target { scale, .. } => {
                Operand::Target(Target::Offset(if scale == 12 { 4096 } else { 16 }))
            }
            Kind::FpImm => Operand::FpImm(0x70),
            Kind::System(s) => Operand::System(s),
        })
        .collect();
    Inst::new(id, operands)
}

fn changed(key: &str, index: usize, op: Operand) -> Inst {
    let mut inst = sample(find(key).unwrap());
    inst.operands[index] = op;
    inst
}

pub fn samples() -> Vec<Inst> {
    let mut result: Vec<_> = (0..FORMS.len()).map(|i| sample(FormId(i as u16))).collect();
    for (key, index, bits, scale) in [
        ("b", 0, 26, 2),
        ("bl", 0, 26, 2),
        ("b.cond", 0, 19, 2),
        ("cbz.X", 1, 19, 2),
        ("tbz.X", 2, 14, 2),
        ("adr", 1, 21, 0),
        ("adrp", 1, 21, 12),
        ("ldr.X.literal", 1, 19, 2),
    ] {
        for n in [
            -(1i64 << (bits - 1)) << scale,
            ((1i64 << (bits - 1)) - 1) << scale,
        ] {
            result.push(changed(key, index, Operand::Target(Target::Offset(n))));
        }
    }
    for (key, value) in [
        ("orr.X.imm", 0xff),
        ("and.X.imm", 0x0000ffff00000000),
        ("and.W.imm", 0xff),
    ] {
        result.push(changed(key, 2, Operand::Bits(value)));
    }
    for key in ["movz.W", "movn.X", "movk.X"] {
        for shift in [0, if key.ends_with('W') { 16 } else { 48 }] {
            result.push(changed(
                key,
                1,
                Operand::Wide {
                    imm16: 65535,
                    shift,
                },
            ));
        }
    }
    for (key, mode) in [
        ("ldr.X.Unsigned", AddressMode::Unsigned(32760)),
        ("strb.W.Unsigned", AddressMode::Unsigned(4095)),
        ("ldr.X.Unscaled", AddressMode::Unscaled(-256)),
        ("str.X.Pre", AddressMode::Pre(-256)),
        ("ldr.X.Post", AddressMode::Post(255)),
        ("ldp.X.Pair", AddressMode::Unsigned(-512)),
        ("stp.X.Pair", AddressMode::Unsigned(504)),
        ("ldp.Q.PairPre", AddressMode::Pre(-1024)),
        ("stp.Q.PairPost", AddressMode::Post(1008)),
        (
            "ldr.X.Register",
            AddressMode::Register {
                index: Reg::x(12),
                extend: Extend::Uxtx,
                scaled: true,
            },
        ),
        (
            "str.X.Register",
            AddressMode::Register {
                index: Reg::w(12),
                extend: Extend::Uxtw,
                scaled: true,
            },
        ),
    ] {
        let id = find(key).unwrap();
        result.push(changed(
            key,
            get(id).unwrap().operands.len() - 1,
            Operand::Address(Address {
                base: Reg::sp(Width::X),
                mode,
            }),
        ));
    }
    result.push(changed("add.X.imm", 0, Operand::Reg(Reg::sp(Width::X))));
    result.push(changed("adds.X.imm", 0, Operand::Reg(Reg::zr(Width::X))));
    result.push(changed("add.X.shift", 1, Operand::Reg(Reg::zr(Width::X))));
    let mut ext = sample(find("add.X.extend").unwrap());
    ext.operands[2] = Operand::Reg(Reg::x(3));
    ext.operands[3] = Operand::Extend(Extend::Sxtx, 4);
    result.push(ext);
    result.push(changed("add.W.extend", 3, Operand::Extend(Extend::Uxtx, 0)));
    result.push(changed("add.W.extend", 3, Operand::Extend(Extend::Sxtx, 0)));
    for key in ["and.X.imm", "orr.X.imm", "eor.X.imm"] {
        result.push(changed(key, 0, Operand::Reg(Reg::sp(Width::X))));
    }
    for (index, extend) in [
        (Reg::x(12), Extend::Uxtx),
        (Reg::w(12), Extend::Uxtw),
        (Reg::w(12), Extend::Sxtw),
    ] {
        result.push(changed(
            "ldrb.W.Register",
            1,
            Operand::Address(Address {
                base: Reg::x(10),
                mode: AddressMode::Register {
                    index,
                    extend,
                    scaled: false,
                },
            }),
        ));
    }
    result
}
