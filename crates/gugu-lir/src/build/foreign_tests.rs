use super::*;
use crate::foreign::{Hfa, Kind};
use gugu_gir::{
    gir::{concrete::Field, passing::PassingClass},
    types::Layout,
};

#[test]
fn packed_member_alignment_is_not_restored_by_outer_alignment() {
    let wide = TypeLayout {
        key: [0; 32],
        layout: Some(Layout {
            size: 16,
            align: 16,
        }),
        kind: TypeKind::Int {
            signed: false,
            bits: 128,
        },
        passing: PassingClass::BITS,
        transparent: false,
        packed: false,
        c_repr: None,
    };
    let mut types = vec![wide];
    for (packed, align, expected) in [(false, 16, 16), (true, 1, 1), (true, 16, 1)] {
        types.push(TypeLayout {
            packed,
            layout: Some(Layout { size: 16, align }),
            kind: TypeKind::Aggregate {
                tag_bytes: 0,
                variants: vec![vec![Field { ty: 0, offset: 0 }]],
            },
            ..types[0].clone()
        });
        assert_eq!(
            describe(&types, types.len() as u32 - 1).kind,
            Kind::Aggregate {
                hfa: None,
                natural_align: expected
            }
        );
    }
    types.push(TypeLayout {
        packed: false,
        kind: TypeKind::Aggregate {
            tag_bytes: 0,
            variants: vec![vec![Field { ty: 3, offset: 0 }]],
        },
        ..types[0].clone()
    });
    assert_eq!(
        describe(&types, 4).kind,
        Kind::Aggregate {
            hfa: None,
            natural_align: 16
        }
    );
}

#[test]
fn canonical_hfa_excludes_tail_and_nested_padding_but_keeps_dense_overalignment() {
    let scalar = TypeLayout {
        key: [0; 32],
        layout: Some(Layout { size: 4, align: 4 }),
        kind: TypeKind::Float(32),
        passing: PassingClass::BITS,
        transparent: false,
        packed: false,
        c_repr: None,
    };
    let mut types = vec![scalar];
    for (size, align, fields) in [
        (12, 4, vec![(0, 0), (0, 4), (0, 8)]),
        (16, 16, vec![(0, 0), (0, 4), (0, 8)]),
        (16, 16, vec![(0, 0), (0, 4), (0, 8), (0, 12)]),
        (32, 16, vec![(2, 0), (0, 16)]),
    ] {
        types.push(TypeLayout {
            key: [0; 32],
            layout: Some(Layout { size, align }),
            kind: TypeKind::Aggregate {
                tag_bytes: 0,
                variants: vec![
                    fields
                        .into_iter()
                        .map(|(ty, offset)| Field { ty, offset })
                        .collect(),
                ],
            },
            passing: PassingClass::BITS,
            transparent: false,
            packed: false,
            c_repr: None,
        });
    }
    for (index, expected) in [
        (1, Some(vec![0, 4, 8])),
        (2, None),
        (3, Some(vec![0, 4, 8, 12])),
        (4, None),
    ] {
        let ty = describe(&types, index);
        assert_eq!(
            ty.kind,
            Kind::Aggregate {
                natural_align: if index == 4 { 16 } else { 4 },
                hfa: expected.map(|offsets| Hfa { bits: 32, offsets })
            },
            "type {index}"
        );
        assert!(ty.valid());
    }
    for (kind, c_repr) in [
        (
            TypeKind::Array {
                element: 2,
                count: 1,
            },
            None,
        ),
        (
            TypeKind::Aggregate {
                tag_bytes: 0,
                variants: vec![vec![Field { ty: 0, offset: 0 }, Field { ty: 2, offset: 0 }]],
            },
            Some(crate::gir::concrete::CRepresentation::Union),
        ),
    ] {
        types.push(TypeLayout {
            kind,
            c_repr,
            ..types[2].clone()
        });
        assert_eq!(
            describe(&types, types.len() as u32 - 1).kind,
            Kind::Aggregate {
                hfa: None,
                natural_align: 16
            }
        );
    }
}
