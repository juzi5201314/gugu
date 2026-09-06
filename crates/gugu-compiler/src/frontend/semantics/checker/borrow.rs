//! 引用只记录已检查位置的投影；下标表达式不重复求值，动态下标保留步长对齐约束。
use super::super::borrow::{BorrowCheck, Projection};
use super::*;

struct AddressPath {
    base: Ty,
    ty: Ty,
    projection: Vec<Projection>,
}
impl AddressPath {
    fn new(ty: Ty) -> Self {
        Self {
            base: ty.clone(),
            ty,
            projection: Vec::new(),
        }
    }
    fn unref(&mut self) {
        if matches!(self.ty, Ty::Ref(_)) {
            self.ty = self.ty.deref().clone();
            self.base = self.ty.clone();
            self.projection.clear();
        }
    }
    fn field(&mut self, model: &Model<'_>, name: &str) -> Option<()> {
        self.unref();
        let (index, ty) = if let Ty::Tuple(fields) = &self.ty {
            let index = name.parse::<usize>().ok()?;
            (index, fields.get(index)?.clone())
        } else {
            let (index, ty, _) = model.find_field(&self.ty, name)?;
            (index, ty)
        };
        self.ty = ty;
        self.projection.push(Projection::Field(index));
        Some(())
    }
    fn index(&mut self, constant: Option<i128>) -> Option<()> {
        self.unref();
        match &self.ty {
            Ty::Array(element, _) => {
                self.ty = (**element).clone();
                self.projection.push(Projection::Index(constant));
            }
            Ty::Slice(element) => {
                self.base = (**element).clone();
                self.ty = self.base.clone();
                self.projection.clear();
            }
            _ => return None,
        }
        Some(())
    }
}

impl Checker<'_, '_> {
    pub(super) fn borrow_check(&mut self, expression: ExprId, place: ExprId, target: &Ty) {
        let path = self.address_path(place);
        self.record_borrow(expression, target, path);
    }

    pub(super) fn borrow_path_check(&mut self, expression: ExprId, path: PathId, target: &Ty) {
        let path = self.path_address(path);
        self.record_borrow(expression, target, path);
    }

    pub(super) fn implicit_borrow_check(
        &mut self,
        expression: ExprId,
        receiver: ExprId,
        target: &Ty,
    ) {
        // 非位置接收者先物化为正常对齐的临时槽；位置接收者不能绕过 packed 检查。
        let path = if expression == receiver
            && let ExprKind::Path(path) = self.arena().exprs[receiver.0 as usize].kind
        {
            let segments = self.arena().paths[path.0 as usize]
                .segments
                .as_slice(&self.arena().segments);
            self.segments_address(&segments[..segments.len() - 1])
        } else {
            self.address_path(receiver)
        };
        if let Some(path) = path {
            self.record_borrow(expression, target, Some(path));
        }
    }

    fn record_borrow(&mut self, expression: ExprId, target: &Ty, path: Option<AddressPath>) {
        if *target == Ty::Error {
            return;
        }
        match path {
            Some(path) => self.borrow_checks.push(BorrowCheck {
                expression,
                base: path.base,
                projection: path.projection,
                target: target.clone(),
            }),
            None => self.error(
                DiagnosticCode::InvalidExpression,
                "该表达式没有可形成引用的槽投影",
                self.arena().exprs[expression.0 as usize].span.clone(),
            ),
        }
    }

    fn path_address(&self, path: PathId) -> Option<AddressPath> {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        self.segments_address(segments)
    }

    fn segments_address(&self, segments: &[PathSegment]) -> Option<AddressPath> {
        let first = segments.first()?;
        let mut path = if let Some(&slot) = self.state.names.get(&first.name) {
            let mut path = AddressPath::new(self.resolve(&self.slots[slot].ty));
            for segment in &segments[1..] {
                path.field(self.model, self.model.name(self.module, segment.name))?;
            }
            path
        } else {
            let names: Vec<_> = segments
                .iter()
                .map(|segment| self.model.name(self.module, segment.name))
                .collect();
            let definition = self.model.resolve(self.module, &names).ok()?;
            if !matches!(
                self.model.modules[definition.module].arena.items[definition.item.0 as usize].kind,
                ItemKind::Static { .. }
            ) {
                return None;
            }
            AddressPath::new(self.model.value_type(definition).ok()?)
        };
        let arguments = segments.last()?.args;
        if arguments.len != 0 {
            let [GenericArg::Expr(index)] = arguments.as_slice(&self.arena().generic_args) else {
                return None;
            };
            path.index(self.model.constant_int(self.module, *index).ok())?;
        }
        Some(path)
    }

    fn address_path(&self, expression: ExprId) -> Option<AddressPath> {
        match self.arena().exprs[expression.0 as usize].kind {
            ExprKind::Path(path) => self.path_address(path),
            ExprKind::Paren(inner) => self.address_path(inner),
            ExprKind::Field { base, name } => {
                let mut path = self.address_path(base)?;
                path.field(self.model, self.model.name(self.module, name))?;
                Some(path)
            }
            ExprKind::TupleField { base, index } => {
                let mut path = self.address_path(base)?;
                path.field(self.model, &index.to_string())?;
                Some(path)
            }
            ExprKind::Index {
                base,
                index: IndexKind::Expr(index),
            } => {
                let mut path = self.address_path(base)?;
                path.index(self.model.constant_int(self.module, index).ok())?;
                Some(path)
            }
            ExprKind::Unary {
                op: UnOp::Deref, ..
            } => self
                .expressions
                .iter()
                .rev()
                .find(|(id, _)| *id == expression)
                .map(|(_, ty)| AddressPath::new(self.resolve(ty))),
            _ => None,
        }
    }
}
