use super::super::ast::{AstArena, AstFile, ExprId, ExprKind, ItemId, ItemKind, StmtKind};
use super::super::intern::SymbolInterner;

pub(crate) fn dump_ast(file: &AstFile, arena: &AstArena, intern: &SymbolInterner) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("file\n");
    for id in file.items.as_slice(&arena.item_ids) {
        dump_item(&mut out, arena, intern, *id, 1);
    }
    out
}

fn dump_item(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    id: ItemId,
    indent: usize,
) {
    let item = &arena.items[id.0 as usize];
    pad(out, indent);
    out.push_str("item#");
    out.push_str(&item.id.local.to_string());
    out.push(' ');
    match item.kind {
        ItemKind::Function(fn_id) => {
            let decl = &arena.fns[fn_id.0 as usize];
            out.push_str("fn");
            if let Some(name) = decl.name {
                out.push(' ');
                out.push_str(intern.get_str(name));
            }
            out.push('\n');
            match decl.body {
                super::super::ast::FnBody::Block(body) | super::super::ast::FnBody::Eq(body) => {
                    dump_expr(out, arena, intern, body, indent + 1);
                }
                super::super::ast::FnBody::None => {}
            }
        }
        ItemKind::Struct { .. } => out.push_str("struct\n"),
        ItemKind::Enum { .. } => out.push_str("enum\n"),
        ItemKind::Union { .. } => out.push_str("union\n"),
        ItemKind::Trait { .. } => out.push_str("trait\n"),
        ItemKind::Impl { .. } => out.push_str("impl\n"),
        ItemKind::Use(_) => out.push_str("use\n"),
        ItemKind::TypeAlias { .. } => out.push_str("type\n"),
        ItemKind::Const { value, .. } => {
            out.push_str("const\n");
            dump_expr(out, arena, intern, value, indent + 1);
        }
        ItemKind::Static { value, .. } => {
            out.push_str("static\n");
            dump_expr(out, arena, intern, value, indent + 1);
        }
        ItemKind::ExternBlock { .. } => out.push_str("extern\n"),
        ItemKind::GlobalAsm { .. } => out.push_str("global_asm\n"),
        ItemKind::SourceMacro { body } => {
            out.push_str("source_macro\n");
            dump_expr(out, arena, intern, body, indent + 1);
        }
        ItemKind::Error => out.push_str("error\n"),
    }
}

fn dump_expr(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    id: ExprId,
    indent: usize,
) {
    let expr = &arena.exprs[id.0 as usize];
    pad(out, indent);
    out.push_str("expr#");
    out.push_str(&expr.id.local.to_string());
    out.push(' ');
    match expr.kind {
        ExprKind::Path(_) => out.push_str("path\n"),
        ExprKind::Literal(_) => out.push_str("lit\n"),
        ExprKind::Paren(inner) => {
            out.push_str("paren\n");
            dump_expr(out, arena, intern, inner, indent + 1);
        }
        ExprKind::Tuple(elems) => {
            out.push_str("tuple\n");
            for inner in elems.as_slice(&arena.expr_ids) {
                dump_expr(out, arena, intern, *inner, indent + 1);
            }
        }
        ExprKind::Array(elems) => {
            out.push_str("array\n");
            for inner in elems.as_slice(&arena.expr_ids) {
                dump_expr(out, arena, intern, *inner, indent + 1);
            }
        }
        ExprKind::Repeat { elem, count } => {
            out.push_str("repeat\n");
            dump_expr(out, arena, intern, elem, indent + 1);
            dump_expr(out, arena, intern, count, indent + 1);
        }
        ExprKind::Struct { .. } => out.push_str("struct_lit\n"),
        ExprKind::Block { stmts, tail } => {
            out.push_str("block\n");
            for stmt in stmts.as_slice(&arena.stmt_ids) {
                dump_stmt(out, arena, intern, *stmt, indent + 1);
            }
            if let Some(tail) = tail {
                dump_expr(out, arena, intern, tail, indent + 1);
            }
        }
        ExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
            out.push_str("if\n");
            dump_expr(out, arena, intern, cond, indent + 1);
            dump_expr(out, arena, intern, then_block, indent + 1);
            if let Some(else_branch) = else_branch {
                dump_expr(out, arena, intern, else_branch, indent + 1);
            }
        }
        ExprKind::Match { scrutinee, .. } => {
            out.push_str("match\n");
            dump_expr(out, arena, intern, scrutinee, indent + 1);
        }
        ExprKind::Loop(body) => {
            out.push_str("loop\n");
            dump_expr(out, arena, intern, body, indent + 1);
        }
        ExprKind::While { cond, body } => {
            out.push_str("while\n");
            dump_expr(out, arena, intern, cond, indent + 1);
            dump_expr(out, arena, intern, body, indent + 1);
        }
        ExprKind::For { iter, body, .. } => {
            out.push_str("for\n");
            dump_expr(out, arena, intern, iter, indent + 1);
            dump_expr(out, arena, intern, body, indent + 1);
        }
        ExprKind::Try(body) => {
            out.push_str("try\n");
            dump_expr(out, arena, intern, body, indent + 1);
        }
        ExprKind::Select { .. } => out.push_str("select\n"),
        ExprKind::Async(inner) => {
            out.push_str("async\n");
            dump_expr(out, arena, intern, inner, indent + 1);
        }
        ExprKind::Closure(_) => out.push_str("closure\n"),
        ExprKind::Call { callee, args } => {
            out.push_str("call\n");
            dump_expr(out, arena, intern, callee, indent + 1);
            for arg in args.as_slice(&arena.expr_ids) {
                dump_expr(out, arena, intern, *arg, indent + 1);
            }
        }
        ExprKind::Field { base, .. } | ExprKind::TupleField { base, .. } => {
            out.push_str("field\n");
            dump_expr(out, arena, intern, base, indent + 1);
        }
        ExprKind::Index { base, .. } => {
            out.push_str("index\n");
            dump_expr(out, arena, intern, base, indent + 1);
        }
        ExprKind::TryOp(inner) => {
            out.push_str("try_op\n");
            dump_expr(out, arena, intern, inner, indent + 1);
        }
        ExprKind::Unary { expr, .. } => {
            out.push_str("unary\n");
            dump_expr(out, arena, intern, expr, indent + 1);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            out.push_str("binary\n");
            dump_expr(out, arena, intern, lhs, indent + 1);
            dump_expr(out, arena, intern, rhs, indent + 1);
        }
        ExprKind::Range { start, end } => {
            out.push_str("range\n");
            dump_expr(out, arena, intern, start, indent + 1);
            dump_expr(out, arena, intern, end, indent + 1);
        }
        ExprKind::Unsafe(inner)
        | ExprKind::Comptime(inner)
        | ExprKind::SourceMacro { body: inner } => {
            out.push_str("prefix_block\n");
            dump_expr(out, arena, intern, inner, indent + 1);
        }
        ExprKind::Intrinsic { kind, .. } => {
            out.push_str("intrinsic ");
            out.push_str(&format!("{kind:?}\n"));
        }
        ExprKind::Asm { .. } => out.push_str("asm\n"),
        ExprKind::Return(_) => out.push_str("return\n"),
        ExprKind::Break(_) => out.push_str("break\n"),
        ExprKind::Continue => out.push_str("continue\n"),
        ExprKind::FString { .. } => out.push_str("fstring\n"),
        ExprKind::Error => out.push_str("error\n"),
    }
}

fn dump_stmt(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    id: super::super::ast::StmtId,
    indent: usize,
) {
    let stmt = &arena.stmts[id.0 as usize];
    pad(out, indent);
    out.push_str("stmt#");
    out.push_str(&stmt.id.local.to_string());
    out.push(' ');
    match stmt.kind {
        StmtKind::Let {
            init, else_block, ..
        } => {
            out.push_str("let\n");
            if let Some(init) = init {
                dump_expr(out, arena, intern, init, indent + 1);
            }
            if let Some(else_block) = else_block {
                dump_expr(out, arena, intern, else_block, indent + 1);
            }
        }
        StmtKind::Assign { place, value, .. } => {
            out.push_str("assign\n");
            dump_expr(out, arena, intern, place, indent + 1);
            dump_expr(out, arena, intern, value, indent + 1);
        }
        StmtKind::Defer { body, .. } | StmtKind::SourceMacro { body } => {
            out.push_str("defer_or_macro\n");
            dump_expr(out, arena, intern, body, indent + 1);
        }
        StmtKind::Yield => out.push_str("yield\n"),
        StmtKind::Expr { expr, discarded } => {
            if discarded {
                out.push_str("expr_discard\n");
            } else {
                out.push_str("expr\n");
            }
            dump_expr(out, arena, intern, expr, indent + 1);
        }
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

pub(crate) fn parent_before_child(arena: &AstArena) -> bool {
    let mut used = vec![false; arena.next_node as usize];
    for item in &arena.items {
        let slot = item.id.local as usize;
        if slot >= used.len() || used[slot] {
            return false;
        }
        used[slot] = true;
    }
    for expr in &arena.exprs {
        let slot = expr.id.local as usize;
        if slot >= used.len() || used[slot] {
            return false;
        }
        used[slot] = true;
        if let ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) = expr.kind {
            let child = &arena.exprs[inner.0 as usize];
            if child.span.start() == expr.span.start() && child.id.local <= expr.id.local {
                return false;
            }
        }
    }
    true
}
