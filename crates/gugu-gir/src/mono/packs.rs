//! Pack bodies are checked parametrically and expanded before instance edge collection.
use super::keys::MonoContext;
use crate::{
    Diagnostic, hir,
    semantics::{Ty, substitute},
};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
mod rebase;
mod types;
use rebase::{Map, children, descendant, dispatches, shift};

fn invalid(message: &str) -> Diagnostic {
    crate::gir::gir_error(message, None)
}

pub struct Expanded {
    pub module: hir::Module,
    pub expressions: usize,
    pub locals: usize,
    pub scopes: Vec<hir::ScopeId>,
    pub cleanup: Vec<u32>,
    pub plans: Vec<u32>,
}

pub fn element(module: &hir::Module, ty: hir::TypeId) -> Option<&str> {
    let hir::Type::Parameter { owner, index } = module.types.get(ty.index())? else {
        return None;
    };
    let name = &module.definitions[owner.index()].parameters[*index as usize].name;
    name.starts_with("$pack:").then_some(name)
}

pub fn expand(
    context: &MonoContext<'_>,
    definition: hir::DefId,
    bindings: &BTreeMap<String, Ty>,
) -> Result<Option<Expanded>, Diagnostic> {
    let Some(index) = context.owner_of[definition.index()] else {
        return Ok(None);
    };
    let source = &context.module.owners[index];
    if !source.expressions.iter().any(|expr| matches!(expr.kind, hir::ExprKind::For { pattern, .. } if element(context.module, source.patterns[pattern.index()].ty).is_some())) { return Ok(None) }
    let mut module = context.module.clone();
    let expressions = source.expressions.len();
    let locals = source.locals.len();
    let mut expander = Expander {
        context,
        source,
        output: source.clone(),
        intern: module
            .types
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, ty)| (ty, hir::TypeId(i as u32)))
            .collect(),
        module: &mut module,
        scopes: (0..source.scopes.len())
            .map(|i| hir::ScopeId(i as u32))
            .collect(),
        cleanup: (0..source.cleanup.len() as u32).collect(),
        plans: (0..source.cleanup_plans.len() as u32).collect(),
    };
    let map = Map::identity(source);
    expander.visit(source.body, &map, bindings)?;
    expander.prune();
    let scopes = expander.scopes;
    let cleanup = expander.cleanup;
    let plans = expander.plans;
    module.owners[index] = expander.output;
    Ok(Some(Expanded {
        module,
        expressions,
        locals,
        scopes,
        cleanup,
        plans,
    }))
}

struct Expander<'a, 'b> {
    context: &'a MonoContext<'b>,
    source: &'a hir::Owner,
    output: hir::Owner,
    module: &'a mut hir::Module,
    intern: BTreeMap<hir::Type, hir::TypeId>,
    scopes: Vec<hir::ScopeId>,
    cleanup: Vec<u32>,
    plans: Vec<u32>,
}
impl Expander<'_, '_> {
    fn visit(
        &mut self,
        source: hir::ExprId,
        map: &Map,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<(), Diagnostic> {
        let expression = &self.source.expressions[source.index()];
        if let hir::ExprKind::For {
            pattern,
            value,
            body,
            ..
        } = expression.kind
            && let Some(name) = element(
                self.context.module,
                self.source.patterns[pattern.index()].ty,
            )
        {
            let name = name.to_owned();
            let tuple = self
                .context
                .type_at(self.source.expression_types[value.index()], bindings)?;
            let Ty::Tuple(elements) = tuple else {
                return Err(invalid("pack loop requires a concrete parameter pack"));
            };
            let mut statements = Vec::new();
            for (index, ty) in elements.into_iter().enumerate() {
                let mut bindings = bindings.clone();
                bindings.insert(name.clone(), ty.clone());
                let child = self.copy(map, expression.scope, &bindings)?;
                self.visit(body, &child, &bindings)?;
                let field = hir::ExprId(self.output.expressions.len() as u32);
                let base = hir::ExprId(field.0 + 1);
                let field_ty = self.ty(&ty)?;
                let mut metadata = expression.clone();
                metadata.scope = child.scopes[expression.scope.index()];
                metadata.unwind_plan += child.plans;
                metadata.adjustments = 0..0;
                metadata.kind = hir::ExprKind::Field {
                    base,
                    index: index as u32,
                };
                self.output.expressions.push(metadata.clone());
                self.output.expression_types.push(field_ty);
                self.output.expression_inputs.push(field_ty);
                metadata.kind = self.source.expressions[value.index()].kind.clone();
                if let hir::ExprKind::Resolved(hir::Res::Local(ref mut local)) = metadata.kind {
                    map.local(local);
                } else {
                    return Err(invalid("pack iteration requires a pack parameter"));
                }
                self.output.expressions.push(metadata.clone());
                let pack_ty = self.source.expression_types[value.index()];
                self.output.expression_types.push(pack_ty);
                self.output.expression_inputs.push(pack_ty);
                for kind in [
                    hir::StatementKind::Let {
                        pattern: hir::PatternId(pattern.0 + child.patterns),
                        value: Some(field),
                        otherwise: None,
                    },
                    hir::StatementKind::Expression(hir::ExprId(body.0 + child.expressions)),
                ] {
                    let stmt = hir::StmtId(self.output.statements.len() as u32);
                    self.output.statements.push(hir::Statement {
                        location: metadata.location.clone(),
                        scope: metadata.scope,
                        unwind_plan: metadata.unwind_plan,
                        kind,
                    });
                    statements.push(stmt);
                }
            }
            let start = self.output.statement_ids.len() as u32;
            self.output.statement_ids.extend(statements);
            self.output.expressions[(source.0 + map.expressions) as usize].kind =
                hir::ExprKind::Block {
                    statements: start..self.output.statement_ids.len() as u32,
                    tail: None,
                    end_plan: None,
                };
        } else {
            for child in children(self.source, source) {
                self.visit(child, map, bindings)?;
            }
        }
        Ok(())
    }
    fn copy(
        &mut self,
        parent: &Map,
        scope: hir::ScopeId,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<Map, Diagnostic> {
        let mut map = parent.clone();
        macro_rules! offset {($($field:ident),*)=>{$(map.$field=self.output.$field.len() as u32;)*};}
        offset!(
            expressions,
            statements,
            patterns,
            expression_ids,
            statement_ids,
            pattern_ids,
            scope_ids,
            arms,
            select_arms,
            fields,
            pattern_fields,
            string_parts,
            dispatches,
            adjustments,
            assembly
        );
        map.plans = self.output.cleanup_plans.len() as u32;
        self.plans.extend(0..self.source.cleanup_plans.len() as u32);
        map.actions = self.output.cleanup_actions.len() as u32;
        let hidden = bindings
            .iter()
            .filter(|(name, _)| name.starts_with("$pack:"))
            .map(|(n, t)| (n.clone(), t.clone()))
            .collect::<BTreeMap<_, _>>();
        let location = &self.source.scopes[scope.index()].location;
        for (i, local) in self.source.locals.iter().enumerate() {
            if local.location.source == location.source
                && local.location.expansion == location.expansion
                && local.location.start >= location.start
                && local.location.end <= location.end
            {
                let mut local = local.clone();
                self.rebind(&mut local.ty, &hidden)?;
                map.locals[i] = hir::LocalId(self.output.locals.len() as u32);
                self.output.locals.push(local);
            }
        }
        for (i, source) in self.source.scopes.iter().enumerate() {
            if descendant(self.source, hir::ScopeId(i as u32), scope) {
                let mut cloned = source.clone();
                if let Some(p) = &mut cloned.parent {
                    map.scope(p)
                }
                cloned.unwind_plan += map.plans;
                map.scopes[i] = hir::ScopeId(self.output.scopes.len() as u32);
                self.output.scopes.push(cloned);
                self.scopes.push(hir::ScopeId(i as u32));
            }
        }
        for (i, cleanup) in self.source.cleanup.iter().enumerate() {
            if descendant(self.source, cleanup.scope, scope) {
                map.cleanup[i] = self.output.cleanup.len() as u32;
                self.output.cleanup.push(cleanup.clone());
                self.cleanup.push(i as u32);
            }
        }
        let mut copy = self.source.clone();
        for id in &mut copy.expression_ids {
            map.expr(id)
        }
        for id in &mut copy.statement_ids {
            id.0 += map.statements
        }
        for id in &mut copy.pattern_ids {
            map.pattern(id)
        }
        for id in &mut copy.scope_ids {
            map.scope(id)
        }
        for ty in copy
            .expression_types
            .iter_mut()
            .chain(&mut copy.expression_inputs)
        {
            self.rebind(ty, &hidden)?
        }
        for p in &mut copy.patterns {
            self.rebind(&mut p.ty, &hidden)?;
            match &mut p.kind {
                hir::PatternKind::Bind(l) => map.local(l),
                hir::PatternKind::Ref(p) => map.pattern(p),
                hir::PatternKind::At { local, pattern } => {
                    map.local(local);
                    map.pattern(pattern)
                }
                hir::PatternKind::Tuple(r) | hir::PatternKind::Or(r) => shift(r, map.pattern_ids),
                hir::PatternKind::Array {
                    prefix,
                    rest,
                    suffix,
                    ..
                } => {
                    shift(prefix, map.pattern_ids);
                    shift(suffix, map.pattern_ids);
                    if let Some(l) = rest {
                        map.local(l)
                    }
                }
                hir::PatternKind::Construct { fields, .. } => shift(fields, map.pattern_fields),
                _ => {}
            }
        }
        for expr in &mut copy.expressions {
            map.scope(&mut expr.scope);
            expr.unwind_plan += map.plans;
            shift(&mut expr.adjustments, map.adjustments);
            self.kind(&mut expr.kind, &map, &hidden)?;
        }
        for stmt in &mut copy.statements {
            map.scope(&mut stmt.scope);
            stmt.unwind_plan += map.plans;
            match &mut stmt.kind {
                hir::StatementKind::Let {
                    pattern,
                    value,
                    otherwise,
                } => {
                    map.pattern(pattern);
                    for v in value.iter_mut().chain(otherwise) {
                        map.expr(v)
                    }
                }
                hir::StatementKind::Static { local, .. } => map.local(local),
                hir::StatementKind::Assign {
                    place,
                    value,
                    dispatch,
                    ..
                } => {
                    map.expr(place);
                    map.expr(value);
                    if let Some(d) = dispatch {
                        *d += map.dispatches
                    }
                }
                hir::StatementKind::Defer(d) => *d = map.cleanup[*d as usize],
                hir::StatementKind::Expression(e) => map.expr(e),
                hir::StatementKind::Yield => {}
            }
        }
        for arm in &mut copy.arms {
            map.pattern(&mut arm.pattern);
            if let Some(g) = &mut arm.guard {
                map.expr(g)
            }
            map.expr(&mut arm.body)
        }
        for arm in &mut copy.select_arms {
            match arm {
                hir::SelectArm::Send {
                    channel,
                    value,
                    body,
                } => {
                    map.expr(channel);
                    map.expr(value);
                    map.expr(body)
                }
                hir::SelectArm::Recv {
                    channel,
                    pattern,
                    body,
                } => {
                    map.expr(channel);
                    map.pattern(pattern);
                    map.expr(body)
                }
                hir::SelectArm::Wait {
                    join,
                    pattern,
                    body,
                } => {
                    map.expr(join);
                    map.pattern(pattern);
                    map.expr(body)
                }
                hir::SelectArm::Default { body } => map.expr(body),
            }
        }
        for field in &mut copy.fields {
            map.expr(&mut field.value)
        }
        for field in &mut copy.pattern_fields {
            map.pattern(&mut field.pattern)
        }
        for part in &mut copy.string_parts {
            if let hir::StringPart::Value {
                expression,
                dispatch,
                format,
            } = part
            {
                map.expr(expression);
                if let Some(d) = dispatch {
                    *d += map.dispatches
                }
                for count in format.width.iter_mut().chain(format.precision.iter_mut()) {
                    if let hir::FormatCount::Value(v) = count {
                        map.expr(v)
                    }
                }
            }
        }
        for dispatch in &mut copy.dispatches {
            self.rebind(&mut dispatch.self_ty, &hidden)?;
            self.rebind(&mut dispatch.signature, &hidden)?;
            if let Some(i) = &mut dispatch.interface {
                for t in &mut i.arguments {
                    self.rebind(t, &hidden)?
                }
            }
        }
        for adjustment in &mut copy.adjustments {
            match adjustment {
                hir::Adjustment::Dereference => {}
                hir::Adjustment::ArrayToSlice(t)
                | hir::Adjustment::NeverTo(t)
                | hir::Adjustment::Erase(t)
                | hir::Adjustment::Opaque(t)
                | hir::Adjustment::Instantiate(t) => self.rebind(t, &hidden)?,
            }
        }
        for check in &mut copy.checks {
            map.expr(&mut check.expression);
            match &mut check.kind {
                hir::CheckKind::Division { ty, divisor } => {
                    self.rebind(ty, &hidden)?;
                    map.expr(divisor)
                }
                hir::CheckKind::Shift { ty, amount } => {
                    self.rebind(ty, &hidden)?;
                    map.expr(amount)
                }
                hir::CheckKind::FloatToInt { value, .. }
                | hir::CheckKind::UnicodeScalar { value } => map.expr(value),
                _ => {}
            }
        }
        for call in &mut copy.variadic_calls {
            map.expr(&mut call.expression);
            self.rebind(&mut call.element, &hidden)?
        }
        for call in &mut copy.foreign_calls {
            map.expr(&mut call.expression)
        }
        for constraint in &mut copy.borrow_constraints {
            map.expr(&mut constraint.expression);
            self.rebind(&mut constraint.base, &hidden)?;
            self.rebind(&mut constraint.target, &hidden)?
        }
        for plan in &mut copy.cleanup_plans {
            map.exit(&mut plan.exit);
            shift(&mut plan.actions, map.actions);
            if let Some(d) = &mut plan.destination {
                map.scope(d)
            }
        }
        for action in &mut copy.cleanup_actions {
            match action {
                hir::CleanupAction::Action { cleanup, .. } => {
                    *cleanup = map.cleanup[*cleanup as usize]
                }
                hir::CleanupAction::DrainChain { until } => {
                    if let Some(c) = until {
                        *c = map.cleanup[*c as usize]
                    }
                }
            }
        }
        for (i, cleanup) in self.source.cleanup.iter().enumerate() {
            if descendant(self.source, cleanup.scope, scope) {
                let mut cleanup = cleanup.clone();
                cleanup.statement.0 += map.statements;
                map.expr(&mut cleanup.body);
                map.scope(&mut cleanup.scope);
                cleanup.unwind_plan += map.plans;
                for l in &mut cleanup.captures {
                    map.local(l)
                }
                self.output.cleanup[map.cleanup[i] as usize] = cleanup
            }
        }
        for asm in &mut copy.assembly {
            for operand in &mut asm.operands {
                map.expr(&mut operand.value)
            }
        }
        macro_rules! append {($($field:ident),*)=>{$(self.output.$field.extend(copy.$field);)*};}
        append!(
            expressions,
            expression_types,
            expression_inputs,
            statements,
            patterns,
            expression_ids,
            statement_ids,
            pattern_ids,
            scope_ids,
            arms,
            select_arms,
            fields,
            pattern_fields,
            string_parts,
            dispatches,
            adjustments,
            checks,
            variadic_calls,
            borrow_constraints,
            foreign_calls,
            cleanup_plans,
            cleanup_actions,
            assembly
        );
        Ok(map)
    }
    fn prune(&mut self) {
        let mut active = BTreeSet::new();
        let mut pending = vec![self.output.body];
        while let Some(id) = pending.pop() {
            if active.insert(id.0) {
                pending.extend(children(&self.output, id));
            }
        }
        let unit = self.intern(hir::Type::Unit);
        let boolean = self.intern(hir::Type::Bool);
        let mut used_dispatches = BTreeSet::new();
        let mut used_locals = BTreeSet::new();
        for &id in &active {
            let e = &self.output.expressions[id as usize];
            if let hir::ExprKind::Resolved(hir::Res::Local(l)) = e.kind {
                used_locals.insert(l.0);
            }
            dispatches(&e.kind, &mut used_dispatches);
        }
        for stmt in &self.output.statements {
            if let hir::StatementKind::Assign {
                place,
                dispatch: Some(d),
                ..
            } = stmt.kind
                && active.contains(&place.0)
            {
                used_dispatches.insert(d);
            }
        }
        for (i, expr) in self.output.expressions.iter_mut().enumerate() {
            if !active.contains(&(i as u32)) {
                expr.kind = hir::ExprKind::Literal(hir::Literal::Bool(false));
                expr.adjustments = 0..0;
                expr.effects = hir::Effects(0);
                self.output.expression_types[i] = boolean;
                self.output.expression_inputs[i] = boolean;
            }
        }
        for (i, d) in self.output.dispatches.iter_mut().enumerate() {
            if !used_dispatches.contains(&(i as u32)) {
                d.interface = None;
                d.function = None;
                d.implementation = None;
                d.dynamic = false;
                d.self_ty = unit;
                d.signature = unit;
            }
        }
        let unresolved = |id: hir::TypeId, module: &hir::Module| -> bool {
            fn has(id: hir::TypeId, m: &hir::Module) -> bool {
                match &m.types[id.index()] {
                    hir::Type::Parameter { .. } => element(m, id).is_some(),
                    hir::Type::Ref(t)
                    | hir::Type::Ptr(t)
                    | hir::Type::Slice(t)
                    | hir::Type::Array(t, _)
                    | hir::Type::Option(t)
                    | hir::Type::Chan(t)
                    | hir::Type::Join(t)
                    | hir::Type::MaybeUninit(t) => has(*t, m),
                    hir::Type::Tuple(ts)
                    | hir::Type::Named { arguments: ts, .. }
                    | hir::Type::Callable { arguments: ts, .. }
                    | hir::Type::Opaque { arguments: ts, .. } => ts.iter().any(|t| has(*t, m)),
                    _ => false,
                }
            }
            has(id, module)
        };
        for (i, l) in self.output.locals.iter_mut().enumerate() {
            if !used_locals.contains(&(i as u32)) && unresolved(l.ty, self.module) {
                l.ty = unit;
            }
        }
        for p in &mut self.output.patterns {
            if unresolved(p.ty, self.module) {
                p.ty = unit;
            }
        }
        self.output
            .checks
            .retain(|p| active.contains(&p.expression.0));
        self.output
            .variadic_calls
            .retain(|p| active.contains(&p.expression.0));
        self.output
            .foreign_calls
            .retain(|p| active.contains(&p.expression.0));
        self.output
            .borrow_constraints
            .retain(|p| active.contains(&p.expression.0));
    }
}
