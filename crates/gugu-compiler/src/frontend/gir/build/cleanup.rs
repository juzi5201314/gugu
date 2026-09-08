use super::*;

impl Builder<'_> {
    pub(super) fn intern_plan(
        &mut self,
        plan: u32,
        chain: CleanupChain,
    ) -> Result<BlockId, Diagnostic> {
        let dest = match chain {
            CleanupChain::Normal => Some(self.return_block),
            CleanupChain::Unwind => None,
        };
        self.intern_cleanup(plan, chain, dest)
    }

    pub(super) fn intern_cleanup(
        &mut self,
        plan: u32,
        chain: CleanupChain,
        destination: Option<BlockId>,
    ) -> Result<BlockId, Diagnostic> {
        let actions = self.plan_actions(plan).to_vec();
        let entry = self.intern_suffix(&actions, chain, destination)?;
        if !self
            .exit_records
            .iter()
            .any(|record| record.plan == plan && record.chain == chain && record.entry == entry)
        {
            self.exit_records.push(ExitRecord {
                plan,
                chain,
                entry,
                destination,
            });
        }
        Ok(entry)
    }

    fn plan_actions(&self, plan: u32) -> &[hir::CleanupAction] {
        let plan = &self.owner.cleanup_plans[plan as usize];
        &self.owner.cleanup_actions[plan.actions.start as usize..plan.actions.end as usize]
    }

    fn intern_suffix(
        &mut self,
        actions: &[hir::CleanupAction],
        chain: CleanupChain,
        destination: Option<BlockId>,
    ) -> Result<BlockId, Diagnostic> {
        if actions.is_empty() {
            return Ok(self.terminal_block(chain, destination));
        }
        let key = (chain, actions.to_vec());
        if let Some(&block) = self.intern.get(&key) {
            return Ok(block);
        }
        let entry = self.fresh(true);
        self.intern.insert(key, entry);
        let rest = self.intern_suffix(&actions[1..], chain, destination)?;
        let saved = std::mem::replace(&mut self.current, entry);
        self.emit_action(&actions[0], chain, rest)?;
        if !self.terminated() {
            self.goto(rest);
        }
        self.current = saved;
        self.cleanup_regions.push(CleanupRegion {
            action: actions[0].clone(),
            chain,
            entry,
            exit: rest,
            order: self.intern_order,
        });
        self.intern_order += 1;
        Ok(entry)
    }

    fn terminal_block(&mut self, chain: CleanupChain, destination: Option<BlockId>) -> BlockId {
        match (chain, destination) {
            (CleanupChain::Normal, Some(dest)) => dest,
            (CleanupChain::Normal, None) => self.return_block,
            (CleanupChain::Unwind, _) => self.resume_block(),
        }
    }

    fn resume_block(&mut self) -> BlockId {
        let block = self.fresh(true);
        let saved = self.current;
        self.current = block;
        self.dead_unpinned();
        self.terminate(Terminator::ResumePanic);
        self.current = saved;
        block
    }

    fn emit_action(
        &mut self,
        action: &hir::CleanupAction,
        chain: CleanupChain,
        rest: BlockId,
    ) -> Result<(), Diagnostic> {
        match action {
            hir::CleanupAction::Action { cleanup, guard } => {
                self.emit_guarded_action(*cleanup, *guard, rest)
            }
            hir::CleanupAction::DrainChain { until } => self.emit_drain(*until, rest, chain),
        }
    }

    fn emit_guarded_action(
        &mut self,
        cleanup: u32,
        guard: hir::Registration,
        rest: BlockId,
    ) -> Result<(), Diagnostic> {
        match guard {
            hir::Registration::Static => self.emit_saved(cleanup),
            hir::Registration::Flag => {
                let flag = self.cleanup_flags[cleanup as usize]
                    .ok_or_else(|| gir_error("Flag 清理缺少标志 local", None))?;
                let taken = self.fresh(true);
                self.terminate(Terminator::SwitchInt {
                    value: copy_of(flag),
                    targets: vec![(1, taken)],
                    otherwise: rest,
                });
                self.switch_to(taken);
                self.emit_saved(cleanup)?;
                Ok(())
            }
            hir::Registration::Chain => Ok(()),
        }
    }

    fn emit_saved(&mut self, cleanup: u32) -> Result<(), Diagnostic> {
        match &self.saved[cleanup as usize] {
            SavedCleanup::Call {
                callee,
                args,
                destination,
                call_kind,
                site,
            } => {
                let callee = callee.clone();
                let args = args.clone();
                let destination = *destination;
                let call_kind = *call_kind;
                let site = *site;
                let normal = self.fresh(true);
                self.terminate(Terminator::Call {
                    callee,
                    args,
                    destination,
                    normal,
                    unwind: None,
                    call_kind,
                    site,
                });
                self.switch_to(normal);
                Ok(())
            }
            SavedCleanup::Body(expr) => {
                let expr = *expr;
                let _ = self.emit_expr(expr)?;
                Ok(())
            }
        }
    }

    pub(super) fn emit_defer(&mut self, action: u32) -> Result<(), Diagnostic> {
        let cleanup = &self.owner.cleanup[action as usize];
        let body = cleanup.body;
        let registration = cleanup.registration;
        self.saved[action as usize] = self.save_cleanup(body)?;
        match registration {
            hir::Registration::Static => {}
            hir::Registration::Flag => {
                if let Some(flag) = self.cleanup_flags[action as usize] {
                    self.assign_bool(flag, true);
                }
            }
            hir::Registration::Chain => self.push_chain(action)?,
        }
        Ok(())
    }

    fn save_cleanup(&mut self, body: ExprId) -> Result<SavedCleanup, Diagnostic> {
        match &self.owner.expressions[body.index()].kind {
            hir::ExprKind::Call {
                target,
                receiver,
                arguments,
            } => {
                let (callee, args) =
                    self.prepare_saved_call(target.clone(), *receiver, arguments.clone())?;
                let destination = Place::local(self.temp(self.expr_ty(body)));
                Ok(SavedCleanup::Call {
                    callee,
                    args,
                    destination,
                    call_kind: CallKind::Managed,
                    site: crate::frontend::mono::instantiate::CallSite::Expression(body.0),
                })
            }
            hir::ExprKind::SpawnCall {
                target,
                receiver,
                arguments,
            } => {
                let (callee, args) =
                    self.prepare_saved_call(target.clone(), *receiver, arguments.clone())?;
                let destination = Place::local(self.temp(self.expr_ty(body)));
                Ok(SavedCleanup::Call {
                    callee,
                    args,
                    destination,
                    call_kind: CallKind::Managed,
                    site: crate::frontend::mono::instantiate::CallSite::Expression(body.0),
                })
            }
            _ => Ok(SavedCleanup::Body(body)),
        }
    }

    fn prepare_saved_call(
        &mut self,
        target: hir::CallTarget,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
    ) -> Result<(Callee, Vec<Operand>), Diagnostic> {
        let callee = match target {
            hir::CallTarget::Value(value) => {
                let local = self
                    .emit_expr(value)?
                    .unwrap_or_else(|| self.temp(self.primitives.unit));
                Callee::Value(copy_of(local))
            }
            hir::CallTarget::Dispatch(dispatch) => Callee::Dispatch(dispatch),
            hir::CallTarget::Builtin(builtin) => Callee::Builtin(builtin),
            hir::CallTarget::Constructor { .. } => Callee::Builtin(hir::Builtin::Some),
        };
        let mut args = Vec::new();
        if let Some(receiver) = receiver {
            if let Some(local) = self.emit_expr(receiver)? {
                args.push(copy_of(local));
            }
        }
        for argument in expr_range(self.owner, &arguments) {
            if let Some(local) = self.emit_expr(argument)? {
                args.push(copy_of(local));
            }
        }
        Ok((callee, args))
    }

    fn push_chain(&mut self, action: u32) -> Result<(), Diagnostic> {
        let head = self
            .chain_head
            .ok_or_else(|| gir_error("Chain 清理缺少链头", None))?;
        let env = match &self.saved[action as usize] {
            SavedCleanup::Call { args, .. } => args.clone(),
            SavedCleanup::Body(_) => Vec::new(),
        };
        let mut operands = vec![copy_of(head)];
        operands.extend(env);
        let next = self.intrinsic_temp(
            IntrinsicOp::DeferChainPush { action },
            operands,
            Vec::new(),
            self.primitives.ptr_unit,
        );
        self.assign(Place::local(head), Rvalue::Use(copy_of(next)));
        Ok(())
    }

    fn emit_drain(
        &mut self,
        until: Option<u32>,
        rest: BlockId,
        _chain: CleanupChain,
    ) -> Result<(), Diagnostic> {
        let head = self
            .chain_head
            .ok_or_else(|| gir_error("DrainChain 缺少链头", None))?;
        let header = self.current;
        let empty = self.temp(self.primitives.bool_ty);
        let empty_const = self.intrinsic_temp(
            IntrinsicOp::DeferChainEmpty,
            Vec::new(),
            Vec::new(),
            self.primitives.ptr_unit,
        );
        self.assign(
            Place::local(empty),
            Rvalue::Compare {
                op: CompareOp::Eq,
                left: copy_of(head),
                right: copy_of(empty_const),
            },
        );
        let body = self.fresh(true);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(empty),
            targets: vec![(1, rest)],
            otherwise: body,
        });
        self.switch_to(body);
        let action = self.intrinsic_temp(
            IntrinsicOp::DeferChainAction,
            vec![copy_of(head)],
            Vec::new(),
            self.int_ty(),
        );
        let sites: Vec<u32> = self
            .owner
            .cleanup
            .iter()
            .enumerate()
            .filter(|(index, cleanup)| {
                cleanup.registration == hir::Registration::Chain
                    && until.is_none_or(|until| *index as u32 > until)
            })
            .map(|(index, _)| index as u32)
            .collect();
        let mut targets = Vec::new();
        let skip = self.fresh(true);
        for site in sites {
            let block = self.fresh(true);
            targets.push((u128::from(site), block));
            let saved = self.current;
            self.current = block;
            self.emit_saved(site)?;
            self.pop_chain(head)?;
            self.goto(header);
            self.current = saved;
        }
        self.terminate(Terminator::SwitchInt {
            value: copy_of(action),
            targets,
            otherwise: skip,
        });
        self.switch_to(skip);
        self.pop_chain(head)?;
        self.goto(header);
        Ok(())
    }

    fn pop_chain(&mut self, head: LocalId) -> Result<(), Diagnostic> {
        let next = self.intrinsic_temp(
            IntrinsicOp::DeferChainPop,
            vec![copy_of(head)],
            Vec::new(),
            self.primitives.ptr_unit,
        );
        self.assign(Place::local(head), Rvalue::Use(copy_of(next)));
        Ok(())
    }
}
