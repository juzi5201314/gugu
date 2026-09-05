use super::super::ast::{
    AstArena, AstFile, AstNodeId, ExprId, ExprKind, Field, ItemId, ItemKind, MatchArm, PatId,
    PatKind, SelectArm, SelectArmKind, StmtKind, StructBody, Variant, VariantKind,
};
use super::super::intern::SymbolInterner;

pub(crate) fn dump_ast(file: &AstFile, arena: &AstArena, intern: &SymbolInterner) -> String {
    let mut out = String::with_capacity(512);
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
    match &item.kind {
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
        ItemKind::Struct { body, .. } => {
            out.push_str("struct\n");
            dump_struct_body(out, arena, intern, body, indent + 1);
        }
        ItemKind::Enum { variants, .. } => {
            out.push_str("enum\n");
            for variant in variants.as_slice(&arena.variants) {
                dump_variant(out, arena, intern, variant, indent + 1);
            }
        }
        ItemKind::Union { fields, .. } => {
            out.push_str("union\n");
            for field in fields.as_slice(&arena.fields) {
                dump_field(out, arena, intern, field, indent + 1);
            }
        }
        ItemKind::Trait { items, .. } => {
            out.push_str("trait\n");
            for nested in items.as_slice(&arena.item_ids) {
                dump_item(out, arena, intern, *nested, indent + 1);
            }
        }
        ItemKind::Impl {
            items, negative, ..
        } => {
            out.push_str(if *negative { "impl!\n" } else { "impl\n" });
            for nested in items.as_slice(&arena.item_ids) {
                dump_item(out, arena, intern, *nested, indent + 1);
            }
        }
        ItemKind::Use(_) => out.push_str("use\n"),
        ItemKind::TypeAlias { .. } => out.push_str("type\n"),
        ItemKind::Const { value, .. } => {
            out.push_str("const\n");
            if let Some(value) = value {
                dump_expr(out, arena, intern, *value, indent + 1);
            }
        }
        ItemKind::Static { value, .. } => {
            out.push_str("static\n");
            dump_expr(out, arena, intern, *value, indent + 1);
        }
        ItemKind::ExternBlock { items, .. } => {
            out.push_str("extern\n");
            for nested in items.as_slice(&arena.item_ids) {
                dump_item(out, arena, intern, *nested, indent + 1);
            }
        }
        ItemKind::GlobalAsm { .. } => out.push_str("global_asm\n"),
        ItemKind::SourceMacro { body } => {
            out.push_str("source_macro\n");
            dump_expr(out, arena, intern, *body, indent + 1);
        }
        ItemKind::Error => out.push_str("error\n"),
    }
}

fn dump_struct_body(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    body: &StructBody,
    indent: usize,
) {
    match body {
        StructBody::Newtype(field) => dump_field(out, arena, intern, field, indent),
        StructBody::Record(fields) => {
            for field in fields.as_slice(&arena.fields) {
                dump_field(out, arena, intern, field, indent);
            }
        }
    }
}

fn dump_field(
    out: &mut String,
    _arena: &AstArena,
    intern: &SymbolInterner,
    field: &Field,
    indent: usize,
) {
    pad(out, indent);
    out.push_str("field#");
    out.push_str(&field.id.local.to_string());
    if let Some(name) = field.name {
        out.push(' ');
        out.push_str(intern.get_str(name));
    }
    out.push('\n');
}

fn dump_variant(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    variant: &Variant,
    indent: usize,
) {
    pad(out, indent);
    out.push_str("variant#");
    out.push_str(&variant.id.local.to_string());
    out.push(' ');
    out.push_str(intern.get_str(variant.name));
    out.push('\n');
    match &variant.kind {
        VariantKind::Unit => {}
        VariantKind::Tuple(fields) => {
            for field in fields.as_slice(&arena.fields) {
                dump_ty(out, arena, field.ty, indent + 1);
            }
        }
        VariantKind::Struct(fields) => {
            for field in fields.as_slice(&arena.fields) {
                dump_field(out, arena, intern, field, indent + 1);
            }
        }
    }
}

fn dump_ty(out: &mut String, arena: &AstArena, id: super::super::ast::TyId, indent: usize) {
    let ty = &arena.tys[id.0 as usize];
    pad(out, indent);
    out.push_str("ty#");
    out.push_str(&ty.id.local.to_string());
    out.push('\n');
}

fn dump_match_arm(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    arm: &MatchArm,
    indent: usize,
) {
    pad(out, indent);
    out.push_str("match_arm#");
    out.push_str(&arm.id.local.to_string());
    out.push('\n');
    dump_pat(out, arena, intern, arm.pat, indent + 1);
    dump_expr(out, arena, intern, arm.body, indent + 1);
}

fn dump_select_arm(
    out: &mut String,
    arena: &AstArena,
    intern: &SymbolInterner,
    arm: &SelectArm,
    indent: usize,
) {
    pad(out, indent);
    out.push_str("select_arm#");
    out.push_str(&arm.id.local.to_string());
    out.push(' ');
    match &arm.kind {
        SelectArmKind::Send {
            chan,
            payload,
            body,
            ..
        } => {
            out.push_str("send\n");
            dump_expr(out, arena, intern, *chan, indent + 1);
            dump_expr(out, arena, intern, *payload, indent + 1);
            dump_expr(out, arena, intern, *body, indent + 1);
        }
        SelectArmKind::Recv {
            pat, chan, body, ..
        } => {
            out.push_str("recv\n");
            dump_pat(out, arena, intern, *pat, indent + 1);
            dump_expr(out, arena, intern, *chan, indent + 1);
            dump_expr(out, arena, intern, *body, indent + 1);
        }
        SelectArmKind::Wait {
            pat, join, body, ..
        } => {
            out.push_str("wait\n");
            dump_pat(out, arena, intern, *pat, indent + 1);
            dump_expr(out, arena, intern, *join, indent + 1);
            dump_expr(out, arena, intern, *body, indent + 1);
        }
        SelectArmKind::Default { body } => {
            out.push_str("default\n");
            dump_expr(out, arena, intern, *body, indent + 1);
        }
        SelectArmKind::Error => out.push_str("error\n"),
    }
}

fn dump_pat(out: &mut String, arena: &AstArena, intern: &SymbolInterner, id: PatId, indent: usize) {
    let pat = &arena.pats[id.0 as usize];
    pad(out, indent);
    out.push_str("pat#");
    out.push_str(&pat.id.local.to_string());
    out.push(' ');
    match &pat.kind {
        PatKind::Or(alts) => {
            out.push_str("or\n");
            for alt in alts.as_slice(&arena.pat_ids) {
                dump_pat(out, arena, intern, *alt, indent + 1);
            }
        }
        PatKind::Array {
            prefix,
            rest,
            suffix,
        } => {
            out.push_str("array\n");
            for elem in prefix.as_slice(&arena.pat_ids) {
                dump_pat(out, arena, intern, *elem, indent + 1);
            }
            if let Some(rest) = rest {
                pad(out, indent + 1);
                out.push_str("rest");
                if let Some(name) = rest.name {
                    out.push(' ');
                    out.push_str(intern.get_str(name));
                }
                out.push('\n');
            }
            for elem in suffix.as_slice(&arena.pat_ids) {
                dump_pat(out, arena, intern, *elem, indent + 1);
            }
        }
        PatKind::At { pat, .. } => {
            out.push_str("at\n");
            dump_pat(out, arena, intern, *pat, indent + 1);
        }
        _ => out.push('\n'),
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
        ExprKind::Struct { fields, .. } => {
            out.push_str("struct_lit\n");
            for field in fields.as_slice(&arena.field_exprs) {
                pad(out, indent + 1);
                out.push_str("field ");
                out.push_str(intern.get_str(field.name));
                out.push('\n');
                if let Some(value) = field.value {
                    dump_expr(out, arena, intern, value, indent + 2);
                }
            }
        }
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
        ExprKind::Match { scrutinee, arms } => {
            out.push_str("match\n");
            dump_expr(out, arena, intern, scrutinee, indent + 1);
            for arm in arms.as_slice(&arena.match_arms) {
                dump_match_arm(out, arena, intern, arm, indent + 1);
            }
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
        ExprKind::Select { arms } => {
            out.push_str("select\n");
            for arm in arms.as_slice(&arena.select_arms) {
                dump_select_arm(out, arena, intern, arm, indent + 1);
            }
        }
        ExprKind::Async(inner) => {
            out.push_str("async\n");
            dump_expr(out, arena, intern, inner, indent + 1);
        }
        ExprKind::Closure(_) => out.push_str("closure\n"),
        ExprKind::Call {
            callee,
            type_args: _,
            args,
        } => {
            out.push_str("call\n");
            dump_expr(out, arena, intern, callee, indent + 1);
            for arg in args.as_slice(&arena.expr_ids) {
                dump_expr(out, arena, intern, *arg, indent + 1);
            }
        }
        ExprKind::TypeApp { base, .. } => {
            out.push_str("type_app\n");
            dump_expr(out, arena, intern, base, indent + 1);
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
        ExprKind::FString { parts } => {
            out.push_str("fstring\n");
            for part in parts.as_slice(&arena.fstring_parts) {
                pad(out, indent + 1);
                match part {
                    super::super::ast::FStringPart::Text { .. } => out.push_str("text\n"),
                    super::super::ast::FStringPart::Interp { expr, .. } => {
                        out.push_str("interp\n");
                        dump_expr(out, arena, intern, *expr, indent + 2);
                    }
                }
            }
        }
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
    let cap = arena.next_node as usize;
    if cap == 0 {
        return true;
    }
    let mut used = vec![false; cap];
    let mut mark = |id: AstNodeId| -> bool {
        let slot = id.local as usize;
        if slot >= used.len() || used[slot] {
            return false;
        }
        used[slot] = true;
        true
    };
    for item in &arena.items {
        if !mark(item.id) {
            return false;
        }
    }
    for expr in &arena.exprs {
        if !mark(expr.id) {
            return false;
        }
        if let ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) = expr.kind {
            let child = &arena.exprs[inner.0 as usize];
            if child.span.start() == expr.span.start() && child.id.local <= expr.id.local {
                return false;
            }
        }
    }
    for stmt in &arena.stmts {
        if !mark(stmt.id) {
            return false;
        }
    }
    for pat in &arena.pats {
        if !mark(pat.id) {
            return false;
        }
    }
    for ty in &arena.tys {
        if !mark(ty.id) {
            return false;
        }
    }
    for path in &arena.paths {
        if !mark(path.id) {
            return false;
        }
    }
    for decl in &arena.fns {
        if !mark(decl.id) {
            return false;
        }
    }
    for attr in &arena.attrs {
        if !mark(attr.id) {
            return false;
        }
    }
    for param in &arena.generic_params {
        if !mark(param.id) {
            return false;
        }
    }
    for bound in &arena.bounds {
        if !mark(bound.id) {
            return false;
        }
    }
    for param in &arena.params {
        if !mark(param.id) {
            return false;
        }
    }
    for field in &arena.fields {
        if !mark(field.id) {
            return false;
        }
    }
    for variant in &arena.variants {
        if !mark(variant.id) {
            return false;
        }
    }
    for arm in &arena.match_arms {
        if !mark(arm.id) {
            return false;
        }
    }
    for arm in &arena.select_arms {
        if !mark(arm.id) {
            return false;
        }
    }
    true
}
