//! C ABI worker entry, invoked by the admitted native thread on its system stack.
use super::{Builder, Fragment, LR, SP, add_imm, condition, imm, next, reg};
use crate::{
    a64::{
        foreign::Platform,
        reg::{Reg, Width},
    },
    runtime::{foreign_packet as packet, foreign_schema as schema},
};

const REQUEST: Reg = Reg::x(19);
const PACKET: Reg = Reg::x(20);
const ERRNO: Reg = Reg::x(21);
const BASE: Reg = Reg::x(22);
const SAVED: [(Reg, usize); 6] = [
    (REQUEST, 0),
    (PACKET, 8),
    (ERRNO, 16),
    (BASE, 24),
    (Reg::x(29), 32),
    (LR, 40),
];

fn call(b: &mut Builder, platform: Platform, name: &str) {
    b.external_branch("bl", &platform.symbol(name));
}
fn jump(b: &mut Builder, form: &str, operands: impl Into<Vec<super::Operand>>) -> u32 {
    let at = b.offset();
    b.emit(form, operands);
    at
}

/// x0 points to the raw NativeRequest. Thread launch/join and credits remain owned
/// by the platform bridge; this entry never publishes early join completion.
pub fn foreign_native_worker(platform: Platform) -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(64)]);
    for (value, offset) in SAVED {
        b.memory("str.X.Unsigned", value, SP, offset);
    }
    b.add(Reg::x(29), SP, 32);
    b.add(REQUEST, Reg::x(0), 0);
    b.memory(
        "ldr.X.Unsigned",
        PACKET,
        REQUEST,
        schema::NATIVE_REQUEST_ARGUMENTS,
    );
    b.add(BASE, SP, 0);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(9),
        PACKET,
        packet::FOREIGN_PACKET_STACK_BYTES,
    );
    b.emit("ubfm.X", [reg(Reg::x(10)), reg(Reg::x(9)), imm(0), imm(3)]);
    let unaligned = jump(&mut b, "cbnz.X", [reg(Reg::x(10)), next()]);
    b.emit(
        "subs.X.imm",
        [
            reg(Reg::zr(Width::X)),
            reg(Reg::x(9)),
            super::Operand::AddImm {
                imm12: 15,
                shift12: true,
            },
        ],
    );
    let oversized = jump(&mut b, "b.cond", [next(), condition(8)]);
    if platform == Platform::Darwin {
        b.emit("movz.X", [reg(Reg::x(0)), super::movz(0)]);
        b.add(Reg::x(1), SP, 48);
        call(&mut b, platform, "pthread_threadid_np");
        let failed = jump(&mut b, "cbnz.W", [reg(Reg::w(0)), next()]);
        b.memory("ldr.X.Unsigned", Reg::x(0), SP, 48);
        publish_thread(&mut b, Reg::x(0));
        call(&mut b, platform, "__error");
        finish_worker(&mut b, platform);
        b.patch_branch(failed, "cbnz.W", &[reg(Reg::w(0))], &[]);
    } else {
        call(&mut b, platform, "gettid");
        b.emit("ubfm.X", [reg(Reg::x(0)), reg(Reg::x(0)), imm(0), imm(31)]);
        publish_thread(&mut b, Reg::x(0));
        call(&mut b, platform, "__errno_location");
        finish_worker(&mut b, platform);
    }
    b.patch_branch(unaligned, "cbnz.X", &[reg(Reg::x(10))], &[]);
    b.patch_branch(oversized, "b.cond", &[], &[condition(8)]);
    b.emit("movz.X", [reg(Reg::x(0)), super::movz(22)]);
    b.memory(
        "str.X.Unsigned",
        Reg::x(0),
        REQUEST,
        schema::NATIVE_REQUEST_ERROR,
    );
    leave(&mut b);
    b.finish()
}

fn finish_worker(b: &mut Builder, _platform: Platform) {
    b.add(ERRNO, Reg::x(0), 0);
    stack_arguments(b);
    for index in 0..8 {
        b.memory(
            "ldr.X.Unsigned",
            Reg::x(index),
            PACKET,
            packet::a64_integer_lane(index).unwrap() as usize,
        );
        b.memory(
            "ldr.D.Unsigned",
            Reg {
                kind: crate::a64::reg::RegKind::Vector(index),
                width: Width::D,
            },
            PACKET,
            packet::FOREIGN_PACKET_FLOATS + usize::from(index) * 8,
        );
    }
    b.memory("ldr.X.Unsigned", Reg::x(8), PACKET, packet::A64_PACKET_SRET);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(16),
        PACKET,
        packet::FOREIGN_PACKET_TARGET,
    );
    b.emit("blr", [reg(Reg::x(16))]);
    for (value, offset) in [(Reg::x(0), 128), (Reg::x(1), 136)] {
        b.memory("str.X.Unsigned", value, PACKET, offset);
    }
    for index in 0..2 {
        b.memory(
            "str.D.Unsigned",
            Reg {
                kind: crate::a64::reg::RegKind::Vector(index),
                width: Width::D,
            },
            PACKET,
            144 + usize::from(index) * 8,
        );
    }
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(9),
        PACKET,
        packet::A64_PACKET_EXTRA_RESULTS,
    );
    let no_extra = jump(b, "cbz.X", [reg(Reg::x(9)), next()]);
    for index in 2..4 {
        b.memory(
            "str.D.Unsigned",
            Reg {
                kind: crate::a64::reg::RegKind::Vector(index),
                width: Width::D,
            },
            Reg::x(9),
            usize::from(index - 2) * 8,
        );
    }
    b.patch_branch(no_extra, "cbz.X", &[reg(Reg::x(9))], &[]);
    b.memory("ldr.W.Unsigned", Reg::w(9), ERRNO, 0);
    b.memory(
        "str.X.Unsigned",
        Reg::x(9),
        PACKET,
        packet::FOREIGN_PACKET_ERROR,
    );
    b.memory(
        "str.X.Unsigned",
        Reg::zr(Width::X),
        REQUEST,
        schema::NATIVE_REQUEST_ERROR,
    );
    leave(b);
}

fn leave(b: &mut Builder) {
    publish_thread(b, Reg::zr(Width::X));
    b.add(SP, BASE, 0);
    for (value, offset) in SAVED {
        b.memory("ldr.X.Unsigned", value, SP, offset);
    }
    b.emit("movz.X", [reg(Reg::x(0)), super::movz(0)]);
    b.add(SP, SP, 64);
    b.ret();
}

fn publish_thread(b: &mut Builder, value: Reg) {
    b.emit(
        "stlr.X",
        [
            reg(value),
            super::Operand::Address(super::Address {
                base: REQUEST,
                mode: super::AddressMode::Base,
            }),
        ],
    );
}

fn stack_arguments(b: &mut Builder) {
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(9),
        PACKET,
        packet::FOREIGN_PACKET_STACK_BYTES,
    );
    let probe = b.offset();
    let probed = jump(b, "cbz.X", [reg(Reg::x(9)), next()]);
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", Reg::zr(Width::X), SP, 0);
    b.emit("sub.X.imm", [reg(Reg::x(9)), reg(Reg::x(9)), add_imm(16)]);
    b.branch("b", &[], probe);
    b.patch_branch(probed, "cbz.X", &[reg(Reg::x(9))], &[]);
    b.add(Reg::x(9), PACKET, packet::A64_PACKET_BYTES);
    b.add(Reg::x(10), SP, 0);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(11),
        PACKET,
        packet::FOREIGN_PACKET_STACK_BYTES,
    );
    let copy = b.offset();
    let done = jump(b, "cbz.X", [reg(Reg::x(11)), next()]);
    b.memory("ldr.X.Unsigned", Reg::x(12), Reg::x(9), 0);
    b.memory("str.X.Unsigned", Reg::x(12), Reg::x(10), 0);
    b.add(Reg::x(9), Reg::x(9), 8);
    b.add(Reg::x(10), Reg::x(10), 8);
    b.emit("sub.X.imm", [reg(Reg::x(11)), reg(Reg::x(11)), add_imm(8)]);
    b.branch("b", &[], copy);
    b.patch_branch(done, "cbz.X", &[reg(Reg::x(11))], &[]);
}
