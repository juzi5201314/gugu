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
    Memory(super::memory::Kind),
    TrapIf {
        condition: u32,
    },
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
    SymbolAddr {
        dest: u32,
        symbol: String,
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
        // 间接调用的 Code-provenance 参数值：不占 ABI 槽，物化到 x8 后 `blr`（x16/x17 是并行拷贝组的破环/槽间暂存）。
        callee: Option<u32>,
        // 动态派发：接收者照常作实参传递，vtable 槽内容化物化到 x8 后 `blr`。
        dispatch: Option<VtableDispatch>,
        arguments: Vec<u32>,
        results: Vec<u32>,
        safepoint: Option<(u32, u32)>,
        // (目标 block 标签, cleanup 链)：invoke 的落点，写入 a64 unwind 表 landing 记录。
        landing: Option<(u32, u32)>,
    },
    TailCall {
        target: String,
        arguments: Vec<u32>,
    },
    ForeignCall {
        call: Box<crate::lir::body::Call>,
        platform: super::foreign::Platform,
        arguments: Vec<u32>,
        results: Vec<u32>,
        safepoint: (u32, u32),
    },
    /// `#[ffi(leaf)]` 直调：当前协程栈上按 C ABI 发 `bl`/`blr`，无 bridge、无安全点记录。
    ForeignLeaf {
        call: Box<crate::lir::body::Call>,
        platform: super::foreign::Platform,
        arguments: Vec<u32>,
        results: Vec<u32>,
    },
    Branch {
        form: FormId,
        operands: Vec<MOperand>,
        target: u32,
    },
    Return,
}

/// `CallTarget::Vtable` 的派发读取计划：接收者在 `arguments` 中照常传参，
/// 这里只描述如何从中取出 vtable 槽内的入口地址。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VtableDispatch {
    /// 接收者参数车道（虚拟寄存器）。
    pub receiver: u32,
    /// vtable 指针相对接收者的偏移：`Metadata` lane 为 0，指向胖对的指针为 8
    /// （`dyn` 值布局 `data@0`、`vtable@8`）。
    pub vtable_offset: u32,
    /// 目标槽在 vtable 中的字节偏移（`slot * 8`）。
    pub slot_offset: u32,
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
            Self::Call { .. }
                | Self::ForeignCall { .. }
                | Self::ForeignLeaf { .. }
                | Self::TailCall { .. }
                | Self::Poll { .. }
        )
    }

    pub fn events(&self) -> Vec<(MReg, Access)> {
        let mut events = Vec::new();
        match self {
            Self::Atomic(_) => {
                events.extend((9..=15).map(|id| (MReg::Physical(id), Access::ReadWrite)))
            }
            Self::Memory(_) => {
                events.extend((9..=12).map(|id| (MReg::Physical(id), Access::ReadWrite)))
            }
            Self::TrapIf { condition } => events.push((MReg::Virtual(*condition), Access::Read)),
            Self::Inst(inst) => operand_events(&inst.operands, &mut events),
            Self::Branch { operands, .. } => operand_events(operands, &mut events),
            Self::Constant { dest, .. }
            | Self::StackAddr { dest, .. }
            | Self::SymbolAddr { dest, .. }
            | Self::Incoming { dest, .. } => events.push((MReg::Virtual(*dest), Access::Write)),
            Self::Copies(pairs) => {
                for (src, dst) in pairs {
                    events.push((*src, Access::Read));
                    events.push((*dst, Access::Write));
                }
            }
            Self::Call {
                callee,
                arguments,
                results,
                ..
            } => {
                // callee 不占 ABI 槽，不在 arguments 里，但发射时要物化到 x8——
                // 必须登记为 Read，否则 regalloc 会在调用点之前回收它的位置。
                if let Some(callee) = callee {
                    events.push((MReg::Virtual(*callee), Access::Read));
                }
                events.extend(
                    arguments
                        .iter()
                        .map(|id| (MReg::Virtual(*id), Access::Read)),
                );
                events.extend(results.iter().map(|id| (MReg::Virtual(*id), Access::Write)));
            }
            Self::ForeignCall {
                arguments, results, ..
            }
            | Self::ForeignLeaf {
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
