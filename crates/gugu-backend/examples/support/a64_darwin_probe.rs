use gugu_backend::{
    NativeImport,
    a64::{
        encode,
        foreign::Platform,
        fragments::Fragment,
        inst::{Address, AddressMode, Operand, RelocKind, Shift, Target},
        machine,
        macho::{DarwinEntry, import_stub_symbol, link_entry},
        reg::{Reg, Width},
        world::{InstructionRelocation, World},
    },
    macho::{Location, Section, SectionContent, Segment},
};
use gugu_runtime_abi::{
    entry::{INITIAL_SLOT_BYTES, Rt0Payload},
    processor::PROCESSOR_PAGE_BYTES,
};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub enum Arg {
    Number(u64),
    Register(u8),
}
use Arg::*;

pub struct Probe {
    world: World,
    code: Vec<u8>,
    failures: Vec<usize>,
    data_index: usize,
}

impl Probe {
    pub fn new(imports: Vec<NativeImport>) -> Self {
        let mut p = Self {
            world: World {
                platform: Platform::Darwin,
                sections: vec![
                    section(Segment::Text, "__text", SectionContent::Code(vec![])),
                    section(
                        Segment::Data,
                        "__payload",
                        SectionContent::Data(vec![0; Rt0Payload::bytes() as usize]),
                    ),
                    section(
                        Segment::Data,
                        "__runtime",
                        SectionContent::Data(vec![
                            0;
                            INITIAL_SLOT_BYTES as usize
                                + PROCESSOR_PAGE_BYTES
                        ]),
                    ),
                ],
                symbols: BTreeMap::new(),
                sizes: BTreeMap::new(),
                relocations: vec![],
                pointers: vec![],
                imports,
                entry: at(0),
                maps: at(0),
                unwind: at(0),
                code: at(0),
            },
            code: vec![],
            failures: vec![],
            data_index: 0,
        };
        p.world.symbols.insert("probe_start".into(), at(0));
        p.world.sizes.insert("probe_start".into(), 4);
        p.emit("sub.X.imm", vec![sp(), sp(), immediate(64)]);
        for (i, n) in [21, 22, 23, 24, 25, 26, 29, 30].into_iter().enumerate() {
            p.emit("str.X.Unsigned", vec![reg(n), stack(i * 8)]);
        }
        p
    }

    /// 注册 fragment 为独立 `__text` 节，返回其内部符号名。
    pub fn fragment(&mut self, name: &str, fragment: Fragment) -> String {
        let location = Location {
            section: self.world.sections.len(),
            offset: 0,
        };
        self.world.symbols.insert(name.to_owned(), location);
        self.world
            .sizes
            .insert(name.to_owned(), fragment.bytes.len() as u64);
        for site in fragment.relocations {
            self.world.relocations.push(InstructionRelocation {
                at: Location {
                    section: location.section,
                    offset: u64::from(site.offset),
                },
                relocation: site.relocation,
            });
        }
        self.world.sections.push(section(
            Segment::Text,
            name,
            SectionContent::Code(fragment.bytes),
        ));
        name.to_owned()
    }

    /// 注册 `__DATA` 数据节，返回其内部符号名。
    pub fn data(&mut self, bytes: Vec<u8>) -> String {
        let name = format!("__probe_data_{}", self.data_index);
        self.data_index += 1;
        let location = Location {
            section: self.world.sections.len(),
            offset: 0,
        };
        self.world.symbols.insert(name.clone(), location);
        self.world.sizes.insert(name.clone(), bytes.len() as u64);
        self.world
            .sections
            .push(section(Segment::Data, &name, SectionContent::Data(bytes)));
        name
    }

    /// 16 字节定长记录：(base, bytes)*，Register 字段由代码写入。
    pub fn record(&mut self, fields: &[(usize, Arg)]) -> String {
        let mut data = vec![0; 16];
        for &(offset, arg) in fields {
            if let Number(value) = arg {
                data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        let name = self.data(data);
        for &(offset, arg) in fields {
            if let Register(n) = arg {
                self.address(9, &name);
                self.store(n, 9, offset);
            }
        }
        name
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

    /// x{n} = 名为 `target` 的符号地址。
    pub fn address(&mut self, n: u8, target: &str) {
        self.fixup(target, RelocKind::AdrpPage21);
        self.emit("adrp", vec![reg(n), Operand::Target(Target::Offset(0))]);
        self.fixup(target, RelocKind::AddLo12);
        self.emit("add.X.imm", vec![reg(n), reg(n), immediate(0)]);
    }

    /// `bl` 到名为 `target` 的符号（片段或 `__gugu_darwin_c_*` 调用桩）。
    pub fn call(&mut self, target: &str) {
        self.fixup(target, RelocKind::Call26);
        self.emit("bl", vec![Operand::Target(Target::Offset(0))]);
    }

    fn fixup(&mut self, target: &str, kind: RelocKind) {
        self.world.relocations.push(InstructionRelocation {
            at: Location {
                section: 0,
                offset: self.code.len() as u64,
            },
            relocation: encode::Relocation {
                kind,
                target: target.to_owned(),
                addend: 0,
            },
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

    /// write(STDOUT_FILENO, bytes, len)，短写视为失败。
    pub fn output(&mut self, bytes: &[u8]) {
        let text = self.data(bytes.to_vec());
        self.number(0, 1);
        self.address(1, &text);
        self.number(2, bytes.len() as u64);
        self.call(&import_stub_symbol("write"));
        self.expect(0, bytes.len() as u64);
    }

    /// 组装 World → MachORequest → 签名镜像，写出可执行文件。
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
        self.world
            .sizes
            .insert("probe_start".into(), self.code.len() as u64);
        self.world.sections[0].content = SectionContent::Code(self.code);
        let request = self.world.macho_request("a64-darwin-probe")?;
        let image = link_entry(
            request,
            DarwinEntry {
                rt0_start: at(0),
                payload: at(1),
                initial_slot: at(2),
                processor: Location {
                    section: 2,
                    offset: INITIAL_SLOT_BYTES,
                },
            },
        )?;
        std::fs::write(path, image.bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions)?;
        }
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
fn section(segment: Segment, name: &str, content: SectionContent) -> Section {
    Section {
        name: name.to_owned(),
        segment,
        align: 8,
        content,
    }
}
