//! rt0 机器码、动态节与 SysV ar。布局与程序头留在 `elf`。

use super::{DT_NEEDED, DT_NULL, DT_STRSZ, DT_STRTAB, LinkRequest, ObjectSymbol, put_u64};
use std::collections::BTreeMap;

/// rt0：从 auxv 取 `AT_PHDR`，加上参数块里的链接期 phdr 虚址得到 bias，
/// 再按 `.data.rel.ro` 的相对重定位表回填，封闭 RELRO，调用入口并 `exit`。
pub(super) fn rt0_bytes(params_disp: i32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x48, 0x8d, 0x1d]);
    out.extend_from_slice(&params_disp.to_le_bytes());
    out.extend_from_slice(&[0x48, 0x89, 0xe6]);
    out.extend_from_slice(&[0x48, 0x8b, 0x0e]);
    out.extend_from_slice(&[0x48, 0x8d, 0x74, 0xce, 0x10]);
    let env = out.len();
    out.extend_from_slice(&[
        0x48, 0x8b, 0x06, 0x48, 0x83, 0xc6, 0x08, 0x48, 0x85, 0xc0, 0x75,
    ]);
    let env_rel = env as i32 - (out.len() as i32 + 1);
    out.push(env_rel as u8);
    let aux = out.len();
    out.extend_from_slice(&[
        0x48, 0x8b, 0x06, 0x48, 0x8b, 0x56, 0x08, 0x48, 0x83, 0xc6, 0x10, 0x83, 0xf8, 0x03, 0x74,
        0x07, 0x48, 0x85, 0xc0, 0x75,
    ]);
    let aux_back = aux as i32 - (out.len() as i32 + 1);
    out.push(aux_back as u8);
    out.extend_from_slice(&[0x0f, 0x0b]);
    // AT_PHDR 的值在 rdx；rax 此时仍是类型码 3。
    out.extend_from_slice(&[0x48, 0x89, 0xd0]);
    out.extend_from_slice(&[0x48, 0x2b, 0x03]);
    out.extend_from_slice(&[0x48, 0x8b, 0x7b, 0x08, 0x48, 0x01, 0xc7]);
    out.extend_from_slice(&[0x48, 0x8b, 0x4b, 0x10]);
    out.push(0xe3);
    let skip_at = out.len();
    out.push(0);
    let loop_at = out.len();
    out.extend_from_slice(&[
        0x48, 0x8b, 0x37, 0x48, 0x8b, 0x57, 0x08, 0x48, 0x01, 0xc6, 0x48, 0x01, 0xc2, 0x48, 0x89,
        0x16, 0x48, 0x83, 0xc7, 0x10, 0x48, 0xff, 0xc9, 0x75,
    ]);
    let loop_back = loop_at as i32 - (out.len() as i32 + 1);
    out.push(loop_back as u8);
    let skip = out.len() as i32 - (skip_at as i32 + 1);
    out[skip_at] = skip as u8;
    out.extend_from_slice(&[
        0x48, 0x8b, 0x7b, 0x18, 0x48, 0x01, 0xc7, 0x48, 0x8b, 0x73, 0x20, 0xba, 0x01, 0x00, 0x00,
        0x00, 0x50, 0xb8, 0x0a, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x58, 0x4c, 0x8b, 0x73, 0x30, 0x49,
        0x01, 0xc6, 0x4c, 0x8b, 0x7b, 0x38, 0x49, 0x01, 0xc7, 0x48, 0x8b, 0x7b, 0x28, 0x48, 0x01,
        0xc7, 0xff, 0xd7, 0x31, 0xff, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0x0f, 0x05,
    ]);
    out
}

pub(super) fn absorb_archive(request: &mut LinkRequest) -> Result<(), String> {
    let packed = pack_archive(&[]);
    extract_archive(&packed, &[])?;
    if request.archive.is_empty() {
        return Ok(());
    }
    let needed = unresolved_c_names(request);
    if needed.is_empty() {
        return Ok(());
    }
    let members = extract_archive(&request.archive, &needed)?;
    for (name, bytes) in needed.into_iter().zip(members) {
        request.symbols.push(ObjectSymbol {
            name,
            bytes,
            relocs: Vec::new(),
        });
    }
    Ok(())
}

fn unresolved_c_names(request: &LinkRequest) -> Vec<String> {
    let mut names = Vec::new();
    for symbol in &request.symbols {
        for reloc in &symbol.relocs {
            if names.contains(&reloc.target) || reloc.target.starts_with("__gugu_") {
                continue;
            }
            if request.symbols.iter().any(|item| item.name == reloc.target) {
                continue;
            }
            names.push(reloc.target.clone());
        }
    }
    names
}

pub(super) fn dynamic_blobs(request: &LinkRequest) -> (Vec<u8>, Vec<u8>) {
    let mut dynstr = Vec::new();
    let mut offsets = BTreeMap::new();
    for (soname, _) in &request.imports {
        if offsets.contains_key(soname) {
            continue;
        }
        let at = dynstr.len() as u64;
        dynstr.extend(soname.as_bytes());
        dynstr.push(0);
        offsets.insert(soname.clone(), at);
    }
    let mut dynamic = Vec::new();
    push_dyn(&mut dynamic, DT_STRTAB, 0);
    push_dyn(&mut dynamic, DT_STRSZ, dynstr.len() as u64);
    for offset in offsets.values() {
        push_dyn(&mut dynamic, DT_NEEDED, *offset);
    }
    push_dyn(&mut dynamic, DT_NULL, 0);
    (dynstr, dynamic)
}

fn push_dyn(out: &mut Vec<u8>, tag: i64, value: u64) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn patch_dynamic(
    file: &mut [u8],
    defined: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let (Some(dynamic), Some(dynstr)) = (defined.get(".dynamic"), defined.get(".dynstr")) else {
        return Ok(());
    };
    let at = usize::try_from(*dynamic).map_err(|_| "动态节越界".to_owned())?;
    put_u64(file, at + 8, *dynstr)
}

/// 按未解析的 C 符号名精确抽取 SysV ar 成员。
pub(crate) fn extract_archive(archive: &[u8], needed: &[String]) -> Result<Vec<Vec<u8>>, String> {
    if !archive.starts_with(b"!<arch>\n") {
        return Err("不是 SysV ar".to_owned());
    }
    let mut members = BTreeMap::new();
    let mut cursor = 8_usize;
    while cursor + 60 <= archive.len() {
        let header = &archive[cursor..cursor + 60];
        if &header[58..60] != b"`\n" {
            return Err("ar 头魔数不符".to_owned());
        }
        let name = std::str::from_utf8(&header[..16])
            .map_err(|_| "ar 成员名不是 UTF-8".to_owned())?
            .trim_end_matches([' ', '/'])
            .to_owned();
        let size: usize = std::str::from_utf8(&header[48..58])
            .map_err(|_| "ar 长度不是 UTF-8".to_owned())?
            .trim()
            .parse()
            .map_err(|_| "ar 长度非法".to_owned())?;
        let body = cursor + 60;
        let end = body
            .checked_add(size)
            .ok_or_else(|| "ar 成员越界".to_owned())?;
        if end > archive.len() {
            return Err("ar 成员越界".to_owned());
        }
        members.insert(name, archive[body..end].to_vec());
        cursor = end + (size & 1);
    }
    let mut out = Vec::new();
    for name in needed {
        let bytes = members
            .get(name)
            .ok_or_else(|| format!("归档缺少符号 {name}"))?;
        out.push(bytes.clone());
    }
    Ok(out)
}

/// 写出确定性的 SysV ar：时间戳、uid、gid 为 0。
pub(crate) fn pack_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, bytes) in members {
        let mut header = [b' '; 60];
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(16);
        header[..name_len].copy_from_slice(&name_bytes[..name_len]);
        header[16..28].copy_from_slice(b"0           ");
        header[28..34].copy_from_slice(b"0     ");
        header[34..40].copy_from_slice(b"0     ");
        header[40..48].copy_from_slice(b"100644  ");
        let size = format!("{:<10}", bytes.len());
        header[48..58].copy_from_slice(size.as_bytes());
        header[58..60].copy_from_slice(b"`\n");
        out.extend_from_slice(&header);
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}
