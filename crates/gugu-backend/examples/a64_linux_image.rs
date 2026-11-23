//! Manual verification: cargo run -p gugu-backend --example a64_linux_image -- /tmp/a64-linux.elf
use gugu_backend::a64::{
    elf::{
        self, ElfRequest, LinuxEntry, Location, Relative, Relocation, Section, SectionContent,
        Segment,
    },
    encode,
    inst::{Address, AddressMode, Operand, RelocKind, Target},
    machine,
    reg::Reg,
};
use gugu_runtime_abi::{
    entry::{INITIAL_SLOT_BYTES, Rt0Payload},
    processor::PROCESSOR_PAGE_BYTES,
};

const MESSAGE: &[u8] = b"hello from aarch64 Linux static PIE\n";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: a64_linux_image <output>")?;
    let image = elf::link_entry(
        request()?,
        LinuxEntry {
            rt0_start: at(0, 0),
            payload: at(3, 0),
            initial_slot: at(4, 0),
            processor: at(4, INITIAL_SLOT_BYTES),
            runtime: None,
        },
    )?;
    std::fs::write(&path, &image.bytes)?;
    println!("wrote {} bytes to {path}", image.bytes.len());
    Ok(())
}

fn request() -> Result<ElfRequest, String> {
    Ok(ElfRequest {
        sections: vec![
            section(Segment::Text, SectionContent::Code(program()?)),
            section(Segment::DataConst, SectionContent::Data(MESSAGE.to_vec())),
            section(Segment::Data, SectionContent::Data(vec![0; 8])),
            section(
                Segment::Data,
                SectionContent::Data(vec![0; Rt0Payload::bytes() as usize]),
            ),
            section(
                Segment::Data,
                SectionContent::ZeroFill(INITIAL_SLOT_BYTES + PROCESSOR_PAGE_BYTES as u64),
            ),
        ],
        entry: at(0, 0),
        relatives: vec![Relative {
            at: at(2, 0),
            target: at(1, 0),
            addend: 0,
        }],
        relocations: vec![
            Relocation {
                at: at(0, 0),
                target: at(2, 0),
                addend: 0,
                kind: RelocKind::AdrpPage21,
            },
            Relocation {
                at: at(0, 4),
                target: at(2, 0),
                addend: 0,
                kind: RelocKind::LdStLo12(3),
            },
        ],
    })
}

fn program() -> Result<Vec<u8>, String> {
    let reg = |n| Operand::Reg(Reg::x(n));
    let wide = |imm16| Operand::Wide { imm16, shift: 0 };
    let instructions = [
        ("adrp", vec![reg(9), Operand::Target(Target::Offset(0))]),
        (
            "ldr.X.Unsigned",
            vec![
                reg(1),
                Operand::Address(Address {
                    base: Reg::x(9),
                    mode: AddressMode::Unsigned(0),
                }),
            ],
        ),
        ("movz.X", vec![reg(0), wide(1)]),
        ("movz.X", vec![reg(2), wide(MESSAGE.len() as u16)]),
        ("movz.X", vec![reg(8), wide(64)]),
        ("svc", vec![Operand::Imm(0)]),
        ("movz.X", vec![reg(0), wide(42)]),
        ("ret", vec![reg(30)]),
    ];
    let mut bytes = Vec::new();
    for (key, operands) in instructions {
        let inst = machine::inst(key, operands).map_err(|e| e.to_string())?;
        bytes.extend(encode::encode(&inst).map_err(|e| e.to_string())?.bytes());
    }
    Ok(bytes)
}

fn at(section: usize, offset: u64) -> Location {
    Location { section, offset }
}

fn section(segment: Segment, content: SectionContent) -> Section {
    Section {
        name: "probe".into(),
        segment,
        align: 8,
        content,
    }
}
