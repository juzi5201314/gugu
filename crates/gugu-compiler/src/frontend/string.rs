mod format;
pub(super) use format::{FormatSpec, ParsedCount, format_spec_error, parse_format};

use crate::diagnostics::DiagnosticCode;

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedEscape {
    pub(super) next: usize,
    pub(super) value: char,
    pub(super) utf8_len: u8,
    pub(super) is_nul: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct EscapeError {
    pub(super) code: DiagnosticCode,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) message: &'static str,
}

/// 输入已经通过词法校验；没有实际转义的字面量直接借用正文。
pub(super) fn decode_string(text: &str) -> std::borrow::Cow<'_, str> {
    let raw = text.starts_with("raw");
    let (open, close) = if text.starts_with("raw\"\"\"") {
        (6, 3)
    } else if raw {
        (4, 1)
    } else {
        (1, 1)
    };
    let body = &text[open..text.len() - close];
    let Some(first) = body
        .as_bytes()
        .windows(2)
        .position(|pair| pair[0] == b'\\' && (!raw || matches!(pair[1], b'\\' | b'"')))
    else {
        return std::borrow::Cow::Borrowed(body);
    };
    let mut result = String::with_capacity(body.len());
    result.push_str(&body[..first]);
    let mut offset = first;
    while offset < body.len() {
        if body.as_bytes()[offset] == b'\\'
            && (!raw || matches!(body.as_bytes().get(offset + 1), Some(b'\\' | b'"')))
        {
            let decoded = scan_escape(body, offset, false, !raw).expect("字符串已通过词法转义校验");
            result.push(decoded.value);
            offset = decoded.next;
        } else {
            let ch = body[offset..].chars().next().expect("正文字符边界");
            result.push(ch);
            offset += ch.len_utf8();
        }
    }
    std::borrow::Cow::Owned(result)
}

/// b/c 字面量的十六进制转义贡献一个字节，Unicode 转义仍贡献 UTF-8 字节。
pub(super) fn decode_bytes(text: &str) -> Vec<u8> {
    let body = &text[2..text.len() - 1];
    let mut bytes = Vec::with_capacity(body.len());
    let mut offset = 0;
    while offset < body.len() {
        if body.as_bytes()[offset] == b'\\' {
            let decoded = scan_escape(body, offset, true, true).expect("字节字符串已通过词法检查");
            if body.as_bytes()[offset + 1] == b'x' {
                bytes.push(decoded.value as u8);
            } else {
                let mut encoded = [0; 4];
                bytes.extend_from_slice(decoded.value.encode_utf8(&mut encoded).as_bytes());
            }
            offset = decoded.next;
        } else {
            bytes.push(body.as_bytes()[offset]);
            offset += 1;
        }
    }
    bytes
}

pub(super) fn decode_fstring_text(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.bytes().any(|byte| matches!(byte, b'\\' | b'{' | b'}')) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        match text.as_bytes()[offset] {
            b'\\' => {
                let decoded =
                    scan_escape(text, offset, false, true).expect("插值文本已通过词法检查");
                result.push(decoded.value);
                offset = decoded.next;
            }
            b'{' | b'}' => {
                result.push(text.as_bytes()[offset] as char);
                offset += 2;
            }
            _ => {
                let character = text[offset..].chars().next().expect("插值文本字符边界");
                result.push(character);
                offset += character.len_utf8();
            }
        }
    }
    std::borrow::Cow::Owned(result)
}

pub(super) fn scan_escape(
    source: &str,
    slash: usize,
    allow_hex: bool,
    allow_unicode: bool,
) -> Result<DecodedEscape, EscapeError> {
    let bytes = source.as_bytes();
    let Some(kind) = bytes.get(slash + 1).copied() else {
        return Err(EscapeError {
            code: DiagnosticCode::LexUnterminated,
            start: slash,
            end: source.len(),
            message: "转义在文件末尾未完成",
        });
    };
    let simple = match kind {
        b'\\' => Some('\\'),
        b'"' => Some('"'),
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        b'0' => Some('\0'),
        b'\'' => Some('\''),
        _ => None,
    };
    if let Some(ch) = simple {
        return Ok(DecodedEscape {
            next: slash + 2,
            value: ch,
            utf8_len: ch.len_utf8() as u8,
            is_nul: ch == '\0',
        });
    }
    if kind == b'x' {
        return scan_hex_escape(source, slash, allow_hex);
    }
    if kind == b'u' {
        return scan_unicode_escape(source, slash, allow_unicode);
    }
    let end = slash + 1 + utf8_len_at(bytes, slash + 1);
    Err(EscapeError {
        code: DiagnosticCode::LexInvalidEscape,
        start: slash,
        end,
        message: "未知转义",
    })
}

fn scan_hex_escape(
    source: &str,
    slash: usize,
    allow_hex: bool,
) -> Result<DecodedEscape, EscapeError> {
    if !allow_hex {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidEscape,
            start: slash,
            end: slash + 2,
            message: "该字面量不允许 \\xHH 转义",
        });
    }
    let bytes = source.as_bytes();
    let hi = bytes.get(slash + 2).copied();
    let lo = bytes.get(slash + 3).copied();
    let (Some(hi), Some(lo)) = (hi, lo) else {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidEscape,
            start: slash,
            end: source.len().min(slash + 4),
            message: "\\x 转义必须恰好两位十六进制",
        });
    };
    let (Some(hi), Some(lo)) = (from_hex(hi), from_hex(lo)) else {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidEscape,
            start: slash,
            end: slash + 4,
            message: "\\x 转义必须恰好两位十六进制",
        });
    };
    let byte = (hi << 4) | lo;
    Ok(DecodedEscape {
        next: slash + 4,
        value: char::from(byte),
        utf8_len: 1,
        is_nul: byte == 0,
    })
}

fn scan_unicode_escape(
    source: &str,
    slash: usize,
    allow_unicode: bool,
) -> Result<DecodedEscape, EscapeError> {
    if !allow_unicode {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidEscape,
            start: slash,
            end: slash + 2,
            message: "该字面量不允许 \\u{HEX} 转义",
        });
    }
    let bytes = source.as_bytes();
    if bytes.get(slash + 2) != Some(&b'{') {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidEscape,
            start: slash,
            end: (slash + 3).min(source.len()),
            message: "Unicode 转义必须写成 \\u{HEX}",
        });
    }
    let mut offset = slash + 3;
    let digits_start = offset;
    while bytes.get(offset).copied().is_some_and(is_hex_digit) {
        offset += 1;
    }
    if bytes.get(offset) != Some(&b'}') {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidEscape,
            start: slash,
            end: offset.min(source.len()),
            message: "Unicode 转义必须写成 \\u{HEX}",
        });
    }
    let digits = &source[digits_start..offset];
    if digits.is_empty() || digits.len() > 6 {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidUnicodeScalar,
            start: slash,
            end: offset + 1,
            message: "非法 Unicode scalar",
        });
    }
    let value = u32::from_str_radix(digits, 16).expect("hex digits");
    let Some(ch) = char::from_u32(value) else {
        return Err(EscapeError {
            code: DiagnosticCode::LexInvalidUnicodeScalar,
            start: slash,
            end: offset + 1,
            message: "非法 Unicode scalar",
        });
    };
    Ok(DecodedEscape {
        next: offset + 1,
        value: ch,
        utf8_len: ch.len_utf8() as u8,
        is_nul: ch == '\0',
    })
}

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

const fn is_hex_digit(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn utf8_len_at(bytes: &[u8], offset: usize) -> usize {
    match bytes.get(offset) {
        Some(byte) if byte.is_ascii() => 1,
        Some(&byte) => byte.leading_ones() as usize,
        None => 0,
    }
}
