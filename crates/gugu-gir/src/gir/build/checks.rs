use super::*;

impl Builder<'_> {
    pub(super) fn emit_check_ops(&mut self, id: ExprId) -> Result<(), Diagnostic> {
        let checks: Vec<(u32, hir::CheckKind)> = self
            .owner
            .checks
            .iter()
            .enumerate()
            .filter(|(_, check)| check.expression == id)
            .map(|(index, check)| (index as u32, check.kind.clone()))
            .collect();
        for (index, kind) in checks {
            self.emit_one_check(id, index, kind)?;
            if self.terminated() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn emit_one_check(
        &mut self,
        id: ExprId,
        index: u32,
        kind: hir::CheckKind,
    ) -> Result<(), Diagnostic> {
        let operands = check_operands(self, id, &kind)?;
        let flag = self.temp(self.primitives.bool_ty);
        self.assign(
            Place::local(flag),
            Rvalue::CheckedOp {
                check: index,
                kind: check_kind(&kind),
                operands,
            },
        );
        let ok = self.fresh(false);
        let fail = self.fresh(true);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(flag),
            targets: vec![(1, ok)],
            otherwise: fail,
        });
        self.switch_to(fail);
        let payload = self.panic_string("runtime check failed");
        let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
        let location = self.owner.expressions[id.index()].location.clone();
        self.terminate(Terminator::Panic {
            payload,
            location,
            unwind,
        });
        self.switch_to(ok);
        Ok(())
    }
}

fn check_operands(
    builder: &mut Builder<'_>,
    id: ExprId,
    kind: &hir::CheckKind,
) -> Result<Vec<Operand>, Diagnostic> {
    match kind {
        hir::CheckKind::Division { divisor, .. } => {
            let local = builder.require_value(*divisor)?;
            Ok(vec![copy_of(local)])
        }
        hir::CheckKind::Shift { amount, .. } => {
            let local = builder.require_value(*amount)?;
            Ok(vec![copy_of(local)])
        }
        hir::CheckKind::Bounds { .. } | hir::CheckKind::Utf8Boundary => {
            match &builder.owner.expressions[id.index()].kind {
                hir::ExprKind::Index { base, index, .. } => {
                    let base = builder.emit_place(*base)?;
                    let index = builder.require_value(*index)?;
                    Ok(vec![Operand::Copy(base), copy_of(index)])
                }
                hir::ExprKind::Slice { base, start, end } => {
                    let base = builder.emit_place(*base)?;
                    let start = start
                        .map(|start| builder.require_value(start))
                        .transpose()?
                        .map(copy_of)
                        .unwrap_or_else(|| {
                            builder.const_operand(builder.int_ty(), ConstValue::Integer(0))
                        });
                    let end = end
                        .map(|end| builder.require_value(end))
                        .transpose()?
                        .map(copy_of)
                        .unwrap_or_else(|| {
                            let length = builder.temp(builder.int_ty());
                            builder.assign(Place::local(length), Rvalue::Len(base));
                            copy_of(length)
                        });
                    Ok(vec![Operand::Copy(base), start, end])
                }
                _ => Ok(Vec::new()),
            }
        }
        hir::CheckKind::FloatToInt { value, .. } | hir::CheckKind::UnicodeScalar { value } => {
            let local = builder.require_value(*value)?;
            Ok(vec![copy_of(local)])
        }
    }
}

fn check_kind(kind: &hir::CheckKind) -> CheckOpKind {
    match kind {
        hir::CheckKind::Division { ty, .. } => CheckOpKind::Division { ty: *ty },
        hir::CheckKind::Shift { ty, .. } => CheckOpKind::Shift { ty: *ty },
        hir::CheckKind::Bounds { slice } => CheckOpKind::Bounds { slice: *slice },
        hir::CheckKind::Utf8Boundary => CheckOpKind::Utf8Boundary,
        hir::CheckKind::FloatToInt { signed, bits, .. } => CheckOpKind::FloatToInt {
            signed: *signed,
            bits: *bits,
        },
        hir::CheckKind::UnicodeScalar { .. } => CheckOpKind::UnicodeScalar,
    }
}
