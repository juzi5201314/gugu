//! 每条指令固定生成一个小端 32 位字；符号字段保留为零。

use super::{Error, inst::*, table::*, verify};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Relocation {
    pub kind: RelocKind,
    pub target: String,
    pub addend: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Encoded {
    pub word: u32,
    pub relocations: Vec<Relocation>,
}

impl Encoded {
    pub fn bytes(&self) -> [u8; 4] {
        self.word.to_le_bytes()
    }
}

pub fn encode(inst: &Inst) -> Result<Encoded, Error> {
    let values = verify::values(inst)?;
    let form = get(inst.form)?;
    let mut word = form.fixed;
    let mut relocations = Vec::new();
    for ((slot, value), op) in form.operands.iter().zip(values).zip(&inst.operands) {
        for field in slot.fields {
            if field.width != 0 {
                word |= ((value >> field.value_lsb) << field.lsb) & field.mask();
            }
        }
        if let Some(relocation) = relocation(slot.kind, op) {
            relocations.push(relocation);
        }
    }
    Ok(Encoded { word, relocations })
}

fn relocation(kind: Kind, op: &Operand) -> Option<Relocation> {
    let (kind, name, addend) = match (kind, op) {
        (
            Kind::Target {
                reloc: Some(kind), ..
            },
            Operand::Target(Target::Symbol { name, addend }),
        ) => (kind, name, addend),
        (Kind::AddImm, Operand::Target(Target::Symbol { name, addend })) => {
            (RelocKind::AddLo12, name, addend)
        }
        (
            Kind::Address {
                mode: AddrMode::Unsigned,
                size,
            },
            Operand::Address(Address {
                mode: AddressMode::Reloc { name, addend },
                ..
            }),
        ) => (RelocKind::LdStLo12(size), name, addend),
        _ => return None,
    };
    Some(Relocation {
        kind,
        target: name.clone(),
        addend: *addend,
    })
}
