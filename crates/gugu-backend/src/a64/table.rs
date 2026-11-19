//! 封闭描述符表；所有可变位均由操作数槽显式拥有。

use super::{inst::RelocKind, reg::Width};

pub use super::rows::FORMS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FormId(pub u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Feature {
    Base,
    FpAdvSimd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddrMode {
    Unsigned,
    Unscaled,
    Pre,
    Post,
    Register,
    Pair,
    PairPre,
    PairPost,
    Base,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Reg {
        width: Width,
        sp: bool,
        vector: bool,
    },
    Imm {
        bits: u8,
        signed: bool,
        scale: u8,
    },
    AddImm,
    Logical(u8),
    Wide(u8),
    ExtendedReg(u8),
    TestBit(bool),
    Shift {
        width: u8,
        ror: bool,
    },
    Extend(u8),
    Condition,
    Address {
        mode: AddrMode,
        size: u8,
    },
    Target {
        bits: u8,
        scale: u8,
        reloc: Option<RelocKind>,
    },
    FpImm,
    System(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Field {
    pub lsb: u8,
    pub width: u8,
    pub value_lsb: u8,
}

impl Field {
    pub const fn mask(self) -> u32 {
        if self.width == 0 {
            0
        } else {
            ((1u32 << self.width) - 1) << self.lsb
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slot {
    pub kind: Kind,
    pub fields: [Field; 3],
}

impl Slot {
    pub const fn mask(self) -> u32 {
        self.fields[0].mask() | self.fields[1].mask() | self.fields[2].mask()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Form {
    pub key: &'static str,
    pub mnemonic: &'static str,
    pub fixed: u32,
    pub mask: u32,
    pub operands: &'static [Slot],
    pub feature: Feature,
}

impl Form {
    pub const fn new(
        key: &'static str,
        mnemonic: &'static str,
        fixed: u32,
        operands: &'static [Slot],
        feature: Feature,
    ) -> Self {
        let mut variable = 0;
        let mut i = 0;
        while i < operands.len() {
            variable |= operands[i].mask();
            i += 1;
        }
        Self {
            key,
            mnemonic,
            fixed,
            mask: !variable,
            operands,
            feature,
        }
    }
}

pub fn find(key: &str) -> Option<FormId> {
    FORMS
        .iter()
        .position(|f| f.key == key)
        .map(|i| FormId(i as u16))
}

pub fn get(id: FormId) -> Result<&'static Form, super::Error> {
    FORMS
        .get(usize::from(id.0))
        .ok_or_else(|| super::error("未知 A64 form"))
}

const EMPTY: Field = Field {
    lsb: 0,
    width: 0,
    value_lsb: 0,
};
const fn f(lsb: u8, width: u8, value_lsb: u8) -> Field {
    Field {
        lsb,
        width,
        value_lsb,
    }
}
const fn slot(kind: Kind, a: Field, b: Field, c: Field) -> Slot {
    Slot {
        kind,
        fields: [a, b, c],
    }
}
pub(crate) const fn reg(width: Width, sp: bool, vector: bool, pos: u8) -> Slot {
    slot(Kind::Reg { width, sp, vector }, f(pos, 5, 0), EMPTY, EMPTY)
}
pub(crate) const fn imm(bits: u8, signed: bool, scale: u8, pos: u8) -> Slot {
    slot(
        Kind::Imm {
            bits,
            signed,
            scale,
        },
        f(pos, bits, 0),
        EMPTY,
        EMPTY,
    )
}
pub(crate) const fn addimm() -> Slot {
    slot(Kind::AddImm, f(10, 13, 0), EMPTY, EMPTY)
}
pub(crate) const fn logical(width: u8) -> Slot {
    slot(Kind::Logical(width), f(10, 13, 0), EMPTY, EMPTY)
}
pub(crate) const fn wide(width: u8) -> Slot {
    slot(Kind::Wide(width), f(5, 18, 0), EMPTY, EMPTY)
}
pub(crate) const fn shift(width: u8, ror: bool) -> Slot {
    slot(Kind::Shift { width, ror }, f(10, 6, 0), f(22, 2, 6), EMPTY)
}
pub(crate) const fn extend(width: u8) -> Slot {
    slot(Kind::Extend(width), f(10, 6, 0), EMPTY, EMPTY)
}
pub(crate) const fn extended_reg(width: u8) -> Slot {
    slot(Kind::ExtendedReg(width), f(16, 5, 0), EMPTY, EMPTY)
}
pub(crate) const fn testbit(high: bool) -> Slot {
    slot(Kind::TestBit(high), f(19, 5, 0), EMPTY, EMPTY)
}
pub(crate) const fn cond(pos: u8) -> Slot {
    slot(Kind::Condition, f(pos, 4, 0), EMPTY, EMPTY)
}
pub(crate) const fn target(bits: u8, scale: u8, pos: u8, reloc: Option<RelocKind>) -> Slot {
    slot(
        Kind::Target { bits, scale, reloc },
        f(pos, bits, 0),
        EMPTY,
        EMPTY,
    )
}
pub(crate) const fn adr(page: bool) -> Slot {
    slot(
        Kind::Target {
            bits: 21,
            scale: if page { 12 } else { 0 },
            reloc: if page {
                Some(RelocKind::AdrpPage21)
            } else {
                None
            },
        },
        f(29, 2, 0),
        f(5, 19, 2),
        EMPTY,
    )
}
pub(crate) const fn address(mode: AddrMode, size: u8) -> Slot {
    let (a, b) = match mode {
        AddrMode::Unsigned => (f(10, 12, 5), EMPTY),
        AddrMode::Unscaled | AddrMode::Pre | AddrMode::Post => (f(12, 9, 5), EMPTY),
        AddrMode::Pair | AddrMode::PairPre | AddrMode::PairPost => (f(15, 7, 5), EMPTY),
        AddrMode::Register => (f(16, 5, 5), f(12, 4, 10)),
        AddrMode::Base => (EMPTY, EMPTY),
    };
    slot(Kind::Address { mode, size }, f(5, 5, 0), a, b)
}
pub(crate) const fn fpimm() -> Slot {
    slot(Kind::FpImm, f(13, 8, 0), EMPTY, EMPTY)
}
pub(crate) const fn system(name: &'static str) -> Slot {
    slot(Kind::System(name), EMPTY, EMPTY, EMPTY)
}
