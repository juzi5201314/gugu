//! 用稠密实例/owner 下标传播 late 依赖；闭包遍历保留全部分支，不执行 late 条件。
use super::universe::invalid;
use crate::Diagnostic;
use crate::frontend::{
    hir::{self, CallTarget, ExprId, ExprKind, StatementKind},
    mono::{MonoWorldV1, instantiate::CallSite, keys::StableTypeKey},
};
use std::collections::BTreeSet;

pub(super) struct Program<'a> {
    pub module: &'a hir::Module,
    pub world: &'a MonoWorldV1,
    pub owners: Vec<Option<usize>>,
    pub late: Vec<Vec<bool>>,
}

impl<'a> Program<'a> {
    pub fn new(module: &'a hir::Module, world: &'a MonoWorldV1) -> Self {
        let owners: Vec<_> = world
            .instances
            .iter()
            .map(|instance| {
                module.owners.iter().position(|owner| {
                    module.definitions[owner.definition.index()].key == instance.mono_key[..32]
                })
            })
            .collect();
        let late = owners
            .iter()
            .map(|owner| {
                owner.map_or_else(Vec::new, |i| {
                    vec![false; module.owners[i].expressions.len()]
                })
            })
            .collect();
        let mut program = Self {
            module,
            world,
            owners,
            late,
        };
        program.propagate();
        program
    }

    pub fn owner(&self, instance: usize) -> Result<&'a hir::Owner, Diagnostic> {
        self.owners[instance]
            .map(|i| &self.module.owners[i])
            .ok_or_else(|| invalid("late 闭包不能调用外部函数"))
    }

    pub fn ty(&self, instance: usize, id: hir::TypeId) -> Result<StableTypeKey, Diagnostic> {
        let bindings = &self.world.instances[instance].type_bindings;
        bindings
            .binary_search_by_key(&id.0, |(id, _)| *id)
            .ok()
            .map(|i| bindings[i].1)
            .ok_or_else(|| invalid("late 表达式缺少冻结的具体类型"))
    }

    pub fn callee(&self, instance: usize, expression: ExprId) -> Option<usize> {
        let targets = &self.world.instances[instance].call_targets;
        targets
            .iter()
            .find(|(site, _)| *site == CallSite::Expression(expression.0))
            .and_then(|(_, key)| {
                self.world
                    .instances
                    .binary_search_by_key(key, |i| crate::frontend::mono::digest_of(&i.mono_key))
                    .ok()
            })
    }

    pub fn constant(&self, definition: hir::DefId) -> Option<usize> {
        let key = self.module.definitions[definition.index()].key;
        self.world
            .instances
            .iter()
            .position(|instance| instance.mono_key[..32] == key)
    }

    pub fn dependencies(&self, instance: usize, expr: ExprId) -> Vec<(usize, ExprId)> {
        let Ok(owner) = self.owner(instance) else {
            return Vec::new();
        };
        let mut edges: Vec<_> = children(owner, expr)
            .into_iter()
            .map(|e| (instance, e))
            .collect();
        if let Some(callee) = self.callee(instance, expr) {
            if let Ok(owner) = self.owner(callee) {
                edges.push((callee, owner.body));
            }
        }
        if let ExprKind::Resolved(
            hir::Res::Def(def)
            | hir::Res::Associated {
                definition: def, ..
            },
        ) = owner.expressions[expr.index()].kind
        {
            if matches!(
                self.module.definitions[def.index()].kind,
                hir::DefinitionKind::Constant | hir::DefinitionKind::Static
            ) {
                if let Some(callee) = self.constant(def) {
                    if let Ok(owner) = self.owner(callee) {
                        edges.push((callee, owner.body));
                    }
                }
            }
        }
        edges
    }

    fn propagate(&mut self) {
        loop {
            let mut changed = false;
            for instance in 0..self.owners.len() {
                let Ok(owner) = self.owner(instance) else {
                    continue;
                };
                // 局部槽是稠密 ID；仅需要一位表示任何赋值或控制依赖带入 late 值。
                let mut locals = vec![false; owner.locals.len()];
                for statement in &owner.statements {
                    match &statement.kind {
                        StatementKind::Let {
                            pattern,
                            value: Some(value),
                            ..
                        } if self.late[instance][value.index()] => {
                            mark_pattern(owner, *pattern, &mut locals)
                        }
                        StatementKind::Assign { place, value, .. }
                            if self.late[instance][value.index()] =>
                        {
                            if let ExprKind::Resolved(hir::Res::Local(local)) =
                                owner.expressions[place.index()].kind
                            {
                                locals[local.index()] = true;
                            }
                        }
                        _ => {}
                    }
                }
                for (index, expression) in owner.expressions.iter().enumerate() {
                    if self.late[instance][index] {
                        continue;
                    }
                    let direct = match &expression.kind {
                        ExprKind::Intrinsic {
                            operation: hir::Builtin::TypeIdCount | hir::Builtin::TypeAsInt,
                            ..
                        } => true,
                        ExprKind::Binary {
                            operation, left, ..
                        } if matches!(
                            operation,
                            crate::frontend::ast::BinOp::Lt
                                | crate::frontend::ast::BinOp::Le
                                | crate::frontend::ast::BinOp::Gt
                                | crate::frontend::ast::BinOp::Ge
                        ) =>
                        {
                            self.module.types[owner.expression_types[left.index()].index()]
                                == hir::Type::TypeId
                        }
                        ExprKind::Resolved(hir::Res::Local(local)) => locals[local.index()],
                        _ => false,
                    };
                    if direct
                        || self
                            .dependencies(instance, ExprId(index as u32))
                            .iter()
                            .any(|(i, e)| self.late[*i][e.index()])
                    {
                        self.late[instance][index] = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    pub fn closure(&self, instance: usize, expr: ExprId) -> Result<[u8; 32], Diagnostic> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![(instance, expr)];
        while let Some((instance, expr)) = pending.pop() {
            if !seen.insert((instance, expr)) {
                continue;
            }
            let owner = self.owner(instance)?;
            if let ExprKind::Call { target, .. } = &owner.expressions[expr.index()].kind {
                if !matches!(
                    target,
                    CallTarget::Builtin(_) | CallTarget::Constructor { .. }
                ) && self.callee(instance, expr).is_none()
                {
                    return Err(invalid("late 闭包中的间接调用不能静态封闭"));
                }
                if let Some(callee) = self.callee(instance, expr) {
                    self.owner(callee)?;
                }
            }
            pending.extend(self.dependencies(instance, expr));
        }
        let keys: Vec<_> = seen
            .into_iter()
            .map(|(i, e)| {
                (
                    &self.world.instances[i].mono_key,
                    e.0,
                    self.world.instances[i].fragment_input_fingerprint,
                )
            })
            .collect();
        Ok(crate::frontend::mono::hash_domain(
            "gugu-late-closure-v1",
            &serde_json::to_vec(&keys).expect("闭包可序列化"),
        ))
    }
}

fn mark_pattern(owner: &hir::Owner, pattern: hir::PatternId, locals: &mut [bool]) {
    match &owner.patterns[pattern.index()].kind {
        hir::PatternKind::Bind(local) => locals[local.index()] = true,
        hir::PatternKind::At { local, pattern } => {
            locals[local.index()] = true;
            mark_pattern(owner, *pattern, locals);
        }
        hir::PatternKind::Tuple(range) | hir::PatternKind::Or(range) => {
            for p in &owner.pattern_ids[range.start as usize..range.end as usize] {
                mark_pattern(owner, *p, locals);
            }
        }
        _ => {}
    }
}

pub(super) fn children(owner: &hir::Owner, id: ExprId) -> Vec<ExprId> {
    let mut out = Vec::new();
    let list = |range: &std::ops::Range<u32>| {
        &owner.expression_ids[range.start as usize..range.end as usize]
    };
    match &owner.expressions[id.index()].kind {
        ExprKind::Tuple(range) | ExprKind::Array(range) => out.extend_from_slice(list(range)),
        ExprKind::Repeat { value, .. }
        | ExprKind::Comptime { value }
        | ExprKind::Unary { value, .. }
        | ExprKind::TryExit { value, .. } => out.push(*value),
        ExprKind::Construct { fields, .. } => out.extend(
            owner.fields[fields.start as usize..fields.end as usize]
                .iter()
                .map(|f| f.value),
        ),
        ExprKind::Block {
            statements, tail, ..
        } => {
            out.extend(tail);
            for id in &owner.statement_ids[statements.start as usize..statements.end as usize] {
                match &owner.statements[id.index()].kind {
                    StatementKind::Let {
                        value, otherwise, ..
                    } => {
                        out.extend(value);
                        out.extend(otherwise);
                    }
                    StatementKind::Assign { place, value, .. } => out.extend([place, value]),
                    StatementKind::Expression(e) => out.push(*e),
                    _ => {}
                }
            }
        }
        ExprKind::If {
            condition,
            then_value,
            else_value,
        } => {
            out.extend([condition, then_value]);
            out.extend(else_value);
        }
        ExprKind::While { condition, body } => out.extend([condition, body]),
        ExprKind::Loop { body } | ExprKind::Try { body, .. } => out.push(*body),
        ExprKind::For { value, body, .. } => out.extend([value, body]),
        ExprKind::Binary { left, right, .. }
        | ExprKind::Range {
            start: left,
            end: right,
        } => out.extend([left, right]),
        ExprKind::Call {
            target,
            receiver,
            arguments,
        } => {
            out.extend(receiver);
            out.extend_from_slice(list(arguments));
            if let CallTarget::Value(value) = target {
                out.push(*value);
            }
        }
        ExprKind::Intrinsic { arguments, .. } => out.extend_from_slice(list(arguments)),
        ExprKind::Field { base, .. } => out.push(*base),
        ExprKind::Index { base, index, .. } => out.extend([base, index]),
        ExprKind::Slice { base, start, end } => {
            out.push(*base);
            out.extend(start);
            out.extend(end);
        }
        ExprKind::Exit { value, .. } => out.extend(value),
        ExprKind::Match { value, arms } => {
            out.push(*value);
            for arm in &owner.arms[arms.start as usize..arms.end as usize] {
                out.push(arm.body);
                out.extend(arm.guard);
            }
        }
        ExprKind::LetCondition { value, .. } => out.push(*value),
        _ => {}
    }
    out
}
