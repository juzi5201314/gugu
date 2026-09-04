use crate::diagnostics::DiagnosticCode;
use crate::source::Span;

use super::super::ast::{
    AstRange, Field, FnBody, FnDecl, Item, ItemId, ItemKind, StructBody, UseItem, UseTreeKind,
    Variant, VariantKind, Visibility, extend_range,
};
use super::super::intern::Symbol;
use super::super::token::TokenKind;
use super::{Mark, Parser};

impl Parser<'_> {
    pub(super) fn parse_item(&mut self) -> ItemId {
        let mark = self.start();
        let attrs = self.parse_outer_attributes();
        if self.at(TokenKind::KwComptime) && self.peek_source() {
            return self.parse_source_macro_item(mark, attrs);
        }
        let visibility = if self.eat(TokenKind::KwPub) {
            Visibility::Pub
        } else {
            Visibility::Private
        };
        match self.kind() {
            TokenKind::KwUse => self.parse_use_item(mark, attrs, visibility),
            TokenKind::KwFn | TokenKind::KwUnsafe => {
                self.parse_function_item(mark, attrs, visibility)
            }
            TokenKind::KwStruct => self.parse_struct_item(mark, attrs, visibility),
            TokenKind::KwEnum => self.parse_enum_item(mark, attrs, visibility),
            TokenKind::KwUnion => self.parse_union_item(mark, attrs, visibility),
            TokenKind::KwTrait => self.parse_trait_item(mark, attrs, visibility),
            TokenKind::KwImpl => self.parse_impl_item(mark, attrs, visibility),
            TokenKind::KwConst => self.parse_const_item(mark, attrs, visibility),
            TokenKind::KwType => self.parse_type_item(mark, attrs, visibility),
            TokenKind::KwStatic => self.parse_static_item(mark, attrs, visibility),
            TokenKind::KwExtern => self.parse_extern_item(mark, attrs, visibility),
            TokenKind::KwGlobalAsm => self.parse_global_asm(mark, attrs, visibility),
            _ => {
                self.error_here(DiagnosticCode::ParseUnexpected, "此处需要模块项");
                self.recover_item();
                self.push_item(mark, attrs, visibility, None, None, ItemKind::Error)
            }
        }
    }

    pub(super) fn peek_source(&self) -> bool {
        self.at(TokenKind::KwComptime)
            && self.nth(1) == TokenKind::Ident
            && self.tokens[self.cursor + 1].text(self.source) == "source"
    }

    fn parse_source_macro_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
    ) -> ItemId {
        self.bump();
        self.bump();
        let body = self.parse_block_expr();
        self.push_item(
            mark,
            attrs,
            Visibility::Private,
            None,
            None,
            ItemKind::SourceMacro { body },
        )
    }

    fn parse_use_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let path = self.parse_path();
        let kind = if self.at(TokenKind::Dot) && self.nth(1) == TokenKind::LBrace {
            self.bump();
            self.bump();
            let mut items = Vec::new();
            while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
                let name_tok = self.expect(TokenKind::Ident, "use 列表需要标识符");
                let name = Self::interned_symbol(name_tok);
                let alias = if self.eat(TokenKind::KwAs) {
                    Some(Self::interned_symbol(
                        self.expect(TokenKind::Ident, "`as` 后需要标识符"),
                    ))
                } else {
                    None
                };
                items.push(UseItem {
                    name,
                    alias,
                    span: self.token_span(name_tok),
                });
                if !self.eat(TokenKind::Comma) && !self.at(TokenKind::RBrace) {
                    if self.at_line_end() {
                        continue;
                    }
                    break;
                }
            }
            self.expect(TokenKind::RBrace, "use 列表需要 `}`");
            UseTreeKind::Brace {
                path,
                items: extend_range(&mut self.arena.use_items, items),
            }
        } else {
            let alias = if self.eat(TokenKind::KwAs) {
                Some(Self::interned_symbol(
                    self.expect(TokenKind::Ident, "`as` 后需要标识符"),
                ))
            } else {
                None
            };
            UseTreeKind::Path { path, alias }
        };
        self.push_item(mark, attrs, visibility, None, None, ItemKind::Use(kind))
    }

    fn parse_function_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        let unsafety = self.eat(TokenKind::KwUnsafe);
        if unsafety
            && !self.at(TokenKind::KwFn)
            && !self.at(TokenKind::KwTrait)
            && !self.at(TokenKind::KwImpl)
        {
            self.error_here(
                DiagnosticCode::ParseExpected,
                "`unsafe` 后需要 `fn`、`trait` 或 `impl`",
            );
        }
        if self.at(TokenKind::KwTrait) {
            return self.parse_trait_after_unsafe(mark, attrs, visibility, true);
        }
        if self.at(TokenKind::KwImpl) {
            return self.parse_impl_after_unsafe(mark, attrs, visibility, true);
        }
        let fn_id = self.parse_fn_decl(unsafety, true);
        let name = self.arena.fns[fn_id.0 as usize].name;
        let name_span = self.arena.fns[fn_id.0 as usize].name_span.clone();
        self.push_item(
            mark,
            attrs,
            visibility,
            name,
            name_span,
            ItemKind::Function(fn_id),
        )
    }

    pub(super) fn parse_fn_decl(
        &mut self,
        unsafety: bool,
        require_name: bool,
    ) -> super::super::ast::FnId {
        let mark = self.start();
        self.expect(TokenKind::KwFn, "需要 `fn`");
        let (name, name_span) = if require_name {
            let token = self.expect(TokenKind::Ident, "函数需要名字");
            (
                Some(Self::interned_symbol(token)),
                Some(self.token_span(token)),
            )
        } else {
            (None, None)
        };
        let generics = self.parse_generic_params();
        self.expect(TokenKind::LParen, "函数参数表需要 `(`");
        let params = self.parse_param_list(TokenKind::RParen);
        self.expect(TokenKind::RParen, "函数参数表需要 `)`");
        let return_ty = self.parse_optional_return_ty();
        let body = self.parse_fn_body();
        let span = self.finish_span(mark);
        self.arena.push_fn(FnDecl {
            id: mark.id,
            span,
            unsafety,
            name,
            name_span,
            generics,
            params,
            return_ty,
            body,
        })
    }

    fn parse_optional_return_ty(&mut self) -> Option<super::super::ast::TyId> {
        if self.at(TokenKind::LBrace) || self.at(TokenKind::Eq) || self.at(TokenKind::Semi) {
            return None;
        }
        if self.newline_before_current() {
            return None;
        }
        if self.at_type_start() {
            Some(self.parse_ty())
        } else {
            None
        }
    }

    fn parse_fn_body(&mut self) -> FnBody {
        if self.at(TokenKind::LBrace) {
            FnBody::Block(self.parse_block_expr())
        } else if self.eat(TokenKind::Eq) {
            let expr = self.parse_expression();
            FnBody::Eq(expr)
        } else if self.at(TokenKind::Semi) || self.at_line_end() || self.at(TokenKind::Eof) {
            self.eat(TokenKind::Semi);
            FnBody::None
        } else {
            self.error_here(DiagnosticCode::ParseExpected, "函数需要函数体或 `=` 表达式");
            FnBody::None
        }
    }

    fn parse_struct_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "结构体需要名字");
        let generics = self.parse_generic_params();
        let body = if self.eat(TokenKind::LParen) {
            let vis = if self.eat(TokenKind::KwPub) {
                Visibility::Pub
            } else {
                Visibility::Private
            };
            let field_mark = self.start();
            let ty = self.parse_ty();
            let field = Field {
                id: field_mark.id,
                span: self.finish_span(field_mark),
                visibility: vis,
                name: None,
                name_span: None,
                ty,
            };
            self.expect(TokenKind::RParen, "newtype 结构体需要 `)`");
            StructBody::Newtype(field)
        } else {
            self.expect(TokenKind::LBrace, "结构体需要 `{`");
            let fields = self.parse_field_list(TokenKind::RBrace);
            self.expect(TokenKind::RBrace, "结构体需要 `}`");
            StructBody::Record(fields)
        };
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Struct { generics, body },
        )
    }

    fn parse_enum_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "枚举需要名字");
        let generics = self.parse_generic_params();
        self.expect(TokenKind::LBrace, "枚举需要 `{`");
        let mut variants = Vec::new();
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            variants.push(self.parse_variant());
            if self.eat(TokenKind::Comma) || self.at_line_end() {
                continue;
            }
            break;
        }
        self.expect(TokenKind::RBrace, "枚举需要 `}`");
        let variants = extend_range(&mut self.arena.variants, variants);
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Enum { generics, variants },
        )
    }

    fn parse_variant(&mut self) -> Variant {
        let mark = self.start();
        let name_tok = self.expect(TokenKind::Ident, "变体需要名字");
        let kind = if self.eat(TokenKind::LParen) {
            let tys = self.parse_ty_list(TokenKind::RParen);
            self.expect(TokenKind::RParen, "元组变体需要 `)`");
            VariantKind::Tuple(tys)
        } else if self.eat(TokenKind::LBrace) {
            let fields = self.parse_field_list(TokenKind::RBrace);
            self.expect(TokenKind::RBrace, "结构体变体需要 `}`");
            VariantKind::Struct(fields)
        } else {
            VariantKind::Unit
        };
        Variant {
            id: mark.id,
            span: self.finish_span(mark),
            name: Self::interned_symbol(name_tok),
            name_span: self.token_span(name_tok),
            kind,
        }
    }

    fn parse_union_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "union 需要名字");
        let generics = self.parse_generic_params();
        self.expect(TokenKind::LBrace, "union 需要 `{`");
        let fields = self.parse_field_list(TokenKind::RBrace);
        self.expect(TokenKind::RBrace, "union 需要 `}`");
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Union { generics, fields },
        )
    }

    fn parse_trait_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.parse_trait_after_unsafe(mark, attrs, visibility, false)
    }

    fn parse_trait_after_unsafe(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
        unsafety: bool,
    ) -> ItemId {
        self.expect(TokenKind::KwTrait, "需要 `trait`");
        let name_tok = self.expect(TokenKind::Ident, "trait 需要名字");
        let generics = self.parse_generic_params();
        self.expect(TokenKind::LBrace, "trait 需要 `{`");
        let items = self.parse_assoc_items();
        self.expect(TokenKind::RBrace, "trait 需要 `}`");
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Trait {
                unsafety,
                generics,
                items,
            },
        )
    }

    fn parse_impl_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.parse_impl_after_unsafe(mark, attrs, visibility, false)
    }

    fn parse_impl_after_unsafe(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
        unsafety: bool,
    ) -> ItemId {
        self.expect(TokenKind::KwImpl, "需要 `impl`");
        let generics = self.parse_generic_params();
        let first = self.parse_ty();
        let (self_ty, trait_ty) = if self.eat(TokenKind::KwFor) {
            (self.parse_ty(), Some(first))
        } else {
            (first, None)
        };
        self.expect(TokenKind::LBrace, "impl 需要 `{`");
        let items = self.parse_assoc_items();
        self.expect(TokenKind::RBrace, "impl 需要 `}`");
        self.push_item(
            mark,
            attrs,
            visibility,
            None,
            None,
            ItemKind::Impl {
                unsafety,
                generics,
                self_ty,
                trait_ty,
                items,
            },
        )
    }

    fn parse_assoc_items(&mut self) -> AstRange<ItemId> {
        let mut items = Vec::new();
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            items.push(self.parse_item());
        }
        extend_range(&mut self.arena.item_ids, items)
    }

    fn parse_const_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "常量需要名字");
        let ty = if self.eat(TokenKind::Colon) {
            Some(self.parse_ty())
        } else {
            None
        };
        self.expect(TokenKind::Eq, "常量需要 `=`");
        let value = self.parse_expression();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Const { ty, value },
        )
    }

    fn parse_type_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "类型别名需要名字");
        let generics = self.parse_generic_params();
        self.expect(TokenKind::Eq, "类型别名需要 `=`");
        let ty = self.parse_ty();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::TypeAlias { generics, ty },
        )
    }

    fn parse_static_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "static 需要名字");
        self.expect(TokenKind::Colon, "static 需要类型");
        let ty = self.parse_ty();
        self.expect(TokenKind::Eq, "static 需要 `=`");
        let value = self.parse_expression();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(Self::interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Static { ty, value },
        )
    }

    fn parse_extern_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let abi_tok = self.expect(TokenKind::String, "extern 需要 `\"C\"` ABI 字符串");
        let abi = Self::interned_symbol(abi_tok);
        if self.eat(TokenKind::LBrace) {
            let mut items = Vec::new();
            while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
                items.push(self.parse_item());
            }
            self.expect(TokenKind::RBrace, "extern 块需要 `}`");
            let items = extend_range(&mut self.arena.item_ids, items);
            self.push_item(
                mark,
                attrs,
                visibility,
                None,
                None,
                ItemKind::ExternBlock { abi, items },
            )
        } else {
            let unsafety = self.eat(TokenKind::KwUnsafe);
            let fn_id = self.parse_fn_decl(unsafety, true);
            let name = self.arena.fns[fn_id.0 as usize].name;
            let name_span = self.arena.fns[fn_id.0 as usize].name_span.clone();
            self.push_item(
                mark,
                attrs,
                visibility,
                name,
                name_span,
                ItemKind::Function(fn_id),
            )
        }
    }

    fn parse_global_asm(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        self.expect(TokenKind::LParen, "global_asm 需要 `(`");
        let template = self.expect(TokenKind::String, "global_asm 需要字符串模板");
        self.expect(TokenKind::RParen, "global_asm 需要 `)`");
        self.push_item(
            mark,
            attrs,
            visibility,
            None,
            None,
            ItemKind::GlobalAsm {
                template: Self::interned_symbol(template),
            },
        )
    }

    pub(super) fn parse_field_list(&mut self, close: TokenKind) -> AstRange<Field> {
        let mut fields = Vec::new();
        while !self.at(close) && !self.at(TokenKind::Eof) {
            fields.push(self.parse_field());
            if self.eat(TokenKind::Comma) || self.at_line_end() {
                continue;
            }
            break;
        }
        extend_range(&mut self.arena.fields, fields)
    }

    fn parse_field(&mut self) -> Field {
        let mark = self.start();
        let visibility = if self.eat(TokenKind::KwPub) {
            Visibility::Pub
        } else {
            Visibility::Private
        };
        let name_tok = self.expect(TokenKind::Ident, "字段需要名字");
        self.expect(TokenKind::Colon, "字段需要 `:`");
        let ty = self.parse_ty();
        Field {
            id: mark.id,
            span: self.finish_span(mark),
            visibility,
            name: Some(Self::interned_symbol(name_tok)),
            name_span: Some(self.token_span(name_tok)),
            ty,
        }
    }

    fn push_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
        name: Option<Symbol>,
        name_span: Option<Span>,
        kind: ItemKind,
    ) -> ItemId {
        let attributes = self.store_attrs(attrs);
        let span = self.finish_span(mark);
        self.arena.push_item(Item {
            id: mark.id,
            span,
            attributes,
            visibility,
            name,
            name_span,
            kind,
        })
    }
}
