//! AAPCS64 classification of original C types, independent of legacy LIR lanes.
use super::{Error, error};
use crate::lir::foreign::{Kind, Signature, Type};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Linux,
    Darwin,
}

impl Platform {
    pub fn symbol(self, name: &str) -> String {
        match self {
            Self::Linux => name.into(),
            Self::Darwin => super::macho::import_stub_symbol(name),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Slot {
    Integer(u8),
    Float(u8),
    Stack(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Piece {
    pub offset: u32,
    pub bytes: u8,
    pub slot: Slot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Argument {
    pub pieces: Vec<Piece>,
    pub indirect: bool,
    pub promote_float: bool,
    pub extend: Option<(u8, bool)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub arguments: Vec<Argument>,
    pub result: Argument,
    pub stack_bytes: u32,
}

fn align(value: u32, alignment: u32) -> Result<u32, Error> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or_else(|| error("C argument area overflow"))
}

fn chunks(bytes: u32, mut slot: impl FnMut(u32) -> Slot) -> Vec<Piece> {
    (0..bytes)
        .step_by(8)
        .map(|offset| Piece {
            offset,
            bytes: (bytes - offset).min(8) as u8,
            slot: slot(offset),
        })
        .collect()
}

fn hfa(ty: &Type) -> Option<(u8, Vec<u32>)> {
    match &ty.kind {
        Kind::Float { bits } => Some(((*bits / 8) as u8, vec![0])),
        Kind::Aggregate { hfa: Some(hfa) } => Some((
            (hfa.bits / 8) as u8,
            hfa.offsets.iter().map(|offset| *offset as u32).collect(),
        )),
        _ => None,
    }
}

fn empty() -> Argument {
    Argument {
        pieces: vec![],
        indirect: false,
        promote_float: false,
        extend: None,
    }
}

struct State {
    gpr: u8,
    fpr: u8,
    stack: u32,
    platform: Platform,
}

impl State {
    fn argument(&mut self, ty: &Type, anonymous: bool) -> Result<Argument, Error> {
        let mut argument = empty();
        if ty.bytes == 0 {
            return Ok(argument);
        }
        let darwin = self.platform == Platform::Darwin;
        let promote_float = anonymous && matches!(ty.kind, Kind::Float { bits: 32 });
        argument.promote_float = promote_float;
        let promoted_integer = anonymous && matches!(ty.kind, Kind::Integer { .. }) && ty.bytes < 4;
        if let Kind::Integer { signed } = ty.kind
            && ty.bytes < 4
            && (darwin || anonymous)
        {
            argument.extend = Some((ty.bytes as u8 * 8, signed));
        }
        let mut bytes = ty.bytes as u32;
        let mut alignment = ty.align;
        if promote_float {
            bytes = 8;
            alignment = 8;
        }
        if promoted_integer {
            bytes = 4;
            alignment = 4;
        }
        let floating = hfa(ty);
        argument.indirect =
            floating.is_none() && matches!(ty.kind, Kind::Aggregate { .. }) && bytes > 16;
        if argument.indirect {
            bytes = 8;
            alignment = 8;
        }
        if !(darwin && anonymous) {
            if let Some((element, offsets)) = floating {
                if self.fpr as usize + offsets.len() <= 8 {
                    argument.pieces = offsets
                        .into_iter()
                        .map(|offset| {
                            let slot = Slot::Float(self.fpr);
                            self.fpr += 1;
                            Piece {
                                offset,
                                bytes: if promote_float { 8 } else { element },
                                slot,
                            }
                        })
                        .collect();
                    return Ok(argument);
                }
                self.fpr = 8;
            } else {
                if !darwin && alignment == 16 {
                    self.gpr = (self.gpr + 1) & !1;
                }
                let words = bytes.div_ceil(8) as u8;
                if self.gpr + words <= 8 {
                    argument.pieces = chunks(bytes, |_| {
                        let slot = Slot::Integer(self.gpr);
                        self.gpr += 1;
                        slot
                    });
                    return Ok(argument);
                }
                self.gpr = 8;
            }
        }
        let grain = if darwin && !anonymous && !matches!(ty.kind, Kind::Aggregate { hfa: None }) {
            1
        } else {
            8
        };
        if bytes > crate::runtime::foreign_schema::BLOCKING_WORKER_STACK_BYTES - 4096 {
            return Err(error("C stack arguments exceed native worker stack budget"));
        }
        self.stack = align(self.stack, alignment.max(grain))?;
        let start = self.stack;
        self.stack = self
            .stack
            .checked_add(align(bytes, grain)?)
            .ok_or_else(|| error("C stack argument overflow"))?;
        argument.pieces = chunks(bytes, |offset| Slot::Stack(start + offset));
        Ok(argument)
    }
}

pub fn classify(signature: &Signature, platform: Platform) -> Result<Layout, Error> {
    if !signature.valid()
        || signature
            .arguments
            .iter()
            .chain([&signature.result])
            .any(|ty| ty.bytes > i32::MAX as u64 || ty.align > 16)
    {
        return Err(error("invalid or oversized/overaligned A64 C signature"));
    }
    let mut state = State {
        gpr: 0,
        fpr: 0,
        stack: 0,
        platform,
    };
    let arguments = signature
        .arguments
        .iter()
        .enumerate()
        .map(|(index, ty)| {
            state.argument(
                ty,
                signature
                    .variadic
                    .is_some_and(|fixed| index >= fixed as usize),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = empty();
    let ty = &signature.result;
    if let Some((bytes, offsets)) = hfa(ty) {
        result.pieces = offsets
            .into_iter()
            .enumerate()
            .map(|(index, offset)| Piece {
                offset,
                bytes,
                slot: Slot::Float(index as u8),
            })
            .collect();
    } else if ty.bytes > 16 {
        result.indirect = true;
    } else {
        result.pieces = chunks(ty.bytes as u32, |offset| Slot::Integer((offset / 8) as u8));
    }
    let stack_bytes = align(state.stack, 16)?;
    if stack_bytes > crate::runtime::foreign_schema::BLOCKING_WORKER_STACK_BYTES - 4096 {
        return Err(error("C stack arguments exceed native worker stack budget"));
    }
    Ok(Layout {
        arguments,
        result,
        stack_bytes,
    })
}

#[cfg(test)]
#[path = "foreign_tests.rs"]
mod tests;
