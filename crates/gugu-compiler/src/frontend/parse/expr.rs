use crate::diagnostics::DiagnosticCode;

use super::super::ast::{
    AssignOp, AstRange, Attribute, BinOp, Expr, ExprId, ExprKind, FStringPart, FieldExpr,
    GenericArg, IndexKind, IntrinsicKind, LitKind, MatchArm, Path, PathId, SelectArm,
    SelectArmKind, Stmt, StmtId, StmtKind, UnOp,
};
use super::super::intern::Symbol;
use super::super::token::TokenKind;
use super::{Mark, Parser, finish_extend};

enum SelectCall {
    Send { chan: ExprId, payload: ExprId },
    Recv { chan: ExprId },
    Wait { join: ExprId },
}

const PREC_OR: u8 = 2;
const PREC_AND: u8 = 4;
const PREC_CMP: u8 = 6;
const PREC_RANGE: u8 = 8;
const PREC_BIT_OR: u8 = 10;
const PREC_BIT_XOR: u8 = 12;
const PREC_BIT_AND: u8 = 14;
const PREC_SHIFT: u8 = 16;
const PREC_ADD: u8 = 18;
const PREC_MUL: u8 = 20;

impl Parser<'_> {
    pub(super) fn parse_expression(&mut self) -> ExprId {
        self.parse_expr_with_attrs()
    }

    fn parse_expr_with_attrs(&mut self) -> ExprId {
        let attrs = self.parse_outer_attributes();
        self.parse_expr_with_outer_attrs(attrs)
    }

    fn parse_expr_with_outer_attrs(&mut self, attrs: Vec<Attribute>) -> ExprId {
        let expr = self.parse_expr_core();
        if !attrs.is_empty() {
            let stored = self.store_attrs(attrs);
            self.arena.exprs[expr.0 as usize].attributes = stored;
        }
        expr
    }

    fn parse_expr_core(&mut self) -> ExprId {
        match self.kind() {
            TokenKind::KwIf => self.parse_if(),
            TokenKind::KwMatch => self.parse_match(),
            TokenKind::KwTry => self.parse_try(),
            TokenKind::KwLoop => self.parse_loop(),
            TokenKind::KwWhile => self.parse_while(),
            TokenKind::KwFor => self.parse_for(),
            TokenKind::KwSelect => self.parse_select(),
            TokenKind::KwAsync => self.parse_async(),
            TokenKind::KwFn => self.parse_closure(),
            TokenKind::KwReturn => self.parse_return(),
            TokenKind::KwBreak => self.parse_break(),
            TokenKind::KwContinue => {
                let mark = self.start();
                self.bump();
                self.push_expr(mark, ExprKind::Continue)
            }
            _ => self.parse_binary(0, true),
        }
    }

    fn parse_expr_no_range(&mut self) -> ExprId {
        self.parse_binary(0, false)
    }

    pub(super) fn parse_pattern_endpoint(&mut self) -> ExprId {
        self.parse_binary(PREC_BIT_OR + 1, false)
    }

    pub(super) fn parse_block_expr(&mut self) -> ExprId {
        let mark = self.start();
        self.expect(TokenKind::LBrace, "块需要 `{`");
        let outer_delimiters = std::mem::replace(&mut self.delim_depth, 0);
        let (stmts, tail) = self.parse_block_contents();
        self.expect(TokenKind::RBrace, "块需要 `}`");
        self.delim_depth = outer_delimiters;
        self.push_expr(mark, ExprKind::Block { stmts, tail })
    }

    fn parse_block_contents(&mut self) -> (AstRange<StmtId>, Option<ExprId>) {
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.at_any(&[
            TokenKind::RBrace,
            TokenKind::RParen,
            TokenKind::RBracket,
            TokenKind::Eof,
        ]) {
            if self.at(TokenKind::Error) {
                self.consume_error_token();
                continue;
            }
            let mark = self.start();
            let attrs = self.parse_outer_attributes();
            if self.is_stmt_start() {
                stmts.push(self.parse_stmt(mark, attrs));
                continue;
            }
            let expr = self.parse_expr_with_outer_attrs(attrs);
            if self.is_assign_op() {
                stmts.push(self.finish_assign(expr));
                continue;
            }
            let discarded = self.eat(TokenKind::Semi);
            if discarded {
                stmts.push(self.expr_stmt(expr, true));
                continue;
            }
            if self.at_any(&[TokenKind::RBrace, TokenKind::RParen, TokenKind::RBracket]) {
                tail = Some(expr);
                break;
            }
            stmts.push(self.expr_stmt(expr, false));
        }
        (
            finish_extend(&mut self.diagnostics, &mut self.arena.stmt_ids, stmts),
            tail,
        )
    }

    fn is_stmt_start(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::KwLet | TokenKind::KwDefer | TokenKind::KwYield | TokenKind::KwStatic
        ) || (self.kind() == TokenKind::KwComptime && self.peek_source())
    }

    fn parse_stmt(&mut self, mark: Mark, attrs: Vec<Attribute>) -> StmtId {
        match self.kind() {
            TokenKind::KwStatic => {
                self.bump();
                let token = self.expect(TokenKind::Ident, "static 需要名称");
                let name = self.interned_symbol(token);
                self.expect(TokenKind::Colon, "static 需要类型标注");
                let ty = self.parse_ty();
                self.expect(TokenKind::Eq, "static 需要初始化器");
                let value = self.parse_expression();
                self.push_stmt(mark, attrs, StmtKind::Static { name, ty, value })
            }
            TokenKind::KwLet => self.parse_let_stmt(mark, attrs),
            TokenKind::KwDefer => self.parse_defer_stmt(mark, attrs),
            TokenKind::KwYield => self.parse_yield_stmt(mark, attrs),
            TokenKind::KwComptime => self.parse_source_macro_stmt(mark, attrs),
            _ => unreachable!("parse_stmt 只接收已识别的语句起始记号"),
        }
    }

    fn parse_let_stmt(&mut self, mark: Mark, attrs: Vec<Attribute>) -> StmtId {
        self.bump();
        let pat = self.parse_pat();
        let ty = if self.eat(TokenKind::Colon) {
            Some(self.parse_ty())
        } else {
            None
        };
        let init = if self.eat(TokenKind::Eq) {
            Some(self.parse_expression())
        } else {
            None
        };
        let else_block = if self.eat(TokenKind::KwElse) {
            Some(self.parse_block_expr())
        } else {
            None
        };
        self.push_stmt(
            mark,
            attrs,
            StmtKind::Let {
                pat,
                ty,
                init,
                else_block,
            },
        )
    }

    fn parse_defer_stmt(&mut self, mark: Mark, attrs: Vec<Attribute>) -> StmtId {
        self.bump();
        let ret = self.ident_text_is("ret") && {
            self.bump();
            true
        };
        let body = if self.at(TokenKind::LBrace) {
            self.parse_block_expr()
        } else {
            self.parse_expression()
        };
        self.push_stmt(mark, attrs, StmtKind::Defer { ret, body })
    }

    fn parse_yield_stmt(&mut self, mark: Mark, attrs: Vec<Attribute>) -> StmtId {
        self.bump();
        self.push_stmt(mark, attrs, StmtKind::Yield)
    }

    fn parse_source_macro_stmt(&mut self, mark: Mark, attrs: Vec<Attribute>) -> StmtId {
        self.bump();
        self.bump();
        let body = self.parse_block_expr();
        self.push_stmt(mark, attrs, StmtKind::SourceMacro { body })
    }

    fn is_assign_op(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::Eq
                | TokenKind::PlusEq
                | TokenKind::MinusEq
                | TokenKind::StarEq
                | TokenKind::SlashEq
                | TokenKind::PercentEq
                | TokenKind::AndEq
                | TokenKind::OrEq
                | TokenKind::CaretEq
                | TokenKind::ShlEq
                | TokenKind::ShrEq
        )
    }

    fn assign_op(&self) -> AssignOp {
        match self.kind() {
            TokenKind::Eq => AssignOp::Assign,
            TokenKind::PlusEq => AssignOp::Add,
            TokenKind::MinusEq => AssignOp::Sub,
            TokenKind::StarEq => AssignOp::Mul,
            TokenKind::SlashEq => AssignOp::Div,
            TokenKind::PercentEq => AssignOp::Rem,
            TokenKind::AndEq => AssignOp::BitAnd,
            TokenKind::OrEq => AssignOp::BitOr,
            TokenKind::CaretEq => AssignOp::BitXor,
            TokenKind::ShlEq => AssignOp::Shl,
            TokenKind::ShrEq => AssignOp::Shr,
            _ => AssignOp::Assign,
        }
    }

    fn finish_assign(&mut self, place: ExprId) -> StmtId {
        if !self.is_place(place) {
            let span = self.expr_span(place);
            self.error_span(
                DiagnosticCode::ParseInvalidPlace,
                "赋值左侧必须是 place 表达式",
                span,
            );
        }
        let op = self.assign_op();
        self.bump();
        let value = self.parse_expression();
        let mark_id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return StmtId(u32::MAX);
            }
        };
        let start = self.expr_span(place).start();
        let end = self.expr_span(value).end();
        let attributes = std::mem::replace(
            &mut self.arena.exprs[place.0 as usize].attributes,
            AstRange::empty(),
        );
        match self.arena.try_push_stmt(Stmt {
            attributes,
            id: mark_id,
            span: self.make_span(start, end),
            kind: StmtKind::Assign { op, place, value },
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                StmtId(u32::MAX)
            }
        }
    }

    fn is_place(&self, expr: ExprId) -> bool {
        matches!(
            self.arena.exprs[expr.0 as usize].kind,
            ExprKind::Path(_)
                | ExprKind::Field { .. }
                | ExprKind::TupleField { .. }
                | ExprKind::Index { .. }
                | ExprKind::Unary {
                    op: UnOp::Deref,
                    ..
                }
        )
    }

    fn expr_stmt(&mut self, expr: ExprId, discarded: bool) -> StmtId {
        let span = self.expr_span(expr);
        let id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return StmtId(u32::MAX);
            }
        };
        let attributes = std::mem::replace(
            &mut self.arena.exprs[expr.0 as usize].attributes,
            AstRange::empty(),
        );
        match self.arena.try_push_stmt(Stmt {
            attributes,
            id,
            span,
            kind: StmtKind::Expr { expr, discarded },
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                StmtId(u32::MAX)
            }
        }
    }

    fn push_stmt(&mut self, mark: Mark, attrs: Vec<Attribute>, kind: StmtKind) -> StmtId {
        let span = self.finish_span(mark);
        let attributes = self.store_attrs(attrs);
        match self.arena.try_push_stmt(Stmt {
            attributes,
            id: mark.id,
            span,
            kind,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                StmtId(u32::MAX)
            }
        }
    }

    fn parse_binary(&mut self, min_prec: u8, allow_range: bool) -> ExprId {
        let mut left = self.parse_unary();
        loop {
            if self.at_line_end() {
                break;
            }
            if allow_range && self.at(TokenKind::DotDot) && PREC_RANGE >= min_prec {
                let op_span = self.token_span(self.current());
                self.bump();
                let right = self.parse_binary(PREC_RANGE + 1, true);
                if self.at(TokenKind::DotDot) {
                    self.error_span(
                        DiagnosticCode::ParseInvalidPrecedence,
                        "比较与区间运算符不结合",
                        self.token_span(self.current()),
                    );
                    self.note_span(
                        DiagnosticCode::ParseInvalidPrecedence,
                        "前一个同级运算符在这里",
                        op_span,
                    );
                }
                left = self.make_range(left, right);
                continue;
            }
            let Some((op, prec, assoc)) = self.current_binop() else {
                break;
            };
            if prec < min_prec {
                break;
            }
            let op_span = self.token_span(self.current());
            self.bump();
            let right = self.parse_binary(prec + 1, allow_range);
            if !assoc
                && let Some((_, next, _)) = self.current_binop()
                && next == prec
            {
                self.error_span(
                    DiagnosticCode::ParseInvalidPrecedence,
                    "比较与区间运算符不结合",
                    self.token_span(self.current()),
                );
                self.note_span(
                    DiagnosticCode::ParseInvalidPrecedence,
                    "前一个同级运算符在这里",
                    op_span,
                );
            }
            left = self.make_binary(op, left, right);
        }
        left
    }

    fn current_binop(&self) -> Option<(BinOp, u8, bool)> {
        Some(match self.kind() {
            TokenKind::Star | TokenKind::Slash | TokenKind::Percent => {
                (self.mul_op(), PREC_MUL, true)
            }
            TokenKind::Plus | TokenKind::Minus => (self.add_op(), PREC_ADD, true),
            TokenKind::Shl | TokenKind::Shr => (self.shift_op(), PREC_SHIFT, true),
            TokenKind::And => (BinOp::BitAnd, PREC_BIT_AND, true),
            TokenKind::Caret => (BinOp::BitXor, PREC_BIT_XOR, true),
            TokenKind::Or => (BinOp::BitOr, PREC_BIT_OR, true),
            TokenKind::EqEq => (BinOp::Eq, PREC_CMP, false),
            TokenKind::Ne => (BinOp::Ne, PREC_CMP, false),
            TokenKind::Lt => (BinOp::Lt, PREC_CMP, false),
            TokenKind::Le => (BinOp::Le, PREC_CMP, false),
            TokenKind::Gt => (BinOp::Gt, PREC_CMP, false),
            TokenKind::Ge => (BinOp::Ge, PREC_CMP, false),
            TokenKind::AndAnd => (BinOp::And, PREC_AND, true),
            TokenKind::OrOr => (BinOp::Or, PREC_OR, true),
            _ => return None,
        })
    }

    fn mul_op(&self) -> BinOp {
        match self.kind() {
            TokenKind::Star => BinOp::Mul,
            TokenKind::Slash => BinOp::Div,
            _ => BinOp::Rem,
        }
    }

    fn add_op(&self) -> BinOp {
        if self.at(TokenKind::Plus) {
            BinOp::Add
        } else {
            BinOp::Sub
        }
    }

    fn shift_op(&self) -> BinOp {
        if self.at(TokenKind::Shl) {
            BinOp::Shl
        } else {
            BinOp::Shr
        }
    }

    fn parse_unary(&mut self) -> ExprId {
        let op = match self.kind() {
            TokenKind::Not => UnOp::Not,
            TokenKind::Minus => UnOp::Neg,
            TokenKind::Tilde => UnOp::BitNot,
            TokenKind::And => UnOp::Ref,
            TokenKind::Star => UnOp::Deref,
            _ => return self.parse_postfix(),
        };
        let mark = self.start();
        self.bump();
        let expr = self.parse_unary();
        self.push_expr(mark, ExprKind::Unary { op, expr })
    }

    fn parse_postfix(&mut self) -> ExprId {
        let expr = self.parse_primary();
        self.parse_postfix_suffix(expr)
    }

    fn parse_postfix_suffix(&mut self, mut expr: ExprId) -> ExprId {
        loop {
            if self.at_line_end() {
                break;
            }
            match self.kind() {
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_expr_list(TokenKind::RParen);
                    self.expect(TokenKind::RParen, "调用需要 `)`");
                    expr = self.make_call(expr, args);
                }
                TokenKind::PathSep if self.nth(1) == TokenKind::LBracket => {
                    self.bump();
                    let args = self.parse_generic_args_required();
                    expr = self.wrap_turbofish(expr, args);
                }
                TokenKind::LBracket => expr = self.parse_index(expr),
                TokenKind::Dot => expr = self.parse_dot(expr),
                TokenKind::Question => {
                    self.bump();
                    expr = self.wrap(expr, ExprKind::TryOp(expr));
                }
                _ => break,
            }
        }
        expr
    }

    fn make_call(&mut self, callee: ExprId, args: AstRange<ExprId>) -> ExprId {
        let (callee, type_args) = self.split_type_args(callee);
        self.wrap(
            callee,
            ExprKind::Call {
                callee,
                type_args,
                args,
            },
        )
    }

    fn split_type_args(&mut self, expr: ExprId) -> (ExprId, AstRange<GenericArg>) {
        match self.arena.exprs[expr.0 as usize].kind {
            ExprKind::TypeApp { base, args } => (base, args),
            _ => (expr, AstRange::empty()),
        }
    }

    fn wrap_turbofish(&mut self, base: ExprId, args: AstRange<GenericArg>) -> ExprId {
        self.wrap(base, ExprKind::TypeApp { base, args })
    }

    fn parse_index(&mut self, base: ExprId) -> ExprId {
        self.bump();
        let index = if self.at(TokenKind::DotDot) {
            self.bump();
            let end = if self.at(TokenKind::RBracket) {
                None
            } else {
                Some(self.parse_expr_no_range())
            };
            IndexKind::Range { start: None, end }
        } else {
            let start = self.parse_expr_no_range();
            if self.eat(TokenKind::DotDot) {
                let end = if self.at(TokenKind::RBracket) {
                    None
                } else {
                    Some(self.parse_expr_no_range())
                };
                IndexKind::Range {
                    start: Some(start),
                    end,
                }
            } else {
                IndexKind::Expr(start)
            }
        };
        self.expect(TokenKind::RBracket, "下标需要 `]`");
        self.wrap(base, ExprKind::Index { base, index })
    }

    fn parse_dot(&mut self, base: ExprId) -> ExprId {
        self.bump();
        if self.at(TokenKind::KwMatch) {
            self.bump();
            let arms = self.parse_match_arms();
            return self.wrap(
                base,
                ExprKind::Match {
                    scrutinee: base,
                    arms,
                },
            );
        }
        if self.at(TokenKind::Int) {
            let token = self.bump();
            let span = self.token_span(token);
            let text = token.text(self.source);
            let index = match text.parse::<u32>() {
                Ok(index) => index,
                Err(_) => {
                    self.error_span(
                        DiagnosticCode::ParseExpected,
                        "元组字段索引超出 u32 范围",
                        span,
                    );
                    0
                }
            };
            return self.wrap(base, ExprKind::TupleField { base, index });
        }
        let name_tok = self.expect(TokenKind::Ident, "字段需要标识符");
        let name = self.interned_symbol(name_tok);
        self.wrap(base, ExprKind::Field { base, name })
    }

    fn parse_primary(&mut self) -> ExprId {
        if self.peek_source() {
            let mark = self.start();
            self.bump();
            self.bump();
            let body = self.parse_block_expr();
            return self.push_expr(mark, ExprKind::SourceMacro { body });
        }
        match self.kind() {
            TokenKind::Ident => self.parse_path_primary(),
            TokenKind::KwTrue | TokenKind::KwFalse => self.parse_bool(),
            TokenKind::Int | TokenKind::Float | TokenKind::Char | TokenKind::ByteChar => {
                self.parse_number_or_char()
            }
            TokenKind::String
            | TokenKind::ByteString
            | TokenKind::CString
            | TokenKind::RawString => self.parse_string_lit(),
            TokenKind::FStringStart => self.parse_fstring(),
            TokenKind::LBrace => self.parse_block_expr(),
            TokenKind::KwUnsafe => self.parse_unsafe(),
            TokenKind::KwComptime => self.parse_comptime(),
            TokenKind::KwSizeOf | TokenKind::KwAlignOf | TokenKind::KwTypeId => {
                self.parse_intrinsic()
            }
            TokenKind::KwOffsetOf => self.parse_offset_of(),
            TokenKind::KwTypeIdCount => self.parse_type_id_count(),
            TokenKind::KwAsm => self.parse_asm(),
            TokenKind::KwChan => self.parse_chan(),
            TokenKind::LParen => self.parse_paren_or_tuple(),
            TokenKind::LBracket => self.parse_array(),
            TokenKind::Error => {
                let mark = self.start();
                self.consume_error_token();
                self.push_expr(mark, ExprKind::Error)
            }
            _ => {
                let mark = self.start();
                self.error_here(DiagnosticCode::ParseUnexpected, "此处需要表达式");
                self.error_expr(mark)
            }
        }
    }

    fn parse_path_primary(&mut self) -> ExprId {
        let mark = self.start();
        let path = if self.looks_like_type_path_generics() {
            self.parse_path_with_args()
        } else {
            self.parse_path()
        };
        if self.allow_struct && self.at(TokenKind::LBrace) && !self.newline_before_current() {
            self.bump();
            let fields = self.parse_field_expr_list();
            self.expect(TokenKind::RBrace, "结构体字面量需要 `}`");
            return self.push_expr(mark, ExprKind::Struct { path, fields });
        }
        self.push_expr(mark, ExprKind::Path(path))
    }

    fn looks_like_type_path_generics(&self) -> bool {
        if !self.at(TokenKind::Ident) {
            return false;
        }
        let mut index = self.cursor + 1;
        while index < self.tokens.len() {
            match self.tokens[index].kind {
                TokenKind::PathSep => {
                    index += 1;
                    if self
                        .tokens
                        .get(index)
                        .is_none_or(|token| token.kind != TokenKind::Ident)
                    {
                        return false;
                    }
                    index += 1;
                }
                TokenKind::LBracket => {
                    let mut depth = 0_u32;
                    for (offset, token) in self.tokens[index..].iter().enumerate() {
                        match token.kind {
                            TokenKind::LBracket => depth += 1,
                            TokenKind::RBracket => {
                                depth = depth.saturating_sub(1);
                                if depth == 0 {
                                    return matches!(
                                        self.tokens.get(index + offset + 1).map(|next| next.kind),
                                        Some(
                                            TokenKind::PathSep
                                                | TokenKind::LBrace
                                                | TokenKind::LParen
                                        )
                                    );
                                }
                            }
                            TokenKind::Eof => return false,
                            _ => {}
                        }
                    }
                    return false;
                }
                _ => return false,
            }
        }
        false
    }

    fn parse_field_expr_list(&mut self) -> AstRange<FieldExpr> {
        let mut fields = Vec::new();
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            let attrs = self.parse_outer_attributes();
            let name_tok = self.expect(TokenKind::Ident, "字段需要名字");
            let value = if self.eat(TokenKind::Colon) {
                Some(self.parse_expression())
            } else {
                None
            };
            fields.push(FieldExpr {
                attributes: self.store_attrs(attrs),
                name: self.interned_symbol(name_tok),
                span: self.token_span(name_tok),
                value,
            });
            if !self.eat(TokenKind::Comma) && !self.at_list_separator() {
                break;
            }
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.field_exprs, fields)
    }

    fn parse_bool(&mut self) -> ExprId {
        let mark = self.start();
        let value = self.at(TokenKind::KwTrue);
        self.bump();
        self.push_expr(mark, ExprKind::Literal(LitKind::Bool(value)))
    }

    fn parse_number_or_char(&mut self) -> ExprId {
        let mark = self.start();
        let token = self.current();
        let kind = match token.kind {
            TokenKind::Int => {
                let (radix, limbs) = self.parse_int_limbs(token.text(self.source));
                LitKind::Int { radix, limbs }
            }
            TokenKind::Float => {
                let (digits, exp10) = self.parse_float_parts(token.text(self.source));
                LitKind::Float { digits, exp10 }
            }
            TokenKind::Char => {
                let text = token.text(self.source);
                LitKind::Char {
                    value: self.parse_char_value(text),
                    text: self.interned_symbol(token),
                }
            }
            TokenKind::ByteChar => {
                let text = token.text(self.source);
                LitKind::ByteChar {
                    value: self.parse_byte_char_value(text),
                    text: self.interned_symbol(token),
                }
            }
            _ => unreachable!(),
        };
        self.bump();
        self.push_expr(mark, ExprKind::Literal(kind))
    }

    fn parse_string_lit(&mut self) -> ExprId {
        let mark = self.start();
        let token = self.bump();
        let text = self.interned_symbol(token);
        let kind = match token.kind {
            TokenKind::ByteString => LitKind::ByteString { text },
            TokenKind::CString => LitKind::CString { text },
            TokenKind::RawString => LitKind::RawString { text },
            _ => LitKind::String { text },
        };
        self.push_expr(mark, ExprKind::Literal(kind))
    }

    fn parse_fstring(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let mut parts = Vec::new();
        while !self.at_any(&[TokenKind::FStringEnd, TokenKind::Eof]) {
            match self.kind() {
                TokenKind::FStringText => {
                    let token = self.bump();
                    parts.push(FStringPart::Text {
                        text: self.interned_symbol(token),
                        span: self.token_span(token),
                    });
                }
                TokenKind::FStringInterpOpen => {
                    self.bump();
                    let interp_mark = self.start();
                    let expr = self.parse_expression();
                    let spec = if self.at(TokenKind::FormatSpec) {
                        let token = self.bump();
                        Some(self.interned_symbol(token))
                    } else {
                        None
                    };
                    self.expect(TokenKind::FStringInterpClose, "插值需要 `}`");
                    parts.push(FStringPart::Interp {
                        span: self.finish_span(interp_mark),
                        expr,
                        spec,
                    });
                }
                _ => {
                    self.error_here(DiagnosticCode::ParseUnexpected, "f-string 片段非法");
                    self.bump();
                }
            }
        }
        self.expect(TokenKind::FStringEnd, "f-string 未结束");
        let parts = finish_extend(&mut self.diagnostics, &mut self.arena.fstring_parts, parts);
        self.push_expr(mark, ExprKind::FString { parts })
    }

    fn parse_unsafe(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let block = self.parse_block_expr();
        self.push_expr(mark, ExprKind::Unsafe(block))
    }

    fn parse_comptime(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let inner = if self.at(TokenKind::LBrace) {
            self.parse_block_expr()
        } else {
            self.parse_unary()
        };
        self.push_expr(mark, ExprKind::Comptime(inner))
    }

    fn parse_intrinsic(&mut self) -> ExprId {
        let mark = self.start();
        let kind = match self.kind() {
            TokenKind::KwSizeOf => IntrinsicKind::SizeOf,
            TokenKind::KwAlignOf => IntrinsicKind::AlignOf,
            _ => IntrinsicKind::TypeId,
        };
        self.bump();
        let tys = self.parse_generic_args_required();
        self.expect(TokenKind::LParen, "intrinsic 需要 `()`");
        self.expect(TokenKind::RParen, "intrinsic 需要 `()`");
        self.push_expr(
            mark,
            ExprKind::Intrinsic {
                kind,
                tys,
                args: AstRange::empty(),
                field: None,
            },
        )
    }

    fn parse_offset_of(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let tys = self.parse_generic_args_required();
        self.expect(TokenKind::LParen, "offset_of 需要 `(`");
        let field = if self.at(TokenKind::Ident) || self.at(TokenKind::Int) {
            let token = self.bump();
            Some(self.interned_symbol(token))
        } else {
            self.error_here(DiagnosticCode::ParseExpected, "offset_of 需要字段名或 `0`");
            None
        };
        self.expect(TokenKind::RParen, "offset_of 需要 `)`");
        self.push_expr(
            mark,
            ExprKind::Intrinsic {
                kind: IntrinsicKind::OffsetOf,
                tys,
                args: AstRange::empty(),
                field,
            },
        )
    }

    fn parse_type_id_count(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        self.expect(TokenKind::LParen, "type_id_count 需要 `()`");
        self.expect(TokenKind::RParen, "type_id_count 需要 `()`");
        self.push_expr(
            mark,
            ExprKind::Intrinsic {
                kind: IntrinsicKind::TypeIdCount,
                tys: AstRange::empty(),
                args: AstRange::empty(),
                field: None,
            },
        )
    }

    fn parse_chan(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let tys = self.parse_generic_args_required();
        self.expect(TokenKind::LParen, "chan 构造需要 `(`");
        let arg = self.parse_expression();
        self.expect(TokenKind::RParen, "chan 构造需要 `)`");
        let args = finish_extend(&mut self.diagnostics, &mut self.arena.expr_ids, [arg]);
        self.push_expr(
            mark,
            ExprKind::Intrinsic {
                kind: IntrinsicKind::Chan,
                tys,
                args,
                field: None,
            },
        )
    }

    fn parse_asm(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        self.expect(TokenKind::LParen, "asm 需要 `(`");
        let template = match self.kind() {
            TokenKind::String | TokenKind::RawString => {
                let token = self.bump();
                self.interned_symbol(token)
            }
            _ => {
                self.error_here(DiagnosticCode::ParseExpected, "asm 需要字符串模板");
                let mark = self.start();
                self.error_expr(mark);
                return self.push_expr(mark, ExprKind::Error);
            }
        };
        let mut operands = Vec::new();
        while self.eat(TokenKind::Comma) {
            if self.at(TokenKind::RParen) {
                break;
            }
            operands.push(self.parse_asm_operand());
        }
        self.expect(TokenKind::RParen, "asm 需要 `)`");
        let operands = finish_extend(
            &mut self.diagnostics,
            &mut self.arena.asm_operands,
            operands,
        );
        self.push_expr(mark, ExprKind::Asm { template, operands })
    }

    fn parse_asm_operand(&mut self) -> super::super::ast::AsmOperand {
        let start = self.current().start;
        let name = self.text();
        let kind = if name == "in" || name == "out" || name == "lateout" || name == "clobber" {
            self.bump();
            self.expect(TokenKind::LParen, "asm 操作数需要 `(`");
            match name {
                "clobber" => {
                    let mut regs = Vec::new();
                    while !self.at_any(&[TokenKind::RParen, TokenKind::Eof]) {
                        let token = self.expect(TokenKind::String, "clobber 需要寄存器名");
                        regs.push(self.interned_symbol(token));
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen, "clobber 需要 `)`");
                    if regs.is_empty() {
                        self.error_here(
                            DiagnosticCode::ParseExpected,
                            "clobber 至少需要一个寄存器",
                        );
                    }
                    super::super::ast::AsmOperandKind::Clobber {
                        regs: finish_extend(&mut self.diagnostics, &mut self.arena.symbols, regs),
                    }
                }
                "in" => {
                    let reg_token = self.expect(TokenKind::String, "in 需要寄存器名");
                    let reg = self.interned_symbol(reg_token);
                    self.expect(TokenKind::RParen, "in 需要 `)`");
                    let expr = self.parse_expression();
                    super::super::ast::AsmOperandKind::In { reg, expr }
                }
                "out" => {
                    let reg_token = self.expect(TokenKind::String, "out 需要寄存器名");
                    let reg = self.interned_symbol(reg_token);
                    self.expect(TokenKind::RParen, "out 需要 `)`");
                    let place = self.parse_expression();
                    super::super::ast::AsmOperandKind::Out { reg, place }
                }
                _ => {
                    let reg_token = self.expect(TokenKind::String, "lateout 需要寄存器名");
                    let reg = self.interned_symbol(reg_token);
                    self.expect(TokenKind::RParen, "lateout 需要 `)`");
                    let place = self.parse_expression();
                    super::super::ast::AsmOperandKind::Lateout { reg, place }
                }
            }
        } else {
            self.error_here(DiagnosticCode::ParseUnexpected, "未知 asm 操作数");
            super::super::ast::AsmOperandKind::Clobber {
                regs: AstRange::empty(),
            }
        };
        super::super::ast::AsmOperand {
            span: self.make_span(start, self.tokens[self.cursor.saturating_sub(1)].end),
            kind,
        }
    }

    fn parse_paren_or_tuple(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        if self.eat(TokenKind::RParen) {
            return self.push_expr(mark, ExprKind::Tuple(AstRange::empty()));
        }
        let first = self.parse_expression();
        if self.eat(TokenKind::Comma) {
            let mut elems = vec![first];
            while !self.at_any(&[TokenKind::RParen, TokenKind::Eof]) {
                elems.push(self.parse_expression());
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RParen, "元组需要 `)`");
            let elems = finish_extend(&mut self.diagnostics, &mut self.arena.expr_ids, elems);
            return self.push_expr(mark, ExprKind::Tuple(elems));
        }
        self.expect(TokenKind::RParen, "括号表达式需要 `)`");
        self.push_expr(mark, ExprKind::Paren(first))
    }

    fn parse_array(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        if self.eat(TokenKind::RBracket) {
            return self.push_expr(mark, ExprKind::Array(AstRange::empty()));
        }
        let first = self.parse_expression();
        if self.eat(TokenKind::Semi) {
            let count = self.parse_expression();
            self.expect(TokenKind::RBracket, "重复数组需要 `]`");
            return self.push_expr(mark, ExprKind::Repeat { elem: first, count });
        }
        let mut elems = vec![first];
        while self.eat(TokenKind::Comma) {
            if self.at(TokenKind::RBracket) {
                break;
            }
            elems.push(self.parse_expression());
        }
        self.expect(TokenKind::RBracket, "数组需要 `]`");
        let elems = finish_extend(&mut self.diagnostics, &mut self.arena.expr_ids, elems);
        self.push_expr(mark, ExprKind::Array(elems))
    }

    fn without_struct<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let previous = self.allow_struct;
        self.allow_struct = false;
        let value = f(self);
        self.allow_struct = previous;
        value
    }

    fn parse_if(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let cond = self.without_struct(|parser| parser.parse_condition());
        let then_block = self.parse_block_expr();
        let else_branch = if self.eat(TokenKind::KwElse) {
            if self.at(TokenKind::KwIf) {
                Some(self.parse_if())
            } else {
                Some(self.parse_block_expr())
            }
        } else {
            None
        };
        self.push_expr(
            mark,
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            },
        )
    }

    fn parse_condition(&mut self) -> ExprId {
        let start = self.cursor;
        let mut left = self.parse_condition_and();
        while self.eat(TokenKind::OrOr) {
            let right = self.parse_condition_and();
            if self.tokens[start..self.cursor]
                .iter()
                .any(|token| token.kind == TokenKind::KwLet)
            {
                self.error_here(
                    DiagnosticCode::ParseUnexpected,
                    "let 链只能使用 &&，不能与 || 混合",
                );
            }
            left = self.make_binary(BinOp::Or, left, right);
        }
        left
    }

    fn parse_condition_and(&mut self) -> ExprId {
        let mut left = self.parse_condition_part();
        while self.eat(TokenKind::AndAnd) {
            let right = self.parse_condition_part();
            left = self.make_binary(BinOp::And, left, right);
        }
        left
    }

    fn parse_condition_part(&mut self) -> ExprId {
        if self.at(TokenKind::KwLet) {
            self.parse_let_condition()
        } else {
            self.parse_binary(PREC_AND + 1, true)
        }
    }

    fn parse_let_condition(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let pat = self.parse_pat();
        self.expect(TokenKind::Eq, "let 条件需要 `=`");
        let init = self.parse_binary(PREC_AND + 1, false);
        let stmt_id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return self.push_expr(mark, ExprKind::Error);
            }
        };
        let span = self.finish_span(mark);
        let stmt = match self.arena.try_push_stmt(Stmt {
            attributes: AstRange::empty(),
            id: stmt_id,
            span,
            kind: StmtKind::Let {
                pat,
                ty: None,
                init: Some(init),
                else_block: None,
            },
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return self.push_expr(mark, ExprKind::Error);
            }
        };
        let stmts = finish_extend(&mut self.diagnostics, &mut self.arena.stmt_ids, [stmt]);
        self.push_expr(mark, ExprKind::Block { stmts, tail: None })
    }

    fn parse_match(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let scrutinee = self.without_struct(|parser| parser.parse_expression());
        let arms = self.parse_match_arms();
        self.push_expr(mark, ExprKind::Match { scrutinee, arms })
    }

    fn parse_match_arms(&mut self) -> AstRange<MatchArm> {
        self.expect(TokenKind::LBrace, "match 需要 `{`");
        let mut arms = Vec::new();
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            let arm_mark = self.start();
            let attrs = self.parse_outer_attributes();
            let pat = self.parse_pat();
            let guard = if self.eat(TokenKind::KwIf) {
                Some(self.parse_expression())
            } else {
                None
            };
            self.expect(TokenKind::FatArrow, "match 臂需要 `=>`");
            let body = self.parse_expression();
            arms.push(MatchArm {
                attributes: self.store_attrs(attrs),
                id: arm_mark.id,
                span: self.finish_span(arm_mark),
                pat,
                guard,
                body,
            });
            if !self.at(TokenKind::RBrace) {
                if !self.eat(TokenKind::Comma) && !self.at_list_separator() {
                    self.error_here(DiagnosticCode::ParseExpected, "match 臂之间需要 `,` 或换行");
                }
            }
        }
        self.expect(TokenKind::RBrace, "match 需要 `}`");
        finish_extend(&mut self.diagnostics, &mut self.arena.match_arms, arms)
    }

    fn parse_try(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let block = self.parse_block_expr();
        self.push_expr(mark, ExprKind::Try(block))
    }

    fn parse_loop(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let body = self.parse_block_expr();
        self.push_expr(mark, ExprKind::Loop(body))
    }

    fn parse_while(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let cond = self.without_struct(|parser| parser.parse_condition());
        let body = self.parse_block_expr();
        self.push_expr(mark, ExprKind::While { cond, body })
    }

    fn parse_for(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let pat = self.parse_pat();
        self.expect(TokenKind::KwIn, "for 需要 `in`");
        let iter = self.without_struct(|parser| parser.parse_expression());
        let body = self.parse_block_expr();
        self.push_expr(mark, ExprKind::For { pat, iter, body })
    }

    fn parse_select(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        self.expect(TokenKind::LBrace, "select 需要 `{`");
        let mut arms = Vec::new();
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            let mark = self.start();
            let attrs = self.parse_outer_attributes();
            arms.push(self.parse_select_arm(mark, attrs));
            if !self.at(TokenKind::RBrace) {
                if !self.eat(TokenKind::Comma) && !self.at_list_separator() {
                    self.error_here(
                        DiagnosticCode::ParseExpected,
                        "select 臂之间需要 `,` 或换行",
                    );
                }
            }
        }
        self.expect(TokenKind::RBrace, "select 需要 `}`");
        let arms = finish_extend(&mut self.diagnostics, &mut self.arena.select_arms, arms);
        self.push_expr(mark, ExprKind::Select { arms })
    }

    fn parse_select_arm(&mut self, mark: Mark, attrs: Vec<Attribute>) -> SelectArm {
        if self.at(TokenKind::Ident) && self.text() == "_" {
            self.bump();
            self.expect(TokenKind::FatArrow, "select 默认臂需要 `=>`");
            let body = self.parse_expression();
            return SelectArm {
                attributes: self.store_attrs(attrs),
                id: mark.id,
                span: self.finish_span(mark),
                kind: SelectArmKind::Default { body },
            };
        }
        if self.at(TokenKind::KwLet) {
            return self.parse_select_let_arm(mark, attrs);
        }
        let chan = self.parse_postfix();
        self.expect_send_arm(mark, attrs, chan)
    }

    fn parse_select_let_arm(&mut self, mark: Mark, attrs: Vec<Attribute>) -> SelectArm {
        self.bump();
        let pat = self.parse_pat();
        self.expect(TokenKind::Eq, "select 接收臂需要 `=`");
        let recv = self.parse_postfix();
        self.expect(TokenKind::FatArrow, "select 臂需要 `=>`");
        let body = self.parse_expression();
        let kind = match self.select_call(recv) {
            Some(SelectCall::Recv { chan }) => SelectArmKind::Recv { pat, chan, body },
            Some(SelectCall::Wait { join }) => SelectArmKind::Wait { pat, join, body },
            _ => {
                self.error_span(
                    DiagnosticCode::ParseInvalidSelectArm,
                    "select 接收/等待臂必须是 `.recv()` 或 `.wait()`",
                    self.expr_span(recv),
                );
                SelectArmKind::Error
            }
        };
        SelectArm {
            attributes: self.store_attrs(attrs),
            id: mark.id,
            span: self.finish_span(mark),
            kind,
        }
    }

    fn expect_send_arm(&mut self, mark: Mark, attrs: Vec<Attribute>, expr: ExprId) -> SelectArm {
        self.expect(TokenKind::FatArrow, "select 发送臂需要 `=>`");
        let body = self.parse_expression();
        let kind = match self.select_call(expr) {
            Some(SelectCall::Send { chan, payload }) => SelectArmKind::Send {
                chan,
                payload,
                body,
            },
            _ => {
                self.error_span(
                    DiagnosticCode::ParseInvalidSelectArm,
                    "select 发送臂必须是 `.send(...)`",
                    self.expr_span(expr),
                );
                SelectArmKind::Error
            }
        };
        SelectArm {
            attributes: self.store_attrs(attrs),
            id: mark.id,
            span: self.finish_span(mark),
            kind,
        }
    }

    fn parse_async(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let inner = if self.at(TokenKind::LBrace) {
            self.parse_block_expr()
        } else {
            self.parse_async_operand()
        };
        let expr = self.push_expr(mark, ExprKind::Async(inner));
        self.parse_postfix_suffix(expr)
    }

    fn parse_async_operand(&mut self) -> ExprId {
        let mut expr = self.parse_primary();
        loop {
            match self.kind() {
                TokenKind::Dot => expr = self.parse_dot(expr),
                TokenKind::PathSep if self.nth(1) == TokenKind::LBracket => {
                    self.bump();
                    let args = self.parse_generic_args_required();
                    expr = self.wrap_turbofish(expr, args);
                }
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_expr_list(TokenKind::RParen);
                    self.expect(TokenKind::RParen, "调用需要 `)`");
                    return self.make_call(expr, args);
                }
                _ => {
                    self.error_here(DiagnosticCode::ParseExpected, "async 操作数必须是一次调用");
                    return expr;
                }
            }
        }
    }

    fn parse_closure(&mut self) -> ExprId {
        let mark = self.start();
        let fn_id = self.parse_fn_decl(super::item::FnDeclContext::Closure, false, None);
        self.push_expr(mark, ExprKind::Closure(fn_id))
    }

    fn parse_return(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let value = if self.at_expr_continue() {
            Some(self.parse_expression())
        } else {
            None
        };
        self.push_expr(mark, ExprKind::Return(value))
    }

    fn parse_break(&mut self) -> ExprId {
        let mark = self.start();
        self.bump();
        let value = if self.at_expr_continue() {
            Some(self.parse_expression())
        } else {
            None
        };
        self.push_expr(mark, ExprKind::Break(value))
    }

    fn at_expr_continue(&self) -> bool {
        !self.at_any(&[
            TokenKind::RBrace,
            TokenKind::RParen,
            TokenKind::RBracket,
            TokenKind::Comma,
            TokenKind::Semi,
            TokenKind::Eof,
            TokenKind::KwElse,
        ]) && !self.at_line_end()
    }

    fn parse_expr_list(&mut self, close: TokenKind) -> AstRange<ExprId> {
        let mut exprs = Vec::new();
        if self.at(close) {
            return AstRange::empty();
        }
        loop {
            exprs.push(self.parse_expression());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.at(close) {
                break;
            }
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.expr_ids, exprs)
    }

    fn make_binary(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
        let start = self.expr_span(lhs).start();
        let end = self.expr_span(rhs).end();
        let id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return ExprId(u32::MAX);
            }
        };
        match self.arena.try_push_expr(Expr {
            id,
            span: self.make_span(start, end),
            attributes: AstRange::empty(),
            kind: ExprKind::Binary { op, lhs, rhs },
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                ExprId(u32::MAX)
            }
        }
    }

    fn make_range(&mut self, start: ExprId, end: ExprId) -> ExprId {
        let lo = self.expr_span(start).start();
        let hi = self.expr_span(end).end();
        let id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return ExprId(u32::MAX);
            }
        };
        match self.arena.try_push_expr(Expr {
            id,
            span: self.make_span(lo, hi),
            attributes: AstRange::empty(),
            kind: ExprKind::Range { start, end },
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                ExprId(u32::MAX)
            }
        }
    }

    fn select_call(&mut self, expr: ExprId) -> Option<SelectCall> {
        let ExprKind::Call {
            callee,
            type_args: _,
            args,
        } = self.arena.exprs[expr.0 as usize].kind
        else {
            return None;
        };
        let (name, receiver) = self.select_callee(callee)?;
        let arg_slice = args.as_slice(&self.arena.expr_ids);
        let payload = arg_slice.first().copied();
        match name {
            "send" => {
                if arg_slice.len() != 1 {
                    self.error_span(
                        DiagnosticCode::ParseInvalidSelectArm,
                        "select 发送臂的 send 必须恰好一个参数",
                        self.expr_span(expr),
                    );
                    return None;
                }
                Some(SelectCall::Send {
                    chan: receiver,
                    payload: payload?,
                })
            }
            "recv" if payload.is_none() => Some(SelectCall::Recv { chan: receiver }),
            "wait" if payload.is_none() => Some(SelectCall::Wait { join: receiver }),
            _ => None,
        }
    }

    fn select_callee(&mut self, callee: ExprId) -> Option<(&'static str, ExprId)> {
        match self.arena.exprs[callee.0 as usize].kind {
            ExprKind::Field { name, base } => Some((self.select_method_name(name)?, base)),
            ExprKind::Path(path) => {
                let (receiver, name) = self.path_receiver_and_last(path)?;
                Some((self.select_method_name(name)?, receiver))
            }
            _ => None,
        }
    }

    fn select_method_name(&self, name: Symbol) -> Option<&'static str> {
        match self.intern.get_str(name) {
            "send" => Some("send"),
            "recv" => Some("recv"),
            "wait" => Some("wait"),
            _ => None,
        }
    }

    fn path_receiver_and_last(&mut self, path: PathId) -> Option<(ExprId, Symbol)> {
        let (prefix, last, lo, hi) = {
            let node = &self.arena.paths[path.0 as usize];
            let segs = node.segments.as_slice(&self.arena.segments);
            if segs.len() < 2 {
                return None;
            }
            let last = segs[segs.len() - 1].name;
            let prefix = segs[..segs.len() - 1].to_vec();
            let lo = prefix[0].span.start();
            let hi = prefix[prefix.len() - 1].span.end();
            (prefix, last, lo, hi)
        };
        let path_id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return None;
            }
        };
        let segments = finish_extend(&mut self.diagnostics, &mut self.arena.segments, prefix);
        let path = match self.arena.try_push_path(Path {
            id: path_id,
            span: self.make_span(lo, hi),
            segments,
            args: AstRange::empty(),
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return None;
            }
        };
        let expr_id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return None;
            }
        };
        let receiver = match self.arena.try_push_expr(Expr {
            id: expr_id,
            span: self.make_span(lo, hi),
            attributes: AstRange::empty(),
            kind: ExprKind::Path(path),
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return None;
            }
        };
        Some((receiver, last))
    }

    fn wrap(&mut self, base: ExprId, kind: ExprKind) -> ExprId {
        let start = self.expr_span(base).start();
        let end = self.tokens[self.cursor.saturating_sub(1)].end;
        let id = match self.arena.alloc_node_or_error(self.file) {
            Some(id) => id,
            None => {
                self.arena_limit();
                return ExprId(u32::MAX);
            }
        };
        match self.arena.try_push_expr(Expr {
            id,
            span: self.make_span(start, end),
            attributes: AstRange::empty(),
            kind,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                ExprId(u32::MAX)
            }
        }
    }

    fn push_expr(&mut self, mark: Mark, kind: ExprKind) -> ExprId {
        let span = self.finish_span(mark);
        match self.arena.try_push_expr(Expr {
            id: mark.id,
            span,
            attributes: AstRange::empty(),
            kind,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                ExprId(u32::MAX)
            }
        }
    }
}
