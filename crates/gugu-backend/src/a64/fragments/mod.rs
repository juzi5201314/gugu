//! 尚未登记目标的 AArch64 运行时机器片段。

mod atomic;
mod callback;
mod coroutine;
mod darwin_fd;
mod darwin_net;
mod darwin_process;
mod darwin_resolver;
mod darwin_vm;
mod entry;
mod foreign;
mod linux_entry;
mod linux_fd;
mod linux_net;
mod linux_process;
mod linux_resolver;
mod linux_signal;
mod linux_syscall;
mod linux_vm;
mod native;
mod poll;
mod runtime;

pub use atomic::{AtomicWidth, atomic};
pub use callback::{callback_control, callback_queue};
pub use coroutine::{
    Context, ContextCode, MorestackCode, MorestackMailbox, MorestackScratch, context_switch,
    morestack_or_poll,
};
pub use darwin_fd::{darwin_fd_dispatch, darwin_fd_imports};
pub use darwin_net::{darwin_net_dispatch, darwin_net_imports};
pub use darwin_process::{darwin_process_call, darwin_process_imports, darwin_process_worker};
pub use darwin_resolver::{darwin_resolver_imports, darwin_resolver_symbol};
pub use darwin_vm::{darwin_page_size, darwin_vm, darwin_vm_imports};
pub(crate) use entry::darwin_main;
pub use entry::rt0;
pub use foreign::foreign_native_worker;
pub(crate) use linux_entry::linux_main;
pub use linux_fd::linux_fd_dispatch;
pub use linux_net::linux_net_dispatch;
pub use linux_process::{linux_process_call, linux_process_worker};
pub use linux_resolver::{
    linux_import_slot_symbol, linux_import_stub, linux_import_stub_symbol, linux_resolver_imports,
    linux_resolver_symbol,
};
pub use linux_signal::linux_signal_dispatch;
pub use linux_vm::{linux_page_size, linux_vm};
pub use native::{
    cage_control, callback_file_worker, darwin_environment_symbol, darwin_fs_poll, darwin_fs_start,
    darwin_monotonic_nanos, darwin_net_close, darwin_parallelism_probe, darwin_startup_call,
    darwin_wall_nanos, linux_environment_symbol, linux_fs_poll, linux_fs_start,
    linux_monotonic_nanos, linux_net_close, linux_parallelism_probe, linux_startup_call,
    linux_wall_nanos, native_imports, process_native_worker, stack_copy_raw, stack_read_word,
};
pub use poll::safepoint_slow;
pub use runtime::{
    context_restore, coroutine_enter, current_slot, gc_config, gc_enter, main_return, panic,
    panic_resume, process_exit, report_write, runtime_poll_defer, thread_id,
};

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
        self.external_branch_at(form, name, 0);
    }

    fn external_branch_at(&mut self, form: &str, name: &str, addend: i64) {
        self.emit(form, [inst_target(name, addend)]);
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

fn inst_target(name: &str, addend: i64) -> Operand {
    Operand::Target(Target::Symbol {
        name: name.into(),
        addend,
    })
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
