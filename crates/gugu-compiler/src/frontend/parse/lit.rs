//! 整数与浮点字面量的任意精度解析：去掉前缀与 `_`，不依赖目标类型。

use super::super::ast::{AstRange, extend_range};
use super::Parser;

impl Parser<'_> {
    pub(super) fn parse_int_limbs(&mut self, text: &str) -> (u8, AstRange<u32>) {
        let (radix, digits) =
            if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                (16_u8, rest)
            } else if let Some(rest) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
                (2, rest)
            } else if let Some(rest) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
                (8, rest)
            } else {
                (10, text)
            };
        let mut limbs = vec![0_u32];
        for byte in digits.bytes() {
            if byte == b'_' {
                continue;
            }
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => 0,
            };
            mul_add(&mut limbs, u32::from(radix), u32::from(digit));
        }
        while limbs.len() > 1 && *limbs.last().unwrap() == 0 {
            limbs.pop();
        }
        (radix, extend_range(&mut self.arena.int_limbs, limbs))
    }

    pub(super) fn parse_float_parts(&self, text: &str) -> (super::super::intern::Symbol, i32) {
        let mut digits = String::with_capacity(text.len());
        let mut exp10 = 0_i32;
        let mut seen_dot = false;
        let mut after_exp = false;
        let mut exp_sign = 1_i32;
        let mut exp_digits = 0_i32;
        let mut exp_started = false;
        for byte in text.bytes() {
            match byte {
                b'_' => {}
                b'.' => seen_dot = true,
                b'e' | b'E' if !after_exp => {
                    after_exp = true;
                }
                b'+' | b'-' if after_exp && !exp_started => {
                    if byte == b'-' {
                        exp_sign = -1;
                    }
                }
                b'0'..=b'9' if after_exp => {
                    exp_started = true;
                    exp_digits = exp_digits
                        .saturating_mul(10)
                        .saturating_add(i32::from(byte - b'0'));
                }
                b'0'..=b'9' => {
                    digits.push(byte as char);
                    if seen_dot {
                        exp10 -= 1;
                    }
                }
                _ => {}
            }
        }
        exp10 = exp10.saturating_add(exp_sign.saturating_mul(exp_digits));
        let _ = digits;
        (Self::interned_symbol(self.current()), exp10)
    }
}

fn mul_add(limbs: &mut Vec<u32>, radix: u32, digit: u32) {
    let mut carry = u64::from(digit);
    for limb in limbs.iter_mut() {
        let value = u64::from(*limb) * u64::from(radix) + carry;
        *limb = value as u32;
        carry = value >> 32;
    }
    if carry != 0 {
        limbs.push(carry as u32);
    }
}
