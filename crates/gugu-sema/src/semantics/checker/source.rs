//! `std.src.file`/`line`/`column` 的调用点折叠：位置在 checker 取定，
//! 下游只消费 `SourceLiteral` 快照，普通函数调用路径接触不到这些项。
use super::super::output::{SourceField, SourceLiteral};
use super::*;

impl Checker<'_, '_> {
    pub(super) fn src_call(&mut self, callee: ExprId, path: PathId, args: &[ExprId]) -> Option<Ty> {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        if segments
            .first()
            .is_some_and(|segment| self.state.names.contains_key(&segment.name))
        {
            return None;
        }
        let path = self
            .model
            .external_path(self.module, &self.model.path(self.module, path))?;
        let kind = match path.as_str() {
            "std.src.file" => SourceField::File,
            "std.src.line" => SourceField::Line,
            "std.src.column" => SourceField::Column,
            _ => return None,
        };
        let span = &self.arena().exprs[callee.0 as usize].span;
        if !args.is_empty() {
            self.error(
                DiagnosticCode::InvalidExpression,
                "std.src 源位置查询不接收参数",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        let result = match kind {
            SourceField::File => Ty::String,
            SourceField::Line | SourceField::Column => Ty::int(),
        };
        self.source_literals.push(SourceLiteral {
            expression: callee,
            kind,
            file: span.path().to_string_lossy().into_owned(),
            line: span.line(),
            column: span.column(),
        });
        Some(result)
    }
}
