use super::super::model::ArrayLength;
use super::*;

pub(super) struct ActiveInit {
    plan: super::super::output::ArrayInitLoop,
    array: Symbol,
    index: Symbol,
    index_slot: Option<usize>,
    valid: bool,
    used: bool,
}

impl Checker<'_, '_> {
    pub(super) fn array_init_for(
        &mut self,
        expression: ExprId,
        pat: PatId,
        iter: ExprId,
        body: ExprId,
    ) -> Ty {
        let proofs = self
            .array_init_candidates(expression, pat, iter, body)
            .unwrap_or_default();
        if proofs.is_empty() {
            return self.looping(body, None, Some((pat, iter)));
        }
        let depth = self.active_array_inits.len();
        self.active_array_inits.extend(proofs);
        let ty = self.looping(body, None, Some((pat, iter)));
        for mut proof in self.active_array_inits.split_off(depth) {
            if proof.valid && proof.used {
                proof.plan.index_slot = proof.index_slot.expect("已绑定循环变量");
                self.initialize(proof.plan.array_slot, true);
                self.array_init_loops.push(proof.plan);
            } else if proof.used {
                self.error(
                    DiagnosticCode::InvalidDeclaration,
                    "读取未初始化的局部槽",
                    self.arena().exprs[proof.plan.place.0 as usize].span.clone(),
                );
            }
        }
        ty
    }

    fn init_length(&self, expression: ExprId) -> Option<ArrayLength> {
        match self.arena().exprs[expression.0 as usize].kind {
            ExprKind::Paren(inner) => return self.init_length(inner),
            ExprKind::Path(path) => {
                let segments = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments);
                if self.state.names.contains_key(&segments.first()?.name) {
                    return None;
                }
            }
            ExprKind::Literal(_) => {}
            _ => return None,
        }
        let params = self
            .model
            .parameters_at(self.module, &self.arena().exprs[expression.0 as usize].span);
        self.model
            .array_length(self.module, expression, &params)
            .ok()
    }

    fn array_init_candidates(
        &self,
        expression: ExprId,
        pat: PatId,
        iter: ExprId,
        body: ExprId,
    ) -> Option<Vec<ActiveInit>> {
        let PatKind::Ident(index) = self.arena().pats[pat.0 as usize].kind else {
            return None;
        };
        let ExprKind::Range { start, end } = self.arena().exprs[iter.0 as usize].kind else {
            return None;
        };
        if self.init_length(start)? != ArrayLength::Known(0) {
            return None;
        }
        let length = self.init_length(end)?;
        let ExprKind::Block { stmts, .. } = self.arena().exprs[body.0 as usize].kind else {
            return None;
        };
        let candidates: Vec<_> = stmts
            .as_slice(&self.arena().stmt_ids)
            .iter()
            .filter_map(|&store| {
                self.array_init_target(store, index, &length)
                    .map(|(array, slot, place)| ActiveInit {
                        plan: super::super::output::ArrayInitLoop {
                            expression,
                            store,
                            place,
                            array_slot: slot,
                            index_slot: 0,
                            length: length.clone(),
                        },
                        array,
                        index,
                        index_slot: None,
                        valid: true,
                        used: false,
                    })
            })
            .collect();
        let mut counts = BTreeMap::new();
        for candidate in &candidates {
            *counts.entry(candidate.plan.array_slot).or_insert(0) += 1;
        }
        Some(
            candidates
                .into_iter()
                .filter(|candidate| counts[&candidate.plan.array_slot] == 1)
                .collect(),
        )
    }

    fn array_init_target(
        &self,
        store: StmtId,
        index_name: Symbol,
        length: &ArrayLength,
    ) -> Option<(Symbol, usize, ExprId)> {
        let StmtKind::Assign {
            op: AssignOp::Assign,
            place,
            ..
        } = self.arena().stmts[store.0 as usize].kind
        else {
            return None;
        };
        let ExprKind::Index {
            base,
            index: IndexKind::Expr(index),
        } = self.arena().exprs[place.0 as usize].kind
        else {
            return None;
        };
        if self.init_name(index)? != index_name {
            return None;
        }
        let array = self.init_name(base)?;
        if array == index_name {
            return None;
        }
        let &slot = self.state.names.get(&array)?;
        if !self.array_init_is_local(slot) || self.state.initialized.get(slot) != Some(&false) {
            return None;
        }
        let Ty::Array(_, actual) = self.resolve(&self.slots[slot].ty) else {
            return None;
        };
        (self.resolve_length(&actual) == *length).then_some((array, slot, place))
    }

    fn init_name(&self, expression: ExprId) -> Option<Symbol> {
        let ExprKind::Path(path) = self.arena().exprs[expression.0 as usize].kind else {
            return None;
        };
        let parts = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        (parts.len() == 1).then(|| parts[0].name)
    }

    fn init_base(&self, expression: ExprId) -> Option<Symbol> {
        match self.arena().exprs[expression.0 as usize].kind {
            ExprKind::Path(path) => self.arena().paths[path.0 as usize]
                .segments
                .as_slice(&self.arena().segments)
                .first()
                .map(|part| part.name),
            ExprKind::Paren(inner)
            | ExprKind::Unary { expr: inner, .. }
            | ExprKind::Field { base: inner, .. }
            | ExprKind::TupleField { base: inner, .. }
            | ExprKind::Index { base: inner, .. } => self.init_base(inner),
            _ => self.init_name(expression),
        }
    }

    pub(super) fn array_init_binding(&mut self, name: Symbol) {
        for proof in &mut self.active_array_inits {
            if name == proof.array {
                proof.valid = false;
            }
            if name == proof.index {
                if proof.index_slot.is_none() {
                    proof.index_slot = Some(self.slots.len());
                } else {
                    proof.valid = false;
                }
            }
        }
    }

    pub(super) fn array_init_assignment(&mut self, place: ExprId) {
        if self.active_array_inits.is_empty() {
            return;
        }
        let base = self.init_base(place);
        for proof in &mut self.active_array_inits {
            if base == Some(proof.index) || (base == Some(proof.array) && place != proof.plan.place)
            {
                proof.valid = false;
            }
        }
    }

    pub(super) fn array_init_index_borrow(&mut self, place: ExprId) {
        if self.active_array_inits.is_empty() {
            return;
        }
        let base = self.init_base(place);
        for proof in &mut self.active_array_inits {
            if base == Some(proof.index) {
                proof.valid = false;
            }
        }
    }

    pub(super) fn array_init_expression(&mut self, expression: ExprId) {
        if self.active_array_inits.is_empty() {
            return;
        }
        let kind = &self.arena().exprs[expression.0 as usize].kind;
        let forbidden = matches!(
            kind,
            ExprKind::Break(_) | ExprKind::Continue | ExprKind::Return(_) | ExprKind::TryOp(_)
        );
        let name = self.init_name(expression);
        let borrowed = match *kind {
            ExprKind::Unary {
                op: UnOp::Ref,
                expr,
            }
            | ExprKind::Field { base: expr, .. } => self.init_base(expr),
            _ => None,
        };
        for proof in &mut self.active_array_inits {
            if forbidden || name == Some(proof.array) || borrowed == Some(proof.index) {
                proof.valid = false;
            }
        }
    }

    pub(super) fn array_init_store(&mut self, expression: ExprId) -> bool {
        for proof in &mut self.active_array_inits {
            if expression != proof.plan.place {
                continue;
            }
            proof.valid &= self.state.names.get(&proof.array) == Some(&proof.plan.array_slot)
                && self.state.names.get(&proof.index) == proof.index_slot.as_ref();
            if proof.valid {
                proof.used = true;
                return true;
            }
        }
        false
    }
}
