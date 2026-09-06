//! 整数与浮点字面量的任意精度解析：去掉前缀与 `_`，不依赖目标类型。

use super::super::ast::AstRange;
use super::super::string::scan_escape;
use super::{Parser, finish_extend};

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
        (
            radix,
            finish_extend(&mut self.diagnostics, &mut self.arena.int_limbs, limbs),
        )
    }

    pub(super) fn parse_float_parts(&mut self, text: &str) -> (super::super::intern::Symbol, i32) {
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
        let symbol = {
            let intern = &mut *self.intern;
            intern.intern_str(&digits)
        };
        (symbol, exp10)
    }

    pub(super) fn parse_char_value(&self, text: &str) -> char {
        let open = char_body_start(text);
        let close = text.len().saturating_sub(1);
        if open >= close {
            return '\0';
        }
        if text.as_bytes()[open] == b'\\' {
            return escaped_char(text, open, false);
        }
        text[open..close].chars().next().unwrap_or('\0')
    }

    pub(super) fn parse_byte_char_value(&self, text: &str) -> u8 {
        let open = char_body_start(text);
        let close = text.len().saturating_sub(1);
        if open >= close {
            return 0;
        }
        let ch = if text.as_bytes()[open] == b'\\' {
            escaped_char(text, open, true)
        } else {
            text[open..close].chars().next().unwrap_or('\0')
        };
        debug_assert!(
            u32::from(ch) <= u32::from(u8::MAX),
            "词法检查保证 byte 字符只有一个字节"
        );
        ch as u8
    }
}

fn char_body_start(text: &str) -> usize {
    if text.starts_with("b'") {
        2
    } else if text.starts_with('\'') {
        1
    } else {
        0
    }
}

fn escaped_char(text: &str, slash: usize, byte_char: bool) -> char {
    scan_escape(text, slash, byte_char, !byte_char)
        .expect("字符已通过词法转义校验")
        .value
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
