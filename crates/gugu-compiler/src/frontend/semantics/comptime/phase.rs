//! 只发现 late 依赖，不执行分支；early 使用点不能通过绑定或函数隐藏阶段依赖。
use super::super::model::Model;
use crate::frontend::ast::*;
use std::collections::BTreeSet;

impl Model<'_> {
    pub(crate) fn depends_on_late(&self, module: usize, expression: ExprId) -> bool {
        self.late_walk(module, expression, &mut BTreeSet::new())
    }

    fn late_walk(
        &self,
        module: usize,
        expression: ExprId,
        seen: &mut BTreeSet<(usize, u32)>,
    ) -> bool {
        if !seen.insert((module, expression.0)) {
            return false;
        }
        let arena = &self.modules[module].arena;
        let mut children = Vec::new();
        match arena.exprs[expression.0 as usize].kind {
            ExprKind::Intrinsic {
                kind: IntrinsicKind::TypeIdCount,
                ..
            } => return true,
            ExprKind::Path(path) => {
                if let Ok(def) = self.resolve(module, &self.path(module, path)) {
                    match self.modules[def.module].arena.items[def.item.0 as usize].kind {
                        ItemKind::Const {
                            value: Some(value), ..
                        }
                        | ItemKind::Static { value, .. } => {
                            return self.late_walk(def.module, value, seen);
                        }
                        _ => {}
                    }
                }
            }
            ExprKind::Call { callee, args, .. } => {
                children.extend_from_slice(args.as_slice(&arena.expr_ids));
                let mut base = callee;
                while let ExprKind::TypeApp { base: inner, .. } | ExprKind::Paren(inner) =
                    arena.exprs[base.0 as usize].kind
                {
                    base = inner;
                }
                match arena.exprs[base.0 as usize].kind {
                    ExprKind::Field { base, name } if self.name(module, name) == "as_int" => {
                        if self
                            .constant_type(module, base)
                            .map_or(true, |ty| ty == super::super::Ty::TypeId)
                        {
                            return true;
                        }
                    }
                    ExprKind::Path(path) => {
                        if let Ok(def) = self.resolve(module, &self.path(module, path)) {
                            if let ItemKind::Function(function) =
                                self.modules[def.module].arena.items[def.item.0 as usize].kind
                            {
                                if let FnBody::Block(body) | FnBody::Eq(body) =
                                    self.modules[def.module].arena.fns[function.0 as usize].body
                                {
                                    if self.late_walk(def.module, body, seen) {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                children.push(callee);
            }
            ExprKind::Binary { lhs, rhs, op } => {
                if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
                    && self
                        .constant_type(module, lhs)
                        .is_ok_and(|ty| ty == super::super::Ty::TypeId)
                {
                    return true;
                }
                children.extend([lhs, rhs]);
            }
            ExprKind::Block { stmts, tail } => {
                children.extend(tail);
                for id in stmts.as_slice(&arena.stmt_ids) {
                    match arena.stmts[id.0 as usize].kind {
                        StmtKind::Let {
                            init, else_block, ..
                        } => {
                            children.extend(init);
                            children.extend(else_block);
                        }
                        StmtKind::Assign { place, value, .. } => children.extend([place, value]),
                        StmtKind::Static { value, .. }
                        | StmtKind::Expr { expr: value, .. }
                        | StmtKind::Defer { body: value, .. }
                        | StmtKind::SourceMacro { body: value } => children.push(value),
                        StmtKind::Yield => {}
                    }
                }
            }
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                children.extend([cond, then_block]);
                children.extend(else_branch);
            }
            ExprKind::While { cond, body } => children.extend([cond, body]),
            ExprKind::For { iter, body, .. } => children.extend([iter, body]),
            ExprKind::Match { scrutinee, arms } => {
                children.push(scrutinee);
                for arm in arms.as_slice(&arena.match_arms) {
                    children.push(arm.body);
                    children.extend(arm.guard);
                }
            }
            ExprKind::Repeat { elem, count } => children.extend([elem, count]),
            ExprKind::Range { start, end } => children.extend([start, end]),
            ExprKind::Tuple(items) | ExprKind::Array(items) => {
                children.extend_from_slice(items.as_slice(&arena.expr_ids))
            }
            ExprKind::Struct { fields, .. } => {
                for field in fields.as_slice(&arena.field_exprs) {
                    children.extend(field.value);
                }
            }
            ExprKind::Index { base, index } => {
                children.push(base);
                match index {
                    IndexKind::Expr(e) => children.push(e),
                    IndexKind::Range { start, end } => {
                        children.extend(start);
                        children.extend(end);
                    }
                }
            }
            ExprKind::Paren(e)
            | ExprKind::Unsafe(e)
            | ExprKind::Comptime(e)
            | ExprKind::Loop(e)
            | ExprKind::Try(e)
            | ExprKind::TryOp(e)
            | ExprKind::Unary { expr: e, .. }
            | ExprKind::TypeApp { base: e, .. }
            | ExprKind::Field { base: e, .. }
            | ExprKind::TupleField { base: e, .. } => children.push(e),
            ExprKind::Return(e) | ExprKind::Break(e) => children.extend(e),
            ExprKind::Intrinsic { args, .. } => {
                children.extend_from_slice(args.as_slice(&arena.expr_ids))
            }
            ExprKind::FString { parts } => {
                for part in parts.as_slice(&arena.fstring_parts) {
                    if let FStringPart::Interp { expr, .. } = part {
                        children.push(*expr);
                    }
                }
            }
            _ => {}
        }
        children
            .into_iter()
            .any(|child| self.late_walk(module, child, seen))
    }
}
