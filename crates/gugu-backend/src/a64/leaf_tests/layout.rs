use super::*;
use crate::a64::layout::{self, Item};

#[test]
fn relaxed_words_invert_condition_and_branch_from_second_pc() {
    use inst::Operand;
    for (key, inverse, operands) in [
        ("b.cond", "b.cond", vec![Operand::Condition(0)]),
        ("cbz.X", "cbnz.X", vec![Operand::Reg(reg::Reg::x(1))]),
        ("cbnz.W", "cbz.W", vec![Operand::Reg(reg::Reg::w(1))]),
        (
            "tbz.X",
            "tbnz.X",
            vec![Operand::Reg(reg::Reg::x(1)), Operand::Imm(40)],
        ),
        (
            "tbnz.W",
            "tbz.W",
            vec![Operand::Reg(reg::Reg::w(1)), Operand::Imm(5)],
        ),
    ] {
        let sequence =
            layout::resolve_branch(machine::form(key).unwrap(), &operands, 1 << 21, true).unwrap();
        assert_eq!(sequence.len(), 2);
        for instruction in &sequence {
            let word = encode::encode(instruction).unwrap().word;
            assert_eq!(
                encode::encode(&decode::decode(word).unwrap()).unwrap().word,
                word
            );
        }
        assert_eq!(table::get(sequence[0].form).unwrap().key, inverse);
        assert!(
            sequence[0]
                .operands
                .contains(&Operand::Target(inst::Target::Offset(8)))
        );
        if key == "b.cond" {
            assert!(sequence[0].operands.contains(&Operand::Condition(1)));
        }
        assert_eq!(
            sequence[1].operands,
            vec![Operand::Target(inst::Target::Offset((1 << 21) - 4))]
        );
    }
}

fn branch(key: &str, target: u32) -> Item {
    Item::Branch {
        form: machine::form(key).unwrap(),
        operands: if key == "b.cond" {
            vec![inst::Operand::Condition(0)]
        } else {
            vec![]
        },
        target,
    }
}

#[test]
fn displacement_limits_and_conditional_relaxation_without_padding() {
    for bits in [14, 19, 26] {
        let limit = (1_i128 << (bits - 1)) * 4;
        assert!(layout::fits(-limit, bits));
        assert!(layout::fits(limit - 4, bits));
        assert!(!layout::fits(-limit - 4, bits));
        assert!(!layout::fits(limit, bits));
        assert!(!layout::fits(1, bits));
    }
    let nop = Item::Inst(machine::inst("brk", vec![inst::Operand::Imm(0)]).unwrap());
    for key in ["b.cond", "cbz.X", "cbnz.W", "tbz.X", "tbnz.W"] {
        let items = [branch(key, 1), nop.clone(), Item::Label(1)];
        let plan = layout::plan(&items, &[4, 1 << 20, 0]).unwrap();
        assert!(plan.expanded[0]);
        assert_eq!(plan.labels[&1], (1 << 20) + 8);
    }
    let items = [branch("b", 1), nop.clone(), Item::Label(1)];
    assert!(
        layout::plan(&items, &[4, 1 << 27, 0])
            .unwrap_err()
            .0
            .contains("BranchOutOfRange")
    );
    let items = [Item::Label(1), nop, branch("b", 1)];
    let (bytes, _) = layout::assemble(&items).unwrap();
    let decoded = decode::decode(u32::from_le_bytes(bytes[4..].try_into().unwrap())).unwrap();
    assert_eq!(
        decoded.operands,
        vec![inst::Operand::Target(inst::Target::Offset(-4))]
    );
}

#[test]
fn cascading_relaxation_and_invalid_labels_symbols() {
    let nop = Item::Inst(machine::inst("brk", vec![inst::Operand::Imm(0)]).unwrap());
    let items = [
        branch("b.cond", 2),
        branch("tbz.X", 3),
        nop.clone(),
        Item::Label(2),
        nop,
        Item::Label(3),
    ];
    let plan = layout::plan(&items, &[4, 4, (1 << 20) - 12, 0, 1 << 20, 0]).unwrap();
    assert!(plan.expanded[0] && plan.expanded[1]);
    assert!(layout::assemble(&[Item::Label(1), Item::Label(1)]).is_err());
    assert!(layout::assemble(&[branch("b", 99)]).is_err());
    assert!(layout::plan(&[], &[4]).is_err());
    let unresolved = Item::Inst(
        machine::inst(
            "b",
            vec![inst::Operand::Target(inst::Target::Symbol {
                name: "external".into(),
                addend: 0,
            })],
        )
        .unwrap(),
    );
    assert!(
        layout::assemble(&[unresolved])
            .unwrap_err()
            .0
            .contains("unresolved symbol")
    );
}
