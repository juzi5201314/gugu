//! 递归下降 parser：消费 `TokenBuffer`，产出稠密 AST。

#[cfg(test)]
mod dump;
mod expr;
mod item;
mod lit;
mod pat;
mod ty;

use crate::diagnostics::{Diagnostic, DiagnosticCode};
use crate::source::{ExpansionId, SourceFileId, SourceMap, Span};

#[cfg(test)]
use super::ast::FnBody;
use super::ast::{
    AstArena, AstFile, AstNodeId, AstRange, AttrKind, Attribute, ExprId, ItemKind, try_extend_range,
};
use super::intern::{Symbol, SymbolInterner};
use super::token::{Token, TokenBuffer, TokenKind, TriviaKind};

#[cfg(test)]
pub(crate) use dump::{dump_ast, parent_before_child};

pub(super) fn finish_extend<T>(
    diagnostics: &mut Vec<Diagnostic>,
    vec: &mut Vec<T>,
    items: impl IntoIterator<Item = T>,
) -> AstRange<T> {
    match try_extend_range(vec, items) {
        Some(range) => range,
        None => {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::ParseImplementationLimit,
                "实现限制：AST 规模超过上限",
                None,
            ));
            AstRange::empty()
        }
    }
}

#[cfg(test)]
pub(crate) fn has_main_fn(file: &AstFile, arena: &AstArena, intern: &SymbolInterner) -> bool {
    file.items.as_slice(&arena.item_ids).iter().any(|id| {
        let item = &arena.items[id.0 as usize];
        match item.kind {
            ItemKind::Function(fn_id) => {
                let decl = &arena.fns[fn_id.0 as usize];
                decl.name.is_some_and(|name| intern.get_str(name) == "main")
                    && decl.params.len == 0
                    && matches!(decl.body, FnBody::Block(_) | FnBody::Eq(_))
            }
            _ => false,
        }
    })
}

pub(crate) struct ParseOutput {
    pub(crate) file: AstFile,
    pub(crate) arena: AstArena,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

pub(crate) fn parse(
    source: &str,
    source_map: &SourceMap,
    file: SourceFileId,
    buffer: &mut TokenBuffer,
) -> ParseOutput {
    let empty_symbol = buffer.intern.intern_str("");
    let mut parser = Parser {
        source,
        source_map,
        file,
        tokens: &buffer.tokens,
        trivia: &buffer.trivia,
        intern: &mut buffer.intern,
        empty_symbol,
        cursor: 0,
        delim_depth: 0,
        brace_depth: 0,
        extern_block_abi: None,
        arena: AstArena::with_token_hint(buffer.tokens.len()),
        diagnostics: Vec::new(),
        allow_struct: true,
        ty_depth: 0,
        diag_seq: 0,
    };
    let ast_file = parser.parse_file();
    ParseOutput {
        file: ast_file,
        arena: parser.arena,
        diagnostics: parser.diagnostics,
    }
}

pub(super) struct Parser<'a> {
    pub(super) source: &'a str,
    pub(super) source_map: &'a SourceMap,
    pub(super) file: SourceFileId,
    pub(super) tokens: &'a [Token],
    trivia: &'a [super::token::Trivia],
    pub(super) intern: &'a mut SymbolInterner,
    pub(super) empty_symbol: Symbol,
    pub(super) cursor: usize,
    pub(super) delim_depth: u32,
    pub(super) brace_depth: u32,
    pub(super) extern_block_abi: Option<Symbol>,
    pub(super) arena: AstArena,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) allow_struct: bool,
    pub(super) ty_depth: u32,
    diag_seq: u32,
}

#[derive(Clone, Copy)]
pub(super) struct Mark {
    pub(super) id: AstNodeId,
    pub(super) start: u32,
}

impl<'a> Parser<'a> {
    pub(super) fn current(&self) -> Token {
        self.tokens[self.cursor]
    }

    pub(super) fn kind(&self) -> TokenKind {
        self.current().kind
    }

    pub(super) fn at(&self, kind: TokenKind) -> bool {
        self.kind() == kind
    }

    pub(super) fn at_any(&self, kinds: &[TokenKind]) -> bool {
        kinds.contains(&self.kind())
    }

    pub(super) fn text(&self) -> &'a str {
        self.current().text(self.source)
    }

    pub(super) fn ident_text_is(&self, expected: &str) -> bool {
        self.at(TokenKind::Ident) && self.text() == expected
    }

    pub(super) fn symbol_from_token(&self, token: Token) -> Symbol {
        token.symbol.unwrap_or(self.empty_symbol)
    }

    pub(super) fn interned_symbol(&self, token: Token) -> Symbol {
        self.symbol_from_token(token)
    }

    pub(super) fn consume_decl_terminator(&mut self) {
        if self.eat(TokenKind::Semi)
            || self.at(TokenKind::Eof)
            || self.at(TokenKind::RBrace)
            || self.at_line_end()
            || self.at_list_separator()
        {
            return;
        }
        self.error_here(DiagnosticCode::ParseExpected, "声明需要分号或换行");
    }

    pub(super) fn bump(&mut self) -> Token {
        let token = self.current();
        match token.kind {
            TokenKind::LParen | TokenKind::LBracket => self.delim_depth += 1,
            TokenKind::RParen | TokenKind::RBracket => {
                self.delim_depth = self.delim_depth.saturating_sub(1);
            }
            TokenKind::LBrace => self.brace_depth += 1,
            TokenKind::RBrace => self.brace_depth = self.brace_depth.saturating_sub(1),
            _ => {}
        }
        if token.kind != TokenKind::Eof {
            self.cursor += 1;
        }
        token
    }

    pub(super) fn leading_newline(&self, token: Token) -> bool {
        let start = token.trivia_start as usize;
        let end = start + token.trivia_len as usize;
        self.trivia[start..end]
            .iter()
            .any(|trivia| trivia.kind == TriviaKind::Newline)
    }

    pub(super) fn prev_continues(&self) -> bool {
        self.cursor > 0 && self.tokens[self.cursor - 1].kind.continues_line()
    }

    pub(super) fn at_line_end(&self) -> bool {
        if self.delim_depth != 0 {
            return false;
        }
        self.leading_newline(self.current()) && !self.prev_continues()
    }

    pub(super) fn at_list_separator(&self) -> bool {
        self.leading_newline(self.current()) && !self.prev_continues()
    }

    pub(super) fn newline_before_current(&self) -> bool {
        self.leading_newline(self.current())
    }

    pub(super) fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(super) fn expect(&mut self, kind: TokenKind, message: &str) -> Token {
        if self.at(kind) {
            return self.bump();
        }
        self.error_here(DiagnosticCode::ParseExpected, message);
        self.synthetic(kind)
    }

    fn synthetic(&mut self, kind: TokenKind) -> Token {
        let pos = self.current().start;
        Token {
            kind,
            start: pos,
            end: pos,
            trivia_start: 0,
            trivia_len: 0,
            symbol: None,
        }
    }

    pub(super) fn arena_limit(&mut self) {
        self.error_here(
            DiagnosticCode::ParseImplementationLimit,
            "实现限制：AST 规模超过上限",
        );
    }

    pub(super) fn start(&mut self) -> Mark {
        let id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                AstNodeId {
                    file: self.file,
                    local: u32::MAX,
                }
            }
        };
        Mark {
            id,
            start: self.current().start,
        }
    }

    pub(super) fn start_at(&mut self, start: u32) -> Mark {
        let id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                AstNodeId {
                    file: self.file,
                    local: u32::MAX,
                }
            }
        };
        Mark { id, start }
    }

    pub(super) fn span_to(&self, mark: Mark, end: u32) -> Span {
        self.make_span(mark.start, end.max(mark.start))
    }

    pub(super) fn finish_span(&self, mark: Mark) -> Span {
        let end = if self.cursor == 0 {
            mark.start
        } else {
            self.tokens[self.cursor - 1].end
        };
        self.span_to(mark, end)
    }

    pub(super) fn make_span(&self, start: u32, end: u32) -> Span {
        self.source_map
            .span(self.file, start as usize, end as usize, ExpansionId::ROOT)
            .unwrap_or_else(|_| {
                Span::detached(
                    std::path::Path::new("<parse>"),
                    start as usize,
                    end as usize,
                )
            })
    }

    pub(super) fn token_span(&self, token: Token) -> Span {
        self.make_span(token.start, token.end)
    }

    pub(super) fn expr_span(&self, id: ExprId) -> crate::source::Span {
        self.arena.exprs[id.0 as usize].span.clone()
    }

    fn next_diag_seq(&mut self) -> u32 {
        let seq = self.diag_seq;
        self.diag_seq += 1;
        seq
    }

    pub(super) fn error_here(&mut self, code: DiagnosticCode, message: impl Into<String>) {
        let token = self.current();
        let span = self.token_span(token);
        let seq = self.next_diag_seq();
        self.diagnostics
            .push(Diagnostic::error(code, message, Some(span)).with_seq(seq));
    }

    pub(super) fn error_span(
        &mut self,
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Span,
    ) {
        let seq = self.next_diag_seq();
        self.diagnostics
            .push(Diagnostic::error(code, message, Some(span)).with_seq(seq));
    }

    pub(super) fn note_span(
        &mut self,
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Span,
    ) {
        let seq = self.next_diag_seq();
        self.diagnostics
            .push(Diagnostic::note(code, message, Some(span)).with_seq(seq));
    }

    pub(super) fn consume_error_token(&mut self) {
        if self.at(TokenKind::Error) {
            self.error_here(DiagnosticCode::ParseUnexpected, "词法错误记号");
            self.bump();
        }
    }

    fn parse_file(&mut self) -> AstFile {
        let inner = self.parse_inner_attributes();
        let mut items = Vec::new();
        while !self.at(TokenKind::Eof) {
            if self.at(TokenKind::Error) {
                self.consume_error_token();
                continue;
            }
            let item_id = self.parse_item();
            if matches!(self.arena.items[item_id.0 as usize].kind, ItemKind::Error) {
                self.recover_item();
            }
            items.push(item_id);
        }
        let eof = self.current();
        AstFile {
            source: self.file,
            inner_attributes: inner,
            items: finish_extend(&mut self.diagnostics, &mut self.arena.item_ids, items),
            eof_span: self.token_span(eof),
        }
    }

    pub(super) fn parse_inner_attributes(&mut self) -> AstRange<Attribute> {
        let mut attrs = Vec::new();
        loop {
            attrs.extend(self.take_inner_docs());
            if self.at_inner_attr() {
                attrs.push(self.parse_attribute(true));
                continue;
            }
            break;
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.attrs, attrs)
    }

    fn at_inner_attr(&self) -> bool {
        self.at(TokenKind::Hash)
            && self.nth(1) == TokenKind::Not
            && self.nth(2) == TokenKind::LBracket
    }

    pub(super) fn nth(&self, n: usize) -> TokenKind {
        self.tokens
            .get(self.cursor + n)
            .map(|token| token.kind)
            .unwrap_or(TokenKind::Eof)
    }

    pub(super) fn parse_outer_attributes(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        loop {
            attrs.extend(self.take_outer_docs());
            if self.at(TokenKind::Hash) && self.nth(1) == TokenKind::LBracket {
                attrs.push(self.parse_attribute(false));
                continue;
            }
            break;
        }
        attrs
    }

    fn take_inner_docs(&mut self) -> Vec<Attribute> {
        self.take_docs(TriviaKind::InnerDocComment, true)
    }

    fn take_outer_docs(&mut self) -> Vec<Attribute> {
        self.take_docs(TriviaKind::DocComment, false)
    }

    fn take_docs(&mut self, kind: TriviaKind, inner: bool) -> Vec<Attribute> {
        let token = self.current();
        let start = token.trivia_start as usize;
        let end = start + token.trivia_len as usize;
        let mut attrs = Vec::new();
        for trivia in &self.trivia[start..end] {
            if trivia.kind != kind {
                continue;
            }
            let mark = self.start_at(trivia.start);
            let attr_kind = if inner {
                AttrKind::InnerDoc {
                    start: trivia.start,
                    end: trivia.end,
                }
            } else {
                AttrKind::Doc {
                    start: trivia.start,
                    end: trivia.end,
                }
            };
            attrs.push(Attribute {
                id: mark.id,
                span: self.make_span(trivia.start, trivia.end),
                kind: attr_kind,
            });
        }
        attrs
    }

    fn parse_attribute(&mut self, inner: bool) -> Attribute {
        let mark = self.start();
        self.bump();
        if inner {
            self.bump();
        }
        let open = self.expect(TokenKind::LBracket, "属性需要 `[`");
        let close_idx = self.skip_to_matching_bracket();
        let close = self.tokens[close_idx];
        if self.cursor != close_idx {
            self.cursor = close_idx;
            self.delim_depth = self.delim_depth.saturating_sub(1);
        }
        self.eat(TokenKind::RBracket);
        let kind = if inner {
            AttrKind::Inner {
                token_open: self.token_index(open),
                token_close: self.token_index(close),
            }
        } else {
            AttrKind::Outer {
                token_open: self.token_index(open),
                token_close: self.token_index(close),
            }
        };
        Attribute {
            id: mark.id,
            span: self.finish_span(mark),
            kind,
        }
    }

    fn token_index(&self, token: Token) -> u32 {
        self.tokens
            .iter()
            .position(|candidate| candidate.start == token.start && candidate.end == token.end)
            .map(super::token::checked_u32)
            .unwrap_or(0)
    }

    fn skip_to_matching_bracket(&mut self) -> usize {
        let mut depth = 1_u32;
        let mut index = self.cursor;
        while index < self.tokens.len() {
            match self.tokens[index].kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => {
                    depth -= 1;
                    if depth == 0 {
                        return index;
                    }
                }
                TokenKind::Eof => {
                    self.error_here(DiagnosticCode::ParseUnclosed, "属性括号未闭合");
                    return index;
                }
                _ => {}
            }
            index += 1;
        }
        self.tokens.len() - 1
    }

    pub(super) fn recover_item(&mut self) {
        let start = self.cursor;
        while !self.at(TokenKind::Eof) {
            if self.cursor > start && self.brace_depth == 0 && self.at_item_start() {
                break;
            }
            if self.at(TokenKind::LBrace) {
                self.skip_balanced(TokenKind::LBrace, TokenKind::RBrace);
                break;
            }
            if self.at_any(&[TokenKind::LParen, TokenKind::LBracket]) {
                let (open, close) = if self.at(TokenKind::LParen) {
                    (TokenKind::LParen, TokenKind::RParen)
                } else {
                    (TokenKind::LBracket, TokenKind::RBracket)
                };
                self.skip_balanced(open, close);
                continue;
            }
            if self.at(TokenKind::RBrace) && self.brace_depth == 0 {
                self.bump();
                break;
            }
            self.bump();
        }
    }

    pub(super) fn skip_balanced(&mut self, open: TokenKind, close: TokenKind) {
        if !self.at(open) {
            return;
        }
        let mut depth = 0_u32;
        loop {
            let kind = self.kind();
            if kind == open {
                depth += 1;
            } else if kind == close {
                depth = depth.saturating_sub(1);
                self.bump();
                if depth == 0 {
                    return;
                }
                continue;
            } else if kind == TokenKind::Eof {
                self.error_here(DiagnosticCode::ParseUnclosed, "分隔符未闭合");
                return;
            }
            self.bump();
        }
    }

    pub(super) fn at_item_start(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::Hash
                | TokenKind::KwPub
                | TokenKind::KwUse
                | TokenKind::KwFn
                | TokenKind::KwUnsafe
                | TokenKind::KwStruct
                | TokenKind::KwEnum
                | TokenKind::KwUnion
                | TokenKind::KwTrait
                | TokenKind::KwImpl
                | TokenKind::KwConst
                | TokenKind::KwType
                | TokenKind::KwStatic
                | TokenKind::KwExtern
                | TokenKind::KwGlobalAsm
                | TokenKind::KwComptime
        )
    }

    pub(super) fn store_attrs(&mut self, attrs: Vec<Attribute>) -> AstRange<Attribute> {
        finish_extend(&mut self.diagnostics, &mut self.arena.attrs, attrs)
    }

    pub(super) fn error_expr(&mut self, mark: Mark) -> ExprId {
        let span = self.token_span(self.current());
        if !self.at(TokenKind::Eof)
            && !self.at_any(&[TokenKind::RBrace, TokenKind::RParen, TokenKind::RBracket])
        {
            self.bump();
        }
        match self.arena.try_push_expr(super::ast::Expr {
            id: mark.id,
            span: self.span_to(mark, span.end()),
            attributes: AstRange::empty(),
            kind: super::ast::ExprKind::Error,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                ExprId(u32::MAX)
            }
        }
    }

    pub(super) fn error_ty(&mut self, mark: Mark) -> super::ast::TyId {
        self.error_here(DiagnosticCode::ParseUnexpected, "此处需要类型");
        let span = self.finish_span(mark);
        match self.arena.try_push_ty(super::ast::Ty {
            id: mark.id,
            span,
            kind: super::ast::TyKind::Error,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                super::ast::TyId(u32::MAX)
            }
        }
    }

    pub(super) fn error_pat(&mut self, mark: Mark) -> super::ast::PatId {
        self.error_here(DiagnosticCode::ParseUnexpected, "此处需要模式");
        if !self.at(TokenKind::Eof) {
            self.bump();
        }
        let span = self.finish_span(mark);
        match self.arena.try_push_pat(super::ast::Pat {
            id: mark.id,
            span,
            kind: super::ast::PatKind::Error,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                super::ast::PatId(u32::MAX)
            }
        }
    }
}
