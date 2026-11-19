//! Manual only: generate an image, then run it with ~/aarch64-runner/run.sh.
use gugu_backend::a64::{
    elf::{
        self, ElfRequest, LinuxEntry, Location, Relative, Relocation, Section, SectionContent,
        Segment,
    },
    encode,
    fragments::linux_fd_dispatch,
    inst::{Address, AddressMode, Operand, RelocKind, Target},
    machine,
    reg::{Reg, Width},
};
use gugu_runtime_abi::{
    entry::{INITIAL_SLOT_BYTES, Rt0Payload},
    foreign_schema::{
        NATIVE_REQUEST_ARGUMENTS, NATIVE_REQUEST_BYTES, NATIVE_REQUEST_ERROR,
        NATIVE_REQUEST_OPERATION, NATIVE_REQUEST_VALUE,
    },
    processor::PROCESSOR_PAGE_BYTES,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_linux_fd_image <output>")?;
    let mut probe = Probe::new();
    file_round_trip(&mut probe);
    directory_and_errors(&mut probe);
    let request = probe.finish();
    let image = elf::link_entry(
        request,
        LinuxEntry {
            rt0_start: at(0),
            payload: at(2),
            initial_slot: at(3),
            processor: Location {
                section: 3,
                offset: INITIAL_SLOT_BYTES,
            },
        },
    )?;
    std::fs::write(&path, image.bytes)?;
    println!("wrote fd/path verification image to {path}");
    Ok(())
}

fn file_round_trip(p: &mut Probe) {
    let path = p.data(Segment::DataConst, b"/tmp/gugu-a64-fd-test\0".to_vec());
    p.path = path;
    let message = p.data(Segment::DataConst, b"fd-path\n".to_vec());
    let open = p.request(2, &[0, 0x80242, 0o600]);
    p.pointer(open, 0, path);
    p.invoke(open, 0);
    p.load(22, 21, NATIVE_REQUEST_VALUE);
    let write = p.request(1, &[0, 0, 8]);
    p.pointer(write, 1, message);
    p.with_fd(write);
    p.load(9, 21, NATIVE_REQUEST_VALUE);
    p.expect(9, 8);
    p.close();
    p.output(b"open+write+close: ok\n");
    check_stat(p, path);
    let reopen = p.request(2, &[0, 0, 0]);
    p.pointer(reopen, 0, path);
    p.invoke(reopen, 0);
    p.load(22, 21, NATIVE_REQUEST_VALUE);
    let buffer = p.data(Segment::Data, vec![0; 8]);
    let read = p.request(0, &[0, 0, 8]);
    p.pointer(read, 1, buffer);
    p.with_fd(read);
    p.load(9, 21, NATIVE_REQUEST_VALUE);
    p.expect(9, 8);
    compare_message(p, buffer, message);
    p.close();
    p.output(b"reopen+read: ");
    let print = p.request(1, &[1, 0, 8]);
    p.pointer(print, 1, buffer);
    p.invoke(print, 0);
}

fn check_stat(p: &mut Probe, path: Location) {
    let buffer = p.data(Segment::Data, vec![0; 144]);
    let stat = p.request(262, &[(-100_i64) as u64, 0, 0, 256]);
    p.pointer(stat, 1, path);
    p.pointer(stat, 2, buffer);
    p.invoke(stat, 0);
    p.address(10, buffer);
    p.load(9, 10, 48);
    p.expect(9, 8);
    p.emit(
        "ldr.W.Unsigned",
        vec![Operand::Reg(Reg::w(9)), address(10, 24)],
    );
    p.emit(
        "ubfm.X",
        vec![reg(9), reg(9), Operand::Imm(12), Operand::Imm(15)],
    );
    p.expect(9, 8);
    p.load(9, 10, 56);
    p.emit("subs.X.imm", vec![zero(), reg(9), immediate(0)]);
    p.fail_if(0);
    p.output(b"stat: size=8, regular file, nonzero block size\n");
}

fn compare_message(p: &mut Probe, buffer: Location, message: Location) {
    p.address(10, buffer);
    p.load(9, 10, 0);
    p.address(10, message);
    p.load(10, 10, 0);
    p.emit(
        "subs.X.shift",
        vec![
            zero(),
            reg(9),
            reg(10),
            Operand::Shift(gugu_backend::a64::inst::Shift::Lsl, 0),
        ],
    );
    p.fail_if(1);
}

fn directory_and_errors(p: &mut Probe) {
    let path = p.data(Segment::DataConst, b"/tmp\0".to_vec());
    let open = p.request(2, &[0, 589824, 0]);
    p.pointer(open, 0, path);
    p.invoke(open, 0);
    p.load(22, 21, NATIVE_REQUEST_VALUE);
    let buffer = p.data(Segment::Data, vec![0; 4000]);
    let dents = p.request(217, &[0, 0, 4000]);
    p.pointer(dents, 1, buffer);
    p.with_fd(dents);
    p.load(9, 21, NATIVE_REQUEST_VALUE);
    p.emit("subs.X.imm", vec![zero(), reg(9), immediate(0)]);
    p.fail_if(0);
    p.close();
    p.output(b"getdents64: nonempty directory\n");
    let unlink = p.request(87, &[0]);
    p.pointer(unlink, 0, p.path);
    p.invoke(unlink, 0);
    p.output(b"unlink: ok\n");
    let missing = p.request(2, &[0, 0, 0]);
    p.pointer(missing, 0, p.path);
    p.invoke(missing, 2);
    p.output(b"missing file: errno=2\n");
    let invalid = p.request(4095, &[]);
    p.invoke(invalid, 22);
    p.output(b"unsupported operation: errno=22\n");
}

struct Probe {
    request: ElfRequest,
    code: Vec<u8>,
    failures: Vec<usize>,
    path: Location,
}

impl Probe {
    fn new() -> Self {
        let worker = linux_fd_dispatch();
        assert!(worker.relocations.is_empty());
        let sections = vec![
            section(Segment::Text, SectionContent::Code(Vec::new())),
            section(Segment::Text, SectionContent::Code(worker.bytes)),
            section(
                Segment::Data,
                SectionContent::Data(vec![0; Rt0Payload::bytes() as usize]),
            ),
            section(
                Segment::Data,
                SectionContent::Data(vec![0; INITIAL_SLOT_BYTES as usize + PROCESSOR_PAGE_BYTES]),
            ),
        ];
        let mut p = Self {
            request: ElfRequest {
                sections,
                entry: at(0),
                relatives: vec![],
                relocations: vec![],
            },
            code: vec![],
            failures: vec![],
            path: at(0),
        };
        p.emit("sub.X.imm", vec![sp(), sp(), immediate(32)]);
        for (n, offset) in [(21, 0), (22, 8), (30, 16)] {
            p.emit(
                "str.X.Unsigned",
                vec![
                    reg(n),
                    Operand::Address(Address {
                        base: Reg::sp(Width::X),
                        mode: AddressMode::Unsigned(offset),
                    }),
                ],
            );
        }
        p
    }

    fn emit(&mut self, key: &str, operands: Vec<Operand>) {
        self.code.extend(instruction(key, operands));
    }

    fn data(&mut self, segment: Segment, bytes: Vec<u8>) -> Location {
        let index = self.request.sections.len();
        self.request
            .sections
            .push(section(segment, SectionContent::Data(bytes)));
        at(index)
    }

    fn request(&mut self, operation: u64, arguments: &[u64]) -> Location {
        let mut bytes = vec![0; NATIVE_REQUEST_BYTES];
        bytes[NATIVE_REQUEST_OPERATION..NATIVE_REQUEST_OPERATION + 8]
            .copy_from_slice(&operation.to_le_bytes());
        for (index, argument) in arguments.iter().enumerate() {
            let offset = NATIVE_REQUEST_ARGUMENTS + index * 8;
            bytes[offset..offset + 8].copy_from_slice(&argument.to_le_bytes());
        }
        self.data(Segment::Data, bytes)
    }

    fn pointer(&mut self, request: Location, argument: usize, target: Location) {
        self.request.relatives.push(Relative {
            at: Location {
                section: request.section,
                offset: (NATIVE_REQUEST_ARGUMENTS + argument * 8) as u64,
            },
            target,
            addend: 0,
        });
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

    fn address(&mut self, destination: u8, target: Location) {
        self.fixup(target, RelocKind::AdrpPage21);
        self.emit(
            "adrp",
            vec![reg(destination), Operand::Target(Target::Offset(0))],
        );
        self.fixup(target, RelocKind::AddLo12);
        self.emit(
            "add.X.imm",
            vec![reg(destination), reg(destination), immediate(0)],
        );
    }

    fn load(&mut self, destination: u8, base: u8, offset: usize) {
        self.emit(
            "ldr.X.Unsigned",
            vec![reg(destination), address(base, offset)],
        );
    }

    fn invoke(&mut self, request: Location, errno: u16) {
        self.address(21, request);
        self.emit("add.X.imm", vec![reg(0), reg(21), immediate(0)]);
        self.fixup(at(1), RelocKind::Call26);
        self.emit("bl", vec![Operand::Target(Target::Offset(0))]);
        self.load(9, 21, NATIVE_REQUEST_ERROR);
        self.expect(9, errno);
    }

    fn with_fd(&mut self, request: Location) {
        self.address(21, request);
        self.emit(
            "str.X.Unsigned",
            vec![reg(22), address(21, NATIVE_REQUEST_ARGUMENTS)],
        );
        self.invoke(request, 0);
    }

    fn close(&mut self) {
        let close = self.request(3, &[0]);
        self.with_fd(close);
    }

    fn output(&mut self, text: &[u8]) {
        let message = self.data(Segment::DataConst, text.to_vec());
        let write = self.request(1, &[1, 0, text.len() as u64]);
        self.pointer(write, 1, message);
        self.invoke(write, 0);
        self.load(9, 21, NATIVE_REQUEST_VALUE);
        self.expect(9, text.len() as u16);
    }

    fn expect(&mut self, value: u8, expected: u16) {
        self.emit("subs.X.imm", vec![zero(), reg(value), immediate(expected)]);
        self.fail_if(1);
    }

    fn fail_if(&mut self, condition: u8) {
        self.failures.push(self.code.len());
        self.emit(
            "b.cond",
            vec![
                Operand::Target(Target::Offset(0)),
                Operand::Condition(condition),
            ],
        );
    }

    fn finish(mut self) -> ElfRequest {
        self.exit(0);
        let failed = self.code.len();
        self.exit(1);
        for offset in self.failures {
            let encoded = u32::from_le_bytes(self.code[offset..offset + 4].try_into().unwrap());
            let bytes = instruction(
                "b.cond",
                vec![
                    Operand::Target(Target::Offset((failed - offset) as i64)),
                    Operand::Condition((encoded & 15) as u8),
                ],
            );
            self.code[offset..offset + 4].copy_from_slice(&bytes);
        }
        self.request.sections[0].content = SectionContent::Code(self.code);
        self.request
    }

    fn exit(&mut self, status: u16) {
        self.emit(
            "movz.X",
            vec![
                reg(0),
                Operand::Wide {
                    imm16: status,
                    shift: 0,
                },
            ],
        );
        for (n, offset) in [(21, 0), (22, 8), (30, 16)] {
            self.emit(
                "ldr.X.Unsigned",
                vec![
                    reg(n),
                    Operand::Address(Address {
                        base: Reg::sp(Width::X),
                        mode: AddressMode::Unsigned(offset),
                    }),
                ],
            );
        }
        self.emit("add.X.imm", vec![sp(), sp(), immediate(32)]);
        self.emit("ret", vec![reg(30)]);
    }
}

fn instruction(key: &str, operands: Vec<Operand>) -> [u8; 4] {
    encode::encode(&machine::inst(key, operands).unwrap())
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
fn address(base: u8, offset: usize) -> Operand {
    Operand::Address(Address {
        base: Reg::x(base),
        mode: AddressMode::Unsigned(offset as i64),
    })
}
fn section(segment: Segment, content: SectionContent) -> Section {
    Section {
        name: "fd_probe".into(),
        segment,
        align: 8,
        content,
    }
}
