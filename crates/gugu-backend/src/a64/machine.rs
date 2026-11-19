//! Virtual selected instructions with explicit operand access events.
use super::{
    Error,
    inst::{Inst, Operand},
    reg::{Reg, Width},
    table::{self, FormId},
};
use crate::lir::body::Type;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MReg {
    Virtual(u32),
    Physical(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MOperand {
    Reg(MReg, Width, Access),
    Address(MReg),
    Fixed(Operand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MInst {
    pub form: FormId,
    pub operands: Vec<MOperand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Item {
    Atomic(super::atomic::Atomic),
    Inst(MInst),
    Constant {
        dest: u32,
        bits: u64,
    },
    StackAddr {
        dest: u32,
        slot: u32,
        root_init: bool,
    },
    Incoming {
        dest: u32,
        index: u32,
    },
    Copies(Vec<(MReg, MReg)>),
    RootInit {
        slot: u32,
        bit: u32,
        initialized: bool,
    },
    Poll {
        block: u32,
        instruction: u32,
    },
    Call {
        suspend: bool,
        target: String,
        arguments: Vec<u32>,
        results: Vec<u32>,
        safepoint: Option<(u32, u32)>,
    },
    TailCall {
        target: String,
        arguments: Vec<u32>,
    },
    Branch {
        form: FormId,
        operands: Vec<MOperand>,
        target: u32,
    },
    Return,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Site {
    pub context: String,
    pub item: Item,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub items: Vec<Site>,
    pub successors: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Function {
    pub name: String,
    pub entry: usize,
    pub checked_entry: bool,
    pub types: Vec<Type>,
    pub blocks: Vec<Block>,
}

impl Function {
    pub fn temp(&mut self, ty: Type) -> Result<u32, Error> {
        let id = u32::try_from(self.types.len())
            .map_err(|_| super::error("too many A64 virtual registers"))?;
        self.types.push(ty);
        Ok(id)
    }
}

pub fn form(key: &str) -> Result<FormId, Error> {
    table::find(key).ok_or_else(|| Error(format!("missing A64 form {key}")))
}

pub fn inst(key: &str, operands: Vec<Operand>) -> Result<Inst, Error> {
    Ok(Inst::new(form(key)?, operands))
}
pub fn read(id: u32) -> MOperand {
    MOperand::Reg(MReg::Virtual(id), Width::X, Access::Read)
}
pub fn write(id: u32) -> MOperand {
    MOperand::Reg(MReg::Virtual(id), Width::X, Access::Write)
}
pub fn fixed(operand: Operand) -> MOperand {
    MOperand::Fixed(operand)
}
pub fn zero() -> MOperand {
    fixed(Operand::Reg(Reg::zr(Width::X)))
}

impl Item {
    pub fn is_call(&self) -> bool {
        matches!(
            self,
            Self::Call { .. } | Self::TailCall { .. } | Self::Poll { .. }
        )
    }

    pub fn events(&self) -> Vec<(MReg, Access)> {
        let mut events = Vec::new();
        match self {
            Self::Atomic(_) => {
                events.extend((9..=15).map(|id| (MReg::Physical(id), Access::ReadWrite)))
            }
            Self::Inst(inst) => operand_events(&inst.operands, &mut events),
            Self::Branch { operands, .. } => operand_events(operands, &mut events),
            Self::Constant { dest, .. }
            | Self::StackAddr { dest, .. }
            | Self::Incoming { dest, .. } => events.push((MReg::Virtual(*dest), Access::Write)),
            Self::Copies(pairs) => {
                for (src, dst) in pairs {
                    events.push((*src, Access::Read));
                    events.push((*dst, Access::Write));
                }
            }
            Self::Call {
                arguments, results, ..
            } => {
                events.extend(
                    arguments
                        .iter()
                        .map(|id| (MReg::Virtual(*id), Access::Read)),
                );
                events.extend(results.iter().map(|id| (MReg::Virtual(*id), Access::Write)));
            }
            Self::TailCall { arguments, .. } => {
                events.extend(
                    arguments
                        .iter()
                        .map(|id| (MReg::Virtual(*id), Access::Read)),
                );
            }
            Self::Return | Self::Poll { .. } | Self::RootInit { .. } => {}
        }
        events
    }
}

fn operand_events(operands: &[MOperand], events: &mut Vec<(MReg, Access)>) {
    for operand in operands {
        match operand {
            MOperand::Reg(reg, _, access) => events.push((*reg, *access)),
            MOperand::Address(base) => events.push((*base, Access::Read)),
            MOperand::Fixed(_) => {}
        }
    }
}
