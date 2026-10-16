//! 统一 unwind 记录与目标展开表。
//!
//! 每个函数一条 `UnwindFunction`，落地记录按 `pc_start` 不重叠。Linux 尾部是
//! DWARF CFI CIE/FDE（augmentation `zR`，地址按 udata8），FDE 的 LSDA 指向落地
//! 表；Windows 尾部是 `RUNTIME_FUNCTION` 与 `UNWIND_INFO`。两边都从同一组记录生成。

use crate::target::TargetName;

const HEADER_BYTES: usize = 48;

/// 有 frame。
const FLAG_FRAME: u16 = 1;
/// 入口容量检查。
const FLAG_ENTRY: u16 = 1 << 1;
/// 存在落地记录。
const FLAG_LANDING: u16 = 1 << 2;

/// 一个函数的展开记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnwindFunction {
    pub(crate) code_rva: u64,
    pub(crate) code_size: u32,
    pub(crate) frame_size: u32,
    pub(crate) saved_gpr_mask: u16,
    pub(crate) landing_start: u32,
    pub(crate) landing_count: u16,
    pub(crate) flags: u16,
}

impl UnwindFunction {
    pub(crate) fn new(
        code_rva: u64,
        code_size: u32,
        frame_size: u32,
        saved: &[(u8, u32)],
        landing_start: u32,
        landing_count: u16,
        checked: bool,
    ) -> Self {
        let mut flags = 0;
        if frame_size != 0 {
            flags |= FLAG_FRAME;
        }
        if checked {
            flags |= FLAG_ENTRY;
        }
        if landing_count != 0 {
            flags |= FLAG_LANDING;
        }
        Self {
            code_rva,
            code_size,
            frame_size,
            saved_gpr_mask: saved_mask(saved),
            landing_start,
            landing_count,
            flags,
        }
    }
}

/// 一条落地记录。`cleanup == u32::MAX` 表示只恢复传播。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Landing {
    pub(crate) pc_start: u32,
    pub(crate) pc_end: u32,
    pub(crate) landing_pc: u32,
    pub(crate) cleanup: u32,
}

/// 编码统一记录与当前目标的平台展开表。
pub(crate) fn encode(
    target: TargetName,
    functions: &[UnwindFunction],
    landings: &[Landing],
    saves: &[Vec<(u8, u32)>],
) -> Result<Vec<u8>, String> {
    verify_records(functions, landings)?;
    let mut canonical = Vec::new();
    for function in functions {
        canonical.extend_from_slice(&function.code_rva.to_le_bytes());
        canonical.extend_from_slice(&function.code_size.to_le_bytes());
        canonical.extend_from_slice(&function.frame_size.to_le_bytes());
        canonical.extend_from_slice(&function.saved_gpr_mask.to_le_bytes());
        canonical.extend_from_slice(&function.landing_start.to_le_bytes());
        canonical.extend_from_slice(&function.landing_count.to_le_bytes());
        canonical.extend_from_slice(&function.flags.to_le_bytes());
        canonical.extend_from_slice(&[0_u8; 6]);
    }
    for landing in landings {
        canonical.extend_from_slice(&landing.pc_start.to_le_bytes());
        canonical.extend_from_slice(&landing.pc_end.to_le_bytes());
        canonical.extend_from_slice(&landing.landing_pc.to_le_bytes());
        canonical.extend_from_slice(&landing.cleanup.to_le_bytes());
    }
    let platform = match target {
        TargetName::X86_64Linux => encode_eh_frame(functions, landings)?,
        TargetName::X86_64Windows => encode_windows(functions, landings, saves)?,
    };
    let mut output = vec![0_u8; HEADER_BYTES];
    output[..8].copy_from_slice(b"GUGUUN01");
    output[8..10].copy_from_slice(&1_u16.to_le_bytes());
    output[10] = u8::from(matches!(target, TargetName::X86_64Windows));
    output[12..16].copy_from_slice(&(functions.len() as u32).to_le_bytes());
    output[16..20].copy_from_slice(&(landings.len() as u32).to_le_bytes());
    let canonical_offset = HEADER_BYTES as u64;
    let platform_offset = canonical_offset + canonical.len() as u64;
    output[20..28].copy_from_slice(&canonical_offset.to_le_bytes());
    output[28..36].copy_from_slice(&platform_offset.to_le_bytes());
    let section_len = platform_offset + platform.len() as u64;
    output[36..44].copy_from_slice(&section_len.to_le_bytes());
    output.extend_from_slice(&canonical);
    output.extend_from_slice(&platform);
    verify_encoded(&output, functions.len(), landings.len())?;
    Ok(output)
}

/// 展开记录的代码范围不重叠，落地范围在函数内且互不重叠。
pub(crate) fn verify_records(
    functions: &[UnwindFunction],
    landings: &[Landing],
) -> Result<(), String> {
    for pair in functions.windows(2) {
        if pair[0].code_rva >= pair[1].code_rva {
            return Err("展开函数没有按 code_rva 严格递增".to_owned());
        }
        let end = pair[0]
            .code_rva
            .checked_add(u64::from(pair[0].code_size))
            .ok_or_else(|| "展开函数代码范围溢出".to_owned())?;
        if pair[1].code_rva < end {
            return Err("展开函数代码范围重叠".to_owned());
        }
    }
    for function in functions {
        let start = function.landing_start as usize;
        let count = function.landing_count as usize;
        let end = start
            .checked_add(count)
            .ok_or_else(|| "落地范围溢出".to_owned())?;
        if end > landings.len() {
            return Err("落地范围越出展开表".to_owned());
        }
        for pair in landings[start..end].windows(2) {
            if pair[0].pc_start >= pair[1].pc_start || pair[0].pc_end > pair[1].pc_start {
                return Err("落地范围重叠或没有按起点递增".to_owned());
            }
        }
        for landing in &landings[start..end] {
            if landing.pc_start >= landing.pc_end || landing.pc_end > function.code_size {
                return Err("落地范围越出函数代码".to_owned());
            }
            if landing.landing_pc >= function.code_size {
                return Err("落地 PC 越出函数代码".to_owned());
            }
        }
    }
    Ok(())
}

fn verify_encoded(bytes: &[u8], functions: usize, landings: usize) -> Result<(), String> {
    if bytes.len() < HEADER_BYTES || &bytes[..8] != b"GUGUUN01" {
        return Err("展开 section 魔数不匹配".to_owned());
    }
    let function_count = u32::from_le_bytes(bytes[12..16].try_into().expect("计数字段"));
    let landing_count = u32::from_le_bytes(bytes[16..20].try_into().expect("计数字段"));
    if function_count as usize != functions || landing_count as usize != landings {
        return Err("展开 section 计数与记录不一致".to_owned());
    }
    let section_len = u64::from_le_bytes(bytes[36..44].try_into().expect("长度字段"));
    if section_len as usize != bytes.len() {
        return Err("展开 section 长度不一致".to_owned());
    }
    Ok(())
}

fn saved_mask(saves: &[(u8, u32)]) -> u16 {
    let mut mask = 0_u16;
    for (code, _) in saves {
        if let Some(bit) = stackmap_bit(*code) {
            mask |= bit;
        }
    }
    mask
}

/// 机器编码到栈图位号。`rsp` 没有根位。
fn stackmap_bit(code: u8) -> Option<u16> {
    let bit = match code {
        0 => 0,  // rax
        1 => 2,  // rcx
        2 => 3,  // rdx
        3 => 1,  // rbx
        5 => 6,  // rbp
        6 => 4,  // rsi
        7 => 5,  // rdi
        8 => 7,  // r8
        9 => 8,  // r9
        10 => 9, // r10
        11 => 10,
        12 => 11,
        13 => 12,
        14 => 13,
        15 => 14,
        _ => return None,
    };
    Some(1 << bit)
}

fn encode_eh_frame(functions: &[UnwindFunction], landings: &[Landing]) -> Result<Vec<u8>, String> {
    let mut cie = vec![0_u8; 4];
    cie.extend_from_slice(&0_u32.to_le_bytes());
    cie.push(1);
    cie.extend_from_slice(b"zR\0");
    cie.push(1);
    cie.push(0x78);
    cie.push(16);
    cie.push(1);
    cie.push(0x04);
    let cie_length = u32::try_from(cie.len() - 4).expect("CIE 长度");
    cie[..4].copy_from_slice(&cie_length.to_le_bytes());
    let mut output = cie;
    let mut lsda = Vec::new();
    for function in functions {
        let start = function.landing_start as usize;
        let count = function.landing_count as usize;
        let lsda_at = lsda.len() as u64;
        lsda.extend_from_slice(&(count as u32).to_le_bytes());
        for landing in &landings[start..start + count] {
            lsda.extend_from_slice(&landing.pc_start.to_le_bytes());
            lsda.extend_from_slice(&landing.pc_end.to_le_bytes());
            lsda.extend_from_slice(&landing.landing_pc.to_le_bytes());
            lsda.extend_from_slice(&landing.cleanup.to_le_bytes());
        }
        let fde_at = output.len();
        output.extend_from_slice(&0_u32.to_le_bytes());
        let pointer_at = output.len();
        output.extend_from_slice(&0_u32.to_le_bytes());
        let pointer = u32::try_from(pointer_at).expect("FDE 偏移");
        output[pointer_at..pointer_at + 4].copy_from_slice(&pointer.to_le_bytes());
        output.extend_from_slice(&function.code_rva.to_le_bytes());
        output.extend_from_slice(&u64::from(function.code_size).to_le_bytes());
        output.push(8);
        output.extend_from_slice(&lsda_at.to_le_bytes());
        let length = u32::try_from(output.len() - fde_at - 4).expect("FDE 长度");
        output[fde_at..fde_at + 4].copy_from_slice(&length.to_le_bytes());
    }
    output.extend_from_slice(&lsda);
    let _ = functions.len();
    Ok(output)
}

fn encode_windows(
    functions: &[UnwindFunction],
    landings: &[Landing],
    saves: &[Vec<(u8, u32)>],
) -> Result<Vec<u8>, String> {
    let mut pdata = Vec::new();
    let mut xdata = Vec::new();
    for (index, function) in functions.iter().enumerate() {
        let info_at = u32::try_from(xdata.len()).map_err(|_| "展开信息偏移溢出".to_owned())?;
        let begin =
            u32::try_from(function.code_rva).map_err(|_| "代码 RVA 超过 PE 32 位".to_owned())?;
        pdata.extend_from_slice(&begin.to_le_bytes());
        let end = function
            .code_rva
            .checked_add(u64::from(function.code_size))
            .ok_or_else(|| "展开函数代码范围溢出".to_owned())?;
        let end = u32::try_from(end).map_err(|_| "代码结束 RVA 超过 PE 32 位".to_owned())?;
        pdata.extend_from_slice(&end.to_le_bytes());
        pdata.extend_from_slice(&info_at.to_le_bytes());
        xdata.extend(unwind_info(function, &saves[index])?);
        let start = function.landing_start as usize;
        let count = function.landing_count as usize;
        for landing in &landings[start..start + count] {
            xdata.extend_from_slice(&landing.pc_start.to_le_bytes());
            xdata.extend_from_slice(&landing.pc_end.to_le_bytes());
            xdata.extend_from_slice(&landing.landing_pc.to_le_bytes());
            xdata.extend_from_slice(&landing.cleanup.to_le_bytes());
        }
    }
    let mut output = Vec::new();
    output.extend_from_slice(&(functions.len() as u32).to_le_bytes());
    output.extend_from_slice(&pdata);
    output.extend_from_slice(&xdata);
    Ok(output)
}

fn unwind_info(function: &UnwindFunction, saves: &[(u8, u32)]) -> Result<Vec<u8>, String> {
    let mut codes = Vec::new();
    if function.frame_size > 0 {
        push_alloc(&mut codes, function.frame_size, saves.len() as u8 + 1)?;
    }
    for (index, (register, offset)) in saves.iter().enumerate() {
        let prolog = u8::try_from(saves.len() - index).unwrap_or(1);
        push_save(&mut codes, prolog, *register, *offset)?;
    }
    codes.sort_by(|left, right| right[0].cmp(&left[0]));
    let nodes = codes.iter().map(Vec::len).sum::<usize>() / 2;
    if nodes > 255 {
        return Err("UNWIND_INFO 代码数超过 255".to_owned());
    }
    let flags = u8::from(function.landing_count != 0);
    let mut info = vec![
        (flags << 3) | 1,
        codes.iter().map(|code| code[0]).max().unwrap_or(0),
        nodes as u8,
        0,
    ];
    for code in &codes {
        info.extend_from_slice(code);
    }
    if !info.len().is_multiple_of(4) {
        info.resize(info.len().div_ceil(4) * 4, 0);
    }
    Ok(info)
}

fn push_alloc(codes: &mut Vec<Vec<u8>>, frame_size: u32, offset: u8) -> Result<(), String> {
    if frame_size % 8 != 0 {
        return Err("帧大小不能被 UNWIND_INFO 描述".to_owned());
    }
    let slots = frame_size / 8;
    if slots <= 16 {
        codes.push(vec![offset, ((slots - 1) << 4) as u8 | 2]);
        return Ok(());
    }
    if slots > u32::from(u16::MAX) {
        return Err("帧大小超过 UNWIND_INFO 上限".to_owned());
    }
    let mut code = vec![offset, 1];
    code.extend_from_slice(&(slots as u16).to_le_bytes());
    codes.push(code);
    Ok(())
}

fn push_save(codes: &mut Vec<Vec<u8>>, offset: u8, register: u8, slot: u32) -> Result<(), String> {
    if slot % 8 != 0 {
        return Err("保存槽偏移不是 8 的倍数".to_owned());
    }
    let scaled = slot / 8;
    if scaled > u32::from(u16::MAX) {
        return Err("保存槽偏移超过 UNWIND_INFO 上限".to_owned());
    }
    let mut code = vec![offset, (register << 4) | 4];
    code.extend_from_slice(&(scaled as u16).to_le_bytes());
    codes.push(code);
    Ok(())
}
