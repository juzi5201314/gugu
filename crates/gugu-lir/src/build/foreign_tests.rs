use super::*;
use crate::foreign::{Hfa, Kind};
use gugu_gir::{
    gir::{concrete::Field, passing::PassingClass},
    types::Layout,
};

#[test]
fn canonical_hfa_excludes_tail_and_nested_padding_but_keeps_dense_overalignment() {
    let scalar = TypeLayout {
        key: [0; 32],
        layout: Some(Layout { size: 4, align: 4 }),
        kind: TypeKind::Float(32),
        passing: PassingClass::BITS,
        transparent: false,
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
                hfa: expected.map(|offsets| Hfa { bits: 32, offsets })
            },
            "type {index}"
        );
        assert!(ty.valid());
    }
}
