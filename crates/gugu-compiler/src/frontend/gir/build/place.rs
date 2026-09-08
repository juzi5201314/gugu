use super::*;

impl Builder<'_> {
    pub(super) fn emit_place(&mut self, id: ExprId) -> Result<Place, Diagnostic> {
        let _ = self.emit_expr(id)?;
        if let Some(place) = self.expression_places[id.index()] {
            return Ok(place);
        }
        let local = self.require_value(id)?;
        Ok(Place::local(local))
    }

    pub(super) fn emit_field(
        &mut self,
        id: ExprId,
        base: ExprId,
        index: u32,
    ) -> Result<(), Diagnostic> {
        let place = self.emit_place(base)?;
        let field_ty = self.expr_ty(id);
        let projected = self.project(
            place,
            Projection::Field {
                index,
                field_ty,
                access: Access::Normal,
            },
        );
        self.set_place(id, projected);
        Ok(())
    }

    pub(super) fn emit_index(
        &mut self,
        id: ExprId,
        base: ExprId,
        index: ExprId,
        _read: Option<u32>,
        _write: Option<u32>,
    ) -> Result<(), Diagnostic> {
        let place = self.emit_place(base)?;
        let Some(index) = self.emit_expr(index)? else {
            return Ok(());
        };
        self.emit_check_ops(id)?;
        let projected = self.project(place, Projection::Index(index));
        self.set_place(id, projected);
        Ok(())
    }

    pub(super) fn emit_slice(
        &mut self,
        id: ExprId,
        base: ExprId,
        start: Option<ExprId>,
        end: Option<ExprId>,
    ) -> Result<(), Diagnostic> {
        let place = self.emit_place(base)?;
        let start = match start {
            Some(start) => self.emit_expr(start)?,
            None => {
                let local = self.temp(self.int_ty());
                let operand = self.const_operand(self.int_ty(), ConstValue::Integer(0));
                self.assign(Place::local(local), Rvalue::Use(operand));
                Some(local)
            }
        };
        let end = match end {
            Some(end) => self.emit_expr(end)?,
            None => {
                let local = self.temp(self.int_ty());
                self.assign(Place::local(local), Rvalue::Len(place));
                Some(local)
            }
        };
        let (Some(start), Some(end)) = (start, end) else {
            return Ok(());
        };
        self.emit_check_ops(id)?;
        let dest = self.temp(self.expr_ty(id));
        let slice = matches!(
            self.module.types.get(self.expr_ty(base).index()),
            Some(hir::Type::Slice(_) | hir::Type::Ref(_))
        );
        self.assign(
            Place::local(dest),
            Rvalue::Intrinsic {
                op: IntrinsicOp::Subslice { slice },
                operands: vec![Operand::Copy(place), copy_of(start), copy_of(end)],
                types: vec![self.expr_ty(id)],
            },
        );
        self.set_value(id, dest);
        Ok(())
    }
}
