use crate::diagnostics::{Diagnostic, DiagnosticCode};
use crate::source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot};

use super::attr::validate_attributes;
use super::string::{EscapeError, format_spec_error, scan_escape};
use super::token::{Token, TokenBuffer, TokenKind, Trivia, TriviaKind, checked_u32, keyword_kind};

pub(crate) struct Lexed {
    pub(crate) buffer: TokenBuffer,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

pub(crate) fn lex(snapshot: &SourceSnapshot, source_map: &SourceMap, file: SourceFileId) -> Lexed {
    lex_in_expansion(snapshot, source_map, file, ExpansionId::ROOT)
}

/// 在指定源码宏展开上下文中执行词法分析；根源码使用 [`lex`]。
pub(crate) fn lex_in_expansion(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: SourceFileId,
    expansion: ExpansionId,
) -> Lexed {
    let mut lexer = Lexer::new(snapshot, source_map, file, expansion);
    lexer.scan_file();
    validate_attributes(
        lexer.source,
        lexer.source_map,
        &mut lexer.buffer,
        &mut lexer.diagnostics,
    );
    Lexed {
        buffer: lexer.buffer,
        diagnostics: lexer.diagnostics,
    }
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    source_map: &'a SourceMap,
    file: SourceFileId,
    expansion: ExpansionId,
    pos: usize,
    trivia_mark: u32,
    fstring_interp: u32,
    buffer: TokenBuffer,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(
        snapshot: &'a SourceSnapshot,
        source_map: &'a SourceMap,
        file: SourceFileId,
        expansion: ExpansionId,
    ) -> Self {
        let source = snapshot.content();
        let cap = source.len() / 2 + 8;
        Self {
            source,
            bytes: source.as_bytes(),
            source_map,
            file,
            expansion,
            pos: 0,
            trivia_mark: 0,
            fstring_interp: 0,
            buffer: TokenBuffer {
                file: Some(file),
                tokens: Vec::with_capacity(cap),
                trivia: Vec::with_capacity(cap / 4 + 4),
                intern: super::intern::SymbolInterner::default(),
            },
            diagnostics: Vec::new(),
        }
    }

    fn scan_file(&mut self) {
        while self.pos < self.bytes.len() {
            self.skip_trivia();
            if self.pos >= self.bytes.len() {
                break;
            }
            self.scan_token();
        }
        self.skip_trivia();
        self.push(TokenKind::Eof, self.pos, self.pos, None);
    }

    fn skip_trivia(&mut self) {
        self.trivia_mark = checked_u32(self.buffer.trivia.len());
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' => self.scan_whitespace(),
                b'\r' | b'\n' => self.scan_newline(),
                b'/' if self.bytes.get(self.pos + 1) == Some(&b'/') => self.scan_line_comment(),
                b'/' if self.bytes.get(self.pos + 1) == Some(&b'*') => self.scan_block_comment(),
                _ => break,
            }
        }
    }

    fn scan_whitespace(&mut self) {
        let start = self.pos;
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
        self.push_trivia(TriviaKind::Whitespace, start, self.pos);
    }

    fn scan_newline(&mut self) {
        let start = self.pos;
        if self.bytes[self.pos] == b'\r' {
            self.pos += 1;
            if self.bytes.get(self.pos) == Some(&b'\n') {
                self.pos += 1;
            }
        } else {
            self.pos += 1;
        }
        self.push_trivia(TriviaKind::Newline, start, self.pos);
    }

    fn scan_line_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let kind = match self.bytes.get(self.pos) {
            Some(b'/') => TriviaKind::DocComment,
            Some(b'!') => TriviaKind::InnerDocComment,
            _ => TriviaKind::LineComment,
        };
        while let Some(&byte) = self.bytes.get(self.pos)
            && byte != b'\n'
            && byte != b'\r'
        {
            self.pos += 1;
        }
        self.push_trivia(kind, start, self.pos);
    }

    fn scan_block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1_u32;
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'/' && self.bytes.get(self.pos + 1) == Some(&b'*') {
                depth += 1;
                self.pos += 2;
                continue;
            }
            if self.bytes[self.pos] == b'*' && self.bytes.get(self.pos + 1) == Some(&b'/') {
                self.pos += 2;
                depth -= 1;
                if depth == 0 {
                    self.push_trivia(TriviaKind::BlockComment, start, self.pos);
                    return;
                }
                continue;
            }
            self.pos += 1;
        }
        self.error(
            start,
            self.pos,
            DiagnosticCode::LexUnterminatedComment,
            "块注释未闭合",
        );
        self.push(TokenKind::Error, start, self.pos, None);
    }

    fn scan_token(&mut self) {
        let start = self.pos;
        let byte = self.bytes[start];
        if byte.is_ascii_alphabetic() || byte == b'_' {
            self.scan_ident_or_prefix();
            return;
        }
        if byte.is_ascii_digit() {
            self.scan_number();
            return;
        }
        match byte {
            b'"' => self.scan_quoted_string(start, TokenKind::String, false, true),
            b'\'' => self.scan_char_literal(start, false),
            b'#' => {
                self.pos += 1;
                self.push(TokenKind::Hash, start, self.pos, None);
            }
            _ => self.scan_punct_or_error(),
        }
    }

    fn scan_ident_or_prefix(&mut self) {
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.bytes.len() && is_ident_continue(self.bytes[self.pos]) {
            self.pos += 1;
        }
        let ident = &self.source[start..self.pos];
        if ident == "f" && self.bytes.get(self.pos) == Some(&b'"') {
            self.scan_fstring(start);
            return;
        }
        if ident == "b" && self.bytes.get(self.pos) == Some(&b'"') {
            let token_start = start;
            self.scan_quoted_string(token_start, TokenKind::ByteString, true, true);
            return;
        }
        if ident == "b" && self.bytes.get(self.pos) == Some(&b'\'') {
            self.scan_char_literal(start, true);
            return;
        }
        if ident == "c" && self.bytes.get(self.pos) == Some(&b'"') {
            self.scan_quoted_string(start, TokenKind::CString, true, true);
            return;
        }
        if ident == "raw" && self.bytes.get(self.pos) == Some(&b'"') {
            self.scan_raw_string(start);
            return;
        }
        if let Some(kind) = keyword_kind(ident) {
            self.push(kind, start, self.pos, None);
            return;
        }
        let symbol = self.buffer.intern.intern_str(ident);
        self.push(TokenKind::Ident, start, self.pos, Some(symbol));
    }

    fn scan_number(&mut self) {
        let start = self.pos;
        if self.bytes[start] == b'0' {
            match self.bytes.get(start + 1).copied() {
                Some(b'x') => return self.scan_radix_int(start, 16, "十六进制"),
                Some(b'b') => return self.scan_radix_int(start, 2, "二进制"),
                Some(b'o') => return self.scan_radix_int(start, 8, "八进制"),
                Some(b'0'..=b'9') => {
                    return self.invalid_number(start, start + 2, "禁止前导 0 表示八进制");
                }
                _ => {}
            }
        }
        self.scan_decimal(start);
    }

    fn scan_radix_int(&mut self, start: usize, radix: u32, name: &str) {
        self.pos = start + 2;
        if self.bytes.get(self.pos) == Some(&b'_') {
            self.invalid_number(start, self.pos + 1, "下划线不能出现在基数前缀之后");
            return;
        }
        if !self.consume_digits(radix) {
            self.invalid_number(start, self.pos.max(start + 2), &format!("{name}数字缺失"));
            return;
        }
        if self.bytes[self.pos - 1] == b'_' {
            self.invalid_number(start, self.pos, "下划线不能出现在数字记号末尾");
            return;
        }
        self.finish_number(start, TokenKind::Int);
    }

    fn scan_decimal(&mut self, start: usize) {
        self.consume_digits(10);
        if self.pos > start && self.bytes[self.pos - 1] == b'_' {
            self.invalid_number(start, self.pos, "下划线不能出现在数字记号末尾");
            return;
        }
        let float_dot = self.bytes.get(self.pos) == Some(&b'.')
            && self
                .bytes
                .get(self.pos + 1)
                .copied()
                .is_some_and(|byte| byte.is_ascii_digit());
        if float_dot {
            self.pos += 1;
            self.consume_digits(10);
            if self.bytes[self.pos - 1] == b'_' {
                self.invalid_number(start, self.pos, "下划线不能出现在数字记号末尾");
                return;
            }
            if matches!(self.bytes.get(self.pos), Some(b'e' | b'E'))
                && !self.consume_exponent(start)
            {
                return;
            }
            self.finish_number(start, TokenKind::Float);
            return;
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            if self.consume_exponent(start) {
                self.finish_number(start, TokenKind::Float);
            }
            return;
        }
        self.finish_number(start, TokenKind::Int);
    }

    fn consume_exponent(&mut self, start: usize) -> bool {
        self.pos += 1;
        if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        if !self.consume_digits(10) {
            self.invalid_number(start, self.pos, "指数缺少数字");
            return false;
        }
        if self.bytes[self.pos - 1] == b'_' {
            self.invalid_number(start, self.pos, "下划线不能出现在数字记号末尾");
            return false;
        }
        true
    }

    fn consume_digits(&mut self, radix: u32) -> bool {
        let mut saw = false;
        loop {
            match self.bytes.get(self.pos).copied() {
                Some(b'_') => {
                    let next = self.bytes.get(self.pos + 1).copied();
                    if next.is_none_or(|byte| digit_value(byte) >= radix) {
                        break;
                    }
                    self.pos += 1;
                }
                Some(byte) if digit_value(byte) < radix => {
                    saw = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        saw
    }

    fn finish_number(&mut self, start: usize, kind: TokenKind) {
        if self
            .bytes
            .get(self.pos)
            .copied()
            .is_some_and(is_ident_continue)
        {
            while self.pos < self.bytes.len() && is_ident_continue(self.bytes[self.pos]) {
                self.pos += 1;
            }
            self.invalid_number(start, self.pos, "数字记号后不能紧跟字母或数字");
            return;
        }
        let symbol = self.buffer.intern.intern_str(&self.source[start..self.pos]);
        self.push(kind, start, self.pos, Some(symbol));
    }

    fn invalid_number(&mut self, start: usize, end: usize, message: &str) {
        self.pos = end.max(self.pos);
        self.error(start, self.pos, DiagnosticCode::LexInvalidNumeric, message);
        self.push(TokenKind::Error, start, self.pos, None);
    }

    fn scan_punct_or_error(&mut self) {
        let start = self.pos;
        if let Some((kind, len)) = match_punct(&self.bytes[start..]) {
            self.pos += len;
            self.push(kind, start, self.pos, None);
            return;
        }
        let ch = self.source[start..].chars().next().expect("UTF-8");
        self.pos += ch.len_utf8();
        self.error(
            start,
            self.pos,
            DiagnosticCode::LexInvalidToken,
            "无法形成记号",
        );
        self.push(TokenKind::Error, start, self.pos, None);
    }
}

impl<'a> Lexer<'a> {
    fn scan_quoted_string(
        &mut self,
        start: usize,
        kind: TokenKind,
        allow_hex: bool,
        allow_unicode: bool,
    ) {
        self.pos += 1;
        let mut saw_nul = false;
        while self.pos < self.bytes.len() {
            let byte = self.bytes[self.pos];
            if byte == b'"' {
                self.pos += 1;
                if kind == TokenKind::CString && saw_nul {
                    self.error(
                        start,
                        self.pos,
                        DiagnosticCode::LexCStringNul,
                        "C 字符串不能包含内嵌 0 字节",
                    );
                    self.push(TokenKind::Error, start, self.pos, None);
                    return;
                }
                let symbol = self.buffer.intern.intern_str(&self.source[start..self.pos]);
                self.push(kind, start, self.pos, Some(symbol));
                return;
            }
            if byte == b'\n' || byte == b'\r' {
                self.unterminated(start, "字符串未闭合");
                return;
            }
            if byte == b'\\' {
                match scan_escape(self.source, self.pos, allow_hex, allow_unicode) {
                    Ok(decoded) => {
                        saw_nul |= decoded.is_nul;
                        self.pos = decoded.next;
                    }
                    Err(error) => {
                        self.report_escape(error);
                        return;
                    }
                }
                continue;
            }
            if byte == 0 {
                saw_nul = true;
            }
            self.pos += 1;
        }
        self.unterminated(start, "字符串未闭合");
    }

    fn scan_char_literal(&mut self, start: usize, byte_char: bool) {
        self.pos = if byte_char { start + 2 } else { start + 1 };
        if self.pos >= self.bytes.len() {
            self.unterminated(start, "字符字面量未闭合");
            return;
        }
        let (next, utf8_len) = if self.bytes[self.pos] == b'\\' {
            match scan_escape(self.source, self.pos, byte_char, true) {
                Ok(decoded) => (decoded.next, decoded.utf8_len),
                Err(error) => {
                    self.report_escape(error);
                    return;
                }
            }
        } else if matches!(self.bytes[self.pos], b'\'' | b'\n' | b'\r') {
            self.error(
                start,
                self.pos + 1,
                DiagnosticCode::LexInvalidToken,
                "字符字面量非法",
            );
            self.push(TokenKind::Error, start, self.pos + 1, None);
            self.pos += 1;
            return;
        } else {
            let ch = self.source[self.pos..].chars().next().expect("UTF-8");
            (self.pos + ch.len_utf8(), ch.len_utf8() as u8)
        };
        self.pos = next;
        if self.bytes.get(self.pos) != Some(&b'\'') {
            self.unterminated(start, "字符字面量未闭合");
            return;
        }
        self.pos += 1;
        if byte_char && utf8_len != 1 {
            self.error(
                start,
                self.pos,
                DiagnosticCode::LexInvalidByteChar,
                "字节字符必须恰好一个字节",
            );
            self.push(TokenKind::Error, start, self.pos, None);
            return;
        }
        let kind = if byte_char {
            TokenKind::ByteChar
        } else {
            TokenKind::Char
        };
        let symbol = self.buffer.intern.intern_str(&self.source[start..self.pos]);
        self.push(kind, start, self.pos, Some(symbol));
    }

    fn scan_raw_string(&mut self, start: usize) {
        debug_assert_eq!(self.bytes.get(self.pos), Some(&b'"'));
        let triple = self.bytes.get(self.pos + 1) == Some(&b'"')
            && self.bytes.get(self.pos + 2) == Some(&b'"');
        if triple {
            self.scan_raw_triple(start);
        } else {
            self.scan_raw_single(start);
        }
    }

    fn scan_raw_single(&mut self, start: usize) {
        self.pos += 1;
        while self.pos < self.bytes.len() {
            let byte = self.bytes[self.pos];
            if byte == b'\n' || byte == b'\r' {
                self.error(
                    start,
                    self.pos,
                    DiagnosticCode::LexUnterminated,
                    "raw\"...\" 不能包含未转义换行",
                );
                self.push(TokenKind::Error, start, self.pos, None);
                return;
            }
            if byte == b'\\' {
                match self.bytes.get(self.pos + 1) {
                    Some(b'\\' | b'"') => {
                        self.pos += 2;
                        continue;
                    }
                    _ => {
                        self.pos += 1;
                        continue;
                    }
                }
            }
            if byte == b'"' {
                self.pos += 1;
                let symbol = self.buffer.intern.intern_str(&self.source[start..self.pos]);
                self.push(TokenKind::RawString, start, self.pos, Some(symbol));
                return;
            }
            self.pos += 1;
        }
        self.unterminated(start, "raw 字符串未闭合");
    }

    fn scan_raw_triple(&mut self, start: usize) {
        self.pos += 3;
        while self.pos + 2 < self.bytes.len() || self.pos < self.bytes.len() {
            if self.bytes.get(self.pos) == Some(&b'\\')
                && matches!(self.bytes.get(self.pos + 1), Some(b'\\' | b'"'))
            {
                self.pos += 2;
                continue;
            }
            if self.bytes.get(self.pos) == Some(&b'"')
                && self.bytes.get(self.pos + 1) == Some(&b'"')
                && self.bytes.get(self.pos + 2) == Some(&b'"')
            {
                self.pos += 3;
                let symbol = self.buffer.intern.intern_str(&self.source[start..self.pos]);
                self.push(TokenKind::RawString, start, self.pos, Some(symbol));
                return;
            }
            if self.pos >= self.bytes.len() {
                break;
            }
            self.pos += 1;
        }
        self.unterminated(start, "raw 字符串未闭合");
    }

    fn scan_fstring(&mut self, start: usize) {
        if self.fstring_interp > 0 {
            self.error(
                start,
                start + 2,
                DiagnosticCode::LexInvalidToken,
                "禁止在插值表达式里再写 f\"...\"",
            );
            self.pos += 1;
            self.push(TokenKind::Error, start, self.pos, None);
            return;
        }
        self.push(TokenKind::FStringStart, start, self.pos + 1, None);
        self.pos += 1;
        self.scan_fstring_text();
    }

    fn scan_fstring_text(&mut self) {
        let mut frag = self.pos;
        while self.pos < self.bytes.len() {
            let byte = self.bytes[self.pos];
            if byte == b'"' {
                self.flush_fstring_text(frag);
                let end = self.pos + 1;
                self.pos = end;
                self.push(TokenKind::FStringEnd, self.pos - 1, self.pos, None);
                return;
            }
            if byte == b'\n' || byte == b'\r' {
                self.unterminated(frag.saturating_sub(1), "插值字符串未闭合");
                return;
            }
            if byte == b'{' {
                if self.bytes.get(self.pos + 1) == Some(&b'{') {
                    self.pos += 2;
                    continue;
                }
                self.flush_fstring_text(frag);
                self.scan_fstring_interp();
                frag = self.pos;
                continue;
            }
            if byte == b'}' {
                if self.bytes.get(self.pos + 1) == Some(&b'}') {
                    self.pos += 2;
                    continue;
                }
                self.error(
                    self.pos,
                    self.pos + 1,
                    DiagnosticCode::LexInvalidToken,
                    "插值字符串中的 `}` 必须成对写成 `}}`",
                );
                self.push(TokenKind::Error, self.pos, self.pos + 1, None);
                self.pos += 1;
                frag = self.pos;
                continue;
            }
            if byte == b'\\' {
                match scan_escape(self.source, self.pos, false, true) {
                    Ok(decoded) => self.pos = decoded.next,
                    Err(error) => {
                        self.report_escape(error);
                        return;
                    }
                }
                continue;
            }
            self.pos += 1;
        }
        self.unterminated(frag, "插值字符串未闭合");
    }

    fn flush_fstring_text(&mut self, start: usize) {
        if start >= self.pos {
            return;
        }
        let symbol = self.buffer.intern.intern_str(&self.source[start..self.pos]);
        self.push(TokenKind::FStringText, start, self.pos, Some(symbol));
    }

    fn scan_fstring_interp(&mut self) {
        let open = self.pos;
        self.pos += 1;
        self.push(TokenKind::FStringInterpOpen, open, self.pos, None);
        self.fstring_interp += 1;
        let mut paren = 0_u32;
        let mut bracket = 0_u32;
        let mut brace = 0_u32;
        loop {
            self.skip_trivia();
            if self.pos >= self.bytes.len() {
                self.unterminated(open, "插值未闭合");
                self.fstring_interp -= 1;
                return;
            }
            let byte = self.bytes[self.pos];
            if byte == b'}' && paren == 0 && bracket == 0 && brace == 0 {
                let close = self.pos;
                self.pos += 1;
                self.push(TokenKind::FStringInterpClose, close, self.pos, None);
                self.fstring_interp -= 1;
                return;
            }
            if byte == b':' && paren == 0 && bracket == 0 && brace == 0 {
                self.scan_format_spec(open);
                self.fstring_interp -= 1;
                return;
            }
            let before = self.buffer.tokens.len();
            self.scan_token();
            if self.buffer.tokens.len() == before {
                continue;
            }
            match self.buffer.tokens.last().map(|token| token.kind) {
                Some(TokenKind::LParen) => paren += 1,
                Some(TokenKind::RParen) => paren = paren.saturating_sub(1),
                Some(TokenKind::LBracket) => bracket += 1,
                Some(TokenKind::RBracket) => bracket = bracket.saturating_sub(1),
                Some(TokenKind::LBrace) => brace += 1,
                Some(TokenKind::RBrace) => brace = brace.saturating_sub(1),
                Some(TokenKind::FStringEnd | TokenKind::Error) if self.pos >= self.bytes.len() => {
                    self.fstring_interp -= 1;
                    return;
                }
                _ => {}
            }
        }
    }

    fn scan_format_spec(&mut self, open: usize) {
        let spec_start = self.pos;
        self.pos += 1;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'}' {
            if matches!(self.bytes[self.pos], b'\n' | b'\r') {
                self.unterminated(open, "插值未闭合");
                return;
            }
            self.pos += 1;
        }
        if self.pos >= self.bytes.len() {
            self.unterminated(open, "插值未闭合");
            return;
        }
        let spec = &self.source[spec_start + 1..self.pos];
        if let Some(message) = format_spec_error(spec) {
            self.error(
                spec_start,
                self.pos,
                DiagnosticCode::LexInvalidFormatSpec,
                message,
            );
            self.push(TokenKind::Error, spec_start, self.pos, None);
        } else {
            let symbol = self
                .buffer
                .intern
                .intern_str(&self.source[spec_start..self.pos]);
            self.push(TokenKind::FormatSpec, spec_start, self.pos, Some(symbol));
        }
        let close = self.pos;
        self.pos += 1;
        self.push(TokenKind::FStringInterpClose, close, self.pos, None);
    }

    fn report_escape(&mut self, error: EscapeError) {
        self.error(error.start, error.end, error.code, error.message);
        self.pos = error.end.max(self.pos);
        while self.pos < self.bytes.len()
            && !matches!(self.bytes[self.pos], b'"' | b'\'' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
        if matches!(self.bytes.get(self.pos), Some(b'"' | b'\'')) {
            self.pos += 1;
        }
        self.push(TokenKind::Error, error.start, self.pos, None);
    }

    fn unterminated(&mut self, start: usize, message: &str) {
        self.pos = self.bytes.len();
        self.error(start, self.pos, DiagnosticCode::LexUnterminated, message);
        self.push(TokenKind::Error, start, self.pos, None);
    }

    fn error(
        &mut self,
        start: usize,
        end: usize,
        code: DiagnosticCode,
        message: impl Into<String>,
    ) {
        let span = self
            .source_map
            .span(self.file, start, end.max(start), self.expansion)
            .ok();
        self.diagnostics
            .push(Diagnostic::error(code, message, span));
    }

    fn push(
        &mut self,
        kind: TokenKind,
        start: usize,
        end: usize,
        symbol: Option<super::intern::Symbol>,
    ) {
        debug_assert!(self.buffer.tokens.len() < u32::MAX as usize);
        let trivia_start = self.trivia_mark;
        let trivia_len = checked_u32(self.buffer.trivia.len()) - trivia_start;
        self.buffer.tokens.push(Token {
            kind,
            start: checked_u32(start),
            end: checked_u32(end),
            trivia_start,
            trivia_len,
            symbol,
        });
        self.trivia_mark = checked_u32(self.buffer.trivia.len());
    }

    fn push_trivia(&mut self, kind: TriviaKind, start: usize, end: usize) {
        debug_assert!(self.buffer.trivia.len() < u32::MAX as usize);
        self.buffer.trivia.push(Trivia {
            kind,
            start: checked_u32(start),
            end: checked_u32(end),
        });
    }
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn digit_value(byte: u8) -> u32 {
    match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'a'..=b'f' => u32::from(byte - b'a') + 10,
        b'A'..=b'F' => u32::from(byte - b'A') + 10,
        _ => 32,
    }
}

fn match_punct(bytes: &[u8]) -> Option<(TokenKind, usize)> {
    let first = *bytes.first()?;
    Some(match first {
        b'(' => (TokenKind::LParen, 1),
        b')' => (TokenKind::RParen, 1),
        b'{' => (TokenKind::LBrace, 1),
        b'}' => (TokenKind::RBrace, 1),
        b'[' => (TokenKind::LBracket, 1),
        b']' => (TokenKind::RBracket, 1),
        b',' => (TokenKind::Comma, 1),
        b';' => (TokenKind::Semi, 1),
        b'~' => (TokenKind::Tilde, 1),
        b'@' => (TokenKind::At, 1),
        b'?' => (TokenKind::Question, 1),
        b'.' => punct_dot(bytes),
        b':' => punct_colon(bytes),
        b'=' => punct_eq(bytes),
        b'!' => punct_not(bytes),
        b'<' => punct_lt(bytes),
        b'>' => punct_gt(bytes),
        b'+' => punct_assign(bytes, TokenKind::Plus, TokenKind::PlusEq),
        b'-' => punct_assign(bytes, TokenKind::Minus, TokenKind::MinusEq),
        b'*' => punct_assign(bytes, TokenKind::Star, TokenKind::StarEq),
        b'/' => punct_assign(bytes, TokenKind::Slash, TokenKind::SlashEq),
        b'%' => punct_assign(bytes, TokenKind::Percent, TokenKind::PercentEq),
        b'&' => punct_amp(bytes),
        b'|' => punct_pipe(bytes),
        b'^' => punct_assign(bytes, TokenKind::Caret, TokenKind::CaretEq),
        _ => return None,
    })
}

fn punct_dot(bytes: &[u8]) -> (TokenKind, usize) {
    if bytes.get(1) == Some(&b'.') {
        if bytes.get(2) == Some(&b'.') {
            (TokenKind::DotDotDot, 3)
        } else {
            (TokenKind::DotDot, 2)
        }
    } else {
        (TokenKind::Dot, 1)
    }
}

fn punct_colon(bytes: &[u8]) -> (TokenKind, usize) {
    if bytes.get(1) == Some(&b':') {
        (TokenKind::PathSep, 2)
    } else {
        (TokenKind::Colon, 1)
    }
}

fn punct_eq(bytes: &[u8]) -> (TokenKind, usize) {
    match bytes.get(1) {
        Some(b'=') => (TokenKind::EqEq, 2),
        Some(b'>') => (TokenKind::FatArrow, 2),
        _ => (TokenKind::Eq, 1),
    }
}

fn punct_not(bytes: &[u8]) -> (TokenKind, usize) {
    if bytes.get(1) == Some(&b'=') {
        (TokenKind::Ne, 2)
    } else {
        (TokenKind::Not, 1)
    }
}

fn punct_lt(bytes: &[u8]) -> (TokenKind, usize) {
    match (bytes.get(1), bytes.get(2)) {
        (Some(b'<'), Some(b'=')) => (TokenKind::ShlEq, 3),
        (Some(b'<'), _) => (TokenKind::Shl, 2),
        (Some(b'='), _) => (TokenKind::Le, 2),
        _ => (TokenKind::Lt, 1),
    }
}

fn punct_gt(bytes: &[u8]) -> (TokenKind, usize) {
    match (bytes.get(1), bytes.get(2)) {
        (Some(b'>'), Some(b'=')) => (TokenKind::ShrEq, 3),
        (Some(b'>'), _) => (TokenKind::Shr, 2),
        (Some(b'='), _) => (TokenKind::Ge, 2),
        _ => (TokenKind::Gt, 1),
    }
}

fn punct_assign(bytes: &[u8], op: TokenKind, assign: TokenKind) -> (TokenKind, usize) {
    if bytes.get(1) == Some(&b'=') {
        (assign, 2)
    } else {
        (op, 1)
    }
}

fn punct_amp(bytes: &[u8]) -> (TokenKind, usize) {
    match bytes.get(1) {
        Some(b'&') => (TokenKind::AndAnd, 2),
        Some(b'=') => (TokenKind::AndEq, 2),
        _ => (TokenKind::And, 1),
    }
}

fn punct_pipe(bytes: &[u8]) -> (TokenKind, usize) {
    match bytes.get(1) {
        Some(b'|') => (TokenKind::OrOr, 2),
        Some(b'=') => (TokenKind::OrEq, 2),
        _ => (TokenKind::Or, 1),
    }
}
