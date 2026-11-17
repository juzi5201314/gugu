use super::*;
use crate::x64::fs_native::protocol_tests::{Machine, Stop};
use crate::x64::inst::Scale;
use crate::x64::lower::Lowered;

fn allocated(sequence: Lowered) -> Sequence {
    let body = live::foreign_root_tests::body(&[]);
    let frame = FrameLayout {
        frame_size: 32,
        payload_bytes: 16,
        outgoing_bytes: 0,
        required_frame: 32,
        entry_required_frame: 40,
        max_leaf_reserve: 0,
        locals: vec![],
        init_bases: vec![],
        scratch: None,
        saved: vec![],
        save_offsets: vec![],
        spill_offsets: vec![0, 8],
        spill_slots: vec![(0, 8, 8, 0), (8, 8, 8, 0)],
        checked: false,
    };
    let values: Vec<_> = (0..2)
        .map(|value| ValueAllocation {
            value,
            class: RegClass::Gpr,
            root: super::super::RootClass::NonPointer,
            range: (0, 100),
            placement: Placement::Location(Location::Slot(value)),
        })
        .collect();
    let plan = live::site_plan(&sequence).unwrap();
    let points: Vec<_> = plan
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let index = u32::try_from(index).unwrap();
            live::Point {
                site: 0,
                kind: live::PointKind::Normal,
                block: 0,
                use_slot: index * 2,
                def_slot: index * 2 + 1,
                instructions: entry.instructions.clone(),
                mask: Clobbers::NONE,
                clobber: Clobbers::NONE,
                pointer_spill: false,
                fixed_physical: false,
                outgoing_roots: vec![],
            }
        })
        .collect();
    let emit = Emit {
        body: &body,
        frame: &frame,
        values: &values,
        points: &points,
        target: TargetName::X86_64Linux,
        scratch_gpr: Clobbers::gpr(Gpr::R11),
        traffic: Cell::new(SpillTraffic::default()),
    };
    let mut pending = pending_stores(
        &emit,
        &sequence,
        &plan,
        &(0..u32::try_from(plan.len()).unwrap()),
    )
    .unwrap();
    let mut builder = Builder::new();
    for (index, instruction) in sequence.sequence.instructions.iter().enumerate() {
        rewrite_instruction(
            &emit,
            &mut builder,
            instruction,
            u32::try_from(index).unwrap(),
            &mut pending,
        )
        .unwrap();
    }
    builder.emit("ret", &[], Access::Read, vec![]);
    builder.finish().sequence
}

#[test]
fn register_only_readwrite_spills_write_the_selected_full_result() {
    for (mnemonic, equal, expected) in
        [("imul", false, 35), ("cmove", true, 7), ("cmove", false, 5)]
    {
        let mut b = Builder::new();
        if mnemonic == "cmove" {
            b.emit(
                "cmp",
                &[OperandKind::Rm64, OperandKind::Imm32],
                Access::Read,
                vec![
                    Operand::Reg(Reg::Gpr(Gpr::Rax)),
                    Operand::Imm(u64::from(equal)),
                ],
            );
        }
        b.emit(
            mnemonic,
            &[OperandKind::Rm64, OperandKind::R64],
            Access::Read,
            vec![
                Operand::Reg(Reg::Gpr(Gpr::Rbx)),
                Operand::Reg(Reg::Virtual(0)),
            ],
        );
        let sequence = allocated(b.finish());
        let mut machine = Machine::new(&[]);
        let stack = machine.get(Gpr::Rsp);
        machine.put(stack, 5);
        machine.set_reg(Gpr::Rax, 1);
        machine.set_reg(Gpr::Rbx, 7);
        assert_eq!(machine.run(&sequence), Stop::Return);
        assert_eq!(machine.word(stack), expected);
    }
}

#[test]
fn lea_pure_output_reuses_its_spilled_address_scratch() {
    let mut b = Builder::new();
    b.emit(
        "lea",
        &[OperandKind::Mem, OperandKind::R64],
        Access::Address,
        vec![
            Operand::Mem(Mem {
                base: Some(Reg::Virtual(0)),
                index: Some(Reg::Gpr(Gpr::Rax)),
                scale: Scale::Eight,
                disp: 16,
            }),
            Operand::Reg(Reg::Virtual(1)),
        ],
    );
    let sequence = allocated(b.finish());
    let mut machine = Machine::new(&[]);
    let stack = machine.get(Gpr::Rsp);
    machine.put(stack, 128);
    machine.set_reg(Gpr::Rax, 3);
    assert_eq!(machine.run(&sequence), Stop::Return);
    assert_eq!(machine.word(stack + 8), 168);
}
