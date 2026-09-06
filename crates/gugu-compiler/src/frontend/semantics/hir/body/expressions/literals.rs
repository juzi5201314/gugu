use super::*;
use crate::frontend::string;

impl BodyBuilder<'_, '_, '_, '_> {
    pub(in super::super) fn literal(
        &self,
        literal: ast::LitKind,
        ty: &Ty,
        negative: bool,
    ) -> Result<hir::Literal, Diagnostic> {
        Ok(match literal {
            ast::LitKind::Int { limbs, .. } => {
                let limbs = limbs.as_slice(&self.arena().int_limbs);
                if limbs.len() > 4 {
                    return Err(self.error("已检查整数字面量超出 u128"));
                }
                let mut value = limbs
                    .iter()
                    .rev()
                    .fold(0u128, |value, &limb| (value << 32) | u128::from(limb));
                if negative {
                    value = value.wrapping_neg();
                }
                hir::Literal::Integer(value)
            }
            ast::LitKind::Float { digits, exp10 } => {
                let value = format!("{}e{exp10}", self.compiler.model.name(self.module, digits))
                    .parse::<f64>()
                    .map_err(|_| self.error("已检查浮点字面量不能形成 IEEE 值"))?;
                let value = if *ty == Ty::Float(32) {
                    f64::from(value as f32)
                } else {
                    value
                };
                hir::Literal::Float(if negative { -value } else { value }.to_bits())
            }
            ast::LitKind::Bool(value) => hir::Literal::Bool(value),
            ast::LitKind::Char { value, .. } => hir::Literal::Char(value),
            ast::LitKind::ByteChar { value, .. } => hir::Literal::Integer(u128::from(value)),
            ast::LitKind::String { text } | ast::LitKind::RawString { text } => {
                hir::Literal::String(
                    string::decode_string(self.compiler.model.name(self.module, text)).into_owned(),
                )
            }
            ast::LitKind::ByteString { text } => hir::Literal::Bytes(string::decode_bytes(
                self.compiler.model.name(self.module, text),
            )),
            ast::LitKind::CString { text } => {
                let mut bytes = string::decode_bytes(self.compiler.model.name(self.module, text));
                bytes.push(0);
                hir::Literal::CString(bytes)
            }
        })
    }

    pub(super) fn formatted_string(
        &mut self,
        parts: ast::AstRange<ast::FStringPart>,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let mut formed = Vec::with_capacity(parts.len as usize);
        for (offset, part) in parts
            .as_slice(&self.arena().fstring_parts)
            .iter()
            .enumerate()
        {
            formed.push(match part {
                ast::FStringPart::Text { text, .. } => hir::StringPart::Text(
                    string::decode_fstring_text(self.compiler.model.name(self.module, *text))
                        .into_owned(),
                ),
                ast::FStringPart::Interp { expr, span, .. } => {
                    let expression = self.expression(*expr)?;
                    let index = parts.start + checked_id(offset)?;
                    let plan = self
                        .facts
                        .body
                        .formatting
                        .iter()
                        .find(|plan| plan.part == index)
                        .ok_or_else(|| self.error("插值缺少已解析格式计划"))?;
                    let format = plan
                        .spec
                        .clone()
                        .try_map(|count| self.format_count(count, span))?;
                    hir::StringPart::Value {
                        expression,
                        format,
                        dispatch: self.selected_dispatch(*expr, Some("Print"), None)?,
                    }
                }
            });
        }
        let start = checked_id(self.output.string_parts.len())?;
        self.output.string_parts.extend(formed);
        Ok(hir::ExprKind::String {
            parts: start..checked_id(self.output.string_parts.len())?,
        })
    }

    fn format_count(
        &mut self,
        count: crate::frontend::semantics::output::FormattingCount,
        span: &Span,
    ) -> Result<hir::FormatCount, Diagnostic> {
        use crate::frontend::semantics::output::FormattingCount;
        match count {
            FormattingCount::Fixed(value) => Ok(hir::FormatCount::Fixed(value)),
            FormattingCount::Slot(slot) => {
                let local =
                    self.slots[slot].ok_or_else(|| self.error("格式计数绑定不属于当前 owner"))?;
                let expression = self.reserve(&Ty::Int {
                    signed: true,
                    bits: 64,
                })?;
                self.set_expression(
                    expression,
                    hir::ExprKind::Resolved(hir::Res::Local(local)),
                    self.scope,
                    span,
                    hir::Effects::READ,
                )?;
                Ok(hir::FormatCount::Value(expression))
            }
        }
    }
}
