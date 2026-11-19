//! 立即数字段的纯整数变换。

use super::{Error, error};

pub(crate) fn unsigned(value: i64, bits: u8, scale: u8) -> Result<u32, Error> {
    if value < 0 || value % (1i64 << scale) != 0 || (value >> scale) >= (1i64 << bits) {
        return Err(error("无符号立即数越界或未对齐"));
    }
    Ok((value >> scale) as u32)
}

pub(crate) fn signed(value: i64, bits: u8, scale: u8) -> Result<u32, Error> {
    let n = value >> scale;
    if value % (1i64 << scale) != 0 || n < -(1i64 << (bits - 1)) || n >= (1i64 << (bits - 1)) {
        return Err(error("有符号立即数越界或未对齐"));
    }
    Ok((n as u32) & ((1u32 << bits) - 1))
}

pub(crate) fn sign_extend(value: u32, bits: u8, scale: u8) -> i64 {
    (i64::from(value) << (64 - bits) >> (64 - bits)) << scale
}

fn rotate(value: u64, amount: u32, width: u32) -> u64 {
    let mask = u64::MAX >> (64 - width);
    if amount == 0 {
        value & mask
    } else {
        ((value >> amount) | (value << (width - amount))) & mask
    }
}

pub(crate) fn decode_logical(raw: u32, width: u8) -> Result<u64, Error> {
    let n = (raw >> 12) & 1;
    let imms = raw & 63;
    let immr = (raw >> 6) & 63;
    let selector = (n << 6) | ((!imms) & 63);
    if selector < 2 || (width == 32 && n != 0) {
        return Err(error("未分配的逻辑立即数"));
    }
    let len = 31 - selector.leading_zeros();
    let size = 1u32 << len;
    let s = imms & (size - 1);
    if s == size - 1 || size > u32::from(width) {
        return Err(error("未分配的逻辑立即数"));
    }
    let element = rotate(u64::MAX >> (63 - s), immr & (size - 1), size);
    let mut value = 0;
    for pos in (0..u32::from(width)).step_by(size as usize) {
        value |= element << pos;
    }
    Ok(value)
}

pub(crate) fn encode_logical(value: u64, width: u8) -> Result<u32, Error> {
    if value == 0 || (width == 32 && value >> 32 != 0) {
        return Err(error("不可编码的逻辑立即数"));
    }
    for len in 1..=if width == 64 { 6 } else { 5 } {
        let size = 1u32 << len;
        let mask = u64::MAX >> (64 - size);
        let element = value & mask;
        let mut repeated = 0;
        for pos in (0..u32::from(width)).step_by(size as usize) {
            repeated |= element << pos;
        }
        if repeated != value || element == mask || element == 0 {
            continue;
        }
        for rotation in 0..size {
            let run = rotate(element, (size - rotation) % size, size);
            if run & (run + 1) != 0 {
                continue;
            }
            let imms = ((!(size - 1) << 1) | (run.count_ones() - 1)) & 63;
            return Ok((u32::from(size == 64) << 12) | (rotation << 6) | imms);
        }
    }
    Err(error("不可编码的逻辑立即数"))
}
