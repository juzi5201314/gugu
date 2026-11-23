//! Manual VM verification on arm64 Darwin, with the page size from libSystem.
#[path = "support/a64_darwin_probe.rs"]
pub mod support;

use gugu_backend::{
    NativeImport,
    a64::{
        fragments::{darwin_page_size, darwin_vm, darwin_vm_imports},
        inst::{Address, AddressMode, Operand, Shift},
        macho::import_stub_symbol,
        reg::Reg,
    },
    macho::LIB_SYSTEM,
};
use gugu_runtime_abi::platform_schema::PlatformOp;
use support::{
    Arg::{Number as N, Register as R},
    Probe, reg,
};

fn shifted(n: u8) -> Operand {
    Operand::Shift(Shift::Lsl, n)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_darwin_vm_image <output>")?;
    let mut imports = darwin_vm_imports();
    imports.push(NativeImport::function(LIB_SYSTEM, "write"));
    let mut p = Probe::new(imports);
    let page = p.fragment("vm_page_size", darwin_page_size());
    let reserve = p.fragment("vm_reserve", darwin_vm(PlatformOp::ReserveAligned).unwrap());
    p.call(&page);
    p.expect_nonzero(0);
    p.mov(21, 0);
    print_page_size(&mut p);
    let extent = reserve_page(&mut p, &reserve);
    exercise(&mut p, &extent);
    p.output(b"VM: reserve, commit, write, guard, unprotect, decommit zero-fill, munmap: ok\n");
    let release = p.fragment("vm_release", darwin_vm(PlatformOp::Release).unwrap());
    let invalid = p.record(&[(0, N(1)), (8, R(21))]);
    p.address(0, &invalid);
    p.call(&release);
    p.expect(0, (-22_i64) as u64);
    p.output(b"unaligned munmap: errno=22\n");
    p.finish(&path)
}

fn reserve_page(p: &mut Probe, reserve: &str) -> String {
    p.mov(0, 21);
    p.mov(1, 21);
    p.call(reserve);
    p.expect_nonzero(0);
    p.mov(22, 0);
    // 页大小是 2 的幂；基址须按页对齐。
    p.number(10, 1);
    p.emit("sub.X.shift", vec![reg(11), reg(21), reg(10), shifted(0)]);
    p.emit("and.X.shift", vec![reg(10), reg(22), reg(11), shifted(0)]);
    p.expect(10, 0);
    p.record(&[(0, R(22)), (8, R(21))])
}

fn exercise(p: &mut Probe, extent: &str) {
    for (name, op) in [
        ("vm_commit", PlatformOp::Commit),
        ("vm_guard", PlatformOp::ProtectGuard),
        ("vm_unprotect", PlatformOp::Unprotect),
        ("vm_decommit", PlatformOp::Decommit),
        ("vm_release_ok", PlatformOp::Release),
    ] {
        let code = p.fragment(name, darwin_vm(op).unwrap());
        p.address(0, extent);
        p.call(&code);
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
            // Darwin 的 decommit 以 PROT_NONE 匿名映射覆盖区间；重新 commit 后应读到零页。
            p.address(0, extent);
            p.call("vm_commit");
            p.expect(0, 0);
            p.load(10, 22, 0);
            p.expect(10, 0);
        }
    }
}

fn print_page_size(p: &mut Probe) {
    let decimal = p.data(vec![0; 8]);
    p.address(22, &decimal);
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
    p.output(b"getpagesize()=");
    let write = p.offset();
    adjust(p, 22, -1);
    p.number(0, 1);
    p.mov(1, 22);
    p.number(2, 1);
    p.call(&import_stub_symbol("write"));
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
