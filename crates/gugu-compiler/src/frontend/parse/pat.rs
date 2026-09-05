use super::super::ast::{FieldPat, LitKind, Pat, PatId, PatKind, RestPat};
use super::super::token::TokenKind;
use super::{Parser, finish_extend};
use crate::diagnostics::DiagnosticCode;

impl Parser<'_> {
    pub(super) fn parse_pat(&mut self) -> PatId {
        let first = self.parse_at_pat();
        if !self.at(TokenKind::Or) {
            return first;
        }
        let or_mark = self.start();
        let mut alts = vec![first];
        while self.eat(TokenKind::Or) {
            alts.push(self.parse_at_pat());
        }
        let span = self.finish_span(or_mark);
        let alts = finish_extend(&mut self.diagnostics, &mut self.arena.pat_ids, alts);
        self.arena.push_pat(Pat {
            id: or_mark.id,
            span,
            kind: PatKind::Or(alts),
        })
    }

    fn parse_at_pat(&mut self) -> PatId {
        let mark = self.start();
        self.parse_at_pat_from(mark)
    }

    fn parse_at_pat_from(&mut self, mark: super::Mark) -> PatId {
        if self.at(TokenKind::Ident) && self.nth(1) == TokenKind::At {
            let name_tok = self.bump();
            self.bump();
            if self.at(TokenKind::DotDot) {
                self.error_here(
                    DiagnosticCode::ParseUnexpected,
                    "rest 绑定只能出现在数组或切片模式内部",
                );
                self.bump();
                return self.push_pat(mark, PatKind::Error);
            }
            let inner = self.parse_pat_atom();
            return self.arena.push_pat(Pat {
                id: mark.id,
                span: self.finish_span(mark),
                kind: PatKind::At {
                    name: self.interned_symbol(name_tok),
                    name_span: self.token_span(name_tok),
                    pat: inner,
                },
            });
        }
        self.parse_pat_atom_from(mark)
    }

    fn parse_pat_atom(&mut self) -> PatId {
        let mark = self.start();
        self.parse_pat_atom_from(mark)
    }

    fn parse_pat_atom_from(&mut self, mark: super::Mark) -> PatId {
        if self.peek_source() {
            self.bump();
            self.bump();
            let body = self.parse_block_expr();
            return self.push_pat(mark, PatKind::SourceMacro { body });
        }
        match self.kind() {
            TokenKind::Ident if self.text() == "_" => {
                self.bump();
                self.push_pat(mark, PatKind::Wildcard)
            }
            TokenKind::Ident => self.parse_ident_or_ctor_pat(mark),
            TokenKind::And => {
                self.bump();
                let inner = self.parse_at_pat();
                self.push_pat(mark, PatKind::Ref(inner))
            }
            TokenKind::LParen => self.parse_tuple_pat(mark),
            TokenKind::LBracket => self.parse_array_pat(mark),
            TokenKind::Int
            | TokenKind::Float
            | TokenKind::Char
            | TokenKind::ByteChar
            | TokenKind::KwTrue
            | TokenKind::KwFalse => self.parse_lit_or_range_pat(mark),
            TokenKind::Minus => {
                self.bump();
                let value = self.parse_pat_literal_expr();
                let super::super::ast::ExprKind::Literal(literal) =
                    self.arena.exprs[value.0 as usize].kind
                else {
                    return self.push_pat(mark, PatKind::Error);
                };
                if self.at(TokenKind::DotDot) {
                    let id = self.arena.alloc_node(self.file);
                    let start = self.arena.push_expr(super::super::ast::Expr {
                        id,
                        span: self.finish_span(mark),
                        attributes: super::super::ast::AstRange::empty(),
                        kind: super::super::ast::ExprKind::Unary {
                            op: super::super::ast::UnOp::Neg,
                            expr: value,
                        },
                    });
                    self.parse_range_after(mark, start)
                } else {
                    self.push_pat(mark, PatKind::NegativeLiteral(literal))
                }
            }
            _ => self.error_pat(mark),
        }
    }

    fn parse_ident_or_ctor_pat(&mut self, mark: super::Mark) -> PatId {
        let path = self.parse_path();
        if self.eat(TokenKind::LParen) {
            let fields = self.parse_pat_list(TokenKind::RParen);
            self.expect(TokenKind::RParen, "构造器模式需要 `)`");
            return self.push_pat(mark, PatKind::Constructor { path, fields });
        }
        if self.eat(TokenKind::LBrace) {
            let (fields, rest) = self.parse_field_pat_list();
            self.expect(TokenKind::RBrace, "结构体模式需要 `}`");
            return self.push_pat(mark, PatKind::Struct { path, fields, rest });
        }
        if self.at(TokenKind::DotDot) {
            let expr = self.path_expr(path);
            return self.parse_range_after(mark, expr);
        }
        let name = self.arena.paths[path.0 as usize]
            .segments
            .as_slice(&self.arena.segments)
            .first()
            .expect("路径至少有一段")
            .name;
        let segs = self.arena.paths[path.0 as usize].segments;
        if segs.len == 1 && segs.as_slice(&self.arena.segments)[0].colon == false {
            self.push_pat(mark, PatKind::Ident(name))
        } else {
            self.push_pat(
                mark,
                PatKind::Constructor {
                    path,
                    fields: super::super::ast::AstRange::empty(),
                },
            )
        }
    }

    fn parse_tuple_pat(&mut self, mark: super::Mark) -> PatId {
        self.bump();
        if self.eat(TokenKind::RParen) {
            return self.push_pat(mark, PatKind::Tuple(super::super::ast::AstRange::empty()));
        }
        let first = self.parse_pat();
        if !self.eat(TokenKind::Comma) {
            self.expect(TokenKind::RParen, "模式括号需要 `)`");
            return first;
        }
        let mut pats = vec![first];
        while !self.at_any(&[TokenKind::RParen, TokenKind::Eof]) {
            pats.push(self.parse_pat());
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen, "元组模式需要 `)`");
        let pats = finish_extend(&mut self.diagnostics, &mut self.arena.pat_ids, pats);
        self.push_pat(mark, PatKind::Tuple(pats))
    }

    fn parse_array_pat(&mut self, mark: super::Mark) -> PatId {
        self.bump();
        let mut prefix = Vec::new();
        let mut rest = None;
        let mut suffix = Vec::new();
        let mut after_rest = false;
        while !self.at_any(&[TokenKind::RBracket, TokenKind::Eof]) {
            if self.at(TokenKind::DotDot) {
                let span = self.token_span(self.current());
                self.bump();
                if rest.is_some() {
                    self.error_span(
                        DiagnosticCode::ParseUnexpected,
                        "数组模式至多包含一个 `..`",
                        span.clone(),
                    );
                }
                rest = Some(RestPat { name: None, span });
                after_rest = true;
                if self.eat(TokenKind::Comma) {
                    continue;
                }
                break;
            }
            if self.at(TokenKind::Ident)
                && self.nth(1) == TokenKind::At
                && self.nth(2) == TokenKind::DotDot
            {
                let name = self.bump();
                self.bump();
                let span = self.token_span(self.current());
                self.bump();
                if rest.is_some() {
                    self.error_span(
                        DiagnosticCode::ParseUnexpected,
                        "数组模式至多包含一个 `..`",
                        span.clone(),
                    );
                }
                rest = Some(RestPat {
                    name: Some(self.interned_symbol(name)),
                    span,
                });
                after_rest = true;
                if self.eat(TokenKind::Comma) {
                    continue;
                }
                break;
            }
            let pat = self.parse_pat();
            if after_rest {
                suffix.push(pat);
            } else {
                prefix.push(pat);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RBracket, "数组模式需要 `]`");
        let prefix = finish_extend(&mut self.diagnostics, &mut self.arena.pat_ids, prefix);
        let suffix = finish_extend(&mut self.diagnostics, &mut self.arena.pat_ids, suffix);
        self.push_pat(
            mark,
            PatKind::Array {
                prefix,
                rest,
                suffix,
            },
        )
    }

    fn parse_field_pat_list(&mut self) -> (super::super::ast::AstRange<FieldPat>, bool) {
        let mut fields = Vec::new();
        let mut rest = false;
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            if self.eat(TokenKind::DotDot) {
                rest = true;
                self.eat(TokenKind::Comma);
                break;
            }
            let name_tok = self.expect(TokenKind::Ident, "字段模式需要名字");
            let pat = if self.eat(TokenKind::Colon) {
                Some(self.parse_pat())
            } else {
                None
            };
            fields.push(FieldPat {
                name: self.interned_symbol(name_tok),
                span: self.token_span(name_tok),
                pat,
            });
            if !self.eat(TokenKind::Comma) && !self.at_list_separator() {
                break;
            }
        }
        (
            finish_extend(&mut self.diagnostics, &mut self.arena.field_pats, fields),
            rest,
        )
    }

    fn parse_pat_list(&mut self, close: TokenKind) -> super::super::ast::AstRange<PatId> {
        let mut pats = Vec::new();
        if self.at(close) {
            return super::super::ast::AstRange::empty();
        }
        loop {
            pats.push(self.parse_pat());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.at(close) {
                break;
            }
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.pat_ids, pats)
    }

    fn parse_lit_or_range_pat(&mut self, mark: super::Mark) -> PatId {
        let start = self.parse_pat_literal_expr();
        if self.at(TokenKind::DotDot) {
            return self.parse_range_after(mark, start);
        }
        let lit = match self.arena.exprs[start.0 as usize].kind {
            super::super::ast::ExprKind::Literal(kind) => kind,
            _ => {
                return self.push_pat(mark, PatKind::Error);
            }
        };
        self.push_pat(mark, PatKind::Literal(lit))
    }

    fn parse_pat_literal_expr(&mut self) -> super::super::ast::ExprId {
        let mark = self.start();
        let kind = match self.kind() {
            TokenKind::Int => {
                let token = self.bump();
                let (radix, limbs) = self.parse_int_limbs(token.text(self.source));
                LitKind::Int { radix, limbs }
            }
            TokenKind::Float => {
                let token = self.bump();
                let (digits, exp10) = self.parse_float_parts(token.text(self.source));
                LitKind::Float { digits, exp10 }
            }
            TokenKind::Char => {
                let token = self.bump();
                LitKind::Char {
                    value: self.parse_char_value(token.text(self.source)),
                    text: self.interned_symbol(token),
                }
            }
            TokenKind::ByteChar => {
                let token = self.bump();
                LitKind::ByteChar {
                    value: self.parse_byte_char_value(token.text(self.source)),
                    text: self.interned_symbol(token),
                }
            }
            TokenKind::KwTrue | TokenKind::KwFalse => {
                let value = self.at(TokenKind::KwTrue);
                self.bump();
                LitKind::Bool(value)
            }
            _ => {
                self.error_here(DiagnosticCode::ParseUnexpected, "此处需要字面量模式");
                return self.error_expr(mark);
            }
        };
        self.arena.push_expr(super::super::ast::Expr {
            id: mark.id,
            span: self.finish_span(mark),
            attributes: super::super::ast::AstRange::empty(),
            kind: super::super::ast::ExprKind::Literal(kind),
        })
    }

    fn parse_range_after(&mut self, mark: super::Mark, start: super::super::ast::ExprId) -> PatId {
        self.bump();
        let end = self.parse_pattern_endpoint();
        self.push_pat(mark, PatKind::Range { start, end })
    }

    fn path_expr(&mut self, path: super::super::ast::PathId) -> super::super::ast::ExprId {
        let span = self.arena.paths[path.0 as usize].span.clone();
        let id = self.arena.alloc_node(self.file);
        self.arena.push_expr(super::super::ast::Expr {
            id,
            span,
            attributes: super::super::ast::AstRange::empty(),
            kind: super::super::ast::ExprKind::Path(path),
        })
    }

    fn push_pat(&mut self, mark: super::Mark, kind: PatKind) -> PatId {
        let span = self.finish_span(mark);
        self.arena.push_pat(Pat {
            id: mark.id,
            span,
            kind,
        })
    }
}
