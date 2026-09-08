use super::*;

impl Builder<'_> {
    pub(super) fn emit_chan_send(
        &mut self,
        id: ExprId,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let args = expr_range(self.owner, &arguments);
        let (Some(channel), Some(value)) = (args.first().copied(), args.get(1).copied()) else {
            return Err(gir_error(
                "chan.send 需要通道与值",
                Some(&self.source_of(id).location),
            ));
        };
        let Some(channel) = self.emit_expr(channel)? else {
            return Ok(None);
        };
        let Some(value) = self.emit_expr(value)? else {
            return Ok(None);
        };
        let cancelled = self.send_cancelled(id)?;
        let resume = self.fresh(false);
        let safepoint = self.safepoint(SafepointKind::Suspend, id);
        self.terminate(Terminator::Suspend {
            reason: SuspendReason::ChanSend {
                channel: copy_of(channel),
                value: copy_of(value),
            },
            destination: None,
            resume,
            cancelled: Some(cancelled),
            safepoint,
        });
        self.switch_to(resume);
        let dest = self.temp(self.expr_ty(id));
        self.assign_unit(dest);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_chan_recv(
        &mut self,
        id: ExprId,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(channel) = expr_range(self.owner, &arguments).into_iter().next() else {
            return Err(gir_error(
                "chan.recv 需要通道",
                Some(&self.source_of(id).location),
            ));
        };
        let Some(channel) = self.emit_expr(channel)? else {
            return Ok(None);
        };
        let dest = self.temp(self.expr_ty(id));
        let resume = self.fresh(false);
        let safepoint = self.safepoint(SafepointKind::Suspend, id);
        self.terminate(Terminator::Suspend {
            reason: SuspendReason::ChanRecv {
                channel: copy_of(channel),
            },
            destination: Some(Place::local(dest)),
            resume,
            cancelled: None,
            safepoint,
        });
        self.switch_to(resume);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_join_wait(
        &mut self,
        id: ExprId,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(join) = expr_range(self.owner, &arguments).into_iter().next() else {
            return Err(gir_error(
                "join.wait 需要句柄",
                Some(&self.source_of(id).location),
            ));
        };
        let Some(join) = self.emit_expr(join)? else {
            return Ok(None);
        };
        let dest = self.temp(self.expr_ty(id));
        let resume = self.fresh(false);
        let safepoint = self.safepoint(SafepointKind::Suspend, id);
        self.terminate(Terminator::Suspend {
            reason: SuspendReason::JoinWait {
                join: copy_of(join),
            },
            destination: Some(Place::local(dest)),
            resume,
            cancelled: None,
            safepoint,
        });
        self.switch_to(resume);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_yield(&mut self) -> Result<(), Diagnostic> {
        let resume = self.fresh(false);
        let location = self.blocks[self.current.index()].source.location.clone();
        let safepoint = self.push_safepoint(SafepointKind::Suspend, location);
        self.terminate(Terminator::Suspend {
            reason: SuspendReason::Yield,
            destination: None,
            resume,
            cancelled: None,
            safepoint,
        });
        self.switch_to(resume);
        Ok(())
    }

    pub(super) fn emit_select(
        &mut self,
        id: ExprId,
        arms: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let dest = self.temp(self.expr_ty(id));
        let index = self.temp(self.int_ty());
        let start = self.select_cases.len() as u32;
        let mut has_send = false;
        let mut default = None;
        let mut ready_arms = Vec::new();
        for (arm_index, arm) in self.owner.select_arms[arms.start as usize..arms.end as usize]
            .iter()
            .enumerate()
        {
            match arm {
                hir::SelectArm::Default { body } => default = Some(*body),
                hir::SelectArm::Send {
                    channel,
                    value,
                    body,
                } => {
                    has_send = true;
                    let Some(channel) = self.emit_expr(*channel)? else {
                        return Ok(None);
                    };
                    let Some(value) = self.emit_expr(*value)? else {
                        return Ok(None);
                    };
                    self.select_cases.push(SelectCase {
                        operation: SelectOperation::Send {
                            channel: copy_of(channel),
                            value: copy_of(value),
                        },
                        destination: None,
                        arm: arm_index as u32,
                    });
                    ready_arms.push(*body);
                }
                hir::SelectArm::Recv {
                    channel,
                    pattern,
                    body,
                } => {
                    let Some(channel) = self.emit_expr(*channel)? else {
                        return Ok(None);
                    };
                    let slot = self.temp(self.owner.patterns[pattern.index()].ty);
                    self.select_cases.push(SelectCase {
                        operation: SelectOperation::Recv {
                            channel: copy_of(channel),
                        },
                        destination: Some(Place::local(slot)),
                        arm: arm_index as u32,
                    });
                    ready_arms.push(*body);
                }
                hir::SelectArm::Wait {
                    join,
                    pattern,
                    body,
                } => {
                    let Some(join) = self.emit_expr(*join)? else {
                        return Ok(None);
                    };
                    let slot = self.temp(self.owner.patterns[pattern.index()].ty);
                    self.select_cases.push(SelectCase {
                        operation: SelectOperation::Wait {
                            join: copy_of(join),
                        },
                        destination: Some(Place::local(slot)),
                        arm: arm_index as u32,
                    });
                    ready_arms.push(*body);
                }
            }
        }
        let ready = self.fresh(false);
        let join = self.fresh(false);
        let suspend = default.is_none().then(|| self.fresh(false));
        let cancelled = has_send.then(|| self.send_cancelled(id)).transpose()?;
        let safepoint = self.safepoint(SafepointKind::Select, id);
        self.terminate(Terminator::SelectCommit {
            cases: start..self.select_cases.len() as u32,
            index,
            ready,
            suspend,
            cancelled,
            safepoint,
        });
        if let Some(suspend) = suspend {
            self.switch_to(suspend);
            self.goto(ready);
        }
        self.switch_to(ready);
        self.emit_select_ready(dest, index, &ready_arms, default, join)?;
        self.switch_to(join);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    fn emit_select_ready(
        &mut self,
        dest: LocalId,
        index: LocalId,
        ready_arms: &[ExprId],
        default: Option<ExprId>,
        join: BlockId,
    ) -> Result<(), Diagnostic> {
        let mut targets = Vec::new();
        let otherwise = self.fresh(false);
        for (arm_index, body) in ready_arms.iter().enumerate() {
            let block = self.fresh(false);
            targets.push((arm_index as u128, block));
            let saved = self.current;
            self.current = block;
            if let Some(case) = self
                .select_cases
                .iter()
                .find(|case| case.arm == arm_index as u32)
                && let Some(place) = case.destination
            {
                let arm = &self.owner.select_arms[case.arm as usize];
                if let hir::SelectArm::Recv { pattern, .. } | hir::SelectArm::Wait { pattern, .. } =
                    arm
                {
                    self.bind_pattern(place, *pattern)?;
                }
            }
            if let Some(value) = self.emit_expr(*body)? {
                self.assign(Place::local(dest), Rvalue::Use(copy_of(value)));
                self.goto(join);
            }
            self.current = saved;
        }
        self.terminate(Terminator::SwitchInt {
            value: copy_of(index),
            targets,
            otherwise,
        });
        self.switch_to(otherwise);
        if let Some(body) = default {
            if let Some(value) = self.emit_expr(body)? {
                self.assign(Place::local(dest), Rvalue::Use(copy_of(value)));
            }
        } else {
            self.assign_unit(dest);
        }
        self.goto(join);
        Ok(())
    }

    fn send_cancelled(&mut self, id: ExprId) -> Result<BlockId, Diagnostic> {
        let block = self.fresh(true);
        let saved = self.current;
        self.current = block;
        let payload = self.panic_string("send on closed channel");
        let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
        self.terminate(Terminator::Panic { payload, unwind });
        self.current = saved;
        Ok(block)
    }

    fn safepoint(&mut self, kind: SafepointKind, id: ExprId) -> SafepointId {
        self.push_safepoint(kind, self.source_of(id).location)
    }

    fn push_safepoint(&mut self, kind: SafepointKind, location: hir::Location) -> SafepointId {
        let id = SafepointId(self.safepoints.len() as u32);
        self.safepoints.push(Safepoint { kind, location });
        id
    }
}
