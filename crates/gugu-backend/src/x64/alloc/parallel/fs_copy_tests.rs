use super::*;

#[test]
fn cycle_value_survives_memory_copy_scratch() {
    let locations = [
        Loc::Gpr(Gpr::Rax),
        Loc::Gpr(Gpr::Rbx),
        Loc::Slot(0),
        Loc::Slot(1),
        Loc::Gpr(Gpr::R11),
        Loc::Scratch,
    ];
    let moves = [
        Move {
            src: locations[0],
            dst: locations[1],
            ty: Type::I64,
        },
        Move {
            src: locations[2],
            dst: locations[0],
            ty: Type::I64,
        },
        Move {
            src: locations[3],
            dst: locations[2],
            ty: Type::I64,
        },
        Move {
            src: locations[1],
            dst: locations[3],
            ty: Type::I64,
        },
    ];
    let (resolved, _, _) = resolve_group(&moves, &[], Some(0)).unwrap();
    let mut values = [11, 22, 33, 44, 0, 0];
    for operation in resolved {
        let EmittedMove::Move { src, dst, .. } = operation else {
            unreachable!()
        };
        let src_index = locations.iter().position(|&loc| loc == src).unwrap();
        let dst_index = locations.iter().position(|&loc| loc == dst).unwrap();
        let value = values[src_index];
        // 实际 x64 发射通过 r11 完成内存到内存搬运。
        if matches!(src, Loc::Slot(_) | Loc::Scratch) && matches!(dst, Loc::Slot(_) | Loc::Scratch)
        {
            values[4] = value;
        }
        values[dst_index] = value;
    }
    assert_eq!(&values[..4], &[33, 11, 44, 22]);
}

#[test]
fn memory_self_copy_needs_no_cycle_scratch() {
    let moves = [Move {
        src: Loc::Slot(0),
        dst: Loc::Slot(0),
        ty: Type::I64,
    }];
    let (resolved, cycles, scratch) = resolve_group(&moves, &[], None).unwrap();
    assert!(resolved.is_empty());
    assert_eq!(cycles, 0);
    assert!(!scratch);
}
