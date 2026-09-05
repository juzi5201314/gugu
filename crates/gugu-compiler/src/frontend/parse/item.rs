use crate::diagnostics::DiagnosticCode;
use crate::source::Span;

use super::super::ast::{
    AstRange, Field, FnBody, FnDecl, Item, ItemId, ItemKind, StructBody, UseItem, UseTreeKind,
    Variant, VariantKind, Visibility,
};
use super::super::intern::Symbol;
use super::super::token::TokenKind;
use super::{Mark, Parser, finish_extend};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FnDeclContext {
    RegularFn,
    Closure,
    TraitMethod,
    ExternImport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssocContext {
    Trait,
    Impl,
    ExternBlock,
}

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
        self.consume_decl_terminator();
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
                let attributes = self.parse_outer_attributes();
                let name_tok = self.expect(TokenKind::Ident, "use 列表需要标识符");
                let name = self.interned_symbol(name_tok);
                let alias = if self.eat(TokenKind::KwAs) {
                    let alias_tok = self.expect(TokenKind::Ident, "`as` 后需要标识符");
                    Some(self.interned_symbol(alias_tok))
                } else {
                    None
                };
                items.push(UseItem {
                    attributes: self.store_attrs(attributes),
                    name,
                    alias,
                    span: self.token_span(name_tok),
                });
                if !self.eat(TokenKind::Comma) && !self.at(TokenKind::RBrace) {
                    if self.at_list_separator() {
                        continue;
                    }
                    break;
                }
            }
            self.expect(TokenKind::RBrace, "use 列表需要 `}`");
            UseTreeKind::Brace {
                path,
                items: finish_extend(&mut self.diagnostics, &mut self.arena.use_items, items),
            }
        } else {
            let alias = if self.eat(TokenKind::KwAs) {
                let alias_tok = self.expect(TokenKind::Ident, "`as` 后需要标识符");
                Some(self.interned_symbol(alias_tok))
            } else {
                None
            };
            UseTreeKind::Path { path, alias }
        };
        self.consume_decl_terminator();
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
        let fn_id = self.parse_fn_decl(FnDeclContext::RegularFn, unsafety, None);
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
        context: FnDeclContext,
        unsafety: bool,
        extern_abi: Option<Symbol>,
    ) -> super::super::ast::FnId {
        let mark = self.start();
        self.expect(TokenKind::KwFn, "需要 `fn`");
        let require_name = context != FnDeclContext::Closure;
        let (name, name_span) = if require_name {
            let token = self.expect(TokenKind::Ident, "函数需要名字");
            (
                Some(self.interned_symbol(token)),
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
        let (require_body, allow_none) = match context {
            FnDeclContext::RegularFn | FnDeclContext::Closure => (true, false),
            FnDeclContext::TraitMethod | FnDeclContext::ExternImport => (false, true),
        };
        let body = self.parse_fn_body(require_body, allow_none, require_name);
        let span = self.finish_span(mark);
        let extern_import = extern_abi.is_some() && matches!(body, FnBody::None);
        match self.arena.try_push_fn(FnDecl {
            id: mark.id,
            span,
            unsafety,
            name,
            name_span,
            generics,
            params,
            return_ty,
            body,
            extern_abi,
            extern_import,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                super::super::ast::FnId(u32::MAX)
            }
        }
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

    fn parse_fn_body(&mut self, require_body: bool, allow_none: bool, declaration: bool) -> FnBody {
        if self.at(TokenKind::LBrace) {
            return FnBody::Block(self.parse_block_expr());
        }
        if self.eat(TokenKind::Eq) {
            let expr = self.parse_expression();
            if declaration {
                self.consume_decl_terminator();
            }
            return FnBody::Eq(expr);
        }
        if allow_none && (self.at(TokenKind::Semi) || self.at_line_end() || self.at(TokenKind::Eof))
        {
            self.consume_decl_terminator();
            return FnBody::None;
        }
        if require_body {
            self.error_here(DiagnosticCode::ParseExpected, "函数需要函数体或 `=` 表达式");
        }
        FnBody::None
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
            let field_mark = self.start();
            let attrs = self.parse_outer_attributes();
            let vis = if self.eat(TokenKind::KwPub) {
                Visibility::Pub
            } else {
                Visibility::Private
            };
            let ty = self.parse_ty();
            let field = Field {
                id: field_mark.id,
                span: self.finish_span(field_mark),
                attributes: self.store_attrs(attrs),
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
            Some(self.interned_symbol(name_tok)),
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
            if self.eat(TokenKind::Comma) || self.at_list_separator() {
                continue;
            }
            break;
        }
        self.expect(TokenKind::RBrace, "枚举需要 `}`");
        let variants = finish_extend(&mut self.diagnostics, &mut self.arena.variants, variants);
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Enum { generics, variants },
        )
    }

    fn parse_variant(&mut self) -> Variant {
        let mark = self.start();
        let attrs = self.parse_outer_attributes();
        let name_tok = self.expect(TokenKind::Ident, "变体需要名字");
        let kind = if self.eat(TokenKind::LParen) {
            let fields = self.parse_tuple_field_list(TokenKind::RParen);
            self.expect(TokenKind::RParen, "元组变体需要 `)`");
            VariantKind::Tuple(fields)
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
            attributes: self.store_attrs(attrs),
            name: self.interned_symbol(name_tok),
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
            Some(self.interned_symbol(name_tok)),
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
        let items = self.parse_assoc_items(AssocContext::Trait);
        self.expect(TokenKind::RBrace, "trait 需要 `}`");
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
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
        let negative = self.at_negative_impl();
        if negative {
            self.bump();
        }
        let first = self.parse_ty();
        let (self_ty, trait_ty) = if self.eat(TokenKind::KwFor) {
            (self.parse_ty(), Some(first))
        } else {
            (first, None)
        };
        self.expect(TokenKind::LBrace, "impl 需要 `{`");
        let items = self.parse_assoc_items(AssocContext::Impl);
        self.expect(TokenKind::RBrace, "impl 需要 `}`");
        self.push_item(
            mark,
            attrs,
            visibility,
            None,
            None,
            ItemKind::Impl {
                negative,
                unsafety,
                generics,
                self_ty,
                trait_ty,
                items,
            },
        )
    }

    fn at_negative_impl(&self) -> bool {
        self.at(TokenKind::Not) && self.nth(1) == TokenKind::Ident
    }

    fn parse_assoc_items(&mut self, context: AssocContext) -> AstRange<ItemId> {
        let mut items = Vec::new();
        while !self.at_any(&[TokenKind::RBrace, TokenKind::Eof]) {
            items.push(self.parse_assoc_item(context));
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.item_ids, items)
    }

    fn parse_assoc_item(&mut self, context: AssocContext) -> ItemId {
        let mark = self.start();
        let attrs = self.parse_outer_attributes();
        let visibility = if self.eat(TokenKind::KwPub) {
            Visibility::Pub
        } else {
            Visibility::Private
        };
        match context {
            AssocContext::ExternBlock => {
                self.parse_extern_block_assoc_item(mark, attrs, visibility)
            }
            AssocContext::Trait | AssocContext::Impl => match self.kind() {
                TokenKind::KwFn | TokenKind::KwUnsafe => {
                    self.parse_assoc_fn_item(mark, attrs, visibility, context)
                }
                TokenKind::KwType => self.parse_assoc_type_item(mark, attrs, visibility),
                TokenKind::KwConst => self.parse_assoc_const_item(mark, attrs, visibility),
                _ => {
                    self.error_here(
                        DiagnosticCode::ParseUnexpected,
                        "关联项只允许 `fn`、`type` 或 `const`",
                    );
                    self.recover_item();
                    self.push_item(mark, attrs, visibility, None, None, ItemKind::Error)
                }
            },
        }
    }

    fn parse_extern_block_assoc_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        if !self.at_any(&[TokenKind::KwFn, TokenKind::KwUnsafe]) {
            self.error_here(DiagnosticCode::ParseUnexpected, "extern 块只允许 `fn` 声明");
            self.recover_item();
            return self.push_item(mark, attrs, visibility, None, None, ItemKind::Error);
        }
        self.parse_assoc_fn_item(mark, attrs, visibility, AssocContext::ExternBlock)
    }

    fn parse_assoc_fn_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
        context: AssocContext,
    ) -> ItemId {
        let unsafety = self.eat(TokenKind::KwUnsafe);
        let fn_context = match context {
            AssocContext::Trait => FnDeclContext::TraitMethod,
            AssocContext::Impl => FnDeclContext::RegularFn,
            AssocContext::ExternBlock => FnDeclContext::ExternImport,
        };
        let extern_abi = self.extern_block_abi;
        let fn_id = self.parse_fn_decl(fn_context, unsafety, extern_abi);
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

    fn parse_assoc_type_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "关联类型需要名字");
        let ty = if self.eat(TokenKind::Eq) {
            Some(self.parse_ty())
        } else {
            None
        };
        self.consume_decl_terminator();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::TypeAlias {
                generics: AstRange::empty(),
                ty,
            },
        )
    }

    fn parse_assoc_const_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let name_tok = self.expect(TokenKind::Ident, "关联常量需要名字");
        let ty = if self.eat(TokenKind::Colon) {
            Some(self.parse_ty())
        } else {
            None
        };
        let value = if self.eat(TokenKind::Eq) {
            Some(self.parse_expression())
        } else {
            None
        };
        self.consume_decl_terminator();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Const { ty, value },
        )
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
        let value = Some(self.parse_expression());
        self.consume_decl_terminator();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
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
        let ty = Some(self.parse_ty());
        self.consume_decl_terminator();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
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
        self.consume_decl_terminator();
        self.push_item(
            mark,
            attrs,
            visibility,
            Some(self.interned_symbol(name_tok)),
            Some(self.token_span(name_tok)),
            ItemKind::Static { ty, value },
        )
    }

    fn parse_extern_abi_string(&mut self) -> Symbol {
        let abi_tok = match self.kind() {
            TokenKind::String | TokenKind::RawString => self.bump(),
            _ => self.expect(TokenKind::String, "extern 需要 `\"C\"` ABI 字符串"),
        };
        self.interned_symbol(abi_tok)
    }

    fn parse_extern_item(
        &mut self,
        mark: Mark,
        attrs: Vec<super::super::ast::Attribute>,
        visibility: Visibility,
    ) -> ItemId {
        self.bump();
        let abi = self.parse_extern_abi_string();
        if self.intern.get_str(abi) != "C" {
            self.error_here(
                DiagnosticCode::ParseExpected,
                "extern ABI 目前只支持 `\"C\"`",
            );
        }
        if self.eat(TokenKind::LBrace) {
            self.extern_block_abi = Some(abi);
            let items = self.parse_assoc_items(AssocContext::ExternBlock);
            self.extern_block_abi = None;
            self.expect(TokenKind::RBrace, "extern 块需要 `}`");
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
            let fn_id = self.parse_fn_decl(FnDeclContext::ExternImport, unsafety, Some(abi));
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
        let template = match self.kind() {
            TokenKind::String | TokenKind::RawString => self.bump(),
            _ => self.expect(TokenKind::String, "global_asm 需要字符串模板"),
        };
        self.expect(TokenKind::RParen, "global_asm 需要 `)`");
        self.consume_decl_terminator();
        self.push_item(
            mark,
            attrs,
            visibility,
            None,
            None,
            ItemKind::GlobalAsm {
                template: self.interned_symbol(template),
            },
        )
    }

    pub(super) fn parse_field_list(&mut self, close: TokenKind) -> AstRange<Field> {
        let mut fields = Vec::new();
        while !self.at(close) && !self.at(TokenKind::Eof) {
            fields.push(self.parse_field());
            if self.eat(TokenKind::Comma) || self.at_list_separator() {
                continue;
            }
            break;
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.fields, fields)
    }

    fn parse_tuple_field_list(&mut self, close: TokenKind) -> AstRange<Field> {
        let mut fields = Vec::new();
        while !self.at(close) && !self.at(TokenKind::Eof) {
            let mark = self.start();
            let attrs = self.parse_outer_attributes();
            let ty = self.parse_ty();
            fields.push(Field {
                id: mark.id,
                span: self.finish_span(mark),
                attributes: self.store_attrs(attrs),
                visibility: Visibility::Private,
                name: None,
                name_span: None,
                ty,
            });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        finish_extend(&mut self.diagnostics, &mut self.arena.fields, fields)
    }

    fn parse_field(&mut self) -> Field {
        let mark = self.start();
        let attrs = self.parse_outer_attributes();
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
            attributes: self.store_attrs(attrs),
            visibility,
            name: Some(self.interned_symbol(name_tok)),
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
        match self.arena.try_push_item(Item {
            id: mark.id,
            span,
            attributes,
            visibility,
            name,
            name_span,
            kind,
        }) {
            Some(id) => id,
            None => {
                self.arena_limit();
                ItemId(u32::MAX)
            }
        }
    }
}
