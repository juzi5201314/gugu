//! 整数字面量范围与序编码；覆盖算法使用闭区间，因此可表示 u128::MAX。
use super::super::ast::{AstArena, LitKind};
use super::model::Ty;

pub(super) fn magnitude(arena: &AstArena, literal: LitKind) -> Option<u128> {
    let LitKind::Int { limbs, .. } = literal else {
        return None;
    };
    limbs
        .as_slice(&arena.int_limbs)
        .iter()
        .rev()
        .try_fold(0u128, |n, &limb| {
            n.checked_mul(1u128 << 32)?.checked_add(u128::from(limb))
        })
}

pub(super) fn domain(ty: &Ty) -> Option<(u128, u128)> {
    match ty {
        Ty::Bool => Some((0, 1)),
        Ty::Char => Some((0, 0x10ffff)),
        Ty::Int { bits, .. } => Some((
            0,
            if *bits == 128 {
                u128::MAX
            } else {
                (1u128 << bits) - 1
            },
        )),
        _ => None,
    }
}

pub(super) fn integer_ordinal(magnitude: u128, negative: bool, ty: &Ty) -> Option<u128> {
    let Ty::Int { signed, bits } = ty else {
        return None;
    };
    debug_assert!([8, 16, 32, 64, 128].contains(bits));
    if *signed {
        let half = 1u128 << (bits - 1);
        if negative {
            half.checked_sub(magnitude)
        } else if magnitude < half {
            Some(half + magnitude)
        } else {
            None
        }
    } else if !negative && magnitude <= domain(ty)?.1 {
        Some(magnitude)
    } else {
        None
    }
}

pub(super) fn literal_ordinal(
    arena: &AstArena,
    literal: LitKind,
    negative: bool,
    ty: &Ty,
) -> Option<u128> {
    match literal {
        LitKind::Int { .. } => integer_ordinal(magnitude(arena, literal)?, negative, ty),
        LitKind::Bool(value) if !negative && *ty == Ty::Bool => Some(u128::from(value)),
        LitKind::Char { value, .. } if !negative && *ty == Ty::Char => {
            Some(u128::from(u32::from(value)))
        }
        LitKind::ByteChar { value, .. }
            if !negative
                && *ty
                    == (Ty::Int {
                        signed: false,
                        bits: 8,
                    }) =>
        {
            Some(u128::from(value))
        }
        _ => None,
    }
}

pub(super) fn ordinal(value: i128, ty: &Ty) -> Option<u128> {
    if *ty == Ty::Char {
        u128::try_from(value).ok().filter(|v| *v <= 0x10ffff)
    } else {
        integer_ordinal(value.unsigned_abs(), value < 0, ty)
    }
}
