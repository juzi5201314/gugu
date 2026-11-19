use super::*;
use crate::lir::foreign::Hfa;

fn integer(bytes: u64, align: u32) -> Type {
    Type {
        bytes,
        align,
        kind: Kind::Integer { signed: true },
    }
}
fn aggregate(bytes: u64, align: u32) -> Type {
    Type {
        bytes,
        align,
        kind: Kind::Aggregate {
            hfa: None,
            natural_align: align,
        },
    }
}
fn float(bits: u16) -> Type {
    Type {
        bytes: (bits / 8).into(),
        align: (bits / 8).into(),
        kind: Kind::Float { bits },
    }
}
fn signature(arguments: Vec<Type>) -> Signature {
    Signature {
        arguments,
        result: Type {
            bytes: 0,
            align: 1,
            kind: Kind::Unit,
        },
        variadic: None,
    }
}

#[test]
fn integer_pairs_do_not_split_and_linux_rounds_aligned_register_numbers() {
    let mut sig = signature(vec![integer(8, 8); 7]);
    sig.arguments.push(aggregate(16, 8));
    sig.arguments.push(integer(1, 1));
    for platform in [Platform::Linux, Platform::Darwin] {
        let layout = classify(&sig, platform).unwrap();
        assert_eq!(layout.arguments[7].pieces[0].slot, Slot::Stack(0));
        assert_eq!(layout.arguments[7].pieces[1].slot, Slot::Stack(8));
        assert_eq!(layout.arguments[8].pieces[0].slot, Slot::Stack(16));
    }
    let sig = signature(vec![integer(8, 8), integer(16, 16)]);
    assert_eq!(
        classify(&sig, Platform::Linux).unwrap().arguments[1].pieces[0].slot,
        Slot::Integer(2)
    );
    assert_eq!(
        classify(&sig, Platform::Darwin).unwrap().arguments[1].pieces[0].slot,
        Slot::Integer(1)
    );
}

#[test]
fn linux_adjusts_overaligned_composite_copies_without_relaxing_darwin_limits() {
    let mut large = aggregate(32, 32);
    if let Kind::Aggregate { natural_align, .. } = &mut large.kind {
        *natural_align = 8;
    }
    let sig = signature(vec![integer(8, 8), large]);
    let layout = classify(&sig, Platform::Linux).unwrap();
    assert!(layout.arguments[1].indirect);
    assert_eq!(layout.arguments[1].pieces[0].slot, Slot::Integer(1));
    assert!(classify(&sig, Platform::Darwin).is_err());
}

#[test]
fn apple_non_hfa_stack_aggregates_use_eightbyte_slots_between_packed_scalars() {
    let mut sig = signature(vec![integer(8, 8); 8]);
    sig.arguments.extend([
        aggregate(3, 1),
        integer(1, 1),
        aggregate(3, 1),
        integer(1, 1),
    ]);
    let layout = classify(&sig, Platform::Darwin).unwrap();
    for (argument, offset) in layout.arguments[8..].iter().zip([0, 8, 16, 24]) {
        assert_eq!(argument.pieces[0].slot, Slot::Stack(offset));
    }
    assert_eq!(layout.stack_bytes, 32);
}

#[test]
fn apple_packs_narrow_stack_arguments_and_extends_narrow_register_arguments() {
    let mut sig = signature(vec![integer(8, 8); 8]);
    sig.arguments
        .extend([integer(1, 1), integer(2, 2), integer(4, 4)]);
    let linux = classify(&sig, Platform::Linux).unwrap();
    let apple = classify(&sig, Platform::Darwin).unwrap();
    for (index, offset) in [0, 8, 16].into_iter().enumerate() {
        assert_eq!(
            linux.arguments[index + 8].pieces[0].slot,
            Slot::Stack(offset)
        );
    }
    for (index, offset) in [0, 2, 4].into_iter().enumerate() {
        assert_eq!(
            apple.arguments[index + 8].pieces[0].slot,
            Slot::Stack(offset)
        );
    }
    let sig = signature(vec![integer(1, 1)]);
    assert_eq!(
        classify(&sig, Platform::Darwin).unwrap().arguments[0].extend,
        Some((8, true))
    );
    assert_eq!(
        classify(&sig, Platform::Linux).unwrap().arguments[0].extend,
        None
    );
}

#[test]
fn hfas_are_all_or_stack_and_large_non_hfas_are_indirect() {
    let hfa = Type {
        bytes: 24,
        align: 8,
        kind: Kind::Aggregate {
            natural_align: 8,
            hfa: Some(Hfa {
                bits: 64,
                offsets: vec![0, 8, 16],
            }),
        },
    };
    let mut sig = signature(vec![float(64); 6]);
    sig.arguments
        .extend([hfa.clone(), float(32), aggregate(24, 8)]);
    sig.result = hfa;
    let layout = classify(&sig, Platform::Linux).unwrap();
    assert_eq!(
        layout.arguments[6]
            .pieces
            .iter()
            .map(|p| p.slot)
            .collect::<Vec<_>>(),
        [Slot::Stack(0), Slot::Stack(8), Slot::Stack(16)]
    );
    assert_eq!(layout.arguments[7].pieces[0].slot, Slot::Stack(24));
    assert!(layout.arguments[8].indirect);
    assert_eq!(layout.arguments[8].pieces[0].slot, Slot::Integer(0));
    assert!(!layout.result.indirect);
    assert_eq!(
        layout
            .result
            .pieces
            .iter()
            .map(|p| p.slot)
            .collect::<Vec<_>>(),
        [Slot::Float(0), Slot::Float(1), Slot::Float(2)]
    );
    sig.result = aggregate(24, 8);
    assert!(classify(&sig, Platform::Linux).unwrap().result.indirect);
}

#[test]
fn apple_anonymous_arguments_are_promoted_and_stacked_without_using_registers() {
    let mut sig = signature(vec![integer(8, 8), float(32), integer(1, 1), float(64)]);
    sig.variadic = Some(1);
    let apple = classify(&sig, Platform::Darwin).unwrap();
    let linux = classify(&sig, Platform::Linux).unwrap();
    for (index, offset) in [0, 8, 16].into_iter().enumerate() {
        assert_eq!(
            apple.arguments[index + 1].pieces[0].slot,
            Slot::Stack(offset)
        );
    }
    assert!(apple.arguments[1].promote_float);
    assert_eq!(apple.arguments[1].pieces[0].bytes, 8);
    assert_eq!(linux.arguments[1].pieces[0].slot, Slot::Float(0));
    assert_eq!(linux.arguments[2].pieces[0].slot, Slot::Integer(1));
    assert_eq!(linux.arguments[2].extend, Some((8, true)));
}

#[test]
fn empty_malformed_and_unbound_symbol_cases_are_explicit() {
    assert_eq!(
        classify(&signature(vec![]), Platform::Linux)
            .unwrap()
            .stack_bytes,
        0
    );
    let mut sig = signature(vec![integer(8, 8)]);
    sig.variadic = Some(2);
    assert!(classify(&sig, Platform::Linux).is_err());
    sig.variadic = None;
    sig.arguments[0].align = 32;
    assert!(classify(&sig, Platform::Linux).is_err());
    assert_eq!(Platform::Linux.symbol("read"), "read");
    assert_eq!(
        Platform::Darwin.symbol("read"),
        super::super::macho::import_stub_symbol("read")
    );
}
