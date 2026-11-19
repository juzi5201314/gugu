use super::*;
use parallel::{Copy, CopyPolicy, CycleBreak};

struct Tiny;
impl RegisterFile for Tiny {
    fn class(&self, reg: PhysReg) -> Class {
        Class(u8::from(reg.0 == 2))
    }
    fn pool(&self, class: Class, cross: bool) -> &[PhysReg] {
        match (class.0, cross) {
            (0, false) => &[PhysReg(0), PhysReg(1)],
            (0, true) => &[PhysReg(1), PhysReg(0)],
            (1, _) => &[PhysReg(2)],
            _ => &[],
        }
    }
    fn spill_layout(&self, class: Class) -> (u32, u32) {
        match class.0 {
            0 => (8, 8),
            1 => (16, 16),
            _ => (0, 0),
        }
    }
    fn copy_scratch(&self, class: Class) -> Option<PhysReg> {
        (class == Class(0)).then_some(PhysReg(3))
    }
}

fn value(range: (u32, u32), weight: u64) -> Value {
    Value {
        class: Class(0),
        root: RootClass::NonPointer,
        allowed: RegSet(u64::MAX),
        rematerializable: false,
        range,
        forced: false,
        weight,
        hints: vec![],
    }
}

fn point(slot: u32) -> Point {
    Point {
        use_slot: slot,
        def_slot: slot + 1,
        mask: RegSet::default(),
        pointer_spill: false,
        call: false,
    }
}

fn placement(reg: u8) -> Placement {
    Placement::Location(Location::Reg(PhysReg(reg)))
}

#[test]
fn masks_constraints_and_last_use_boundaries() {
    assert_eq!(RegSet::single(PhysReg(63)).unwrap(), RegSet(1 << 63));
    assert!(RegSet::single(PhysReg(64)).is_err());
    assert!(!RegSet(u64::MAX).contains(PhysReg(64)));
    let mut input = LiveInfo {
        values: vec![value((0, 1), 1)],
        points: vec![point(0), point(2)],
    };
    input.points[1].mask = RegSet(1);
    assert_eq!(
        scan::scan(&input, &Tiny).unwrap().values[0].placement,
        placement(0)
    );
    input.values[0].range.1 = 2;
    assert_eq!(
        scan::scan(&input, &Tiny).unwrap().values[0].placement,
        placement(1)
    );
    input.values[0].allowed = RegSet(1);
    assert_eq!(
        scan::scan(&input, &Tiny).unwrap().values[0].location(),
        Some(Location::Slot(u32::MAX))
    );
    input.values[0].rematerializable = true;
    assert_eq!(
        scan::scan(&input, &Tiny).unwrap().values[0].placement,
        Placement::Rematerialize
    );
}

#[test]
fn cross_call_hints_and_forced_roots() {
    let mut input = LiveInfo {
        values: vec![value((0, 5), 1)],
        points: vec![point(2)],
    };
    input.points[0].call = true;
    assert_eq!(
        scan::scan(&input, &Tiny).unwrap().values[0].placement,
        placement(1)
    );
    input.values[0].hints.push(Hint::Register(PhysReg(0)));
    assert_eq!(
        scan::scan(&input, &Tiny).unwrap().values[0].placement,
        placement(0)
    );
    input.points[0].pointer_spill = true;
    for root in [RootClass::HeapPointer, RootClass::StackPointer] {
        input.values[0].root = root;
        let result = scan::scan(&input, &Tiny).unwrap();
        assert_eq!(result.values[0].location(), Some(Location::Slot(u32::MAX)));
        assert_eq!(result.stats.safepoint_spills, 1);
    }
    input.values[0].root = RootClass::NonPointer;
    input.values[0].forced = true;
    assert_eq!(scan::scan(&input, &Tiny).unwrap().stats.safepoint_spills, 1);
}

#[test]
fn weighted_victims_expiration_dead_values_and_copy_hints() {
    let mut input = LiveInfo {
        values: vec![
            value((0, 9), 0),
            value((0, 9), 2),
            value((2, 9), 20),
            value((10, 11), 1),
            value((1, 0), 0),
        ],
        points: vec![],
    };
    input.values[0].rematerializable = true;
    input.values[3].hints.push(Hint::SameAs(1));
    let result = scan::scan(&input, &Tiny).unwrap();
    assert_eq!(result.values[0].placement, Placement::Rematerialize);
    assert_eq!(result.values[1].placement, placement(1));
    assert_eq!(result.values[2].placement, placement(0));
    assert_eq!(result.values[3].placement, placement(1));
    assert_eq!(result.values[4].placement, Placement::Dead);
    assert_eq!(result.stats.peaks, vec![(Class(0), 2)]);
    input.values = vec![value((0, 4), 1); 3];
    let result = scan::scan(&input, &Tiny).unwrap();
    assert_eq!(result.values[2].location(), Some(Location::Slot(u32::MAX)));
}

#[test]
fn scan_empty_and_invalid_inputs() {
    let mut input = LiveInfo {
        values: vec![],
        points: vec![],
    };
    assert!(scan::scan(&input, &Tiny).unwrap().values.is_empty());
    input.points = vec![point(2), point(0)];
    assert!(scan::scan(&input, &Tiny).is_err());
    struct Bad;
    impl RegisterFile for Bad {
        fn class(&self, _: PhysReg) -> Class {
            Class(0)
        }
        fn pool(&self, _: Class, _: bool) -> &[PhysReg] {
            &[PhysReg(64)]
        }
        fn spill_layout(&self, _: Class) -> (u32, u32) {
            (0, 0)
        }
        fn copy_scratch(&self, _: Class) -> Option<PhysReg> {
            None
        }
    }
    input.points.clear();
    input.values.push(value((0, 1), 1));
    assert!(scan::scan(&input, &Bad).is_err());
    input.values[0].class = Class(1);
    assert!(scan::scan(&input, &Bad).is_err());
    input.values[0].hints.push(Hint::Register(PhysReg(64)));
    assert!(scan::scan(&input, &Tiny).is_ok());
}

#[test]
fn slots_separate_roots_layouts_and_overlapping_endpoints() {
    let roots = [
        RootClass::NonPointer,
        RootClass::HeapPointer,
        RootClass::StackPointer,
    ];
    let mut values = (0..6)
        .map(|id| ValueAllocation {
            value: id,
            class: Class(0),
            root: roots[(id % 3) as usize],
            range: (id / 3 * 2, id / 3 * 2 + 1),
            placement: Placement::Location(Location::Slot(u32::MAX)),
        })
        .collect::<Vec<_>>();
    let assigned = slots::assign(&mut values, &Tiny).unwrap();
    assert_eq!(assigned.slots.len(), 3);
    assert_eq!(assigned.spill_bytes, 24);
    for id in 0..3 {
        assert_eq!(values[id].location(), values[id + 3].location());
    }
    values[3].range = (1, 2);
    values[4].class = Class(1);
    let assigned = slots::assign(&mut values, &Tiny).unwrap();
    assert_eq!(assigned.slots.len(), 5);
    assert_eq!((assigned.slots[4].size, assigned.slots[4].align), (16, 16));
    assert!(slots::assign(&mut [], &Tiny).unwrap().slots.is_empty());
    values[0].class = Class(9);
    assert!(slots::assign(&mut values, &Tiny).is_err());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Loc {
    Reg(PhysReg),
    Slot(u32),
    Scratch,
}

impl CopyPolicy<Loc, Class> for Tiny {
    fn break_cycle(&self, saved: Loc, ty: Class, _: &[Copy<Loc, Class>]) -> CycleBreak<Loc, Class> {
        let holder = self.copy_scratch(ty).map_or(Loc::Scratch, Loc::Reg);
        CycleBreak {
            holder,
            saves: vec![Copy {
                src: saved,
                dst: holder,
                ty,
            }],
            uses_scratch: holder == Loc::Scratch,
        }
    }
}

#[test]
fn copies_cycles_fanout_self_moves_and_frame_probe() {
    for (class, a, b) in [
        (Class(0), Loc::Reg(PhysReg(0)), Loc::Reg(PhysReg(1))),
        (Class(1), Loc::Slot(0), Loc::Slot(1)),
    ] {
        let moves = [
            Copy {
                src: a,
                dst: b,
                ty: class,
            },
            Copy {
                src: b,
                dst: a,
                ty: class,
            },
            Copy {
                src: a,
                dst: a,
                ty: class,
            },
        ];
        let probe = parallel::resolve_copies(&moves, &Tiny, false).unwrap();
        assert_eq!(probe.uses_scratch, class == Class(1));
        let plan = parallel::resolve_copies(&moves, &Tiny, true).unwrap();
        assert_eq!(plan.cycles, 1);
        assert_eq!(plan.moves.len(), 3);
        let mut state = vec![(a, 10), (b, 20)];
        for mov in &plan.moves {
            let old = state.iter().find(|(loc, _)| *loc == mov.src).unwrap().1;
            if let Some((_, value)) = state.iter_mut().find(|(loc, _)| *loc == mov.dst) {
                *value = old;
            } else {
                state.push((mov.dst, old));
            }
        }
        assert_eq!(state[0].1, 20);
        assert_eq!(state[1].1, 10);
    }
    let a = Loc::Reg(PhysReg(0));
    let moves = [
        Copy {
            src: a,
            dst: Loc::Slot(0),
            ty: Class(0),
        },
        Copy {
            src: a,
            dst: Loc::Slot(1),
            ty: Class(0),
        },
    ];
    assert_eq!(
        parallel::resolve_copies(&moves, &Tiny, true).unwrap().moves,
        moves
    );
    assert!(
        parallel::resolve_copies::<Loc, Class>(&[], &Tiny, false)
            .unwrap()
            .moves
            .is_empty()
    );
}

#[test]
fn copy_policy_cannot_alias_the_cycle_destination() {
    struct Bad;
    impl CopyPolicy<u8, ()> for Bad {
        fn break_cycle(&self, saved: u8, _: (), _: &[Copy<u8, ()>]) -> CycleBreak<u8, ()> {
            CycleBreak {
                holder: saved,
                saves: vec![],
                uses_scratch: false,
            }
        }
    }
    assert!(
        parallel::resolve_copies(
            &[
                Copy {
                    src: 0,
                    dst: 1,
                    ty: ()
                },
                Copy {
                    src: 1,
                    dst: 0,
                    ty: ()
                }
            ],
            &Bad,
            true
        )
        .is_err()
    );
}

fn program() -> live::Program {
    live::Program {
        successors: vec![],
        predecessors: vec![],
        entry: 0,
        uses: vec![],
        defs: vec![],
        points: vec![],
        values: vec![],
    }
}

#[test]
fn normalized_cfg_intervals_and_weights() {
    let mut input = program();
    input.successors = vec![vec![1], vec![2], vec![]];
    input.predecessors = vec![vec![], vec![0], vec![1]];
    input.uses = vec![vec![0], vec![0], vec![1]];
    input.defs = vec![vec![1], vec![0], vec![0]];
    input.points = (0..3)
        .map(|block| live::BlockPoint {
            block,
            use_slot: block * 2,
            def_slot: block * 2 + 1,
            fixed_physical: false,
            call: block == 1,
        })
        .collect();
    input.values = vec![live::Interval {
        start: 1,
        end: 4,
        uses: vec![4],
        rematerializable: false,
        weight: 0,
    }];
    let intervals = live::analyze(input).unwrap();
    assert_eq!((intervals[0].start, intervals[0].end), (1, 4));
    assert_eq!(intervals[0].weight, 1);
    assert!(live::analyze(program()).unwrap().is_empty());
    let mut invalid = program();
    invalid.successors = vec![vec![1]];
    invalid.predecessors = vec![vec![]];
    invalid.uses = vec![vec![]];
    invalid.defs = vec![vec![]];
    assert!(live::analyze(invalid).is_err());
    assert_eq!(live::loop_factor(0), 1);
    assert_eq!(live::loop_factor(100), 1_000_000);
}
