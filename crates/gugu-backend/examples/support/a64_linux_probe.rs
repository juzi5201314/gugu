use gugu_backend::a64::{
    elf::{
        self, ElfRequest, LinuxEntry, Location, Relative, Relocation, Section, SectionContent,
        Segment,
    },
    encode,
    fragments::Fragment,
    inst::{Address, AddressMode, Operand, RelocKind, Shift, Target},
    machine,
    reg::{Reg, Width},
};
use gugu_runtime_abi::{
    entry::{INITIAL_SLOT_BYTES, Rt0Payload},
    processor::PROCESSOR_PAGE_BYTES,
};

#[derive(Clone, Copy)]
pub enum Arg {
    Number(u64),
    Pointer(Location),
    Register(u8),
}
use Arg::*;

pub struct Probe {
    request: ElfRequest,
    code: Vec<u8>,
    failures: Vec<usize>,
    dynamic: Option<elf::DynamicLink>,
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

impl Probe {
    pub fn new() -> Self {
        let sections = vec![
            section(Segment::Text, SectionContent::Code(vec![])),
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
            dynamic: None,
        };
        p.emit("sub.X.imm", vec![sp(), sp(), immediate(64)]);
        for (i, n) in [21, 22, 23, 24, 25, 26, 29, 30].into_iter().enumerate() {
            p.emit("str.X.Unsigned", vec![reg(n), stack(i * 8)]);
        }
        p
    }

    pub fn fragment(&mut self, fragment: Fragment) -> Location {
        self.symbolic_fragment(fragment, &std::collections::BTreeMap::new())
    }

    pub fn dynamic(&mut self, dynamic: elf::DynamicLink) {
        self.dynamic = Some(dynamic);
    }

    pub fn symbolic_fragment(
        &mut self,
        fragment: Fragment,
        symbols: &std::collections::BTreeMap<String, Location>,
    ) -> Location {
        let location = at(self.request.sections.len());
        for site in fragment.relocations {
            self.request.relocations.push(Relocation {
                at: Location {
                    section: location.section,
                    offset: u64::from(site.offset),
                },
                target: symbols[&site.relocation.target],
                kind: site.relocation.kind,
                addend: site.relocation.addend,
            });
        }
        self.request
            .sections
            .push(section(Segment::Text, SectionContent::Code(fragment.bytes)));
        location
    }

    pub fn data(&mut self, bytes: Vec<u8>) -> Location {
        let location = at(self.request.sections.len());
        self.request
            .sections
            .push(section(Segment::Data, SectionContent::Data(bytes)));
        location
    }

    pub fn record(&mut self, bytes: usize, fields: &[(usize, Arg)]) -> Location {
        let mut data = vec![0; bytes];
        for &(offset, arg) in fields {
            if let Number(value) = arg {
                data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        let location = self.data(data);
        for &(offset, arg) in fields {
            match arg {
                Pointer(target) => self.request.relatives.push(Relative {
                    at: Location {
                        section: location.section,
                        offset: offset as u64,
                    },
                    target,
                    addend: 0,
                }),
                Register(n) => {
                    self.address(9, location);
                    self.store(n, 9, offset);
                }
                Number(_) => {}
            }
        }
        location
    }

    pub fn emit(&mut self, key: &str, args: Vec<Operand>) {
        self.code.extend(instruction(key, args));
    }
    pub fn offset(&self) -> usize {
        self.code.len()
    }

    pub fn number(&mut self, n: u8, value: u64) {
        for shift in [0, 16, 32, 48] {
            let imm16 = (value >> shift) as u16;
            if shift == 0 || imm16 != 0 {
                self.emit(
                    if shift == 0 { "movz.X" } else { "movk.X" },
                    vec![reg(n), Operand::Wide { imm16, shift }],
                );
            }
        }
    }

    pub fn mov(&mut self, dst: u8, src: u8) {
        self.emit("add.X.imm", vec![reg(dst), reg(src), immediate(0)]);
    }
    pub fn load(&mut self, dst: u8, base: u8, offset: usize) {
        self.emit("ldr.X.Unsigned", vec![reg(dst), memory(base, offset)]);
    }
    pub fn load32(&mut self, dst: u8, base: u8, offset: usize) {
        self.emit(
            "ldr.W.Unsigned",
            vec![Operand::Reg(Reg::w(dst)), memory(base, offset)],
        );
    }
    pub fn store(&mut self, src: u8, base: u8, offset: usize) {
        self.emit("str.X.Unsigned", vec![reg(src), memory(base, offset)]);
    }

    pub fn address(&mut self, n: u8, target: Location) {
        self.fixup(target, RelocKind::AdrpPage21);
        self.emit("adrp", vec![reg(n), Operand::Target(Target::Offset(0))]);
        self.fixup(target, RelocKind::AddLo12);
        self.emit("add.X.imm", vec![reg(n), reg(n), immediate(0)]);
    }

    pub fn call(&mut self, target: Location) {
        self.fixup(target, RelocKind::Call26);
        self.emit("bl", vec![Operand::Target(Target::Offset(0))]);
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

    pub fn expect(&mut self, n: u8, value: u64) {
        self.number(11, value);
        self.equal(n, 11);
    }

    pub fn equal(&mut self, left: u8, right: u8) {
        self.emit(
            "subs.X.shift",
            vec![
                Operand::Reg(Reg::zr(Width::X)),
                reg(left),
                reg(right),
                Operand::Shift(Shift::Lsl, 0),
            ],
        );
        self.fail_when(1);
    }

    fn fail_when(&mut self, condition: u8) {
        self.number(26, self.failures.len() as u64 + 1);
        self.failures.push(self.code.len());
        self.emit(
            "b.cond",
            vec![
                Operand::Target(Target::Offset(0)),
                Operand::Condition(condition),
            ],
        );
    }

    pub fn expect_nonzero(&mut self, n: u8) {
        self.emit(
            "subs.X.imm",
            vec![Operand::Reg(Reg::zr(Width::X)), reg(n), immediate(0)],
        );
        self.fail_when(0);
    }

    pub fn repeat_if_equal(&mut self, n: u8, value: u64, at: usize) {
        self.repeat_if(n, value, at, 0);
    }

    pub fn repeat_if(&mut self, n: u8, value: u64, at: usize, condition: u8) {
        self.number(11, value);
        self.emit(
            "subs.X.shift",
            vec![
                Operand::Reg(Reg::zr(Width::X)),
                reg(n),
                reg(11),
                Operand::Shift(Shift::Lsl, 0),
            ],
        );
        self.emit(
            "b.cond",
            vec![
                Operand::Target(Target::Offset(at as i64 - self.code.len() as i64)),
                Operand::Condition(condition),
            ],
        );
    }

    pub fn output(&mut self, bytes: &[u8]) {
        let text = self.data(bytes.to_vec());
        self.number(0, 1);
        self.address(1, text);
        self.number(2, bytes.len() as u64);
        self.number(8, 64);
        self.emit("svc", vec![Operand::Imm(0)]);
        self.expect(0, bytes.len() as u64);
    }

    pub fn finish(mut self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.number(0, 0);
        self.return_to_rt0();
        let failed = self.code.len();
        self.mov(0, 26);
        self.return_to_rt0();
        for offset in self.failures {
            let condition =
                (u32::from_le_bytes(self.code[offset..offset + 4].try_into().unwrap()) & 15) as u8;
            self.code[offset..offset + 4].copy_from_slice(&instruction(
                "b.cond",
                vec![
                    Operand::Target(Target::Offset((failed - offset) as i64)),
                    Operand::Condition(condition),
                ],
            ));
        }
        self.request.sections[0].content = SectionContent::Code(self.code);
        let entry = LinuxEntry {
            rt0_start: at(0),
            payload: at(1),
            initial_slot: at(2),
            processor: Location {
                section: 2,
                offset: INITIAL_SLOT_BYTES,
            },
        };
        let image = if let Some(dynamic) = self.dynamic {
            elf::link_entry_dynamic(self.request, entry, &dynamic)?
        } else {
            elf::link_entry(self.request, entry)?
        };
        std::fs::write(path, image.bytes)?;
        println!("wrote verification image to {path}");
        Ok(())
    }

    fn return_to_rt0(&mut self) {
        for (i, n) in [21, 22, 23, 24, 25, 26, 29, 30].into_iter().enumerate() {
            self.emit("ldr.X.Unsigned", vec![reg(n), stack(i * 8)]);
        }
        self.emit("add.X.imm", vec![sp(), sp(), immediate(64)]);
        self.emit("ret", vec![reg(30)]);
    }
}

fn instruction(key: &str, args: Vec<Operand>) -> [u8; 4] {
    encode::encode(&machine::inst(key, args).unwrap())
        .unwrap()
        .bytes()
}
pub fn at(section: usize) -> Location {
    Location { section, offset: 0 }
}
pub fn reg(n: u8) -> Operand {
    Operand::Reg(Reg::x(n))
}
fn sp() -> Operand {
    Operand::Reg(Reg::sp(Width::X))
}
fn immediate(imm12: u16) -> Operand {
    Operand::AddImm {
        imm12,
        shift12: false,
    }
}
fn memory(base: u8, offset: usize) -> Operand {
    Operand::Address(Address {
        base: Reg::x(base),
        mode: AddressMode::Unsigned(offset as i64),
    })
}
fn stack(offset: usize) -> Operand {
    Operand::Address(Address {
        base: Reg::sp(Width::X),
        mode: AddressMode::Unsigned(offset as i64),
    })
}
fn section(segment: Segment, content: SectionContent) -> Section {
    Section {
        name: "probe".into(),
        segment,
        align: 8,
        content,
    }
}
