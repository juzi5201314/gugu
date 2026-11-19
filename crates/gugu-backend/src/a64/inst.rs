//! 操作数采用汇编顺序；地址是一个操作数，位移以字节为单位。

use super::{reg::Reg, table::FormId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Extend {
    Uxtb,
    Uxth,
    Uxtw,
    Uxtx,
    Sxtb,
    Sxth,
    Sxtw,
    Sxtx,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelocKind {
    Branch26,
    Call26,
    CondBr19,
    TestBr14,
    AdrpPage21,
    AddLo12,
    LdStLo12(u8),
    Literal19,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Offset(i64),
    Symbol { name: String, addend: i64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddressMode {
    Unsigned(i64),
    Unscaled(i64),
    Pre(i64),
    Post(i64),
    Register {
        index: Reg,
        extend: Extend,
        scaled: bool,
    },
    Reloc {
        name: String,
        addend: i64,
    },
    Base,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Address {
    pub base: Reg,
    pub mode: AddressMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operand {
    Reg(Reg),
    Imm(i64),
    Bits(u64),
    AddImm { imm12: u16, shift12: bool },
    Wide { imm16: u16, shift: u8 },
    Shift(Shift, u8),
    Extend(Extend, u8),
    Condition(u8),
    Address(Address),
    Target(Target),
    FpImm(u8),
    System(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inst {
    pub form: FormId,
    pub operands: Vec<Operand>,
}

impl Inst {
    pub fn new(form: FormId, operands: Vec<Operand>) -> Self {
        Self { form, operands }
    }
    pub fn text(&self) -> Result<String, super::Error> {
        super::print::text(self)
    }
}
