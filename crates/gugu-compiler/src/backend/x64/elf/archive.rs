//! 确定性 SysV archive。成员按符号名排序，时间戳、uid 与 gid 固定为 0。

use std::collections::BTreeMap;

use super::ElfError;

const MAGIC: &[u8] = b"!<arch>\n";
const HEADER: usize = 60;

#[allow(
    dead_code,
    reason = "staticlib 写出与抽取测试往返；可执行链接只抽取成员"
)]
pub(super) fn write_archive(members: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::from(MAGIC);
    for (name, bytes) in members {
        let mut header = [b' '; HEADER];
        let label = format!("{name}/");
        header[..label.len().min(16)].copy_from_slice(&label.as_bytes()[..label.len().min(16)]);
        write_field(&mut header[16..28], b"0");
        write_field(&mut header[28..34], b"0");
        write_field(&mut header[34..40], b"0");
        write_field(&mut header[40..48], b"644");
        write_field(&mut header[48..58], bytes.len().to_string().as_bytes());
        header[58] = b'`';
        header[59] = b'\n';
        out.extend_from_slice(&header);
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// 按未解析的 C 符号名精确抽取成员。同名成员保留先出现的一份并报告重复。
pub(super) fn extract(archive: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, ElfError> {
    if !archive.starts_with(MAGIC) {
        return Err(ElfError::new("静态归档缺少 SysV 魔数"));
    }
    let mut members = BTreeMap::new();
    let mut cursor = MAGIC.len();
    while cursor < archive.len() {
        if archive.len() - cursor < HEADER {
            return Err(ElfError::new("静态归档头被截断"));
        }
        let header = &archive[cursor..cursor + HEADER];
        if &header[58..60] != b"`\n" {
            return Err(ElfError::new("静态归档头结束标记错误"));
        }
        let size = parse_size(&header[48..58])?;
        let name = member_name(&header[..16])?;
        cursor += HEADER;
        let end = cursor
            .checked_add(size)
            .ok_or_else(|| ElfError::new("静态归档成员越界"))?;
        if end > archive.len() {
            return Err(ElfError::new("静态归档成员越界"));
        }
        if members
            .insert(name.clone(), archive[cursor..end].to_vec())
            .is_some()
        {
            return Err(ElfError::new("静态归档成员重复"));
        }
        cursor = end + usize::from(size % 2 == 1);
    }
    Ok(members)
}

fn member_name(field: &[u8]) -> Result<String, ElfError> {
    let text = std::str::from_utf8(field).map_err(|_| ElfError::new("归档成员名不是 UTF-8"))?;
    let text = text.trim();
    let text = text.strip_suffix('/').unwrap_or(text);
    if text.is_empty() || text.starts_with('/') {
        return Err(ElfError::new("归档成员名不能作为 C 符号"));
    }
    Ok(text.to_owned())
}

fn parse_size(field: &[u8]) -> Result<usize, ElfError> {
    let text = std::str::from_utf8(field).map_err(|_| ElfError::new("归档长度不是十进制"))?;
    text.trim()
        .parse()
        .map_err(|_| ElfError::new("归档长度不是十进制"))
}

fn write_field(slot: &mut [u8], value: &[u8]) {
    slot[..value.len()].copy_from_slice(value);
}
