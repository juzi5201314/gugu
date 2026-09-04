use crate::diagnostics::DiagnosticCode;

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedEscape {
    pub(super) next: usize,
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
        utf8_len: ch.len_utf8() as u8,
        is_nul: ch == '\0',
    })
}

pub(super) fn format_spec_error(spec: &str) -> Option<&'static str> {
    let bytes = spec.as_bytes();
    let mut i = 0;
    if bytes.len() >= 2 && is_align(bytes[1]) {
        i = 2;
    } else if bytes.first().copied().is_some_and(is_align) {
        i = 1;
    }
    if bytes.get(i).copied().is_some_and(is_sign) {
        i += 1;
    }
    if bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    if bytes.get(i) == Some(&b'0') {
        i += 1;
    }
    let Some(next) = skip_width(bytes, i) else {
        return Some("未知格式码");
    };
    i = next;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let Some(next) = skip_precision(bytes, i) else {
            return Some("未知格式码");
        };
        i = next;
    }
    if let Some(&type_code) = bytes.get(i)
        && is_type_code(type_code)
    {
        i += 1;
    } else if bytes.get(i).is_some() {
        return Some("未知格式码");
    }
    if i == bytes.len() {
        None
    } else {
        Some("未知格式码")
    }
}

fn skip_width(bytes: &[u8], i: usize) -> Option<usize> {
    skip_count(bytes, i)
}

fn skip_precision(bytes: &[u8], i: usize) -> Option<usize> {
    skip_count(bytes, i)
}

fn skip_count(bytes: &[u8], mut i: usize) -> Option<usize> {
    if bytes.get(i).copied().is_some_and(|b| b.is_ascii_digit()) {
        while bytes.get(i).copied().is_some_and(|b| b.is_ascii_digit()) {
            i += 1;
        }
        return Some(i);
    }
    if bytes.get(i).copied().is_some_and(is_ident_start) {
        let mut end = i + 1;
        while bytes.get(end).copied().is_some_and(is_ident_continue) {
            end += 1;
        }
        if bytes.get(end) == Some(&b'$') {
            return Some(end + 1);
        }
        return Some(i);
    }
    Some(i)
}

const fn is_align(byte: u8) -> bool {
    matches!(byte, b'<' | b'^' | b'>')
}

const fn is_sign(byte: u8) -> bool {
    matches!(byte, b'+' | b'-' | b' ')
}

const fn is_type_code(byte: u8) -> bool {
    matches!(byte, b'?' | b'b' | b'o' | b'x' | b'X' | b'e' | b'E')
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
