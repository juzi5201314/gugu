use super::super::output::CheckedBody;
use super::*;
use crate::Span;
mod expressions;
mod patterns;
mod plans;
mod statements;

struct Facts<'m> {
    body: &'m CheckedBody,
    bindings: BTreeMap<(&'m str, u32, u32), Vec<usize>>,
    dispatch_order: Vec<usize>,
}
impl<'m> Facts<'m> {
    fn new(body: &'m CheckedBody) -> Self {
        let mut bindings: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (slot, origin) in body.slot_origins.iter().enumerate() {
            bindings
                .entry((origin.name.as_str(), origin.start, origin.end))
                .or_default()
                .push(slot);
        }
        let mut dispatch_order: Vec<_> = (0..body.dispatches.len()).collect();
        dispatch_order.sort_by_key(|&index| body.dispatches[index].expression.0);
        Self {
            body,
            bindings,
            dispatch_order,
        }
    }
    fn ty(&self, expression: ast::ExprId) -> Option<&'m Ty> {
        self.body
            .expressions
            .binary_search_by_key(&expression.0, |(id, _)| id.0)
            .ok()
            .map(|index| &self.body.expressions[index].1)
    }
}

struct Inherited {
    slot: usize,
    owner: hir::DefId,
    local: hir::LocalId,
    read: bool,
    written: bool,
    coroutine: bool,
}

struct BodyBuilder<'b, 'f, 'm, 'a> {
    compiler: &'b mut Builder<'m, 'a>,
    facts: &'f Facts<'m>,
    module: usize,
    output: hir::Owner,
    expressions: Vec<Option<hir::Expression>>,
    statements: Vec<Option<hir::Statement>>,
    patterns: Vec<Option<hir::Pattern>>,
    adjustments: Vec<Vec<hir::Adjustment>>,
    expression_map: Vec<Option<hir::ExprId>>,
    statement_map: Vec<Option<hir::StmtId>>,
    dispatch_map: Vec<Option<u32>>,
    slots: Vec<Option<hir::LocalId>>,
    local_sources: Vec<usize>,
    names: BTreeMap<&'m str, hir::LocalId>,
    scope: hir::ScopeId,
    loops: Vec<hir::ScopeId>,
    tries: Vec<hir::ScopeId>,
    unsafe_depth: usize,
}

impl<'m, 'a> Builder<'m, 'a> {
    pub(super) fn lower_owners(&mut self) -> Result<(), Diagnostic> {
        for checked in &self.checked.bodies {
            let module = checked.definition.module;
            let item = &self.model.modules[module].arena.items[checked.definition.item.0 as usize];
            let (function, expression) = match item.kind {
                ast::ItemKind::Function(function) => (
                    Some(function),
                    body_expression(self.model.modules[module].arena.fns[function.0 as usize].body),
                ),
                ast::ItemKind::Const { value, .. } => (None, value),
                ast::ItemKind::Static { value, .. } => (None, Some(value)),
                ast::ItemKind::GlobalAsm { template } => (None, Some(template)),
                _ => continue,
            };
            let Some(expression) = expression else {
                continue;
            };
            let definition = self.identities.item(checked.definition);
            let facts = Facts::new(checked);
            self.lower_owner(&facts, definition, module, function, expression, Vec::new())?;
        }
        Ok(())
    }

    fn lower_owner(
        &mut self,
        facts: &Facts<'m>,
        definition: hir::DefId,
        module: usize,
        function: Option<ast::FnId>,
        expression: ast::ExprId,
        inherited: Vec<Inherited>,
    ) -> Result<(), Diagnostic> {
        if self
            .output
            .owners
            .iter()
            .any(|owner| owner.definition == definition)
        {
            return Ok(());
        }
        let span = function.map_or_else(
            || {
                self.model.modules[module].arena.exprs[expression.0 as usize]
                    .span
                    .clone()
            },
            |function| {
                self.model.modules[module].arena.fns[function.0 as usize]
                    .span
                    .clone()
            },
        );
        let mut body = BodyBuilder::new(self, facts, definition, module, &span)?;
        for capture in inherited {
            body.capture(capture)?;
        }
        if let Some(function) = function {
            body.parameters(function)?;
        }
        body.output.body = if body.compiler.output.definitions[definition.index()].kind
            == hir::DefinitionKind::GlobalAsm
        {
            let id = body.reserve(&Ty::Unit)?;
            body.expression_map[expression.0 as usize] = Some(id);
            let kind = body.assembly(expression)?;
            body.set_expression(id, kind, hir::ScopeId(0), &span, hir::Effects::UNSAFE)?;
            id
        } else {
            body.expression(expression)?
        };
        let owner = body.finish()?;
        self.output.owners.push(owner);
        Ok(())
    }
}

impl<'b, 'f, 'm, 'a> BodyBuilder<'b, 'f, 'm, 'a> {
    fn new(
        compiler: &'b mut Builder<'m, 'a>,
        facts: &'f Facts<'m>,
        definition: hir::DefId,
        module: usize,
        span: &Span,
    ) -> Result<Self, Diagnostic> {
        let location = identity::location(compiler.sources, span)?;
        let output = hir::Owner {
            definition,
            parameters: Vec::new(),
            body: hir::ExprId(0),
            expressions: Vec::new(),
            expression_types: Vec::new(),
            statements: Vec::new(),
            patterns: Vec::new(),
            locals: Vec::new(),
            scopes: vec![hir::Scope {
                parent: None,
                kind: hir::ScopeKind::Function,
                location,
            }],
            expression_ids: Vec::new(),
            statement_ids: Vec::new(),
            pattern_ids: Vec::new(),
            scope_ids: Vec::new(),
            arms: Vec::new(),
            select_arms: Vec::new(),
            fields: Vec::new(),
            pattern_fields: Vec::new(),
            string_parts: Vec::new(),
            dispatches: Vec::new(),
            adjustments: Vec::new(),
            checks: Vec::new(),
            captures: Vec::new(),
            cleanup: Vec::new(),
            assembly: Vec::new(),
            variadic_calls: Vec::new(),
            borrow_constraints: Vec::new(),
            input_fingerprint: compiler.checked.input_fingerprint,
            foreign_calls: Vec::new(),
            expression_inputs: Vec::new(),
        };
        let expression_map = vec![None; compiler.model.modules[module].arena.exprs.len()];
        let statement_map = vec![None; compiler.model.modules[module].arena.stmts.len()];
        let slots = vec![None; facts.body.slots.len()];
        let dispatch_map = vec![None; facts.body.dispatches.len()];
        Ok(Self {
            compiler,
            facts,
            module,
            output,
            expressions: Vec::new(),
            statements: Vec::new(),
            patterns: Vec::new(),
            adjustments: Vec::new(),
            expression_map,
            statement_map,
            dispatch_map,
            slots,
            local_sources: Vec::new(),
            names: BTreeMap::new(),
            scope: hir::ScopeId(0),
            loops: Vec::new(),
            tries: Vec::new(),
            unsafe_depth: 0,
        })
    }

    fn finish(mut self) -> Result<hir::Owner, Diagnostic> {
        self.lower_plans()?;
        for (expression, adjustments) in self.expressions.iter_mut().zip(self.adjustments) {
            let start = checked_id(self.output.adjustments.len())?;
            self.output.adjustments.extend(adjustments);
            expression
                .as_mut()
                .ok_or_else(|| {
                    Diagnostic::error(DiagnosticCode::InvalidType, "HIR 表达式尚未完成形成", None)
                })?
                .adjustments = start..checked_id(self.output.adjustments.len())?;
        }
        self.output.expressions = completed(self.expressions, "表达式")?;
        self.output.statements = completed(self.statements, "语句")?;
        self.output.patterns = completed(self.patterns, "模式")?;
        let fingerprint = *blake3::Hasher::new_derive_key("gugu-hir-owner-input-v1")
            .update(&self.compiler.output.definitions[self.output.definition.index()].key)
            .update(&self.compiler.checked.input_fingerprint)
            .finalize()
            .as_bytes();
        self.output.input_fingerprint = fingerprint;
        Ok(self.output)
    }

    fn arena(&self) -> &'a ast::AstArena {
        &self.compiler.model.modules[self.module].arena
    }
    fn error(&self, message: &str) -> Diagnostic {
        self.compiler.error(self.output.definition, message)
    }
    fn ty(&self, expression: ast::ExprId) -> Result<&'m Ty, Diagnostic> {
        self.facts
            .ty(expression)
            .ok_or_else(|| self.error("HIR 表达式缺少已检查类型"))
    }
    fn type_id(&mut self, ty: &Ty) -> Result<hir::TypeId, Diagnostic> {
        self.compiler.type_id(ty, self.output.definition)
    }
    fn reserve(&mut self, ty: &Ty) -> Result<hir::ExprId, Diagnostic> {
        let id = hir::ExprId(checked_id(self.expressions.len())?);
        self.expressions.push(None);
        self.adjustments.push(Vec::new());
        let ty = self.type_id(ty)?;
        self.output.expression_types.push(ty);
        self.output.expression_inputs.push(ty);
        Ok(id)
    }
    fn set_expression(
        &mut self,
        id: hir::ExprId,
        kind: hir::ExprKind,
        scope: hir::ScopeId,
        span: &Span,
        effects: u32,
    ) -> Result<(), Diagnostic> {
        self.expressions[id.index()] = Some(hir::Expression {
            kind,
            scope,
            location: identity::location(self.compiler.sources, span)?,
            adjustments: 0..0,
            effects: hir::Effects::new(effects),
        });
        Ok(())
    }
    fn new_scope(&mut self, kind: hir::ScopeKind, span: &Span) -> Result<hir::ScopeId, Diagnostic> {
        let id = hir::ScopeId(checked_id(self.output.scopes.len())?);
        self.output.scopes.push(hir::Scope {
            parent: Some(self.scope),
            kind,
            location: identity::location(self.compiler.sources, span)?,
        });
        self.scope = id;
        Ok(id)
    }
    fn expression_list(
        &mut self,
        expressions: impl IntoIterator<Item = hir::ExprId>,
    ) -> Result<std::ops::Range<u32>, Diagnostic> {
        let start = checked_id(self.output.expression_ids.len())?;
        self.output.expression_ids.extend(expressions);
        Ok(start..checked_id(self.output.expression_ids.len())?)
    }
    fn local(&mut self, slot: usize) -> Result<hir::LocalId, Diagnostic> {
        if let Some(local) = self.slots[slot] {
            return Ok(local);
        }
        let origin = &self.facts.body.slot_origins[slot];
        let group = &self.facts.bindings[&(origin.name.as_str(), origin.start, origin.end)];
        let id = hir::LocalId(checked_id(self.output.locals.len())?);
        let ty = self.type_id(&self.facts.body.slots[slot])?;
        let mut storage = 0;
        for &slot in group {
            self.slots[slot] = Some(id);
            storage |= self.facts.body.slot_storage[slot];
        }
        let mut location = identity::location(
            self.compiler.sources,
            &self.compiler.model.modules[self.module].file.eof_span,
        )?;
        location.start = origin.start;
        location.end = origin.end;
        self.output.locals.push(hir::Local {
            name: origin.name.clone(),
            ty,
            location,
            storage,
        });
        self.local_sources.push(slot);
        self.names.insert(origin.name.as_str(), id);
        Ok(id)
    }
    fn source_slot(&self, name: &str, span: &Span) -> Result<usize, Diagnostic> {
        self.facts
            .bindings
            .get(&(name, span.start(), span.end()))
            .and_then(|slots| slots.first())
            .copied()
            .ok_or_else(|| self.error("源码绑定没有已检查槽身份"))
    }
    fn capture(&mut self, capture: Inherited) -> Result<(), Diagnostic> {
        let local = self.local(capture.slot)?;
        if let Some(existing) = self
            .output
            .captures
            .iter_mut()
            .find(|entry| entry.local == local)
        {
            existing.read_before_write |= capture.read;
            existing.written |= capture.written;
            existing.coroutine |= capture.coroutine;
        } else {
            self.output.captures.push(hir::Capture {
                local,
                owner: capture.owner,
                source: capture.local,
                read_before_write: capture.read,
                written: capture.written,
                coroutine: capture.coroutine,
            });
        }
        Ok(())
    }
    fn parameters(&mut self, function: ast::FnId) -> Result<(), Diagnostic> {
        let function = &self.arena().fns[function.0 as usize];
        self.unsafe_depth = usize::from(function.unsafety);
        for (offset, parameter) in function
            .params
            .as_slice(&self.arena().params)
            .iter()
            .enumerate()
        {
            if !self.compiler.model.modules[self.module]
                .configured
                .param_active(function.params.start as usize + offset)
            {
                continue;
            }
            if let Some(pattern) = parameter.pat {
                let pattern = self.bind_pattern(pattern)?;
                self.output.parameters.push(pattern);
            } else if let Some(name) = parameter.variadic_name {
                let name = self.compiler.model.name(self.module, name);
                let slot = self.source_slot(name, &parameter.span)?;
                let local = self.local(slot)?;
                let id = hir::PatternId(checked_id(self.patterns.len())?);
                self.patterns.push(Some(hir::Pattern {
                    location: identity::location(self.compiler.sources, &parameter.span)?,
                    ty: self.output.locals[local.index()].ty,
                    kind: hir::PatternKind::Bind(local),
                }));
                self.output.parameters.push(id);
            }
        }
        Ok(())
    }
}

fn completed<T>(nodes: Vec<Option<T>>, kind: &str) -> Result<Vec<T>, Diagnostic> {
    nodes
        .into_iter()
        .map(|node| {
            node.ok_or_else(|| {
                Diagnostic::error(
                    DiagnosticCode::InvalidType,
                    format!("HIR {kind}尚未完成形成"),
                    None,
                )
            })
        })
        .collect()
}

fn body_expression(body: ast::FnBody) -> Option<ast::ExprId> {
    match body {
        ast::FnBody::Block(expression) | ast::FnBody::Eq(expression) => Some(expression),
        ast::FnBody::None => None,
    }
}
