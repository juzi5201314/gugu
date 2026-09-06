//! 外部调用效应不进入用户类型系统；直调保留声明身份，擦除后由桥接包装承担边界。
use super::super::ast::{
    AstRange, Attribute, ExprId, ExprKind, FnBody, ItemId, ItemKind, StmtKind,
};
use super::model::{CallableId, DefRef, Model};
use crate::{Diagnostic, DiagnosticCode};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ForeignEffect {
    Bridge,
    DirtyCpu,
    Leaf { stack: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ForeignDefinition {
    pub(crate) callable: CallableId,
    pub(crate) effect: Option<ForeignEffect>,
    // naked 与 imported 是两个固定标志，不为每个函数分配标志集合。
    pub(crate) flags: u8,
}
impl ForeignDefinition {
    pub(crate) const NAKED: u8 = 1;
    pub(crate) const IMPORTED: u8 = 2;
    pub(crate) fn naked(&self) -> bool {
        self.flags & Self::NAKED != 0
    }
    pub(crate) fn imported(&self) -> bool {
        self.flags & Self::IMPORTED != 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ForeignCall {
    pub(crate) expression: ExprId,
    pub(crate) callable: CallableId,
    pub(crate) effect: ForeignEffect,
}

#[derive(Default)]
pub(super) struct Foreign {
    // FnDecl 编号是每个模块内的稠密编号；只有实际携带边界/链接属性的函数分配描述。
    by_function: Vec<Vec<Option<usize>>>,
    pub(super) definitions: Vec<ForeignDefinition>,
    pub(super) linkage: Vec<super::linkage::Linkage>,
}

impl Model<'_> {
    pub(super) fn collect_foreign(&mut self) -> Result<(), Vec<Diagnostic>> {
        self.foreign.by_function = self
            .modules
            .iter()
            .map(|module| vec![None; module.arena.fns.len()])
            .collect();
        for (module, parsed) in self.modules.iter().enumerate() {
            for (index, item) in parsed.arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                let item_ref = DefRef {
                    module,
                    item: ItemId(index as u32),
                };
                if let Some(linkage) = self.item_linkage(item_ref).map_err(|error| vec![error])? {
                    self.foreign.linkage.push(linkage);
                }
                let ItemKind::Function(id) = item.kind else {
                    if self.attributes(module, item.attributes).any(|(name, _)| {
                        matches!(name, "ffi" | "naked" | "export_name" | "link_name")
                    }) {
                        return Err(vec![self.trait_error(
                            DefRef {
                                module,
                                item: ItemId(index as u32),
                            },
                            "该边界属性只能用于函数声明",
                        )]);
                    }
                    continue;
                };
                let callable = CallableId {
                    module,
                    function: id.0,
                };
                if let Some(definition) = self
                    .foreign_definition(callable)
                    .map_err(|error| vec![error])?
                {
                    let index = self.foreign.definitions.len();
                    self.foreign.by_function[module][id.0 as usize] = Some(index);
                    self.foreign.definitions.push(definition);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn foreign_definition_at(&self, callable: CallableId) -> Option<&ForeignDefinition> {
        self.foreign.by_function[callable.module][callable.function as usize]
            .map(|index| &self.foreign.definitions[index])
    }

    pub(super) fn foreign_effect_attribute(
        &self,
        module: usize,
        attributes: AstRange<Attribute>,
    ) -> Result<Option<ForeignEffect>, Diagnostic> {
        let mut effect = None;
        for (_, tokens) in self
            .attributes(module, attributes)
            .filter(|(name, _)| *name == "ffi")
        {
            if effect.is_some() {
                return Err(self.error(module, "同一位置只能声明一个 ffi 效应"));
            }
            effect = Some(
                match self.name(module, tokens[2].symbol.expect("ffi 模式经过词法检查")) {
                    "bridge" => ForeignEffect::Bridge,
                    "dirty_cpu" => ForeignEffect::DirtyCpu,
                    "leaf" => {
                        let stack = if tokens.len() == 4 {
                            0
                        } else {
                            let stack = self
                                .attribute_integer(module, tokens[6])
                                .ok_or_else(|| self.error(module, "ffi stack 常量超出 u64 范围"))?;
                            stack
                                .checked_add(15)
                                .map(|stack| stack & !15)
                                .ok_or_else(|| self.error(module, "ffi stack 对齐溢出"))?
                        };
                        ForeignEffect::Leaf { stack }
                    }
                    _ => unreachable!("ffi 模式经过词法检查"),
                },
            );
        }
        Ok(effect)
    }

    fn foreign_definition(
        &self,
        callable: CallableId,
    ) -> Result<Option<ForeignDefinition>, Diagnostic> {
        let parsed = &self.modules[callable.module];
        let function = &parsed.arena.fns[callable.function as usize];
        let item_id = self
            .function_definition(callable)
            .expect("具名函数具有声明");
        let item = &parsed.arena.items[item_id.item.0 as usize];
        let error = |message: &str| {
            Diagnostic::error(
                DiagnosticCode::InvalidDeclaration,
                message,
                Some(function.span.clone()),
            )
        };
        let mut definition = ForeignDefinition {
            callable,
            effect: None,
            flags: 0,
        };
        if self.has_attribute(callable.module, item.attributes, "naked") {
            definition.flags |= ForeignDefinition::NAKED;
        }
        debug_assert_eq!(definition.flags & !3, 0, "外部声明只有两个固定标志");
        let imported = function.body == FnBody::None;
        let external = function.extern_abi.is_some();
        let explicit = self.foreign_effect_attribute(callable.module, item.attributes)?;
        if definition.naked() && (!external || !function.unsafety || imported) {
            return Err(error("naked 必须用于带函数体的 unsafe extern C 函数"));
        }
        if definition.naked() && !self.single_asm_body(callable.module, function.body) {
            return Err(error("naked 函数体必须恰好包含一次 asm 调用"));
        }
        if let Some(effect) = explicit {
            if !external {
                return Err(error("ffi 声明属性只允许用于 extern C 函数"));
            }
            match effect {
                ForeignEffect::Bridge => {
                    return Err(error("ffi(bridge) 只能用于直接导入 C 调用表达式"));
                }
                ForeignEffect::Leaf { .. } if !imported && !definition.naked() => {
                    return Err(error("ffi(leaf) 只允许导入声明或 naked 函数"));
                }
                ForeignEffect::DirtyCpu if !imported && !function.unsafety => {
                    return Err(error(
                        "dirty_cpu native definition 必须是 unsafe extern C 函数",
                    ));
                }
                _ => {}
            }
        }
        if imported && external {
            definition.flags |= ForeignDefinition::IMPORTED;
        }
        if external {
            definition.effect = Some(explicit.unwrap_or(if definition.naked() {
                ForeignEffect::DirtyCpu
            } else {
                ForeignEffect::Bridge
            }));
        }
        Ok((external || definition.flags != 0).then_some(definition))
    }

    fn single_asm_body(&self, module: usize, body: FnBody) -> bool {
        let (FnBody::Eq(mut expression) | FnBody::Block(mut expression)) = body else {
            return false;
        };
        let parsed = &self.modules[module];
        loop {
            match parsed.arena.exprs[expression.0 as usize].kind {
                ExprKind::Asm { .. } => return true,
                ExprKind::Paren(inner) | ExprKind::Unsafe(inner) => expression = inner,
                ExprKind::Block { stmts, tail } => {
                    let mut only = tail;
                    for &statement in stmts.as_slice(&parsed.arena.stmt_ids) {
                        if !parsed.configured.stmt_active(statement) {
                            continue;
                        }
                        let StmtKind::Expr { expr, .. } =
                            parsed.arena.stmts[statement.0 as usize].kind
                        else {
                            return false;
                        };
                        if only.replace(expr).is_some() {
                            return false;
                        }
                    }
                    let Some(inner) = only else {
                        return false;
                    };
                    expression = inner;
                }
                _ => return false,
            }
        }
    }
}
