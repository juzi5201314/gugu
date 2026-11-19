//! Manual process verification; run the generated image with ~/aarch64-runner/run.sh.
#[path = "support/a64_linux_probe.rs"]
pub mod support;

use gugu_backend::a64::{
    elf::Location,
    fragments::{linux_process_call, linux_process_worker},
};
use support::{
    Arg,
    Arg::{Number as N, Pointer as P, Register as R},
    Probe, reg,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_linux_process_image <output>")?;
    let mut p = Probe::new();
    let call = p.fragment(linux_process_call());
    let worker = p.fragment(linux_process_worker());
    p.call(worker);
    p.mov(25, 0);
    let packet = spawn_packet(&mut p);
    spawn_and_read(&mut p, call, packet);
    failure_cases(&mut p, call, packet);
    p.finish(&path)
}

fn request(p: &mut Probe, op: u64, args: &[Arg]) -> Location {
    let mut fields = vec![(0, N(op))];
    fields.extend(args.iter().enumerate().map(|(i, a)| (8 + i * 8, *a)));
    p.record(88, &fields)
}

fn invoke(p: &mut Probe, call: Option<Location>, op: u64, args: &[Arg], errno: u64) -> Location {
    let req = invoke_raw(p, call, op, args);
    p.address(9, req);
    p.load(10, 9, 80);
    p.expect(10, errno);
    p.load(0, 9, 72);
    req
}

fn invoke_raw(p: &mut Probe, call: Option<Location>, op: u64, args: &[Arg]) -> Location {
    let req = request(p, op, args);
    if let Some(call) = call {
        p.number(0, op);
        p.address(1, req);
        p.call(call);
    } else {
        p.address(0, req);
        p.emit("blr", vec![reg(25)]);
    }
    req
}

fn spawn_packet(p: &mut Probe) -> Location {
    let executable = p.data(b"/bin/busybox\0".to_vec());
    let echo = p.data(b"echo\0".to_vec());
    let message = p.data(b"a64-process\0".to_vec());
    let argv = p.record(32, &[(0, P(executable)), (8, P(echo)), (16, P(message))]);
    let env = p.data(vec![0; 8]);
    let cwd = p.data(b"/\0".to_vec());
    p.record(
        80,
        &[(0, P(executable)), (8, P(argv)), (16, P(env)), (24, P(cwd))],
    )
}

fn spawn_and_read(p: &mut Probe, call: Location, packet: Location) {
    let pipe = invoke(p, None, 1, &[N(1)], 0);
    p.mov(21, 0);
    p.address(9, pipe);
    p.load(22, 9, 24);
    p.address(9, packet);
    p.store(22, 9, 40);
    invoke(p, None, 2, &[], 0);
    p.mov(24, 0);
    p.address(9, packet);
    p.store(24, 9, 32);
    invoke(p, None, 3, &[R(24)], 0);
    p.address(9, packet);
    p.store(0, 9, 48);
    invoke(p, None, 0, &[P(packet)], 0);
    p.mov(23, 0);
    for fd in [22, 24] {
        invoke(p, Some(call), 2, &[R(fd)], 0);
    }
    p.address(9, packet);
    p.load(24, 9, 48);
    invoke(p, Some(call), 2, &[R(24)], 0);
    read_output(p, call);
    wait(p, call);
}

fn read_output(p: &mut Probe, call: Location) {
    let buffer = p.data(vec![0; 32]);
    p.address(22, buffer);
    p.number(24, 12);
    let again = p.offset();
    let req = invoke_raw(p, Some(call), 3, &[R(21), R(22), R(24)]);
    p.address(9, req);
    p.load(10, 9, 80);
    p.repeat_if_equal(10, 11, again);
    p.repeat_if_equal(10, 4, again);
    p.expect(10, 0);
    p.load(0, 9, 72);
    p.expect_nonzero(0);
    p.emit(
        "add.X.shift",
        vec![
            reg(22),
            reg(22),
            reg(0),
            gugu_backend::a64::inst::Operand::Shift(gugu_backend::a64::inst::Shift::Lsl, 0),
        ],
    );
    p.emit(
        "sub.X.shift",
        vec![
            reg(24),
            reg(24),
            reg(0),
            gugu_backend::a64::inst::Operand::Shift(gugu_backend::a64::inst::Shift::Lsl, 0),
        ],
    );
    p.repeat_if(24, 0, again, 1);
    p.address(9, buffer);
    p.load(10, 9, 0);
    p.expect(10, u64::from_le_bytes(*b"a64-proc"));
    p.load(10, 9, 8);
    p.expect(10, u64::from_le_bytes(*b"ess\n\0\0\0\0"));
    p.output(b"spawn /bin/busybox echo: ");
    p.number(0, 1);
    p.address(1, buffer);
    p.number(2, 12);
    p.number(8, 64);
    p.emit("svc", vec![gugu_backend::a64::inst::Operand::Imm(0)]);
    invoke(p, Some(call), 2, &[R(21)], 0);
}

fn wait(p: &mut Probe, call: Location) {
    let again = p.offset();
    let req = invoke(p, Some(call), 0, &[R(23)], 0);
    p.repeat_if_equal(0, 0, again);
    p.expect(0, 1);
    p.address(9, req);
    p.load(10, 9, 16);
    p.expect(10, 0);
    p.output(b"wait: exited 0\n");
}

fn failure_cases(p: &mut Probe, call: Location, packet: Location) {
    invoke(p, None, 2, &[], 0);
    p.mov(24, 0);
    p.address(9, packet);
    for offset in [32, 40, 48] {
        p.store(24, 9, offset);
    }
    let missing = p.data(b"/missing-a64-executable\0".to_vec());
    p.address(10, missing);
    p.address(9, packet);
    p.store(10, 9, 0);
    invoke(p, None, 0, &[P(packet)], 2);
    invoke(p, Some(call), 2, &[R(24)], 0);
    p.output(b"missing executable: errno=2\n");
    invoke(p, Some(call), 3, &[N(u64::MAX), N(0), N(0)], 9);
    p.output(b"invalid pipe read: errno=9\n");
}
