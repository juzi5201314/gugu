use super::super::output::{FormattingCount, FormattingPart};
use super::*;
use crate::frontend::string::{FormatSpec, ParsedCount, ValueClass, flag_conflict, parse_format};

/// 等待推断收敛后再检查的格式化插值。
pub(super) struct PendingFormat {
    expr: ExprId,
    name: String,
    method: String,
    flags: FormatSpec<()>,
    span: Span,
}

/// 收敛后的类型在标志兼容规则里的分类；`&string` 与 `string` 同类。
fn value_class(ty: &Ty) -> ValueClass {
    match ty {
        Ty::Int { .. } => ValueClass::Int,
        Ty::Float(_) => ValueClass::Float,
        Ty::String => ValueClass::Str,
        Ty::Ref(inner) => value_class(inner),
        _ => ValueClass::Other,
    }
}

impl Checker<'_, '_> {
    pub(super) fn formatted_parts(&mut self, parts: AstRange<FStringPart>) {
        for (offset, part) in parts
            .as_slice(&self.arena().fstring_parts)
            .iter()
            .enumerate()
        {
            let FStringPart::Interp { expr, spec, span } = part else {
                continue;
            };
            let spec = spec.map_or_else(FormatSpec::default, |spec| {
                // FormatSpec token 保留起始冒号；语法解析只消费冒号后的说明。
                parse_format(&self.model.name(self.module, spec)[1..])
                    .expect("格式说明已经通过词法检查")
            });
            let ty = self.expression(*expr, None);
            let (name, method) = spec.kind.trait_method();
            self.require_format_trait(*expr, &ty, name, method, spec.flags(), span);
            if let Ok(spec) = spec.try_map(|count| self.formatting_count(count, span)) {
                debug_assert!(
                    offset < u32::MAX as usize && parts.start.checked_add(offset as u32).is_some()
                );
                self.formatting.push(FormattingPart {
                    part: parts.start + offset as u32,
                    expression: *expr,
                    spec,
                });
            }
        }
    }

    fn require_format_trait(
        &mut self,
        expr: ExprId,
        ty: &Ty,
        name: &str,
        method: &str,
        flags: FormatSpec<()>,
        span: &Span,
    ) {
        if matches!(self.resolve(ty), Ty::Error | Ty::Never) {
            return;
        }
        self.format_traits.push(PendingFormat {
            expr,
            name: name.to_owned(),
            method: method.to_owned(),
            flags,
            span: span.clone(),
        });
    }

    /// 推断结束后检查格式 trait，并按收敛后的类型检查标志兼容性。
    pub(super) fn finish_format_traits(&mut self) {
        for pending in std::mem::take(&mut self.format_traits) {
            let Some(stored) = self
                .expressions
                .iter()
                .rev()
                .find(|(id, _)| *id == pending.expr)
                .map(|(_, ty)| ty.clone())
            else {
                continue;
            };
            let ty = self.resolve(&stored);
            if matches!(ty, Ty::Error | Ty::Never) {
                continue;
            }
            if matches!(ty, Ty::Var(_)) {
                self.error(
                    DiagnosticCode::InvalidType,
                    "格式化表达式的类型未能收敛",
                    pending.span,
                );
                continue;
            }
            if let Some(conflict) = flag_conflict(&pending.flags, value_class(&ty)) {
                self.error(DiagnosticCode::InvalidType, conflict, pending.span.clone());
            }
            self.language_method(
                pending.expr,
                &ty,
                &pending.name,
                Vec::new(),
                &pending.method,
                &pending.span,
            );
        }
    }

    fn formatting_count(
        &mut self,
        count: ParsedCount<'_>,
        span: &Span,
    ) -> Result<FormattingCount, ()> {
        let name = match count {
            ParsedCount::Fixed(value) => return Ok(FormattingCount::Fixed(value)),
            ParsedCount::Name(name) => name,
        };
        let symbol = self.model.modules[self.module]
            .tokens
            .intern
            .lookup_str(name);
        let binding = symbol.and_then(|symbol| {
            self.state
                .names
                .get(&symbol)
                .copied()
                .map(|slot| (symbol, slot))
        });
        let Some((symbol, slot)) = binding else {
            self.error(
                DiagnosticCode::InvalidExpression,
                format!("格式计数 `{name}` 必须引用当前作用域的 int 绑定"),
                span.clone(),
            );
            return Err(());
        };
        let ty = self
            .local(symbol, true, span)
            .expect("已找到当前作用域绑定");
        self.unify(
            &ty,
            &Ty::Int {
                signed: true,
                bits: 64,
            },
            span,
        );
        Ok(FormattingCount::Slot(slot))
    }
}
