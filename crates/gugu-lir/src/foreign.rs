//! Original C types, independent of target-specific flattened value lanes.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Type {
    pub bytes: u64,
    pub align: u32,
    pub kind: Kind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    Unit,
    Integer { signed: bool },
    Float { bits: u16 },
    Pointer,
    Aggregate { hfa: Option<Hfa> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Hfa {
    pub bits: u16,
    pub offsets: Vec<u64>,
}

impl Hfa {
    pub(crate) fn valid_for(&self, bytes: u64) -> bool {
        let element = u64::from(self.bits / 8);
        matches!(self.bits, 32 | 64)
            && (1..=4).contains(&self.offsets.len())
            && bytes == self.offsets.len() as u64 * element
            && self
                .offsets
                .iter()
                .enumerate()
                .all(|(index, offset)| *offset == index as u64 * element)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    /// Original source arguments, including zero-sized values; excludes sret/callee lanes.
    pub arguments: Vec<Type>,
    pub result: Type,
    /// Number of fixed C arguments, not flattened lanes. None means non-variadic.
    pub variadic: Option<u32>,
}

impl Type {
    pub fn valid(&self) -> bool {
        if !self.align.is_power_of_two() || !self.bytes.is_multiple_of(u64::from(self.align)) {
            return false;
        }
        match &self.kind {
            Kind::Unit => self.bytes == 0 && self.align == 1,
            Kind::Integer { .. } => matches!(self.bytes, 1 | 2 | 4 | 8 | 16),
            Kind::Float { bits } => matches!(bits, 32 | 64) && self.bytes == u64::from(*bits / 8),
            Kind::Pointer => self.bytes == 8 && self.align == 8,
            Kind::Aggregate { hfa: None } => true,
            Kind::Aggregate { hfa: Some(hfa) } => hfa.valid_for(self.bytes),
        }
    }
}

impl Signature {
    pub fn valid(&self) -> bool {
        self.result.valid()
            && self.arguments.iter().all(Type::valid)
            && self
                .variadic
                .is_none_or(|fixed| fixed as usize <= self.arguments.len())
    }

    pub fn matches(&self, call: &crate::body::Call) -> bool {
        let mut arguments = self.arguments.iter().filter(|ty| ty.bytes != 0);
        self.valid()
            && call.foreign_arguments.iter().all(|group| {
                arguments
                    .next()
                    .is_some_and(|ty| ty.bytes == u64::from(group.bytes) && ty.align == group.align)
            })
            && arguments.next().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_signatures_validate_empty_variadic_and_malformed_shapes() {
        let unit = Type {
            bytes: 0,
            align: 1,
            kind: Kind::Unit,
        };
        let mut signature = Signature {
            arguments: vec![],
            result: unit.clone(),
            variadic: None,
        };
        assert!(signature.valid());
        signature.variadic = Some(0);
        assert!(signature.valid());
        signature.variadic = Some(1);
        assert!(!signature.valid());
        signature.arguments.push(Type {
            bytes: 8,
            align: 8,
            kind: Kind::Float { bits: 64 },
        });
        assert!(signature.valid());
        let bytes = serde_json::to_vec(&signature).unwrap();
        assert_eq!(
            serde_json::from_slice::<Signature>(&bytes).unwrap(),
            signature
        );
        for ty in [
            Type {
                align: 3,
                ..unit.clone()
            },
            Type { bytes: 1, ..unit },
            Type {
                bytes: 3,
                align: 1,
                kind: Kind::Integer { signed: false },
            },
            Type {
                bytes: 8,
                align: 8,
                kind: Kind::Float { bits: 32 },
            },
            Type {
                bytes: 4,
                align: 4,
                kind: Kind::Pointer,
            },
        ] {
            assert!(!ty.valid(), "{ty:?}");
        }
    }

    #[test]
    fn homogeneous_aggregates_reject_invalid_elements_offsets_counts_and_overflow() {
        let aggregate = |bits, offsets| Type {
            bytes: 32,
            align: 8,
            kind: Kind::Aggregate {
                hfa: Some(Hfa { bits, offsets }),
            },
        };
        assert!(aggregate(64, vec![0, 8, 16, 24]).valid());
        for (bits, offsets) in [
            (64, vec![0, 8, 16]),
            (64, vec![8, 16, 24]),
            (64, vec![0, 16, 24]),
            (16, vec![0]),
            (32, vec![]),
            (32, vec![0, 4, 8, 12, 16]),
            (64, vec![0, 4]),
            (64, vec![8, 0]),
            (64, vec![32]),
            (64, vec![u64::MAX - 7]),
        ] {
            assert!(!aggregate(bits, offsets).valid());
        }
    }
}
