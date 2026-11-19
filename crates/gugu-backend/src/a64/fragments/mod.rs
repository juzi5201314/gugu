//! 尚未登记目标的 AArch64 运行时机器片段。

mod atomic;
mod coroutine;
mod entry;
mod poll;

pub use atomic::{AtomicWidth, atomic};
pub use coroutine::{
    Context, ContextCode, MorestackCode, MorestackMailbox, MorestackScratch, context_switch,
    morestack_or_poll,
};
pub(crate) use entry::darwin_main;
pub use entry::rt0;
pub use poll::safepoint_slow;

use super::{
    encode::{self, Relocation},
    inst::{Address, AddressMode, Inst, Operand, Shift, Target},
    reg::{Reg, Width},
    table,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fragment {
    pub bytes: Vec<u8>,
    pub relocations: Vec<FragmentRelocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentRelocation {
    /// 指令字在片段中的字节偏移。
    pub offset: u32,
    pub relocation: Relocation,
}

const SP: Reg = Reg::sp(Width::X);
const HOT: Reg = Reg::x(27);
const PROCESSOR: Reg = Reg::x(28);
const LR: Reg = Reg::x(30);

#[derive(Default)]
struct Builder {
    bytes: Vec<u8>,
    relocations: Vec<FragmentRelocation>,
}

impl Builder {
    fn offset(&self) -> u32 {
        u32::try_from(self.bytes.len()).expect("机器片段长度适配 u32")
    }

    fn emit(&mut self, form: &str, operands: impl Into<Vec<Operand>>) {
        let offset = self.offset();
        let encoded = encode_inst(form, operands.into());
        self.bytes.extend(encoded.bytes());
        self.relocations.extend(
            encoded
                .relocations
                .into_iter()
                .map(|relocation| FragmentRelocation { offset, relocation }),
        );
    }

    fn branch(&mut self, form: &str, leading: &[Operand], destination: u32) {
        let at = self.offset();
        self.emit(form, local_branch(leading, at, destination, &[]));
    }

    fn patch_branch(&mut self, at: u32, form: &str, leading: &[Operand], trailing: &[Operand]) {
        let operands = local_branch(leading, at, self.offset(), trailing);
        let encoded = encode_inst(form, operands);
        let at = at as usize;
        self.bytes[at..at + 4].copy_from_slice(&encoded.bytes());
    }

    fn external_branch(&mut self, form: &str, name: &str) {
        self.emit(
            form,
            [Operand::Target(Target::Symbol {
                name: name.into(),
                addend: 0,
            })],
        );
    }

    fn memory(&mut self, form: &str, value: Reg, base: Reg, offset: usize) {
        self.emit(
            form,
            [reg(value), mem(base, AddressMode::Unsigned(offset as i64))],
        );
    }

    fn add(&mut self, dst: Reg, src: Reg, imm12: usize) {
        self.emit("add.X.imm", [reg(dst), reg(src), add_imm(imm12)]);
    }

    fn ret(&mut self) {
        self.emit("ret", [reg(LR)]);
    }

    fn finish(self) -> Fragment {
        Fragment {
            bytes: self.bytes,
            relocations: self.relocations,
        }
    }
}

fn encode_inst(form: &str, operands: Vec<Operand>) -> encode::Encoded {
    let id = table::find(form).unwrap_or_else(|| panic!("未登记 A64 形式 {form}"));
    encode::encode(&Inst::new(id, operands))
        .unwrap_or_else(|error| panic!("A64 片段编码 {form}: {error}"))
}

fn local_branch(
    leading: &[Operand],
    at: u32,
    destination: u32,
    trailing: &[Operand],
) -> Vec<Operand> {
    let mut operands = leading.to_vec();
    operands.push(Operand::Target(Target::Offset(
        i64::from(destination) - i64::from(at),
    )));
    operands.extend_from_slice(trailing);
    operands
}

fn add_imm(value: usize) -> Operand {
    Operand::AddImm {
        imm12: u16::try_from(value).expect("A64 立即数适配 u16"),
        shift12: false,
    }
}

fn reg(register: Reg) -> Operand {
    Operand::Reg(register)
}

fn mem(base: Reg, mode: AddressMode) -> Operand {
    Operand::Address(Address { base, mode })
}

fn imm(value: i64) -> Operand {
    Operand::Imm(value)
}

fn unshifted() -> Operand {
    Operand::Shift(Shift::Lsl, 0)
}

fn condition(value: u8) -> Operand {
    Operand::Condition(value)
}

fn next() -> Operand {
    Operand::Target(Target::Offset(0))
}

fn movz(value: u16) -> Operand {
    Operand::Wide {
        imm16: value,
        shift: 0,
    }
}

#[cfg(test)]
mod tests;
