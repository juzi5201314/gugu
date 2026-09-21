//! 确定性 COFF 对象与静态库。成员时间戳为 0。

use std::collections::BTreeMap;

use super::super::elf::write_archive;

pub(super) fn staticlib(text: &[u8]) -> Vec<u8> {
    let mut members = BTreeMap::new();
    members.insert("gugu.obj".to_owned(), object(text));
    write_archive(&members)
}

fn object(text: &[u8]) -> Vec<u8> {
    let raw = align_up(text.len(), 4);
    let mut out = vec![0; 20 + 40 + raw];
    write_u16(&mut out, 0, 0x8664);
    write_u16(&mut out, 2, 1);
    write_u16(&mut out, 16, 0);
    let mut name = [0; 8];
    name[..5].copy_from_slice(b".text");
    out[20..28].copy_from_slice(&name);
    write_u32(&mut out, 28, text.len() as u32);
    write_u32(&mut out, 36, text.len() as u32);
    write_u32(&mut out, 40, 60);
    write_u32(&mut out, 56, 0x6000_0020);
    out[60..60 + text.len()].copy_from_slice(text);
    out
}

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

fn write_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
