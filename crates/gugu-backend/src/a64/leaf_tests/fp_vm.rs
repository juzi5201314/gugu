//! IEEE scalar and SIMD transport semantics for the emitted-byte VM.
use super::*;
use inst::{AddressMode, Operand};
use reg::{Reg, RegKind, Width};
use std::collections::BTreeMap;

fn register(op: &Operand) -> Reg {
    let Operand::Reg(reg) = op else {
        panic!("expected register")
    };
    *reg
}

fn get(reg: Reg, gprs: &[u64; 32], vectors: &[u128; 32]) -> u128 {
    let value = match reg.kind {
        RegKind::Vector(id) => vectors[id as usize],
        RegKind::Gpr(id) => u128::from(gprs[id as usize]),
        RegKind::Zero => 0,
        RegKind::Sp => u128::from(gprs[31]),
    };
    value
        & match reg.width {
            Width::S | Width::W => u128::from(u32::MAX),
            Width::D | Width::X => u128::from(u64::MAX),
            Width::Q => u128::MAX,
            _ => panic!("unexpected FP width"),
        }
}

fn put(reg: Reg, value: u128, gprs: &mut [u64; 32], vectors: &mut [u128; 32]) {
    let value = value
        & match reg.width {
            Width::S | Width::W => u128::from(u32::MAX),
            Width::D | Width::X => u128::from(u64::MAX),
            Width::Q => u128::MAX,
            _ => panic!("unexpected FP width"),
        };
    match reg.kind {
        RegKind::Vector(id) => vectors[id as usize] = value,
        RegKind::Gpr(id) => gprs[id as usize] = value as u64,
        RegKind::Zero => {}
        _ => panic!("unexpected FP destination"),
    }
}

fn float(reg: Reg, gprs: &[u64; 32], vectors: &[u128; 32]) -> f64 {
    let bits = get(reg, gprs, vectors);
    if reg.width == Width::S {
        f64::from(f32::from_bits(bits as u32))
    } else {
        f64::from_bits(bits as u64)
    }
}

pub(super) fn clobber(vectors: &mut [u128; 32], entry: bool) {
    for (id, vector) in vectors.iter_mut().enumerate() {
        if (8..16).contains(&id) {
            *vector = (*vector & u128::from(u64::MAX)) | (0xdead_u128 << 64);
        } else if !entry || id >= 16 {
            *vector = 0xdead_dead_dead_dead_dead_dead_dead_dead;
        }
    }
}

pub(super) fn execute(
    inst: &inst::Inst,
    gprs: &mut [u64; 32],
    vectors: &mut [u128; 32],
    flags: &mut (bool, bool, bool, bool),
    memory: &mut BTreeMap<u64, u8>,
    condition: impl Fn(u8, (bool, bool, bool, bool)) -> bool,
) -> bool {
    let key = table::get(inst.form).unwrap().key;
    let op = &inst.operands;
    if (key.starts_with("ldr.") || key.starts_with("str."))
        && matches!(register(&op[0]).kind, RegKind::Vector(_))
    {
        let reg = register(&op[0]);
        let Operand::Address(address) = &op[1] else {
            panic!()
        };
        let AddressMode::Unsigned(offset) = address.mode else {
            panic!("FP VM address mode")
        };
        let at = get(address.base, gprs, vectors) as u64 + offset as u64;
        let bytes = match reg.width {
            Width::S => 4,
            Width::D => 8,
            Width::Q => 16,
            _ => panic!(),
        };
        if key.starts_with("ldr") {
            let bits = (0..bytes).fold(0_u128, |value, byte| {
                value | (u128::from(*memory.get(&(at + byte)).unwrap_or(&0)) << (byte * 8))
            });
            put(reg, bits, gprs, vectors);
        } else {
            let bits = get(reg, gprs, vectors);
            for byte in 0..bytes {
                memory.insert(at + byte, (bits >> (byte * 8)) as u8);
            }
        }
        return true;
    }
    if !(key.starts_with('f')
        || key.starts_with("scvtf.")
        || key.starts_with("ucvtf.")
        || matches!(key, "orr.Q" | "dup.Q.X" | "bsl.Q"))
    {
        return false;
    }
    if key.starts_with("fcmp.") {
        let a = float(register(&op[0]), gprs, vectors);
        let b = float(register(&op[1]), gprs, vectors);
        *flags = if a.is_nan() || b.is_nan() {
            (false, false, true, true)
        } else if a == b {
            (false, true, true, false)
        } else if a < b {
            (true, false, false, false)
        } else {
            (false, false, true, false)
        };
        return true;
    }
    let dest = register(&op[0]);
    let source = register(&op[1]);
    let bits = if key.starts_with("fmov.") {
        get(source, gprs, vectors)
    } else if key.starts_with("fneg.") {
        get(source, gprs, vectors) ^ (1_u128 << if dest.width == Width::S { 31 } else { 63 })
    } else if key.starts_with("fcsel.") {
        let Operand::Condition(code) = op[3] else {
            panic!()
        };
        get(
            register(&op[if condition(code, *flags) { 1 } else { 2 }]),
            gprs,
            vectors,
        )
    } else if key == "orr.Q" {
        get(source, gprs, vectors) | get(register(&op[2]), gprs, vectors)
    } else if key == "dup.Q.X" {
        let bits = get(source, gprs, vectors);
        bits | (bits << 64)
    } else if key == "bsl.Q" {
        let mask = get(dest, gprs, vectors);
        (mask & get(source, gprs, vectors)) | (!mask & get(register(&op[2]), gprs, vectors))
    } else if key.starts_with("fcvtzs.") {
        u128::from(float(source, gprs, vectors) as i64 as u64)
    } else if key.starts_with("fcvtzu.") {
        u128::from(float(source, gprs, vectors) as u64)
    } else if key.starts_with("scvtf.") || key.starts_with("ucvtf.") {
        let bits = get(source, gprs, vectors) as u64;
        if dest.width == Width::S {
            u128::from(if key.starts_with('s') {
                (bits as i64 as f32).to_bits()
            } else {
                (bits as f32).to_bits()
            })
        } else {
            u128::from(if key.starts_with('s') {
                (bits as i64 as f64).to_bits()
            } else {
                (bits as f64).to_bits()
            })
        }
    } else if key.starts_with("fcvt.") {
        let value = float(source, gprs, vectors);
        if dest.width == Width::S {
            u128::from((value as f32).to_bits())
        } else {
            u128::from(value.to_bits())
        }
    } else if dest.width == Width::S {
        let a = f32::from_bits(get(source, gprs, vectors) as u32);
        let b = f32::from_bits(get(register(&op[2]), gprs, vectors) as u32);
        u128::from(
            match key {
                "fadd.S" => a + b,
                "fsub.S" => a - b,
                "fmul.S" => a * b,
                "fdiv.S" => a / b,
                _ => panic!("unmodeled FP {key}"),
            }
            .to_bits(),
        )
    } else {
        let a = float(source, gprs, vectors);
        let b = float(register(&op[2]), gprs, vectors);
        u128::from(
            match key {
                "fadd.D" => a + b,
                "fsub.D" => a - b,
                "fmul.D" => a * b,
                "fdiv.D" => a / b,
                _ => panic!("unmodeled FP {key}"),
            }
            .to_bits(),
        )
    };
    put(dest, bits, gprs, vectors);
    true
}
