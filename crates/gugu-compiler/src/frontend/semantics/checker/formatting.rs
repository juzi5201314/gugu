use super::super::output::{FormattingCount, FormattingPart};
use super::*;
use crate::frontend::string::{FormatSpec, ParsedCount, parse_format};

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
            self.expression(*expr, None);
            let spec = spec.map_or_else(FormatSpec::default, |spec| {
                // FormatSpec token 保留起始冒号；语法解析只消费冒号后的说明。
                parse_format(&self.model.name(self.module, spec)[1..])
                    .expect("格式说明已经通过词法检查")
            });
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
