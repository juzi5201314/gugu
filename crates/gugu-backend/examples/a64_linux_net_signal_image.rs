//! Manual: cargo run -p gugu-backend --example a64_linux_net_signal_image -- /tmp/net.elf
use gugu_backend::a64::{
    elf::{
        self, ElfRequest, LinuxEntry, Location, Relative, Relocation, Section, SectionContent,
        Segment,
    },
    encode,
    fragments::{linux_fd_dispatch, linux_net_dispatch, linux_signal_dispatch},
    inst::{Address, AddressMode, Operand, RelocKind, Shift, Target},
    machine,
    reg::{Reg, Width},
};
use gugu_runtime_abi::{
    entry::{INITIAL_SLOT_BYTES, Rt0Payload},
    processor::PROCESSOR_PAGE_BYTES,
};

#[derive(Clone, Copy)]
enum Arg {
    Number(u64),
    Pointer(Location),
    Register(u8),
}
use Arg::{Number as N, Pointer as P, Register as R};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_linux_net_signal_image <output>")?;
    let mut p = Probe::new();
    tcp(&mut p);
    signal(&mut p);
    p.call(1, 4095, &[], 22);
    p.call(1, 7, &[N(u64::MAX)], 9);
    p.output(b"net errors: EINVAL=22, EBADF=9\n");
    let image = elf::link_entry(
        p.finish(),
        LinuxEntry {
            rt0_start: at(0),
            payload: at(4),
            initial_slot: at(5),
            processor: Location {
                section: 5,
                offset: INITIAL_SLOT_BYTES,
            },
            runtime: None,
        },
    )?;
    std::fs::write(&path, image.bytes)?;
    println!("wrote net/signal verification image to {path}");
    Ok(())
}

fn tcp(p: &mut Probe) {
    let address = p.data(vec![2, 0, 0, 0, 127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    let size = p.data(16_u64.to_le_bytes().to_vec());
    p.call(1, 0, &[N(2), N(1), N(0)], 0);
    p.save(22);
    p.call(1, 2, &[R(22), P(address), N(16)], 0);
    p.call(1, 5, &[R(22), P(address), P(size)], 0);
    p.call(1, 3, &[R(22), N(4)], 0);
    p.call(1, 0, &[N(2), N(1), N(0)], 0);
    p.save(23);
    p.call(1, 21, &[R(23), P(address), N(16)], 0);
    p.call(1, 4, &[R(22), N(0), N(0), N(524288)], 0);
    p.save(24);
    let payload = p.data(b"ping".to_vec());
    p.call(1, 25, &[R(23), P(payload), N(4), N(16384), N(0), N(0)], 0);
    p.expect(0, 4);
    epoll_and_receive(p, payload);
    for fd in [22, 23, 24, 25] {
        p.call(1, 7, &[R(fd)], 0);
    }
    p.output(b"TCP loopback + epoll: ping\n");
}

fn epoll_and_receive(p: &mut Probe, payload: Location) {
    p.call(1, 8, &[N(524288)], 0);
    p.save(25);
    let tag = 0x123456789abcdef0_u64;
    let event = p.data([1_u32.to_le_bytes().as_slice(), tag.to_le_bytes().as_slice()].concat());
    p.call(1, 9, &[R(25), N(1), R(24), P(event)], 0);
    let events = p.data(vec![0; 48]);
    p.call(1, 10, &[R(25), P(events), N(4), N(1000)], 0);
    p.expect(0, 1);
    p.address(9, events);
    p.emit(
        "ldr.X.Unscaled",
        vec![reg(10), memory(9, AddressMode::Unscaled(4))],
    );
    p.address(9, event);
    p.emit(
        "ldr.X.Unscaled",
        vec![reg(11), memory(9, AddressMode::Unscaled(4))],
    );
    p.equal(10, 11);
    let buffer = p.data(vec![0; 8]);
    p.call(1, 24, &[R(24), P(buffer), N(4), N(0), N(0), N(0)], 0);
    p.expect(0, 4);
    p.address(9, buffer);
    p.emit(
        "ldr.W.Unsigned",
        vec![
            Operand::Reg(Reg::w(10)),
            memory(9, AddressMode::Unsigned(0)),
        ],
    );
    p.address(9, payload);
    p.emit(
        "ldr.W.Unsigned",
        vec![
            Operand::Reg(Reg::w(11)),
            memory(9, AddressMode::Unsigned(0)),
        ],
    );
    p.equal(10, 11);
    p.call(1, 10, &[R(25), P(events), N(4), N(0)], 0);
    p.expect(0, 0);
}

fn signal(p: &mut Probe) {
    let mask = p.data((1_u64 << 9).to_le_bytes().to_vec());
    p.call(2, 14, &[N(0), P(mask), N(0), N(8)], 0);
    p.call(2, 289, &[N(u64::MAX), P(mask), N(8), N(526336)], 0);
    p.save(26);
    p.call(2, 39, &[], 0);
    p.save(22);
    p.call(2, 62, &[R(22), N(10)], 0);
    let info = p.data(vec![0; 128]);
    p.call(3, 0, &[R(26), P(info), N(128)], 0);
    p.expect(0, 128);
    p.address(9, info);
    p.emit(
        "ldr.W.Unsigned",
        vec![
            Operand::Reg(Reg::w(10)),
            memory(9, AddressMode::Unsigned(0)),
        ],
    );
    p.expect(10, 10);
    p.call(3, 3, &[R(26)], 0);
    p.call(2, 14, &[N(1), P(mask), N(0), N(8)], 0);
    p.output(b"SIGUSR1: blocked, kill, signalfd read signo=10\n");
    signal_action(p);
}

fn signal_action(p: &mut Probe) {
    let ignored = p.data(
        [1_u64, 1 << 26, 0, 0]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect(),
    );
    let previous = p.data(vec![0; 32]);
    p.call(2, 13, &[N(12), P(ignored), P(previous), N(8)], 0);
    p.call(2, 62, &[R(22), N(12)], 0);
    p.call(2, 13, &[N(12), P(previous), N(0), N(8)], 0);
    p.output(b"sigaction: SIGUSR2 ignored and restored\n");
}

struct Probe {
    request: ElfRequest,
    code: Vec<u8>,
    failures: Vec<usize>,
}

impl Probe {
    fn new() -> Self {
        let fragments = [
            linux_net_dispatch(),
            linux_signal_dispatch(),
            linux_fd_dispatch(false),
        ];
        let mut sections = vec![section(Segment::Text, SectionContent::Code(vec![]))];
        for fragment in fragments {
            assert!(fragment.relocations.is_empty());
            sections.push(section(Segment::Text, SectionContent::Code(fragment.bytes)));
        }
        sections.push(section(
            Segment::Data,
            SectionContent::Data(vec![0; Rt0Payload::bytes() as usize]),
        ));
        sections.push(section(
            Segment::Data,
            SectionContent::Data(vec![0; INITIAL_SLOT_BYTES as usize + PROCESSOR_PAGE_BYTES]),
        ));
        let mut p = Self {
            request: ElfRequest {
                sections,
                entry: at(0),
                relatives: vec![],
                relocations: vec![],
            },
            code: vec![],
            failures: vec![],
        };
        p.emit("sub.X.imm", vec![sp(), sp(), immediate(48)]);
        for (i, n) in [22, 23, 24, 25, 26, 30].into_iter().enumerate() {
            p.emit(
                "str.X.Unsigned",
                vec![
                    reg(n),
                    Operand::Address(Address {
                        base: Reg::sp(Width::X),
                        mode: AddressMode::Unsigned(i as i64 * 8),
                    }),
                ],
            );
        }
        p
    }

    fn data(&mut self, bytes: Vec<u8>) -> Location {
        let location = at(self.request.sections.len());
        self.request
            .sections
            .push(section(Segment::Data, SectionContent::Data(bytes)));
        location
    }

    fn emit(&mut self, key: &str, args: Vec<Operand>) {
        self.code.extend(instruction(key, args));
    }

    fn fixup(&mut self, target: Location, kind: RelocKind) {
        self.request.relocations.push(Relocation {
            at: Location {
                section: 0,
                offset: self.code.len() as u64,
            },
            target,
            addend: 0,
            kind,
        });
    }

    fn address(&mut self, register: u8, target: Location) {
        self.fixup(target, RelocKind::AdrpPage21);
        self.emit(
            "adrp",
            vec![reg(register), Operand::Target(Target::Offset(0))],
        );
        self.fixup(target, RelocKind::AddLo12);
        self.emit(
            "add.X.imm",
            vec![reg(register), reg(register), immediate(0)],
        );
    }

    fn packet(&mut self, adapter: usize, operation: u16, arguments: &[Arg]) -> Location {
        let mut bytes = vec![0; 144];
        if adapter != 1 {
            bytes[8..16].copy_from_slice(&u64::from(operation).to_le_bytes());
        }
        let base = if adapter == 1 { 0 } else { 16 };
        for (index, arg) in arguments.iter().enumerate() {
            if let N(value) = arg {
                bytes[base + index * 8..base + index * 8 + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        let location = self.data(bytes);
        for (index, arg) in arguments.iter().enumerate() {
            let offset = base + index * 8;
            match *arg {
                P(target) => self.request.relatives.push(Relative {
                    at: Location {
                        section: location.section,
                        offset: offset as u64,
                    },
                    target,
                    addend: 0,
                }),
                R(n) => {
                    self.address(9, location);
                    self.emit(
                        "str.X.Unsigned",
                        vec![reg(n), memory(9, AddressMode::Unsigned(offset as i64))],
                    );
                }
                N(_) => {}
            }
        }
        location
    }

    fn call(&mut self, adapter: usize, operation: u16, arguments: &[Arg], errno: u16) {
        let packet = self.packet(adapter, operation, arguments);
        if adapter == 1 {
            self.address(1, packet);
            self.emit("movz.X", vec![reg(0), wide(operation)]);
        } else {
            self.address(0, packet);
        }
        self.fixup(at(adapter), RelocKind::Call26);
        self.emit("bl", vec![Operand::Target(Target::Offset(0))]);
        self.address(9, packet);
        let error = if adapter == 1 { 80 } else { 72 };
        self.emit(
            "ldr.X.Unsigned",
            vec![reg(10), memory(9, AddressMode::Unsigned(error))],
        );
        self.expect(10, errno);
        self.emit(
            "ldr.X.Unsigned",
            vec![reg(0), memory(9, AddressMode::Unsigned(error - 8))],
        );
    }

    fn save(&mut self, n: u8) {
        self.emit("add.X.imm", vec![reg(n), reg(0), immediate(0)]);
    }

    fn output(&mut self, bytes: &[u8]) {
        let message = self.data(bytes.to_vec());
        self.call(3, 1, &[N(1), P(message), N(bytes.len() as u64)], 0);
        self.expect(0, bytes.len() as u16);
    }

    fn expect(&mut self, n: u8, value: u16) {
        self.emit("subs.X.imm", vec![zero(), reg(n), immediate(value)]);
        self.fail();
    }

    fn equal(&mut self, left: u8, right: u8) {
        self.emit(
            "subs.X.shift",
            vec![zero(), reg(left), reg(right), Operand::Shift(Shift::Lsl, 0)],
        );
        self.fail();
    }

    fn fail(&mut self) {
        self.failures.push(self.code.len());
        self.emit(
            "b.cond",
            vec![Operand::Target(Target::Offset(0)), Operand::Condition(1)],
        );
    }

    fn finish(mut self) -> ElfRequest {
        self.exit(0);
        let failed = self.code.len();
        self.exit(1);
        for offset in self.failures {
            self.code[offset..offset + 4].copy_from_slice(&instruction(
                "b.cond",
                vec![
                    Operand::Target(Target::Offset((failed - offset) as i64)),
                    Operand::Condition(1),
                ],
            ));
        }
        self.request.sections[0].content = SectionContent::Code(self.code);
        self.request
    }

    fn exit(&mut self, status: u16) {
        self.emit("movz.X", vec![reg(0), wide(status)]);
        for (i, n) in [22, 23, 24, 25, 26, 30].into_iter().enumerate() {
            self.emit(
                "ldr.X.Unsigned",
                vec![
                    reg(n),
                    Operand::Address(Address {
                        base: Reg::sp(Width::X),
                        mode: AddressMode::Unsigned(i as i64 * 8),
                    }),
                ],
            );
        }
        self.emit("add.X.imm", vec![sp(), sp(), immediate(48)]);
        self.emit("ret", vec![reg(30)]);
    }
}

fn instruction(key: &str, args: Vec<Operand>) -> [u8; 4] {
    encode::encode(&machine::inst(key, args).unwrap())
        .unwrap()
        .bytes()
}
fn at(section: usize) -> Location {
    Location { section, offset: 0 }
}
fn reg(n: u8) -> Operand {
    Operand::Reg(Reg::x(n))
}
fn sp() -> Operand {
    Operand::Reg(Reg::sp(Width::X))
}
fn zero() -> Operand {
    Operand::Reg(Reg::zr(Width::X))
}
fn immediate(imm12: u16) -> Operand {
    Operand::AddImm {
        imm12,
        shift12: false,
    }
}
fn wide(imm16: u16) -> Operand {
    Operand::Wide { imm16, shift: 0 }
}
fn memory(base: u8, mode: AddressMode) -> Operand {
    Operand::Address(Address {
        base: Reg::x(base),
        mode,
    })
}
fn section(segment: Segment, content: SectionContent) -> Section {
    Section {
        name: "net_signal_probe".into(),
        segment,
        align: 8,
        content,
    }
}
