use super::{bits, contract, decode, encode, inst::*, reg::*, table::*, verify};

mod sample;

#[test]
fn byte_exact_fixtures() {
    let fixture = include_str!("fixtures/llvm.txt");
    let lines: Vec<_> = fixture.lines().collect();
    let samples = sample::samples();
    assert_eq!(lines.len(), samples.len());
    for (inst, line) in samples.iter().zip(lines) {
        let (text, hex) = line.split_once(" => ").unwrap();
        assert_eq!(inst.text().unwrap(), text);
        let expected = u32::from_str_radix(hex, 16).unwrap();
        let encoded = encode::encode(inst).unwrap();
        assert_eq!(encoded.word, expected, "{text}");
        assert_eq!(encoded.bytes(), expected.to_le_bytes());
        assert!(encoded.relocations.is_empty());
        let decoded = decode::decode(expected).unwrap();
        assert_eq!(decoded.form, inst.form, "{text}");
        assert_eq!(encode::encode(&decoded).unwrap().word, expected, "{text}");
    }
}

#[test]
fn frozen_contract() {
    contract::verify().unwrap();
    assert_eq!(contract::catalog().len(), 307);
    let hex: String = contract::fingerprint()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hex,
        "6b469e9c57669964bd6041dda63612e77c7d7294e9574c26b1cb46c646e8c445"
    );
}

#[test]
fn relocation_fields_are_zero() {
    let symbol = Target::Symbol {
        name: "callee".to_owned(),
        addend: 8,
    };
    let cases = [
        ("b", 0, RelocKind::Branch26),
        ("bl", 0, RelocKind::Call26),
        ("b.cond", 0, RelocKind::CondBr19),
        ("cbnz.W", 1, RelocKind::CondBr19),
        ("tbnz.X", 2, RelocKind::TestBr14),
        ("adrp", 1, RelocKind::AdrpPage21),
        ("add.W.imm", 2, RelocKind::AddLo12),
        ("add.X.imm", 2, RelocKind::AddLo12),
        ("ldr.X.literal", 1, RelocKind::Literal19),
    ];
    for (key, index, kind) in cases {
        let mut inst = sample::sample(find(key).unwrap());
        inst.operands[index] = Operand::Target(symbol.clone());
        check_relocation(&inst, index, kind);
    }
    for (key, size) in [
        ("ldrb.W.Unsigned", 0),
        ("ldrh.W.Unsigned", 1),
        ("ldr.W.Unsigned", 2),
        ("ldr.X.Unsigned", 3),
        ("ldr.Q.Unsigned", 4),
    ] {
        let mut inst = sample::sample(find(key).unwrap());
        inst.operands[1] = Operand::Address(Address {
            base: Reg::sp(Width::X),
            mode: AddressMode::Reloc {
                name: "callee".to_owned(),
                addend: 8,
            },
        });
        check_relocation(&inst, 1, RelocKind::LdStLo12(size));
    }
}

#[test]
fn rejects_add_lo12_on_non_add_forms() {
    for key in [
        "sub.W.imm",
        "sub.X.imm",
        "subs.W.imm",
        "subs.X.imm",
        "adds.W.imm",
        "adds.X.imm",
    ] {
        let mut inst = sample::sample(find(key).unwrap());
        inst.operands[2] = Operand::Target(Target::Symbol {
            name: "callee".to_owned(),
            addend: 8,
        });
        assert!(verify::verify(&inst).is_err(), "{key}");
        assert!(encode::encode(&inst).is_err(), "{key}");
    }
}

fn check_relocation(inst: &Inst, index: usize, kind: RelocKind) {
    let encoded = encode::encode(inst).unwrap();
    let mask = get(inst.form).unwrap().operands[index].mask();
    let base_mask = if matches!(kind, RelocKind::LdStLo12(_)) {
        31 << 5
    } else {
        0
    };
    assert_eq!(encoded.word & mask & !base_mask, 0);
    assert_eq!(
        encoded.relocations,
        vec![encode::Relocation {
            kind,
            target: "callee".to_owned(),
            addend: 8
        }]
    );
}

#[test]
fn rejects_invalid_operands() {
    let cases = [
        ("and.X.imm", 2, Operand::Bits(0x123456789)),
        ("and.X.imm", 2, Operand::Bits(0)),
        ("and.W.imm", 2, Operand::Bits(u64::MAX)),
        ("add.X.imm", 0, Operand::Reg(Reg::zr(Width::X))),
        ("add.X.shift", 0, Operand::Reg(Reg::sp(Width::X))),
        ("ands.X.imm", 0, Operand::Reg(Reg::sp(Width::X))),
        ("orr.X.imm", 0, Operand::Reg(Reg::zr(Width::X))),
        ("add.W.shift", 3, Operand::Shift(Shift::Lsl, 32)),
        ("sub.X.shift", 3, Operand::Shift(Shift::Ror, 1)),
        ("add.X.extend", 3, Operand::Extend(Extend::Sxtw, 5)),
        ("add.X.extend", 3, Operand::Extend(Extend::Sxtx, 1)),
        (
            "movk.W",
            1,
            Operand::Wide {
                imm16: 1,
                shift: 32,
            },
        ),
        (
            "movk.X",
            1,
            Operand::Wide {
                imm16: 1,
                shift: 17,
            },
        ),
        (
            "add.X.imm",
            2,
            Operand::AddImm {
                imm12: 4096,
                shift12: false,
            },
        ),
        ("tbz.X", 1, Operand::Imm(31)),
        ("tbz.X", 1, Operand::Imm(i64::MIN)),
        ("tbz.W", 1, Operand::Imm(32)),
        ("b", 0, Operand::Target(Target::Offset(1 << 27))),
        ("bl", 0, Operand::Target(Target::Offset(-(1 << 27) - 4))),
        ("b.cond", 0, Operand::Target(Target::Offset(2))),
        (
            "adr",
            1,
            Operand::Target(Target::Symbol {
                name: "x".into(),
                addend: 0,
            }),
        ),
        ("isb", 0, Operand::Imm(3)),
        ("dmb", 0, Operand::Imm(4)),
        ("ldr.X.Unsigned", 1, addr(AddressMode::Unsigned(32768))),
        ("ldr.X.Unsigned", 1, addr(AddressMode::Unsigned(7))),
        ("ldr.X.Pre", 1, addr(AddressMode::Pre(-257))),
        ("ldp.X.Pair", 2, addr(AddressMode::Unsigned(512))),
        (
            "ldr.X.Register",
            1,
            addr(AddressMode::Register {
                index: Reg::x(1),
                extend: Extend::Uxtw,
                scaled: true,
            }),
        ),
        (
            "ldr.X.Register",
            1,
            addr(AddressMode::Register {
                index: Reg::w(1),
                extend: Extend::Uxth,
                scaled: true,
            }),
        ),
    ];
    for (key, index, op) in cases {
        let mut inst = sample::sample(find(key).unwrap());
        inst.operands[index] = op;
        assert!(verify::verify(&inst).is_err(), "{key}");
        assert!(encode::encode(&inst).is_err(), "{key}");
    }
    assert!(encode::encode(&Inst::new(FormId(u16::MAX), vec![])).is_err());
    assert!(encode::encode(&Inst::new(find("add.X.imm").unwrap(), vec![])).is_err());
    assert!(encode::encode(&Inst::new(find("nop").unwrap(), vec![Operand::Imm(0)])).is_err());
}

fn addr(mode: AddressMode) -> Operand {
    Operand::Address(Address {
        base: Reg::x(10),
        mode,
    })
}

#[test]
fn rejects_unallocated_words() {
    for word in [
        0xffffffff,
        0x02000000,
        0x12800000 | 0x600000,
        0x12400000,
        0x1200fc00,
        0xd50334bf,
        0xd50333df,
        0xf8600800,
    ] {
        assert!(decode::decode(word).is_err(), "{word:08x}");
    }
}

#[test]
fn logical_immediate_space() {
    for width in [32, 64] {
        for raw in 0..8192 {
            if let Ok(value) = bits::decode_logical(raw, width) {
                let canonical = bits::encode_logical(value, width).unwrap();
                assert_eq!(bits::decode_logical(canonical, width).unwrap(), value);
            }
        }
    }
}

#[test]
fn rejects_register_hazards_and_invalid_views() {
    for (key, index, op) in [
        ("ldr.X.Pre", 0, Operand::Reg(Reg::x(10))),
        ("ldp.X.Pair", 1, Operand::Reg(Reg::x(1))),
        ("ldp.D.Pair", 1, Operand::Reg(Reg::v(1, Width::D))),
        ("stxr.X", 0, Operand::Reg(Reg::w(2))),
        ("stlxr.X", 0, Operand::Reg(Reg::w(10))),
        ("add.X.shift", 0, Operand::Reg(Reg::x(31))),
        ("add.X.shift", 0, Operand::Reg(Reg::w(1))),
        ("fadd.D", 0, Operand::Reg(Reg::v(32, Width::D))),
        ("fadd.D", 0, Operand::Reg(Reg::v(1, Width::Q))),
    ] {
        let mut inst = sample::sample(find(key).unwrap());
        inst.operands[index] = op;
        assert!(encode::encode(&inst).is_err(), "{key}");
    }
}
