//! x86_64 编码器：把 form 与操作数序列写成机器字节、重定位与寄存器约束。
//!
//! 单一 layout 同时驱动长度计算与写入；`assemble` 在写入后复核每条指令的实际长度，
//! 长度不符即报错（release 也不依赖 `debug_assert`）。局部标签按 form 声明的 rel32/rel8
//! 字段宽度编码并在序列内回填；指向符号与冷边的操作数产生重定位记录。

use std::fmt;

use super::inst::{
    Assembled, ConstraintKind, Inst, LabelId, Mem, Operand, RegisterConstraint, RelocKind,
    RelocTarget, Relocation, Scale, Sequence,
};
use super::reg::{Clobbers, Gpr, Reg};
use super::table::{self, Access, Form, ImmKind, Lock, Map, OperandKind, Prefix, RexW};

/// 编码失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EncodeError {
    message: String,
}

impl EncodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EncodeError {}

/// 返回单条指令的编码字节数（与写入共用同一 layout）；仅供字节 fixture 断言。
#[cfg(test)]
pub(crate) fn encoded_len(inst: &Inst) -> Result<u32, EncodeError> {
    Ok(build(inst)?.len())
}

/// 编码整段序列，解析局部标签并收集重定位、约束与 clobber 集合。
pub(crate) fn assemble(sequence: &Sequence) -> Result<Assembled, EncodeError> {
    let mut encoded = Vec::with_capacity(sequence.instructions.len());
    let mut offsets = Vec::with_capacity(sequence.instructions.len());
    let mut total = 0_u32;
    for inst in &sequence.instructions {
        let item = build(inst)?;
        offsets.push(total);
        total = total
            .checked_add(item.len())
            .ok_or_else(|| EncodeError::new("序列字节长度超出 u32"))?;
        encoded.push(item);
    }
    let mut label_offsets: Vec<Option<u32>> = Vec::new();
    for definition in &sequence.labels {
        let index = usize::try_from(definition.label.0)
            .map_err(|_| EncodeError::new("标签编号超出宿主范围"))?;
        let at =
            usize::try_from(definition.at).map_err(|_| EncodeError::new("标签位置超出宿主范围"))?;
        let offset = offsets.get(at).copied().unwrap_or(total);
        if label_offsets.len() <= index {
            label_offsets.resize(index + 1, None);
        }
        if label_offsets[index].replace(offset).is_some() {
            return Err(EncodeError::new("标签被重复定义"));
        }
    }
    if label_offsets.iter().any(Option::is_none) {
        return Err(EncodeError::new("标签编号必须从 0 起连续定义"));
    }

    let mut bytes = Vec::with_capacity(usize::try_from(total).unwrap_or(usize::MAX));
    let mut relocations = Vec::new();
    let mut constraints = Vec::new();
    let mut clobbers = Clobbers::NONE;
    for (inst, item) in sequence.instructions.iter().zip(&encoded) {
        let start = u32::try_from(bytes.len()).expect("序列字节数在 u32 内");
        if item.lock {
            bytes.push(0xF0);
        }
        if let Some(prefix) = item.prefix {
            bytes.push(prefix);
        }
        if let Some(rex) = item.rex {
            bytes.push(rex);
        }
        let opcode_len = usize::from(item.opcode_len);
        bytes.extend_from_slice(&item.opcode[..opcode_len]);
        if let Some(modrm) = item.modrm {
            bytes.push(modrm);
        }
        if let Some(sib) = item.sib {
            bytes.push(sib);
        }
        write_field(&mut bytes, u64::from(item.disp), item.disp_len);
        write_field(&mut bytes, item.imm, item.imm_len);
        let written = u32::try_from(bytes.len()).expect("序列字节数在 u32 内") - start;
        if written != item.len() {
            return Err(EncodeError::new("编码写入长度与 layout 不一致"));
        }
        if let Some(relocation) = &item.relocation {
            relocations.push(Relocation {
                offset: start + relocation.offset,
                kind: relocation.kind,
                target: relocation.target.clone(),
                addend: relocation.addend,
            });
        }
        if let Some((offset, label, field_len)) = item.label {
            let index =
                usize::try_from(label.0).map_err(|_| EncodeError::new("标签编号超出宿主范围"))?;
            let target = label_offsets[index].expect("编号连续已校验");
            let next = start
                .checked_add(offset)
                .and_then(|value| value.checked_add(u32::from(field_len)))
                .ok_or_else(|| EncodeError::new("分支修正位置溢出"))?;
            let value = i64::from(target) - i64::from(next);
            let at = usize::try_from(start + offset).expect("序列字节数在 u32 内");
            let width = usize::from(field_len);
            match field_len {
                1 => {
                    let value =
                        i8::try_from(value).map_err(|_| EncodeError::new("分支距离超出 rel8"))?;
                    bytes[at] = value.to_le_bytes()[0];
                }
                4 => {
                    let value =
                        i32::try_from(value).map_err(|_| EncodeError::new("分支距离超出 rel32"))?;
                    bytes[at..at + width].copy_from_slice(&value.to_le_bytes());
                }
                _ => return Err(EncodeError::new("分支字段宽度必须是 1 或 4")),
            }
        }
        for (offset, register) in &item.constraint {
            constraints.push(RegisterConstraint {
                offset: start + offset,
                register: *register,
                kind: ConstraintKind::ByteEncodable,
            });
        }
        clobbers = clobbers.union(instruction_clobbers(inst));
    }
    Ok(Assembled {
        bytes,
        relocations,
        labels: label_offsets
            .into_iter()
            .map(|offset| offset.expect("编号连续已校验"))
            .collect(),
        instruction_offsets: offsets
            .into_iter()
            .enumerate()
            .map(|(index, offset)| (u32::try_from(index).expect("指令数在 u32 内"), offset))
            .collect(),
        constraints,
        clobbers,
    })
}

/// 序列写到的物理寄存器集合：只统计寄存器操作数的写与读写位置。
pub(crate) fn sequence_clobbers(sequence: &Sequence) -> Clobbers {
    sequence
        .instructions
        .iter()
        .fold(Clobbers::NONE, |accumulated, inst| {
            accumulated.union(instruction_clobbers(inst))
        })
}

fn instruction_clobbers(inst: &Inst) -> Clobbers {
    let form = table::form(inst.form);
    let mut clobbers = form.clobbers;
    for (index, operand) in inst.operands.iter().enumerate() {
        let Some(access) = form.access.get(index) else {
            continue;
        };
        if !matches!(access, Access::Write | Access::ReadWrite) {
            continue;
        }
        if let Operand::Reg(reg) = operand {
            match reg {
                Reg::Gpr(gpr) => clobbers = clobbers.union(Clobbers::gpr(*gpr)),
                Reg::Xmm(xmm) => clobbers = clobbers.union(Clobbers::xmm(*xmm)),
                Reg::Virtual(_) => {}
            }
        }
    }
    clobbers
}

/// 重定位在指令内的字段位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldPlacement {
    Disp,
    Imm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingReloc {
    placement: FieldPlacement,
    kind: RelocKind,
    target: RelocTarget,
    addend: i64,
}

/// 重定位字段：`offset` 相对指令起点。
#[derive(Clone, Debug, Eq, PartialEq)]
struct RelocField {
    offset: u32,
    kind: RelocKind,
    target: RelocTarget,
    addend: i64,
}

/// 单条指令的编码布局。
#[derive(Clone, Debug, Eq, PartialEq)]
struct Encoded {
    /// 显式 `lock` 前缀（写在其它 legacy 前缀之前）。
    lock: bool,
    prefix: Option<u8>,
    rex: Option<u8>,
    opcode: [u8; 3],
    opcode_len: u8,
    modrm: Option<u8>,
    sib: Option<u8>,
    disp: u32,
    disp_len: u8,
    imm: u64,
    imm_len: u8,
    /// 以指令起点为基准的字段偏移。
    relocation: Option<RelocField>,
    /// 局部标签回填：相对指令起点的字段偏移、标签、字段宽度。
    label: Option<(u32, LabelId, u8)>,
    /// 物理分配约束；offset 相对指令起点。
    constraint: Vec<(u32, Reg)>,
}

impl Encoded {
    fn len(&self) -> u32 {
        u32::from(self.lock)
            + u32::from(self.prefix.is_some())
            + u32::from(self.rex.is_some())
            + u32::from(self.opcode_len)
            + u32::from(self.modrm.is_some())
            + u32::from(self.sib.is_some())
            + u32::from(self.disp_len)
            + u32::from(self.imm_len)
    }
}

fn write_field(out: &mut Vec<u8>, value: u64, len: u8) {
    match len {
        0 => {}
        1 => out.push(value as u8),
        2 => out.extend_from_slice(&(value as u16).to_le_bytes()),
        4 => out.extend_from_slice(&(value as u32).to_le_bytes()),
        8 => out.extend_from_slice(&value.to_le_bytes()),
        _ => unreachable!("字段长度只有 0/1/2/4/8"),
    }
}

fn build(inst: &Inst) -> Result<Encoded, EncodeError> {
    let form = table::form(inst.form);
    if inst.operands.len() != form.operands.len() {
        return Err(EncodeError::new(format!(
            "{} 需要 {} 个操作数，实际 {} 个",
            form.mnemonic,
            form.operands.len(),
            inst.operands.len()
        )));
    }
    if matches!(form.map, Map::Vex) {
        return Err(EncodeError::new(format!(
            "{} 使用 VEX 编码，当前编码器未实现",
            form.mnemonic
        )));
    }
    if inst.lock {
        if form.lock != Lock::Allowed {
            return Err(EncodeError::new(format!(
                "{} 不接受显式 lock 前缀",
                form.mnemonic
            )));
        }
        if !inst
            .operands
            .iter()
            .any(|operand| matches!(operand, Operand::Mem(_) | Operand::Rip(..)))
        {
            return Err(EncodeError::new("lock 前缀要求内存操作数"));
        }
    }

    let mut addressed: Option<&Operand> = None;
    let mut register: Option<&Operand> = None;
    let mut immediate: Option<(OperandKind, &Operand)> = None;
    for (kind, operand) in form.operands.iter().zip(&inst.operands) {
        match kind {
            OperandKind::Imm8
            | OperandKind::Imm16
            | OperandKind::Imm32
            | OperandKind::Imm64
            | OperandKind::Rel32
            | OperandKind::Rel8 => {
                if immediate.replace((*kind, operand)).is_some() {
                    return Err(EncodeError::new("form 声明了多个立即数操作数"));
                }
            }
            OperandKind::Cl => {
                if !matches!(operand, Operand::Reg(Reg::Gpr(Gpr::Rcx))) {
                    return Err(EncodeError::new("cl 操作数必须绑定 rcx"));
                }
            }
            _ => {
                if addressed.is_none() {
                    addressed = Some(operand);
                } else if register.is_none() && !form.reg_in_opcode {
                    register = Some(operand);
                } else {
                    return Err(EncodeError::new("form 声明的编码位置多于两个"));
                }
            }
        }
    }
    let mut opcode = [0_u8; 3];
    let mut opcode_len = 1_u8;
    match form.map {
        Map::OneByte => {}
        Map::TwoByte => {
            opcode[0] = 0x0F;
            opcode_len = 2;
        }
        Map::ThreeByte38 => {
            opcode[0] = 0x0F;
            opcode[1] = 0x38;
            opcode_len = 3;
        }
        Map::ThreeByte3A => {
            opcode[0] = 0x0F;
            opcode[1] = 0x3A;
            opcode_len = 3;
        }
        Map::Vex => unreachable!("VEX 已在前面拒绝"),
    }
    let slot = usize::from(opcode_len) - 1;

    let rex_w = form.rex_w == RexW::Force;
    let mut rex_r = false;
    let mut rex_x = false;
    let mut rex_b = false;
    let mut modrm = None;
    let mut sib = None;
    let mut disp = 0_u32;
    let mut disp_len = 0_u8;
    let mut pending: Option<PendingReloc> = None;

    if form.reg_in_opcode {
        let code = operand_gpr_code(
            addressed.ok_or_else(|| EncodeError::new("+r 形式缺少寄存器操作数"))?,
        )?;
        opcode[slot] = form.opcode | (code & 7);
        rex_b = code >= 8;
    } else if let Some(addressed) = addressed {
        let reg_code = match form.opcode_ext {
            Some(ext) => ext,
            None => match register {
                Some(register) => {
                    let code = operand_code(register)?;
                    rex_r = code >= 8;
                    code & 7
                }
                // 单操作数形式（`setcc` 等）：ModRM.reg 保留为 0。
                None => 0,
            },
        };
        match addressed {
            Operand::Reg(reg) => {
                let code = physical_code(*reg);
                rex_b = code >= 8;
                modrm = Some(0b11_000_000 | (reg_code << 3) | (code & 7));
            }
            Operand::Mem(mem) => {
                let layout = memory_layout(mem)?;
                rex_x = layout.rex_x;
                rex_b = layout.rex_b;
                modrm = Some((layout.mod_bits << 6) | (reg_code << 3) | layout.modrm_rm);
                sib = layout.sib;
                disp = layout.disp;
                disp_len = layout.disp_len;
            }
            Operand::Rip(target, addend) => {
                modrm = Some((reg_code << 3) | 0b101);
                disp = *addend as u32;
                disp_len = 4;
                pending = Some(PendingReloc {
                    placement: FieldPlacement::Disp,
                    kind: RelocKind::PcRel32,
                    target: target.clone(),
                    addend: i64::from(*addend),
                });
            }
            _ => return Err(EncodeError::new("可寻址操作数不是寄存器或内存")),
        }
        opcode[slot] = form.opcode;
    } else {
        // 纯立即数分支或无操作数形式：只有显式固定 ModRM 的形式（`mfence` 一族）才生成 ModRM。
        opcode[slot] = form.opcode;
        modrm = form.fixed_modrm;
    }

    let mut imm = 0_u64;
    let mut imm_len = 0_u8;
    let mut label = None;
    if let Some((kind, operand)) = immediate {
        match operand {
            Operand::Imm(value) => {
                let (declared, len) = immediate_width(kind)?;
                if declared != form.imm {
                    return Err(EncodeError::new("立即数操作数与 form 声明不一致"));
                }
                imm = *value;
                imm_len = len;
            }
            Operand::Reloc(target, reloc_kind) => {
                let (_, len) = immediate_width(kind)?;
                if len != reloc_width(*reloc_kind) {
                    return Err(EncodeError::new("重定位宽度与立即数字段不一致"));
                }
                imm_len = len;
                pending = Some(PendingReloc {
                    placement: FieldPlacement::Imm,
                    kind: *reloc_kind,
                    target: target.clone(),
                    addend: 0,
                });
            }
            Operand::Label(id) => {
                if !matches!(kind, OperandKind::Rel32 | OperandKind::Rel8) {
                    return Err(EncodeError::new("标签只能出现在 rel32/rel8 分支位置"));
                }
                let (_, len) = immediate_width(kind)?;
                imm_len = len;
                label = Some(*id);
            }
            _ => return Err(EncodeError::new("立即数位置不是立即数、重定位或标签")),
        }
    } else if form.imm != ImmKind::None {
        return Err(EncodeError::new("form 声明了立即数但缺少立即数操作数"));
    }

    // 字节位置的寄存器必须可低字节编码：物理寄存器当场拒绝，虚拟寄存器登记约束
    // 交给分配阶段（偏移指向该指令的 ModRM 字节）。
    let mut byte_registers = Vec::new();
    for (kind, operand) in form.operands.iter().zip(&inst.operands) {
        if !matches!(kind, OperandKind::Rm8 | OperandKind::R8) {
            continue;
        }
        match operand {
            Operand::Reg(Reg::Gpr(gpr)) if !gpr.is_byte_encodable() => {
                return Err(EncodeError::new(format!(
                    "{} 的低字节无法编码，需回避 {}",
                    form.mnemonic,
                    gpr.name()
                )));
            }
            Operand::Reg(Reg::Virtual(_)) => byte_registers.push(operand_reg(operand)?),
            _ => {}
        }
    }

    let prefix = match form.prefix {
        Prefix::None => None,
        Prefix::OperandSize66 => Some(0x66),
        Prefix::RepF3 => Some(0xF3),
        Prefix::RepF2 => Some(0xF2),
    };
    let needs_rex = rex_w || rex_r || rex_x || rex_b;
    let rex = needs_rex.then_some({
        let mut byte = 0x40;
        if rex_w {
            byte |= 0x08;
        }
        if rex_r {
            byte |= 0x04;
        }
        if rex_x {
            byte |= 0x02;
        }
        if rex_b {
            byte |= 0x01;
        }
        byte
    });
    let head = u32::from(inst.lock)
        + u32::from(prefix.is_some())
        + u32::from(needs_rex)
        + u32::from(opcode_len)
        + u32::from(modrm.is_some())
        + u32::from(sib.is_some());
    let mut relocation = None;
    if let Some(item) = pending {
        let offset = match item.placement {
            FieldPlacement::Disp => head,
            FieldPlacement::Imm => head + u32::from(disp_len),
        };
        relocation = Some(RelocField {
            offset,
            kind: item.kind,
            target: item.target,
            addend: item.addend,
        });
    }
    let label = label.map(|label| {
        let offset = head + u32::from(disp_len);
        (offset, label, imm_len)
    });
    // 字节约束的偏移：字节寄存器只出现在 ModRM 的 r/m 或 reg 字段，偏移即 ModRM 字节。
    let modrm_offset = head - u32::from(sib.is_some()) - 1;
    let constraint = byte_registers
        .into_iter()
        .map(|register| (modrm_offset, register))
        .collect();

    Ok(Encoded {
        lock: inst.lock,
        prefix,
        rex,
        opcode,
        opcode_len,
        modrm,
        sib,
        disp,
        disp_len,
        imm,
        imm_len,
        relocation,
        label,
        constraint,
    })
}

/// 立即数操作数声明的 (form 立即数种类, 字段宽度)。
fn immediate_width(kind: OperandKind) -> Result<(ImmKind, u8), EncodeError> {
    match kind {
        OperandKind::Imm8 => Ok((ImmKind::Ib, 1)),
        OperandKind::Imm16 => Ok((ImmKind::Iw, 2)),
        OperandKind::Imm32 => Ok((ImmKind::Id, 4)),
        OperandKind::Imm64 => Ok((ImmKind::Io, 8)),
        OperandKind::Rel32 => Ok((ImmKind::None, 4)),
        OperandKind::Rel8 => Ok((ImmKind::None, 1)),
        _ => Err(EncodeError::new("操作数不是立即数位置")),
    }
}

fn reloc_width(kind: RelocKind) -> u8 {
    match kind {
        RelocKind::PcRel32 | RelocKind::Rva32 => 4,
        RelocKind::Abs64 => 8,
    }
}

fn operand_reg(operand: &Operand) -> Result<Reg, EncodeError> {
    match operand {
        Operand::Reg(reg) => Ok(*reg),
        _ => Err(EncodeError::new("操作数不是寄存器")),
    }
}

/// 取寄存器编码；虚拟寄存器用低 3 位占位（物理分配由约束与分配阶段负责）。
fn reg_code(reg: Reg) -> u8 {
    match reg {
        Reg::Gpr(gpr) => gpr.code(),
        Reg::Xmm(xmm) => xmm.code(),
        Reg::Virtual(id) => u8::try_from(id & 7).expect("虚拟编号掩码后适配 u8"),
    }
}

fn operand_code(operand: &Operand) -> Result<u8, EncodeError> {
    Ok(reg_code(operand_reg(operand)?))
}

/// `+r` 形式的操作数编码：只接受通用寄存器或虚拟寄存器。
fn operand_gpr_code(operand: &Operand) -> Result<u8, EncodeError> {
    match operand {
        Operand::Reg(Reg::Gpr(gpr)) => Ok(gpr.code()),
        Operand::Reg(Reg::Virtual(id)) => Ok(u8::try_from(*id & 7).expect("虚拟编号掩码后适配 u8")),
        _ => Err(EncodeError::new("+r 形式需要通用寄存器操作数")),
    }
}

fn physical_code(reg: Reg) -> u8 {
    reg_code(reg)
}

struct MemoryLayout {
    mod_bits: u8,
    modrm_rm: u8,
    sib: Option<u8>,
    disp: u32,
    disp_len: u8,
    rex_x: bool,
    rex_b: bool,
}

fn memory_layout(mem: &Mem) -> Result<MemoryLayout, EncodeError> {
    if mem.index == Some(Reg::Gpr(Gpr::Rsp)) {
        return Err(EncodeError::new("SIB index 不能是 rsp"));
    }
    if mem.index.is_none() && mem.scale != Scale::One {
        return Err(EncodeError::new("无 index 的内存操作数不能带 scale"));
    }
    let disp = mem.disp as u32;
    match (mem.base, mem.index) {
        (None, None) => Ok(MemoryLayout {
            mod_bits: 0,
            modrm_rm: 0b100,
            sib: Some(0b00_100_101),
            disp,
            disp_len: 4,
            rex_x: false,
            rex_b: false,
        }),
        (None, Some(index)) => {
            let code = physical_code(index);
            Ok(MemoryLayout {
                mod_bits: 0,
                modrm_rm: 0b100,
                sib: Some((mem.scale.shift() << 6) | ((code & 7) << 3) | 0b101),
                disp,
                disp_len: 4,
                rex_x: code >= 8,
                rex_b: false,
            })
        }
        (Some(base), None) => {
            let code = physical_code(base);
            let (mod_bits, disp_len) = base_disp(base, mem.disp);
            // `rsp`/`r12` 的 r/m 编码与「SIB 跟随」冲突，必须改用 SIB 形式。
            let sib = (code & 7 == 0b100).then_some(0b00_100_100 | (code & 7));
            Ok(MemoryLayout {
                mod_bits,
                modrm_rm: code & 7,
                sib,
                disp,
                disp_len,
                rex_x: false,
                rex_b: code >= 8,
            })
        }
        (Some(base), Some(index)) => {
            let base_code = physical_code(base);
            let index_code = physical_code(index);
            let (mod_bits, disp_len) = base_disp(base, mem.disp);
            Ok(MemoryLayout {
                mod_bits,
                modrm_rm: 0b100,
                sib: Some((mem.scale.shift() << 6) | ((index_code & 7) << 3) | (base_code & 7)),
                disp,
                disp_len,
                rex_x: index_code >= 8,
                rex_b: base_code >= 8,
            })
        }
    }
}

/// 基址寻址的 ModRM.mod 与位移宽度：`rbp`/`r13` 与虚拟基址的零位移必须写 disp8。
fn base_disp(base: Reg, disp: i32) -> (u8, u8) {
    let zero_needs_disp8 = matches!(base, Reg::Gpr(Gpr::Rbp | Gpr::R13) | Reg::Virtual(_));
    if disp == 0 && !zero_needs_disp8 {
        (0b00, 0)
    } else if i8::try_from(disp).is_ok() {
        (0b01, 1)
    } else {
        (0b10, 4)
    }
}

/// 表内 form 的一致性自检：操作数与访问语义等长，`+r` 形式的首操作数是寄存器，
/// 带立即数的形式末操作数是立即数。
///
/// 由 `EncoderContract::verify` 在每次构建契约时调用：表本身的一致性属于编码规则的一部分。
pub(crate) fn form_shapes_are_consistent() -> bool {
    table::FORMS.iter().all(|form: &Form| {
        form.operands.len() == form.access.len()
            && (!form.reg_in_opcode
                || matches!(
                    form.operands.first(),
                    Some(OperandKind::R8 | OperandKind::R16 | OperandKind::R32 | OperandKind::R64)
                ))
            && (form.imm == ImmKind::None
                || matches!(
                    form.operands.last(),
                    Some(
                        OperandKind::Imm8
                            | OperandKind::Imm16
                            | OperandKind::Imm32
                            | OperandKind::Imm64
                    )
                ))
            // 固定 ModRM 只服务无操作数形式（`mfence` 一族）；有操作数时 ModRM 由操作数派生。
            && (form.fixed_modrm.is_none() || form.operands.is_empty())
    })
}
