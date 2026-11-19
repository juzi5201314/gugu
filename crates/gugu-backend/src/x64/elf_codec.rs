//! rt0 机器码、动态节与 SysV ar。布局与程序头留在 `elf`。

use super::{LinkRequest, ObjectSymbol};
use std::collections::BTreeMap;

/// rt0：从初始栈定位 envp，再从 auxv 取 `AT_PHDR`，加上参数块里的链接期 phdr 虚址得到 bias，
/// 按 `.data.rel.ro` 的相对重定位表回填，封闭 RELRO，调用入口并 `exit`。envp 写入载荷的
/// `boot_environment` 字段时保持 native 地址，不参与 image bias。
pub fn rt0_bytes(params_disp: i32, initialize_environment: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x48, 0x8d, 0x1d]);
    out.extend_from_slice(&params_disp.to_le_bytes());
    out.extend_from_slice(&[0x48, 0x89, 0xe6]);
    out.extend_from_slice(&[0x48, 0x8b, 0x0e]);
    // r9/r10 跨 auxv 与 relocation 循环保存 argc/argv，不借用循环计数 rcx。
    out.extend_from_slice(&[0x49, 0x89, 0xc9, 0x4c, 0x8d, 0x56, 0x08]);
    out.extend_from_slice(&[0x48, 0x8d, 0x74, 0xce, 0x10]);
    // envp 由 rt0 保存在 r8；这是 native 启动栈地址，不参与 image bias。
    out.extend_from_slice(&[0x49, 0x89, 0xf0]);
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
    // 把启动时捕获的 envp 写入载荷 boot_environment（字段偏移 120）。
    out.extend_from_slice(&[
        0x48, 0x8b, 0x53, 0x48, 0x48, 0x01, 0xc2, 0x4c, 0x89, 0x82, 0x78, 0x00, 0x00, 0x00,
    ]);
    out.extend_from_slice(&[
        0x4c, 0x89, 0x8a, 0x80, 0x00, 0x00, 0x00, 0x4c, 0x89, 0x92, 0x88, 0x00, 0x00, 0x00,
    ]);
    if initialize_environment {
        // reserved_14 是 environ 的 GOT 槽 RVA。只初始化 NULL，保留 loader/构造器修改。
        out.extend_from_slice(&[
            0x4c, 0x8b, 0x53, 0x70, // mov r10, [rbx+112]
            0x49, 0x01, 0xc2, // add r10, rax（加载 bias）
            0x4d, 0x8b, 0x12, // mov r10, [r10]（变量地址）
            0x49, 0x83, 0x3a, 0x00, // cmp qword [r10], 0
            0x75, 0x03, // jne 保留现值
            0x4d, 0x89, 0x02, // mov [r10], r8（启动 envp）
        ]);
    }
    // rt_sigaction(SIGPIPE, {SIG_IGN, 0, NULL, 0}, NULL, 8)，保留 rax 中的 image bias。
    out.extend_from_slice(&[
        0x50, 0x48, 0x83, 0xec, 0x20, 0x48, 0xc7, 0x04, 0x24, 1, 0, 0, 0, 0x48, 0xc7, 0x44, 0x24,
        8, 0, 0, 0, 0, 0x48, 0xc7, 0x44, 0x24, 16, 0, 0, 0, 0, 0x48, 0xc7, 0x44, 0x24, 24, 0, 0, 0,
        0, 0xbf, 13, 0, 0, 0, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 8, 0, 0, 0, 0xb8, 13, 0, 0,
        0, 0x0f, 0x05, 0x48, 0x83, 0xc4, 0x20, 0x58,
    ]);
    out.extend_from_slice(&[
        0x4c, 0x8b, 0x73, 0x30, 0x49, 0x01, 0xc6, 0x4c, 0x8b, 0x7b, 0x38, 0x49, 0x01, 0xc7,
    ]);
    // 绑定 rt0_start 时交给镜像内 runtime：内部 ABI 参数寄存器是 rax/rbx，因此先保存加载偏移，
    // 把 params 放进 rax、bias 放进 rbx，退出码取 eax；未绑定时保留直调 main 的过渡路径。
    out.extend_from_slice(&[0x4c, 0x8b, 0x5b, 0x40]);
    out.extend_from_slice(&[0x4d, 0x85, 0xdb]);
    out.push(0x74);
    let branch_at = out.len();
    out.push(0);
    out.extend_from_slice(&[0x49, 0x01, 0xc3]);
    out.extend_from_slice(&[0x48, 0x89, 0xc2]);
    out.extend_from_slice(&[0x48, 0x89, 0xd8]);
    out.extend_from_slice(&[0x48, 0x89, 0xd3]);
    out.extend_from_slice(&[0x41, 0xff, 0xd3]);
    out.extend_from_slice(&[0x89, 0xc7]);
    out.extend_from_slice(&[0xb8, 0x3c, 0x00, 0x00, 0x00]);
    out.extend_from_slice(&[0x0f, 0x05]);
    let legacy = out.len() - (branch_at + 1);
    out[branch_at] = legacy as u8;
    out.extend_from_slice(&[0x48, 0x8b, 0x7b, 0x28]);
    out.extend_from_slice(&[0x48, 0x01, 0xc7]);
    out.extend_from_slice(&[0xff, 0xd7]);
    out.extend_from_slice(&[0x31, 0xff]);
    out.extend_from_slice(&[0xb8, 0x3c, 0x00, 0x00, 0x00]);
    out.extend_from_slice(&[0x0f, 0x05]);
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
            aliases: Vec::new(),
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

/// 按未解析的 C 符号名精确抽取 SysV ar 成员。
pub fn extract_archive(archive: &[u8], needed: &[String]) -> Result<Vec<Vec<u8>>, String> {
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
pub fn pack_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    #[test]
    fn linux_startup_ignores_sigpipe_before_entering_runtime() {
        let ignore_sigpipe = [
            0x50, // push rax (image bias)
            0x48, 0x83, 0xec, 0x20, // sub rsp, 32
            0x48, 0xc7, 0x04, 0x24, 1, 0, 0, 0, // handler = SIG_IGN
            0x48, 0xc7, 0x44, 0x24, 8, 0, 0, 0, 0, // flags = 0
            0x48, 0xc7, 0x44, 0x24, 16, 0, 0, 0, 0, // restorer = NULL
            0x48, 0xc7, 0x44, 0x24, 24, 0, 0, 0, 0, // mask = 0
            0xbf, 13, 0, 0, 0, // edi = SIGPIPE
            0x48, 0x89, 0xe6, // rsi = &action
            0x31, 0xd2, // rdx = NULL (old action)
            0x41, 0xba, 8, 0, 0, 0, // r10d = sizeof(kernel_sigset_t)
            0xb8, 13, 0, 0, 0, 0x0f, 0x05, // rt_sigaction
            0x48, 0x83, 0xc4, 0x20, 0x58, // restore rsp and image bias
        ];
        for initialize_environment in [false, true] {
            let bytes = super::rt0_bytes(0, initialize_environment);
            let sites: Vec<_> = bytes
                .windows(ignore_sigpipe.len())
                .enumerate()
                .filter_map(|(at, window)| (window == ignore_sigpipe).then_some(at))
                .collect();
            assert_eq!(sites.len(), 1, "startup must install SIG_IGN exactly once");
            let runtime_entry = bytes
                .windows(4)
                .position(|window| window == [0x4c, 0x8b, 0x5b, 0x40])
                .unwrap();
            assert!(sites[0] + ignore_sigpipe.len() <= runtime_entry);
        }
    }
}
