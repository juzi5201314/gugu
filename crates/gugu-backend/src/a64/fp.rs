//! Physical FP register identities and width-aware stack traffic.
use super::{
    Error,
    inst::{Address, AddressMode, Operand},
    layout,
    reg::{Reg, Width},
    rewrite,
};

pub fn reg(id: u8, width: Width) -> Reg {
    if id >= 32 {
        Reg::v(id - 32, width)
    } else {
        Reg {
            kind: super::reg::RegKind::Gpr(id),
            width,
        }
    }
}

pub fn width(ty: crate::lir::body::Type) -> Width {
    use crate::lir::body::Type;
    match ty {
        Type::F32 => Width::S,
        Type::F64 => Width::D,
        Type::V128(_) => Width::Q,
        _ => Width::X,
    }
}

pub fn stack(
    out: &mut Vec<layout::Item>,
    load: bool,
    id: u8,
    width: Width,
    offset: u32,
) -> Result<(), Error> {
    let bytes = match width {
        Width::S => 4,
        Width::D | Width::X => 8,
        Width::Q => 16,
        _ => return Err(super::error("invalid FP stack width")),
    };
    let (base, offset) = if offset.is_multiple_of(bytes) && offset / bytes <= 4095 {
        (Reg::sp(Width::X), offset)
    } else {
        let scratch = if id == 17 { 16 } else { 17 };
        rewrite::adjust(out, Reg::x(scratch), Reg::sp(Width::X), offset, false)?;
        (Reg::x(scratch), 0)
    };
    rewrite::emit(
        out,
        &format!(
            "{}.{}.Unsigned",
            if load { "ldr" } else { "str" },
            match width {
                Width::S => "S",
                Width::D => "D",
                Width::Q => "Q",
                _ => "X",
            }
        ),
        vec![
            Operand::Reg(reg(id, width)),
            Operand::Address(Address {
                base,
                mode: AddressMode::Unsigned(i64::from(offset)),
            }),
        ],
    )
}
