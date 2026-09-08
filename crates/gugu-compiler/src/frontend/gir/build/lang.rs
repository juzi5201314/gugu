//! lang item 钩子：scoped view 与 NoSafepoint 只能由登记路径产生。
use super::*;

impl Builder<'_> {
    pub(super) fn emit_lang_call(
        &mut self,
        id: ExprId,
        callee: &Callee,
        args: &[Operand],
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(name) = lang_name(self.module, callee) else {
            return Ok(None);
        };
        if matches!(
            name.as_str(),
            "std.mem.with_ref" | "std.mem.with_read_ref" | "std.mem.for_each_ref"
        ) {
            return self.emit_scoped_view(id, name.as_str(), args).map(Some);
        }
        if matches!(
            name.as_str(),
            "std.runtime.no_safepoint_lock"
                | "std.runtime.ownership_publish"
                | "std.runtime.root_publish"
        ) {
            return self.emit_no_safepoint(id, name.as_str(), args).map(Some);
        }
        Ok(None)
    }

    fn emit_scoped_view(
        &mut self,
        id: ExprId,
        name: &str,
        args: &[Operand],
    ) -> Result<LocalId, Diagnostic> {
        let source = match args.first() {
            Some(Operand::Copy(place) | Operand::MoveInternal(place)) => *place,
            _ => Place::local(self.temp(self.primitives.unit)),
        };
        let token = self.temp(self.primitives.unit);
        let mode = if name.ends_with("with_ref") {
            ViewMode::ScopedWrite
        } else {
            ViewMode::ScopedRead
        };
        self.push_stmt(StatementKind::ScopedViewBegin {
            source,
            mode,
            token,
        });
        let dest = self.temp(self.expr_ty(id));
        if let Some(callback) = args.get(1).cloned() {
            let normal = self.fresh(false);
            let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
            self.terminate(Terminator::Call {
                callee: Callee::Value(callback),
                args: vec![Operand::Copy(source)],
                destination: Place::local(dest),
                normal,
                unwind: Some(unwind),
                call_kind: CallKind::Managed,
                site: crate::frontend::mono::instantiate::CallSite::Expression(id.0),
            });
            self.switch_to(normal);
        } else {
            self.assign_unit(dest);
        }
        self.push_stmt(StatementKind::ScopedViewEnd { token });
        self.set_value(id, dest);
        Ok(dest)
    }

    fn emit_no_safepoint(
        &mut self,
        id: ExprId,
        name: &str,
        args: &[Operand],
    ) -> Result<LocalId, Diagnostic> {
        let reason = match name {
            "std.runtime.no_safepoint_lock" => NoSafepointReason::RuntimeLock,
            "std.runtime.ownership_publish" => NoSafepointReason::OwnershipPublish,
            _ => NoSafepointReason::RootPublish,
        };
        let region = NoSafepointRegionId(self.no_safepoint_regions.len() as u32);
        self.no_safepoint_regions.push(reason);
        self.push_stmt(StatementKind::NoSafepointBegin(region));
        let dest = self.temp(self.expr_ty(id));
        if let Some(callback) = args.first().cloned() {
            let normal = self.fresh(false);
            let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
            self.terminate(Terminator::Call {
                callee: Callee::Value(callback),
                args: Vec::new(),
                destination: Place::local(dest),
                normal,
                unwind: Some(unwind),
                call_kind: CallKind::Managed,
                site: crate::frontend::mono::instantiate::CallSite::Expression(id.0),
            });
            self.switch_to(normal);
        } else {
            self.assign_unit(dest);
        }
        self.push_stmt(StatementKind::NoSafepointEnd(region));
        self.set_value(id, dest);
        Ok(dest)
    }
}

fn lang_name(module: &hir::Module, callee: &Callee) -> Option<String> {
    let definition = match callee {
        Callee::Dispatch(index) => module
            .owners
            .iter()
            .find_map(|owner| owner.dispatches.get(*index as usize))
            .and_then(|dispatch| dispatch.function)?,
        Callee::Value(Operand::Function(candidate)) => candidate.definition,
        _ => return None,
    };
    Some(module.definitions[definition.index()].name.clone())
}
