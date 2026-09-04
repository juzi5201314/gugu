use crate::diagnostics::DiagnosticCode;

use super::super::ast::{
    ArenaLens, AstRange, Bound, BoundKind, GenericArg, GenericParam, GenericParamKind, Param, Path,
    PathSegment, Ty, TyId, TyKind, extend_range,
};
use super::super::token::TokenKind;
use super::Parser;

type Checkpoint = (usize, u32, usize, ArenaLens);

impl Parser<'_> {
    pub(super) fn at_type_start(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::Not
                | TokenKind::Ident
                | TokenKind::And
                | TokenKind::Star
                | TokenKind::KwFn
                | TokenKind::LBracket
                | TokenKind::LParen
                | TokenKind::KwDyn
                | TokenKind::KwImpl
                | TokenKind::KwComptime
                | TokenKind::KwChan
                | TokenKind::KwUnsafe
        ) || self.ident_text_is("Self")
    }

    pub(super) fn parse_ty(&mut self) -> TyId {
        let mark = self.start();
        match self.kind() {
            TokenKind::Not => {
                self.bump();
                self.push_ty(mark, TyKind::Never)
            }
            TokenKind::And => self.parse_ref_or_slice(mark),
            TokenKind::Star => {
                self.bump();
                let inner = self.parse_ty();
                self.push_ty(mark, TyKind::Ptr(inner))
            }
            TokenKind::KwFn => self.parse_fn_ty(mark),
            TokenKind::LBracket => self.parse_array_ty(mark),
            TokenKind::LParen => self.parse_tuple_ty(mark),
            TokenKind::KwDyn => self.parse_dyn_ty(mark),
            TokenKind::KwImpl => self.parse_impl_ty(mark),
            TokenKind::KwChan => {
                self.bump();
                let args = self.parse_generic_args_required();
                self.push_ty(mark, TyKind::Chan(args))
            }
            TokenKind::KwComptime if self.peek_source() => {
                self.bump();
                self.bump();
                let body = self.parse_block_expr();
                self.push_ty(mark, TyKind::SourceMacro { body })
            }
            TokenKind::Ident if self.text() == "_" => {
                self.bump();
                self.push_ty(mark, TyKind::Infer)
            }
            TokenKind::Ident => {
                let path = self.parse_path_with_args();
                self.push_ty(mark, TyKind::Path(path))
            }
            _ => self.error_ty(mark),
        }
    }

    fn parse_ref_or_slice(&mut self, mark: super::Mark) -> TyId {
        self.bump();
        if self.eat(TokenKind::LBracket) {
            let elem = self.parse_ty();
            if self.eat(TokenKind::Semi) {
                let len = self.parse_expression();
                self.expect(TokenKind::RBracket, "数组类型需要 `]`");
                let array = {
                    let array_mark = self.start_at(mark.start);
                    self.push_ty(array_mark, TyKind::Array { elem, len })
                };
                return self.push_ty(mark, TyKind::Ref(array));
            }
            self.expect(TokenKind::RBracket, "切片类型需要 `]`");
            return self.push_ty(mark, TyKind::Slice(elem));
        }
        let inner = self.parse_ty();
        self.push_ty(mark, TyKind::Ref(inner))
    }

    fn parse_array_ty(&mut self, mark: super::Mark) -> TyId {
        self.bump();
        let elem = self.parse_ty();
        self.expect(TokenKind::Semi, "数组类型必须写成 `[T; N]`");
        let len = self.parse_expression();
        self.expect(TokenKind::RBracket, "数组类型需要 `]`");
        self.push_ty(mark, TyKind::Array { elem, len })
    }

    fn parse_tuple_ty(&mut self, mark: super::Mark) -> TyId {
        self.bump();
        if self.eat(TokenKind::RParen) {
            return self.push_ty(mark, TyKind::Tuple(AstRange::empty()));
        }
        let first = self.parse_ty();
        if !self.eat(TokenKind::Comma) {
            self.expect(TokenKind::RParen, "类型括号需要 `)`");
            return first;
        }
        let mut tys = vec![first];
        while !self.at_any(&[TokenKind::RParen, TokenKind::Eof]) {
            tys.push(self.parse_ty());
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen, "元组类型需要 `)`");
        let tys = extend_range(&mut self.arena.ty_ids, tys);
        self.push_ty(mark, TyKind::Tuple(tys))
    }

    fn parse_fn_ty(&mut self, mark: super::Mark) -> TyId {
        self.bump();
        self.expect(TokenKind::LParen, "函数类型需要 `(`");
        let params = self.parse_ty_list(TokenKind::RParen);
        self.expect(TokenKind::RParen, "函数类型需要 `)`");
        let ret = if self.newline_before_current() || !self.at_type_start() {
            None
        } else {
            Some(self.parse_ty())
        };
        self.push_ty(mark, TyKind::Fn { params, ret })
    }

    fn parse_dyn_ty(&mut self, mark: super::Mark) -> TyId {
        self.bump();
        let mut paths = vec![self.parse_path_with_args()];
        while self.eat(TokenKind::Plus) {
            paths.push(self.parse_path_with_args());
        }
        let paths = extend_range(&mut self.arena.path_ids, paths);
        self.push_ty(mark, TyKind::Dyn(paths))
    }

    fn parse_impl_ty(&mut self, mark: super::Mark) -> TyId {
        self.bump();
        let bounds = self.parse_bounds();
        self.push_ty(mark, TyKind::Impl(bounds))
    }

    pub(super) fn parse_ty_list(&mut self, close: TokenKind) -> AstRange<TyId> {
        let mut tys = Vec::new();
        if self.at(close) {
            return AstRange::empty();
        }
        loop {
            tys.push(self.parse_ty());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.at(close) {
                break;
            }
        }
        extend_range(&mut self.arena.ty_ids, tys)
    }

    pub(super) fn parse_generic_params(&mut self) -> AstRange<GenericParam> {
        if !self.eat(TokenKind::LBracket) {
            return AstRange::empty();
        }
        let mut params = Vec::new();
        while !self.at_any(&[TokenKind::RBracket, TokenKind::Eof]) {
            params.push(self.parse_generic_param());
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RBracket, "泛型参数表需要 `]`");
        extend_range(&mut self.arena.generic_params, params)
    }

    fn parse_generic_param(&mut self) -> GenericParam {
        let mark = self.start();
        if self.eat(TokenKind::KwComptime) {
            let name_tok = self.expect(TokenKind::Ident, "comptime 参数需要名字");
            self.expect(TokenKind::Colon, "comptime 参数需要类型");
            let ty = self.parse_ty();
            return GenericParam {
                id: mark.id,
                span: self.finish_span(mark),
                kind: GenericParamKind::Comptime {
                    name: Self::interned_symbol(name_tok),
                    name_span: self.token_span(name_tok),
                    ty,
                },
            };
        }
        let name_tok = self.expect(TokenKind::Ident, "泛型参数需要名字");
        let bounds = if self.eat(TokenKind::Colon) {
            self.parse_bounds()
        } else {
            AstRange::empty()
        };
        let pack = self.eat(TokenKind::DotDotDot);
        GenericParam {
            id: mark.id,
            span: self.finish_span(mark),
            kind: GenericParamKind::Type {
                name: Self::interned_symbol(name_tok),
                name_span: self.token_span(name_tok),
                bounds,
                pack,
            },
        }
    }

    fn parse_bounds(&mut self) -> AstRange<Bound> {
        let mut bounds = vec![self.parse_bound()];
        while self.eat(TokenKind::Plus) {
            bounds.push(self.parse_bound());
        }
        extend_range(&mut self.arena.bounds, bounds)
    }

    fn parse_bound(&mut self) -> Bound {
        let mark = self.start();
        if self.ident_text_is("Fn") && self.nth(1) == TokenKind::LParen {
            self.bump();
            self.bump();
            let params = self.parse_ty_list(TokenKind::RParen);
            self.expect(TokenKind::RParen, "`Fn` 绑定需要 `)`");
            let ret = if self.at_type_start() && !self.newline_before_current() {
                Some(self.parse_ty())
            } else {
                None
            };
            return Bound {
                id: mark.id,
                span: self.finish_span(mark),
                kind: BoundKind::Fn { params, ret },
            };
        }
        let path = self.parse_path_with_args();
        Bound {
            id: mark.id,
            span: self.finish_span(mark),
            kind: BoundKind::Path(path),
        }
    }

    pub(super) fn parse_generic_args_required(&mut self) -> AstRange<GenericArg> {
        self.expect(TokenKind::LBracket, "需要泛型实参 `[...]`");
        self.parse_generic_arg_list()
    }

    pub(super) fn parse_generic_args_optional(&mut self) -> AstRange<GenericArg> {
        if !self.eat(TokenKind::LBracket) {
            return AstRange::empty();
        }
        self.parse_generic_arg_list()
    }

    fn parse_generic_arg_list(&mut self) -> AstRange<GenericArg> {
        let mut args = Vec::new();
        while !self.at_any(&[TokenKind::RBracket, TokenKind::Eof]) {
            args.push(self.parse_generic_arg());
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RBracket, "泛型实参表需要 `]`");
        extend_range(&mut self.arena.generic_args, args)
    }

    fn parse_generic_arg(&mut self) -> GenericArg {
        if self.at_literal_or_expr_only() {
            return GenericArg::Expr(self.parse_expression());
        }
        let saved = self.checkpoint();
        let ty = self.parse_ty();
        if self.at_any(&[TokenKind::RBracket, TokenKind::Comma, TokenKind::Eof])
            || self.at_line_end()
        {
            return GenericArg::Type(ty);
        }
        self.restore(saved);
        GenericArg::Expr(self.parse_expression())
    }

    fn at_literal_or_expr_only(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::Int
                | TokenKind::Float
                | TokenKind::Char
                | TokenKind::ByteChar
                | TokenKind::String
                | TokenKind::ByteString
                | TokenKind::CString
                | TokenKind::RawString
                | TokenKind::FStringStart
                | TokenKind::KwTrue
                | TokenKind::KwFalse
                | TokenKind::KwIf
                | TokenKind::KwMatch
                | TokenKind::KwLoop
                | TokenKind::Minus
        )
    }

    pub(super) fn parse_path(&mut self) -> super::super::ast::PathId {
        self.parse_path_common(false)
    }

    pub(super) fn parse_path_with_args(&mut self) -> super::super::ast::PathId {
        self.parse_path_common(true)
    }

    fn parse_path_common(&mut self, with_args: bool) -> super::super::ast::PathId {
        let mark = self.start();
        let mut segments = Vec::new();
        let first = self.expect_path_ident();
        segments.push(first);
        while self.at(TokenKind::Dot) && self.nth(1) == TokenKind::Ident {
            self.bump();
            segments.push(self.expect_path_ident_colon(false));
        }
        while self.eat(TokenKind::PathSep) {
            segments.push(self.expect_path_ident_colon(true));
        }
        let args = if with_args && self.at(TokenKind::LBracket) {
            self.parse_generic_args_optional()
        } else {
            AstRange::empty()
        };
        let segments = extend_range(&mut self.arena.segments, segments);
        let span = self.finish_span(mark);
        self.arena.push_path(Path {
            id: mark.id,
            span,
            segments,
            args,
        })
    }

    fn expect_path_ident(&mut self) -> PathSegment {
        self.expect_path_ident_colon(false)
    }

    fn expect_path_ident_colon(&mut self, colon: bool) -> PathSegment {
        let token = if self.ident_text_is("Self") || self.at(TokenKind::Ident) {
            self.bump()
        } else {
            self.expect(TokenKind::Ident, "路径需要标识符")
        };
        PathSegment {
            name: Self::interned_symbol(token),
            span: self.token_span(token),
            colon,
        }
    }

    pub(super) fn parse_param_list(&mut self, close: TokenKind) -> AstRange<Param> {
        let mut params = Vec::new();
        if self.at(close) {
            return AstRange::empty();
        }
        loop {
            params.push(self.parse_param());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.at(close) {
                break;
            }
        }
        extend_range(&mut self.arena.params, params)
    }

    fn parse_param(&mut self) -> Param {
        let mark = self.start();
        if self.at(TokenKind::DotDotDot) {
            self.bump();
            let _name = self.expect(TokenKind::Ident, "变参需要名字");
            self.expect(TokenKind::Colon, "变参需要类型");
            let ty = self.parse_ty();
            return Param {
                id: mark.id,
                span: self.finish_span(mark),
                comptime: false,
                variadic: true,
                pat: None,
                ty: Some(ty),
            };
        }
        let comptime = self.eat(TokenKind::KwComptime);
        let pat = self.parse_pat();
        let ty = if self.eat(TokenKind::Colon) {
            Some(self.parse_ty())
        } else if comptime {
            self.error_here(DiagnosticCode::ParseExpected, "comptime 参数需要类型");
            None
        } else {
            None
        };
        Param {
            id: mark.id,
            span: self.finish_span(mark),
            comptime,
            variadic: false,
            pat: Some(pat),
            ty,
        }
    }

    fn push_ty(&mut self, mark: super::Mark, kind: TyKind) -> TyId {
        let span = self.finish_span(mark);
        self.arena.push_ty(Ty {
            id: mark.id,
            span,
            kind,
        })
    }

    fn checkpoint(&self) -> Checkpoint {
        (
            self.cursor,
            self.delim_depth,
            self.diagnostics.len(),
            self.arena.lens(),
        )
    }

    fn restore(&mut self, saved: Checkpoint) {
        self.cursor = saved.0;
        self.delim_depth = saved.1;
        self.diagnostics.truncate(saved.2);
        self.arena.truncate(saved.3);
    }
}
