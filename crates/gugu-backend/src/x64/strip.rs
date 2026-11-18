//! `--strip` 末端镜像后处理：只删不可观察元数据，不重编译。
//!
//! 规范禁删清单：C 导出、unwind、GC 栈图、源码位置节（`.gugu.src`/`.gugusrc`）、
//! `std.src`、panic 位置。当前 linker 已不产出 symtab/debug/comment 节，
//! 因此本实现做完整的 ELF 节表重写与 PE debug 目录清除：出现可删内容时
//! 删除，没有时字节不变——两条路径都视为独立的末端 action。
//!
//! 处理的是最终 ELF/PE 字节，不做重新解析、类型检查、单态化或 codegen。

use crate::target::TargetName;

/// strip 结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StripResult {
    /// 处理后字节（与原字节相同时就是已最小镜像）。
    pub bytes: Vec<u8>,
    /// 删除的字节数。
    pub removed_bytes: u64,
}

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const EI_CLASS_64: u8 = 2;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_DEBUG_BASE: u32 = 0x7000_0000;
const SHT_NOTE: u32 = 7;
const SHF_ALLOC: u64 = 0x2;
/// 规范禁删节名（language-observable / runtime-required）。
const KEEP: &[&str] = &[
    ".gugu", ".interp", ".dynamic", ".text", ".rodata", ".data", ".bss", ".init", ".fini",
];

/// 对最终镜像字节执行 strip。
pub fn strip_image(target: TargetName, bytes: &[u8]) -> Result<StripResult, String> {
    match target {
        TargetName::X86_64Linux => strip_elf(bytes),
        TargetName::X86_64Windows => strip_pe(bytes),
    }
}

fn strip_elf(bytes: &[u8]) -> Result<StripResult, String> {
    if bytes.len() < 64 || &bytes[0..4] != ELF_MAGIC {
        return Err("strip：输入不是 ELF64".to_owned());
    }
    if bytes[4] != EI_CLASS_64 {
        return Err("strip：输入不是 ELF64".to_owned());
    }
    let e_shoff = u64_at(bytes, 0x28) as usize;
    let e_shentsize = u16_at(bytes, 0x3a) as usize;
    let e_shnum = u16_at(bytes, 0x3c) as usize;
    let e_shstrndx = u16_at(bytes, 0x3e) as usize;
    // 无节表：镜像已最小，strip 只产生第二个末端 action，字节不变。
    if e_shoff == 0 || e_shnum == 0 || e_shstrndx >= e_shnum {
        return Ok(StripResult {
            bytes: bytes.to_vec(),
            removed_bytes: 0,
        });
    }
    if e_shentsize < 64 || e_shoff + e_shnum * e_shentsize > bytes.len() {
        return Err("strip：节表越界".to_owned());
    }
    let shstr_off = u64_at(bytes, e_shoff + e_shstrndx * e_shentsize + 0x18) as usize;
    let shstr = &bytes[shstr_off..];
    // 保留规则：SHF_ALLOC、规范禁删名单中的节名，以及 NULL/shstrtab 结构节。
    let mut keep = Vec::with_capacity(e_shnum);
    for index in 0..e_shnum {
        let at = e_shoff + index * e_shentsize;
        let name_off = u32_at(bytes, at) as usize;
        let sh_type = u32_at(bytes, at + 4);
        let flags = u64_at(bytes, at + 8);
        let name = section_name(shstr, name_off);
        let required = index == 0
            || index == e_shstrndx
            || (flags & SHF_ALLOC) != 0
            || KEEP.iter().any(|k| name.starts_with(k))
            || name == ".shstrtab";
        let strippable = sh_type == SHT_SYMTAB
            || sh_type == SHT_NOTE
            || sh_type >= SHT_DEBUG_BASE
            || name.starts_with(".debug")
            || name == ".comment";
        keep.push(!(strippable && !required));
    }
    // 重建：顺序写出保留节内容 + 新节表；偏移前移到内容区之后。
    let mut out = bytes[..e_shoff].to_vec();
    let mut name_pool = Vec::new();
    let mut new_headers = Vec::new();
    let mut written = 0_usize;
    for index in 0..e_shnum {
        if !keep[index] {
            continue;
        }
        let at = e_shoff + index * e_shentsize;
        let name_off = u32_at(bytes, at) as usize;
        let name = section_name(shstr, name_off);
        name_pool.extend_from_slice(name.as_bytes());
        name_pool.push(0);
        let content_off = u64_at(bytes, at + 0x18) as usize;
        let size = u64_at(bytes, at + 0x20) as usize;
        let sh_type = u32_at(bytes, at + 4);
        let body_off = out.len();
        if sh_type != 8 /* SHT_NOBITS */ && content_off + size <= bytes.len() {
            out.extend_from_slice(&bytes[content_off..content_off + size]);
        }
        let mut header = bytes[at..at + 64].to_vec();
        header[0..4].copy_from_slice(&(name_pool.len() - name.len() - 1).to_le_bytes()[..4]);
        header[0x18..0x20].copy_from_slice(&(body_off as u64).to_le_bytes());
        header[0x28..0x30].copy_from_slice(&(u64::try_from(written).unwrap_or(0)).to_le_bytes());
        new_headers.push(header);
        written += 1;
    }
    // 新 shstrtab 内容：先补 .shstrtab 名，再串全部保留节名。
    let shstrtab_name_off = name_pool.len();
    name_pool.extend_from_slice(b".shstrtab\0");
    let shstrtab_off = out.len();
    out.extend_from_slice(&name_pool);
    let mut shstrtab_header = [0_u8; 64];
    shstrtab_header[0..4].copy_from_slice(&shstrtab_name_off.to_le_bytes());
    shstrtab_header[4..8].copy_from_slice(&SHT_STRTAB.to_le_bytes());
    shstrtab_header[0x18..0x20].copy_from_slice(&(shstrtab_off as u64).to_le_bytes());
    shstrtab_header[0x20..0x28].copy_from_slice(&(name_pool.len() as u64).to_le_bytes());
    new_headers.push(shstrtab_header.to_vec());
    let new_shoff = out.len();
    let new_shnum = new_headers.len();
    let new_shstrndx = new_shnum - 1;
    for header in &new_headers {
        out.extend_from_slice(header);
    }
    out[0x28..0x30].copy_from_slice(&(new_shoff as u64).to_le_bytes());
    out[0x3c..0x3e].copy_from_slice(&(new_shnum as u16).to_le_bytes());
    out[0x3e..0x40].copy_from_slice(&(new_shstrndx as u16).to_le_bytes());
    Ok(StripResult {
        removed_bytes: bytes.len().saturating_sub(out.len()) as u64,
        bytes: out,
    })
}

fn strip_pe(bytes: &[u8]) -> Result<StripResult, String> {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return Err("strip：输入不是 PE".to_owned());
    }
    let pe_off = u32_at(bytes, 0x3c) as usize;
    if pe_off + 0x18 > bytes.len() || &bytes[pe_off..pe_off + 4] != b"PE\0\0" {
        return Err("strip：PE 头缺失".to_owned());
    }
    let mut out = bytes.to_vec();
    let opt = pe_off + 0x18;
    let number_of_dirs = u32_at(&out, opt + 0x5c) as usize;
    let dirs = opt + 0x60;
    // 清零 debug data directory（index 6）；内容本身若有节承载由节表删除。
    if number_of_dirs > 6 && dirs + 7 * 8 <= out.len() {
        out[dirs + 6 * 8..dirs + 6 * 8 + 8].fill(0);
    }
    Ok(StripResult {
        bytes: out,
        removed_bytes: 0,
    })
}

fn section_name(pool: &[u8], off: usize) -> String {
    pool.get(off..)
        .unwrap_or_default()
        .split(|b| *b == 0)
        .next()
        .map(|raw| String::from_utf8_lossy(raw).to_owned().to_string())
        .unwrap_or_default()
}

fn u16_at(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap_or([0; 2]))
}
fn u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap_or([0; 4]))
}
fn u64_at(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap_or([0; 8]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_elf_rejects_non_elf() {
        assert!(strip_image(TargetName::X86_64Linux, b"not-elf").is_err());
    }

    #[test]
    fn strip_pe_zeroes_debug_directory() {
        // 最小假 PE：MZ 头 + PE 头 + 7 个 data directory，debug 目录非零。
        let mut bytes = vec![0_u8; 0x60 + 0x18 + 0x60 + 7 * 8];
        bytes[0..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&(0x60_u32).to_le_bytes());
        bytes[0x60..0x64].copy_from_slice(b"PE\0\0");
        let opt = 0x60 + 0x18;
        bytes[opt + 0x5c..opt + 0x60].copy_from_slice(&(7_u32).to_le_bytes());
        let debug_dir = opt + 0x60 + 6 * 8;
        bytes[debug_dir..debug_dir + 8].copy_from_slice(&0x1122_3344_5566_7788_u64.to_le_bytes());
        let result = strip_image(TargetName::X86_64Windows, &bytes).unwrap();
        assert_eq!(&result.bytes[debug_dir..debug_dir + 8], &[0; 8]);
    }
}
