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

#[test]
fn rematerialized_values_preserve_old_sources_and_survive_memory_copies() {
    use crate::x64::alloc::{RegClass, RootClass};
    use crate::x64::copies::Copy;

    let locations = [
        Loc::Gpr(Gpr::Rax),
        Loc::Slot(0),
        Loc::Slot(1),
        Loc::Slot(2),
        Loc::Gpr(Gpr::R11),
    ];
    let allocations: Vec<_> = [
        Placement::Rematerialize,
        Placement::Location(Location::Slot(0)),
        Placement::Location(Location::Slot(1)),
        Placement::Location(Location::Slot(2)),
    ]
    .into_iter()
    .enumerate()
    .map(|(value, placement)| ValueAllocation {
        value: u32::try_from(value).expect("固定四项测试分配表"),
        class: RegClass::Gpr,
        root: RootClass::NonPointer,
        range: (0, 1),
        placement,
    })
    .collect();
    let pairs = [
        Copy {
            src: Reg::Virtual(1),
            dest: Reg::Virtual(2),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Virtual(0),
            dest: Reg::Virtual(1),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Gpr(Gpr::Rax),
            dest: Reg::Virtual(3),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Virtual(0),
            dest: Reg::Gpr(Gpr::Rax),
            ty: Type::I64,
        },
    ];
    let (moves, constants) = group_moves(&pairs, &allocations).unwrap();
    let (resolved, _, _) = resolve_group(&moves, &constants, None).unwrap();
    let mut values = [11, 22, 33, 44, 0];
    for operation in resolved {
        match operation {
            EmittedMove::Move { src, dst, .. } => {
                let value = values[locations.iter().position(|&loc| loc == src).unwrap()];
                if matches!(src, Loc::Slot(_)) && matches!(dst, Loc::Slot(_)) {
                    values[4] = value;
                }
                values[locations.iter().position(|&loc| loc == dst).unwrap()] = value;
            }
            EmittedMove::Rematerialize { value, dest } => {
                assert_eq!(value, 0);
                if matches!(dest, Loc::Slot(_)) {
                    values[4] = 191;
                }
                values[locations.iter().position(|&loc| loc == dest).unwrap()] = 191;
            }
        }
    }
    assert_eq!(&values[..4], &[191, 191, 22, 11]);
}
