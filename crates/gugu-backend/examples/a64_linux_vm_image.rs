//! Manual VM verification, with the native page size obtained from the Linux auxv.
#[path = "support/a64_linux_probe.rs"]
pub mod support;

use gugu_backend::a64::{
    elf::Location,
    fragments::{linux_page_size, linux_vm},
    inst::{Address, AddressMode, Operand, Shift},
    reg::Reg,
};
use gugu_runtime_abi::{entry::Rt0Payload, platform_schema::PlatformOp};
use support::{
    Arg::{Number as N, Register as R},
    Probe, at, reg,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_linux_vm_image <output>")?;
    let mut p = Probe::new();
    let page = p.fragment(linux_page_size());
    let reserve = p.fragment(linux_vm(PlatformOp::ReserveAligned).unwrap());
    p.address(9, at(1));
    p.load(0, 9, std::mem::offset_of!(Rt0Payload, boot_environment));
    p.call(page);
    p.expect_nonzero(0);
    p.mov(21, 0);
    print_page_size(&mut p);
    let extent = reserve_page(&mut p, reserve);
    exercise(&mut p, extent);
    p.output(b"VM: reserve, commit, write, guard, unprotect, madvise zero-fill, munmap: ok\n");
    let release = p.fragment(linux_vm(PlatformOp::Release).unwrap());
    let invalid = p.record(16, &[(0, N(1)), (8, R(21))]);
    p.address(0, invalid);
    p.call(release);
    p.expect(0, (-22_i64) as u64);
    p.output(b"unaligned munmap: errno=22\n");
    p.finish(&path)
}

fn reserve_page(p: &mut Probe, reserve: Location) -> Location {
    p.mov(0, 21);
    p.mov(1, 21);
    p.call(reserve);
    p.expect_nonzero(0);
    p.mov(22, 0);
    p.emit(
        "sub.X.imm",
        vec![
            reg(10),
            reg(21),
            Operand::AddImm {
                imm12: 1,
                shift12: false,
            },
        ],
    );
    p.emit(
        "and.X.shift",
        vec![reg(10), reg(22), reg(10), Operand::Shift(Shift::Lsl, 0)],
    );
    p.expect(10, 0);
    p.record(16, &[(0, R(22)), (8, R(21))])
}

fn exercise(p: &mut Probe, extent: Location) {
    for op in [
        PlatformOp::Commit,
        PlatformOp::ProtectGuard,
        PlatformOp::Unprotect,
        PlatformOp::Decommit,
        PlatformOp::Release,
    ] {
        let code = p.fragment(linux_vm(op).unwrap());
        p.address(0, extent);
        p.call(code);
        p.expect(0, 0);
        if op == PlatformOp::Commit {
            p.number(10, 0x12345678);
            p.store(10, 22, 0);
        }
        if op == PlatformOp::Unprotect {
            p.load(10, 22, 0);
            p.expect(10, 0x12345678);
        }
        if op == PlatformOp::Decommit {
            p.load(10, 22, 0);
            p.expect(10, 0);
        }
    }
}

fn print_page_size(p: &mut Probe) {
    let decimal = p.data(vec![0; 8]);
    p.address(22, decimal);
    p.number(23, 0);
    p.mov(0, 21);
    p.number(1, 10);
    let digits = p.offset();
    p.emit("udiv.X", vec![reg(2), reg(0), reg(1)]);
    p.emit("msub.X", vec![reg(3), reg(2), reg(1), reg(0)]);
    adjust(p, 3, 48);
    p.emit(
        "strb.W.Unsigned",
        vec![
            Operand::Reg(Reg::w(3)),
            Operand::Address(Address {
                base: Reg::x(22),
                mode: AddressMode::Unsigned(0),
            }),
        ],
    );
    adjust(p, 22, 1);
    adjust(p, 23, 1);
    p.mov(0, 2);
    p.repeat_if(0, 0, digits, 1);
    p.output(b"AT_PAGESZ=");
    let write = p.offset();
    adjust(p, 22, -1);
    p.number(0, 1);
    p.mov(1, 22);
    p.number(2, 1);
    p.number(8, 64);
    p.emit("svc", vec![Operand::Imm(0)]);
    adjust(p, 23, -1);
    p.repeat_if(23, 0, write, 1);
    p.output(b"\n");
}

fn adjust(p: &mut Probe, n: u8, amount: i16) {
    p.emit(
        if amount < 0 { "sub.X.imm" } else { "add.X.imm" },
        vec![
            reg(n),
            reg(n),
            Operand::AddImm {
                imm12: amount.unsigned_abs(),
                shift12: false,
            },
        ],
    );
}
