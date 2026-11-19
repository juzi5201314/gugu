use super::{
    Error, decode, encode, error,
    fragments::Fragment,
    inst::{AddressMode, Operand, RelocKind, Target},
    table::{self, Kind},
};

pub(crate) fn relocate(
    fragment: Fragment,
    base: u64,
    resolve: impl Fn(&str) -> Result<u64, String>,
) -> Result<Vec<u8>, String> {
    let mut bytes = fragment.bytes;
    for site in fragment.relocations {
        let rel = site.relocation;
        let target = resolve(&rel.target)?
            .checked_add_signed(rel.addend)
            .ok_or_else(|| format!("A64 relocation target overflow: {}", rel.target))?;
        let offset = site.offset as usize;
        let pc = base.checked_add(offset as u64).ok_or("A64 PC overflow")?;
        let field = bytes
            .get_mut(offset..offset + 4)
            .filter(|_| offset.is_multiple_of(4))
            .ok_or("A64 relocation outside instruction")?;
        let word = u32::from_le_bytes(field.try_into().unwrap());
        field.copy_from_slice(&patch(word, rel.kind, pc, target).map_err(|e| e.to_string())?);
    }
    Ok(bytes)
}

pub(crate) fn patch(word: u32, kind: RelocKind, pc: u64, target: u64) -> Result<[u8; 4], Error> {
    let mut inst = decode::decode(word)?;
    let form = table::get(inst.form)?;
    let low = (target & 4095) as u16;
    let (operand, value) = match (kind, form.key) {
        (RelocKind::AdrpPage21, "adrp") => {
            let pages = (target >> 12) as i64 - (pc >> 12) as i64;
            (1, Operand::Target(Target::Offset(pages << 12)))
        }
        (RelocKind::AddLo12, "add.X.imm") => (
            2,
            Operand::AddImm {
                imm12: low,
                shift12: false,
            },
        ),
        (RelocKind::Call26, "bl") | (RelocKind::Branch26, "b") => {
            let delta = i64::try_from(i128::from(target) - i128::from(pc))
                .map_err(|_| error("branch displacement overflow"))?;
            (0, Operand::Target(Target::Offset(delta)))
        }
        (RelocKind::LdStLo12(scale), _) => {
            let index = form
                .operands
                .iter()
                .position(|slot| matches!(slot.kind, Kind::Address { size, .. } if size == scale))
                .ok_or_else(|| error("load/store relocation scale mismatch"))?;
            let Operand::Address(mut address) = inst.operands[index].clone() else {
                return Err(error("load/store relocation requires an address"));
            };
            if !matches!(address.mode, AddressMode::Unsigned(_)) {
                return Err(error("load/store relocation requires unsigned addressing"));
            }
            address.mode = AddressMode::Unsigned(i64::from(low));
            (index, Operand::Address(address))
        }
        _ => return Err(error("unexpected A64 instruction relocation")),
    };
    inst.operands[operand] = value;
    Ok(encode::encode(&inst)?.bytes())
}
