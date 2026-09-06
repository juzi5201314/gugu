//! 属性正文复用 parser 的 token 范围，只形成一次布局和外部调用约束。
use super::{AstRange, AttrKind, Attribute, DefRef, Model};
use crate::Diagnostic;
use crate::frontend::token::{Token, TokenKind};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Representation {
    // C、packed、transparent 是三个独立标志，固定编码在一个字节内。
    flags: u8,
    pub(crate) align: u64,
    pub(crate) tag: Option<(bool, u16)>,
}
impl Representation {
    pub(crate) fn c(self) -> bool {
        self.flags & 1 != 0
    }
    pub(crate) fn packed(self) -> bool {
        self.flags & 2 != 0
    }
    pub(crate) fn transparent(self) -> bool {
        self.flags & 4 != 0
    }
}

impl Model<'_> {
    pub(crate) fn attributes(
        &self,
        module: usize,
        range: AstRange<Attribute>,
    ) -> impl Iterator<Item = (&str, &[Token])> {
        let parsed = &self.modules[module];
        range
            .as_slice(&parsed.arena.attrs)
            .iter()
            .filter_map(move |attribute| {
                let AttrKind::Outer {
                    token_open,
                    token_close,
                } = attribute.kind
                else {
                    return None;
                };
                let tokens = &parsed.tokens.tokens[token_open as usize + 1..token_close as usize];
                let name = self.name(module, tokens[0].symbol?);
                Some((name, tokens))
            })
    }

    pub(crate) fn has_attribute(
        &self,
        module: usize,
        range: AstRange<Attribute>,
        name: &str,
    ) -> bool {
        self.attributes(module, range)
            .any(|(attribute, _)| attribute == name)
    }

    pub(in super::super) fn attribute_integer(&self, module: usize, token: Token) -> Option<u64> {
        integer(self.name(module, token.symbol?))
    }

    pub(super) fn representation(&self, definition: DefRef) -> Result<Representation, Diagnostic> {
        let module = definition.module;
        let item = &self.modules[module].arena.items[definition.item.0 as usize];
        let mut repr = Representation {
            flags: u8::from(matches!(item.kind, super::ItemKind::Union { .. })),
            align: 1,
            tag: None,
        };
        for (_, tokens) in self
            .attributes(module, item.attributes)
            .filter(|(name, _)| *name == "repr")
        {
            let mut index = 2;
            while index + 1 < tokens.len() {
                let token = tokens[index];
                if token.kind == TokenKind::Comma {
                    index += 1;
                    continue;
                }
                let name = self.name(module, token.symbol.expect("repr 参数经过词法检查"));
                match name {
                    "C" => repr.flags |= 1,
                    "packed" => repr.flags |= 2,
                    "transparent" => repr.flags |= 4,
                    "align" => {
                        let token = tokens[index + 2];
                        let value =
                            integer(self.name(module, token.symbol.expect("align 整数载荷")))
                                .ok_or_else(|| {
                                    self.trait_error(definition, "repr 对齐值超出 u64 范围")
                                })?;
                        if !value.is_power_of_two() {
                            return Err(self.trait_error(definition, "repr 对齐必须是非零二的幂"));
                        }
                        repr.align = repr.align.max(value);
                        index += 4;
                        continue;
                    }
                    _ => {
                        let Some(super::Ty::Int { signed, bits }) = super::Ty::primitive(name)
                        else {
                            unreachable!("repr 参数已经过词法检查");
                        };
                        if repr.tag.is_some_and(|previous| previous != (signed, bits)) {
                            return Err(self.trait_error(definition, "枚举只能选择一个整数 repr"));
                        }
                        repr.tag = Some((signed, bits));
                    }
                }
                index += 1;
            }
        }
        debug_assert_eq!(repr.flags & !7, 0, "repr 只有三个布局标志");
        if repr.transparent() && (repr.c() || repr.packed()) {
            return Err(self.trait_error(definition, "transparent 不能与 C 或 packed 同时指定"));
        }
        Ok(repr)
    }
}

fn integer(text: &str) -> Option<u64> {
    let (radix, digits) =
        if let Some(digits) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            (16, digits)
        } else if let Some(digits) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
            (8, digits)
        } else if let Some(digits) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
            (2, digits)
        } else {
            (10, text)
        };
    digits
        .chars()
        .filter(|ch| *ch != '_')
        .try_fold(0_u64, |value, ch| {
            value
                .checked_mul(radix)?
                .checked_add(u64::from(ch.to_digit(radix as u32)?))
        })
}
