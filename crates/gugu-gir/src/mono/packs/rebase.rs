use super::*;

#[derive(Clone)]
pub(super) struct Map {
    pub(super) expressions: u32,
    pub(super) statements: u32,
    pub(super) patterns: u32,
    pub(super) expression_ids: u32,
    pub(super) statement_ids: u32,
    pub(super) pattern_ids: u32,
    pub(super) scope_ids: u32,
    pub(super) arms: u32,
    pub(super) select_arms: u32,
    pub(super) fields: u32,
    pub(super) pattern_fields: u32,
    pub(super) string_parts: u32,
    pub(super) dispatches: u32,
    pub(super) adjustments: u32,
    pub(super) plans: u32,
    pub(super) actions: u32,
    pub(super) assembly: u32,
    pub(super) locals: Vec<hir::LocalId>,
    pub(super) scopes: Vec<hir::ScopeId>,
    pub(super) cleanup: Vec<u32>,
}
impl Map {
    pub(super) fn identity(owner: &hir::Owner) -> Self {
        Self {
            expressions: 0,
            statements: 0,
            patterns: 0,
            expression_ids: 0,
            statement_ids: 0,
            pattern_ids: 0,
            scope_ids: 0,
            arms: 0,
            select_arms: 0,
            fields: 0,
            pattern_fields: 0,
            string_parts: 0,
            dispatches: 0,
            adjustments: 0,
            plans: 0,
            actions: 0,
            assembly: 0,
            locals: (0..owner.locals.len())
                .map(|i| hir::LocalId(i as u32))
                .collect(),
            scopes: (0..owner.scopes.len())
                .map(|i| hir::ScopeId(i as u32))
                .collect(),
            cleanup: (0..owner.cleanup.len() as u32).collect(),
        }
    }
    pub(super) fn expr(&self, id: &mut hir::ExprId) {
        id.0 += self.expressions;
    }
    pub(super) fn pattern(&self, id: &mut hir::PatternId) {
        id.0 += self.patterns;
    }
    pub(super) fn local(&self, id: &mut hir::LocalId) {
        *id = self.locals[id.index()];
    }
    pub(super) fn scope(&self, id: &mut hir::ScopeId) {
        *id = self.scopes[id.index()];
    }
    pub(super) fn target(&self, target: &mut hir::ExitTarget) {
        match target {
            hir::ExitTarget::Return => {}
            hir::ExitTarget::Break(scope)
            | hir::ExitTarget::Continue(scope)
            | hir::ExitTarget::Try(scope) => self.scope(scope),
        }
    }
    pub(super) fn exit(&self, target: &mut hir::ExitKind) {
        match target {
            hir::ExitKind::Return => {}
            hir::ExitKind::Break(scope)
            | hir::ExitKind::Continue(scope)
            | hir::ExitKind::Try(scope)
            | hir::ExitKind::BlockEnd(scope)
            | hir::ExitKind::Unwind(scope) => self.scope(scope),
        }
    }
    pub(super) fn call(&self, target: &mut hir::CallTarget) {
        match target {
            hir::CallTarget::Value(id) => self.expr(id),
            hir::CallTarget::Dispatch(id) => *id += self.dispatches,
            _ => {}
        }
    }
}
pub(super) fn shift(range: &mut Range<u32>, offset: u32) {
    range.start += offset;
    range.end += offset;
}
pub(super) fn descendant(
    owner: &hir::Owner,
    mut scope: hir::ScopeId,
    ancestor: hir::ScopeId,
) -> bool {
    loop {
        if scope == ancestor {
            return true;
        }
        let Some(parent) = owner.scopes[scope.index()].parent else {
            return false;
        };
        scope = parent;
    }
}

impl Expander<'_, '_> {
    pub(super) fn kind(
        &mut self,
        kind: &mut hir::ExprKind,
        m: &Map,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<(), Diagnostic> {
        use hir::ExprKind as E;
        match kind {
            E::Resolved(r) => match r {
                hir::Res::Local(l) => m.local(l),
                hir::Res::Primitive(t) => self.rebind(t, bindings)?,
                hir::Res::Associated {
                    self_ty, interface, ..
                } => {
                    self.rebind(self_ty, bindings)?;
                    if let Some(i) = interface {
                        for t in &mut i.arguments {
                            self.rebind(t, bindings)?
                        }
                    }
                }
                _ => {}
            },
            E::Closure { captures, .. } => {
                for local in captures {
                    m.local(local)
                }
            }
            E::Literal(_) | E::Spawn { .. } => {}
            E::Tuple(r) | E::Array(r) => shift(r, m.expression_ids),
            E::Repeat { value, .. } | E::Comptime { value } => m.expr(value),
            E::Construct { fields, .. } => shift(fields, m.fields),
            E::Block {
                statements,
                tail,
                end_plan,
            } => {
                shift(statements, m.statement_ids);
                if let Some(v) = tail {
                    m.expr(v)
                }
                if let Some(p) = end_plan {
                    *p += m.plans
                }
            }
            E::If {
                condition,
                then_value,
                else_value,
            } => {
                m.expr(condition);
                m.expr(then_value);
                if let Some(v) = else_value {
                    m.expr(v)
                }
            }
            E::Match { value, arms } => {
                m.expr(value);
                shift(arms, m.arms)
            }
            E::Loop { body } => m.expr(body),
            E::While { condition, body } => {
                m.expr(condition);
                m.expr(body)
            }
            E::For {
                pattern,
                value,
                body,
                into_iter,
                next,
            } => {
                m.pattern(pattern);
                m.expr(value);
                m.expr(body);
                for d in into_iter.iter_mut().chain(next) {
                    *d += m.dispatches
                }
            }
            E::Try { body, from_value } => {
                m.expr(body);
                if let Some(d) = from_value {
                    *d += m.dispatches
                }
            }
            E::TryExit {
                value,
                branch,
                from_error,
                target,
                cleanup,
                plan,
            } => {
                m.expr(value);
                for d in branch.iter_mut().chain(from_error) {
                    *d += m.dispatches
                }
                m.target(target);
                shift(cleanup, m.scope_ids);
                *plan += m.plans
            }
            E::Select { arms } => shift(arms, m.select_arms),
            E::Call {
                target,
                receiver,
                arguments,
            }
            | E::SpawnCall {
                target,
                receiver,
                arguments,
            } => {
                m.call(target);
                if let hir::CallTarget::Constructor { ty, .. } = target {
                    self.rebind(ty, bindings)?
                }
                if let Some(r) = receiver {
                    m.expr(r)
                }
                shift(arguments, m.expression_ids)
            }
            E::Intrinsic {
                arguments, types, ..
            } => {
                shift(arguments, m.expression_ids);
                for t in types {
                    self.rebind(t, bindings)?
                }
            }
            E::Field { base, .. } => m.expr(base),
            E::Index {
                base,
                index,
                read,
                write,
            } => {
                m.expr(base);
                m.expr(index);
                for d in read.iter_mut().chain(write) {
                    *d += m.dispatches
                }
            }
            E::Slice { base, start, end } => {
                m.expr(base);
                for v in start.iter_mut().chain(end) {
                    m.expr(v)
                }
            }
            E::Unary { value, .. } => m.expr(value),
            E::Binary {
                left,
                right,
                dispatch,
                ..
            } => {
                m.expr(left);
                m.expr(right);
                if let Some(d) = dispatch {
                    *d += m.dispatches
                }
            }
            E::Range { start, end } => {
                m.expr(start);
                m.expr(end)
            }
            E::Assembly(i) => *i += m.assembly,
            E::Exit {
                target,
                value,
                cleanup,
                plan,
            } => {
                m.target(target);
                if let Some(v) = value {
                    m.expr(v)
                }
                shift(cleanup, m.scope_ids);
                *plan += m.plans
            }
            E::String { parts } => shift(parts, m.string_parts),
            E::LetCondition { pattern, value } => {
                m.pattern(pattern);
                m.expr(value)
            }
        }
        Ok(())
    }
}

pub(super) fn dispatches(kind: &hir::ExprKind, out: &mut BTreeSet<u32>) {
    use hir::ExprKind as E;
    match kind {
        E::Call {
            target: hir::CallTarget::Dispatch(d),
            ..
        }
        | E::SpawnCall {
            target: hir::CallTarget::Dispatch(d),
            ..
        } => {
            out.insert(*d);
        }
        E::For {
            into_iter, next, ..
        } => out.extend(into_iter.iter().chain(next).copied()),
        E::Try { from_value, .. } => out.extend(from_value),
        E::TryExit {
            branch, from_error, ..
        } => out.extend(branch.iter().chain(from_error).copied()),
        E::Index { read, write, .. } => out.extend(read.iter().chain(write).copied()),
        E::Binary { dispatch, .. } => out.extend(dispatch),
        _ => {}
    }
}

pub(super) fn children(o: &hir::Owner, id: hir::ExprId) -> Vec<hir::ExprId> {
    use hir::ExprKind as E;
    let mut out = Vec::new();
    let ids = |r: &Range<u32>| o.expression_ids[r.start as usize..r.end as usize].to_vec();
    match &o.expressions[id.index()].kind {
        E::Tuple(r) | E::Array(r) | E::Intrinsic { arguments: r, .. } => out.extend(ids(r)),
        E::Repeat { value, .. }
        | E::Comptime { value }
        | E::Field { base: value, .. }
        | E::Unary { value, .. }
        | E::LetCondition { value, .. } => out.push(*value),
        E::Construct { fields, .. } => out.extend(
            o.fields[fields.start as usize..fields.end as usize]
                .iter()
                .map(|f| f.value),
        ),
        E::Block {
            statements, tail, ..
        } => {
            for s in &o.statement_ids[statements.start as usize..statements.end as usize] {
                match &o.statements[s.index()].kind {
                    hir::StatementKind::Let {
                        value, otherwise, ..
                    } => out.extend(value.iter().chain(otherwise).copied()),
                    hir::StatementKind::Assign { place, value, .. } => out.extend([*place, *value]),
                    hir::StatementKind::Defer(d) => out.push(o.cleanup[*d as usize].body),
                    hir::StatementKind::Expression(e) => out.push(*e),
                    _ => {}
                }
            }
            out.extend(tail)
        }
        E::If {
            condition,
            then_value,
            else_value,
        } => {
            out.extend([*condition, *then_value]);
            out.extend(else_value)
        }
        E::Match { value, arms } => {
            out.push(*value);
            for a in &o.arms[arms.start as usize..arms.end as usize] {
                out.extend(a.guard);
                out.push(a.body)
            }
        }
        E::Loop { body } | E::Try { body, .. } => out.push(*body),
        E::While { condition, body } => out.extend([*condition, *body]),
        E::For { value, body, .. } => out.extend([*value, *body]),
        E::TryExit { value, .. } => out.push(*value),
        E::Call {
            target,
            receiver,
            arguments,
        }
        | E::SpawnCall {
            target,
            receiver,
            arguments,
        } => {
            if let hir::CallTarget::Value(v) = target {
                out.push(*v)
            }
            out.extend(receiver);
            out.extend(ids(arguments))
        }
        E::Index { base, index, .. } => out.extend([*base, *index]),
        E::Slice { base, start, end } => {
            out.push(*base);
            out.extend(start);
            out.extend(end)
        }
        E::Binary { left, right, .. } => out.extend([*left, *right]),
        E::Range { start, end } => out.extend([*start, *end]),
        E::Exit { value, .. } => out.extend(value),
        E::Assembly(i) => out.extend(o.assembly[*i as usize].operands.iter().map(|p| p.value)),
        E::Select { arms } => {
            for a in &o.select_arms[arms.start as usize..arms.end as usize] {
                match a {
                    hir::SelectArm::Send {
                        channel,
                        value,
                        body,
                    } => out.extend([*channel, *value, *body]),
                    hir::SelectArm::Recv { channel, body, .. } => out.extend([*channel, *body]),
                    hir::SelectArm::Wait { join, body, .. } => out.extend([*join, *body]),
                    hir::SelectArm::Default { body } => out.push(*body),
                }
            }
        }
        E::String { parts } => {
            for p in &o.string_parts[parts.start as usize..parts.end as usize] {
                if let hir::StringPart::Value { expression, .. } = p {
                    out.push(*expression)
                }
            }
        }
        _ => {}
    }
    out
}
