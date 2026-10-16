use crate::Span;
use super::super::{ast::{AstArena, ExprKind, ItemKind, PatKind, TyKind}, token::{TokenBuffer, TokenKind}};
use super::{opens, token_at};

#[derive(Clone, Copy, Default)]
pub(super) struct Mark {
    pub close: usize,
    pub block: bool,
    pub list: bool,
    pub singleton: bool,
    pub before: bool,
    pub comma: bool,
    pub tight: bool,
    pub space: bool,
    pub after: bool,
    pub opaque_end: usize,
}

pub(super) struct Layout {
    pub marks: Vec<Mark>,
}

impl Layout {
    pub fn new(_source: &str, buffer: &TokenBuffer, arena: &AstArena) -> Self {
        // 每个标记对应一个稠密 token 下标，避免按字节大小分配或散列查找。
        let mut this = Self { marks: vec![Mark::default(); buffer.tokens.len()] };
        let mut stack = Vec::new();
        let mut parent = vec![None; buffer.tokens.len()];
        for (index, token) in buffer.tokens.iter().enumerate() {
            parent[index] = stack.last().copied();
            if opens(token.kind) {
                stack.push(index);
                this.marks[index].block = token.kind == TokenKind::LBrace;
                this.marks[index].list = token.kind == TokenKind::LBracket;
            } else if matches!(token.kind, TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace) {
                let open = stack.pop().expect("parser 已校验括号配对");
                this.marks[open].close = index;
                if index == open + 1 { this.marks[open].block = false; }
            } else if token.kind == TokenKind::Comma {
                if let Some(&open) = stack.last() { this.marks[open].list = true; }
            }
        }
        for item in &arena.items {
            this.before(buffer, &item.span);
            if matches!(item.kind, ItemKind::Use(_)) {
                let start = token_at(buffer, item.span.start());
                let end = token_at(buffer, item.span.end());
                for index in start..end {
                    if buffer.tokens[index].kind == TokenKind::LBrace {
                        this.marks[index].block = false;
                        this.marks[index].list = true;
                    }
                }
            }
        }
        for stmt in &arena.stmts { this.before(buffer, &stmt.span); }
        for arm in &arena.match_arms { this.before(buffer, &arm.span); }
        for arm in &arena.select_arms { this.before(buffer, &arm.span); }
        for attribute in &arena.attrs {
            let start = token_at(buffer, attribute.span.start());
            let end = token_at(buffer, attribute.span.end());
            if buffer.tokens[start].kind == TokenKind::Hash && end > start {
                this.marks[start].before = true;
                this.marks[end - 1].after = true;
                let bracket = (start..end).find(|&i| buffer.tokens[i].kind == TokenKind::LBracket).expect("属性方括号");
                this.marks[bracket].list = false;
            }
        }
        for field in &arena.fields { this.field(buffer, &field.span, &parent); }
        for field in &arena.field_exprs { this.field(buffer, &field.span, &parent); }
        for variant in &arena.variants { this.field(buffer, &variant.span, &parent); }
        for expr in &arena.exprs {
            let start = token_at(buffer, expr.span.start());
            let end = token_at(buffer, expr.span.end());
            match expr.kind {
                ExprKind::Block { tail: Some(tail), .. } => this.before(buffer, &arena.exprs[tail.0 as usize].span),
                ExprKind::Unary { .. } => this.marks[start].tight = true,
                ExprKind::FString { .. } => this.marks[start].opaque_end = end,
                ExprKind::Tuple(elements) => this.tuple(buffer, start, elements.as_slice(&arena.expr_ids).len()),
                ExprKind::Call { callee, .. } => {
                    let callee_end = arena.exprs[callee.0 as usize].span.end();
                    let open = (token_at(buffer, callee_end)..end).find(|&i| buffer.tokens[i].kind == TokenKind::LParen);
                    if let Some(open) = open { this.marks[open].list = true; }
                }
                ExprKind::Index { base, .. } => {
                    let open = token_at(buffer, arena.exprs[base.0 as usize].span.end());
                    this.marks[open].list = false;
                }
                ExprKind::Repeat { .. } => this.marks[start].list = false,
                _ => {}
            }
        }
        for ty in &arena.tys {
            let start = token_at(buffer, ty.span.start());
            match ty.kind {
                TyKind::Ref(_) | TyKind::Ptr(_) | TyKind::Slice(_) => this.marks[start].tight = true,
                TyKind::Array { .. } => this.marks[start].list = false,
                TyKind::Tuple(elements) => this.tuple(buffer, start, elements.as_slice(&arena.ty_ids).len()),
                _ => {}
            }
            if matches!(ty.kind, TyKind::Slice(_)) && start + 1 < this.marks.len() { this.marks[start + 1].list = false; }
        }
        for pat in &arena.pats {
            let start = token_at(buffer, pat.span.start());
            match pat.kind {
                PatKind::Ref(_) => this.marks[start].tight = true,
                PatKind::Tuple(elements) => this.tuple(buffer, start, elements.as_slice(&arena.pat_ids).len()),
                _ => {}
            }
        }
        for function in &arena.fns {
            let start = token_at(buffer, function.span.start());
            let end = token_at(buffer, function.span.end());
            if let Some(open) = (start..end).find(|&i| buffer.tokens[i].kind == TokenKind::LParen) { this.marks[open].list = true; }
            if let Some(ty) = function.return_ty { this.marks[token_at(buffer, arena.tys[ty.0 as usize].span.start())].space = true; }
        }
        this
    }

    fn before(&mut self, buffer: &TokenBuffer, span: &Span) {
        self.marks[token_at(buffer, span.start())].before = true;
    }

    fn field(&mut self, buffer: &TokenBuffer, span: &Span, parent: &[Option<usize>]) {
        let start = token_at(buffer, span.start());
        let end = token_at(buffer, span.end());
        if let Some(open) = parent[start] {
            if buffer.tokens[open].kind == TokenKind::LBrace && end > start {
                self.marks[open].list = true;
                self.marks[start].before = true;
                let last = end - 1;
                self.marks[last].comma = buffer.tokens[last].kind != TokenKind::Comma && buffer.tokens[end].kind != TokenKind::Comma;
            }
        }
    }

    fn tuple(&mut self, buffer: &TokenBuffer, start: usize, count: usize) {
        if buffer.tokens[start].kind == TokenKind::LParen {
            self.marks[start].list = true;
            self.marks[start].singleton = count == 1;
        }
    }
}
