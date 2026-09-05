//! 布局只消费语义类型，禁止将未解析路径伪装成零大小值。
use super::{
    ast::{ItemKind, StructBody},
    semantics::{
        CheckedSemantics,
        model::{Model, Ty},
    },
};
use crate::{Diagnostic, DiagnosticCode};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Layout {
    pub(crate) size: u64,
    pub(crate) align: u64,
}

pub(crate) fn form_and_layout(
    model: &Model<'_>,
    semantics: &CheckedSemantics,
) -> Result<Vec<Layout>, Vec<Diagnostic>> {
    let mut arena = Layouts {
        model,
        capturing: capturing_functions(model, semantics),
        complete: BTreeMap::new(),
        active: Vec::new(),
    };
    for body in &semantics.bodies {
        for ty in body
            .slots
            .iter()
            .chain(body.expressions.iter().map(|(_, ty)| ty))
        {
            arena.layout(ty).map_err(|error| vec![error])?;
        }
    }
    for (index, nominal) in model.nominal.iter().enumerate() {
        if !nominal.params.is_empty() {
            continue;
        }
        arena
            .layout(&Ty::Named(index, Vec::new()))
            .map_err(|error| vec![error])?;
        for variant in &nominal.variants {
            for field in &variant.fields {
                arena.layout(&field.ty).map_err(|error| vec![error])?;
            }
        }
    }
    Ok(arena.complete.into_values().collect())
}

fn capturing_functions(model: &Model<'_>, semantics: &CheckedSemantics) -> Vec<Vec<bool>> {
    // 模块和 FnDecl 都是稠密编号；Vec<bool> 为每个现有函数保留一位。
    let mut capturing: Vec<_> = model
        .modules
        .iter()
        .map(|module| vec![false; module.arena.fns.len()])
        .collect();
    for plan in semantics.bodies.iter().flat_map(|body| &body.captures) {
        if let Some(id) = plan.function {
            debug_assert!(
                id.module < capturing.len() && (id.function as usize) < capturing[id.module].len()
            );
            capturing[id.module][id.function as usize] |= !plan.captures.is_empty();
        }
    }
    capturing
}

struct Layouts<'m, 'a> {
    model: &'m Model<'a>,
    capturing: Vec<Vec<bool>>,
    complete: BTreeMap<Ty, Layout>,
    active: Vec<Ty>,
}
impl Layouts<'_, '_> {
    fn layout(&mut self, ty: &Ty) -> Result<Option<Layout>, Diagnostic> {
        if let Some(layout) = self.complete.get(ty) {
            return Ok(Some(*layout));
        }
        if self.active.contains(ty) {
            return Err(Diagnostic::error(
                DiagnosticCode::RecursiveType,
                "类型形成无限大小递归",
                None,
            ));
        }
        self.active.push(ty.clone());
        let result = self.compute(ty);
        self.active.pop();
        let result = result?;
        if let Some(layout) = result {
            self.complete.insert(ty.clone(), layout);
        }
        Ok(result)
    }
    fn compute(&mut self, ty: &Ty) -> Result<Option<Layout>, Diagnostic> {
        let word = Layout { size: 8, align: 8 };
        Ok(Some(match ty {
            Ty::Error | Ty::Var(_) => return Err(invalid("布局类型尚未收敛")),
            Ty::Param(_) => return Ok(None),
            Ty::Unit | Ty::Never => Layout { size: 0, align: 1 },
            Ty::Bool => Layout { size: 1, align: 1 },
            Ty::Char => Layout { size: 4, align: 4 },
            Ty::Int { bits, .. } | Ty::Float(bits) => Layout {
                size: u64::from(*bits) / 8,
                align: u64::from(*bits) / 8,
            },
            Ty::Ptr(_) | Ty::Chan(_) | Ty::Join(_) => word,
            Ty::Ref(t) => {
                if matches!(**t, Ty::Slice(_)) {
                    Layout { size: 16, align: 8 }
                } else {
                    word
                }
            }
            Ty::Slice(_) => return Ok(None),
            Ty::String | Ty::Function(..) | Ty::Range => Layout { size: 16, align: 8 },
            Ty::Callable(id, _, _) => {
                if !self.capturing[id.module][id.function as usize] {
                    Layout { size: 0, align: 1 }
                } else {
                    Layout { size: 8, align: 8 }
                }
            }
            Ty::Array(elem, n) => {
                let Some(elem) = self.layout(elem)? else {
                    return Ok(None);
                };
                Layout {
                    size: elem
                        .size
                        .checked_mul(*n)
                        .ok_or_else(|| invalid("数组布局溢出"))?,
                    align: elem.align,
                }
            }
            Ty::Tuple(ts) => {
                let Some(layout) = self.aggregate(ts.iter())? else {
                    return Ok(None);
                };
                layout
            }
            Ty::Option(t) => {
                let Some(payload) = self.layout(t)? else {
                    return Ok(None);
                };
                tagged(payload)?
            }
            Ty::Result(t, e) => {
                let (Some(t), Some(e)) = (self.layout(t)?, self.layout(e)?) else {
                    return Ok(None);
                };
                tagged(Layout {
                    size: t.size.max(e.size),
                    align: t.align.max(e.align),
                })?
            }
            Ty::Named(index, _) => {
                let nominal = &self.model.nominal[*index];
                let item = &self.model.modules[nominal.definition.module].arena.items
                    [nominal.definition.item.0 as usize];
                let variants = self.model.variants(ty).expect("名义类型字段");
                match item.kind {
                    ItemKind::Struct {
                        body: StructBody::Record(_),
                        ..
                    } => {
                        let Some(layout) =
                            self.aggregate(variants[0].fields.iter().map(|field| &field.ty))?
                        else {
                            return Ok(None);
                        };
                        layout
                    }
                    ItemKind::Struct {
                        body: StructBody::Newtype(_),
                        ..
                    } => {
                        let Some(layout) = self.layout(&variants[0].fields[0].ty)? else {
                            return Ok(None);
                        };
                        layout
                    }
                    ItemKind::Enum { .. } => {
                        let mut payload = Layout { size: 0, align: 1 };
                        for variant in variants {
                            let Some(layout) =
                                self.aggregate(variant.fields.iter().map(|field| &field.ty))?
                            else {
                                return Ok(None);
                            };
                            payload.size = payload.size.max(layout.size);
                            payload.align = payload.align.max(layout.align);
                        }
                        if nominal.variants.is_empty() {
                            payload
                        } else {
                            tagged(payload)?
                        }
                    }
                    ItemKind::Union { .. } => {
                        let fields = variants[0].fields.iter().map(|field| &field.ty);
                        let mut payload = Layout { size: 0, align: 1 };
                        for field in fields {
                            let Some(layout) = self.layout(field)? else {
                                return Ok(None);
                            };
                            payload.size = payload.size.max(layout.size);
                            payload.align = payload.align.max(layout.align);
                        }
                        align_up(payload.size, payload.align).map(|size| Layout {
                            size,
                            align: payload.align,
                        })?
                    }
                    _ => return Err(invalid("布局要求名义类型声明")),
                }
            }
        }))
    }
    fn aggregate<'t>(
        &mut self,
        fields: impl Iterator<Item = &'t Ty>,
    ) -> Result<Option<Layout>, Diagnostic> {
        let mut total = Layout { size: 0, align: 1 };
        for ty in fields {
            let Some(layout) = self.layout(ty)? else {
                return Ok(None);
            };
            total.size = align_up(total.size, layout.align)?
                .checked_add(layout.size)
                .ok_or_else(|| invalid("聚合布局溢出"))?;
            total.align = total.align.max(layout.align);
        }
        total.size = align_up(total.size, total.align)?;
        Ok(Some(total))
    }
}
fn tagged(payload: Layout) -> Result<Layout, Diagnostic> {
    let align = payload.align.max(1);
    let size = align_up(1, align)?
        .checked_add(payload.size)
        .ok_or_else(|| invalid("枚举布局溢出"))?;
    Ok(Layout {
        size: align_up(size, align)?,
        align,
    })
}
fn align_up(size: u64, align: u64) -> Result<u64, Diagnostic> {
    debug_assert!(align.is_power_of_two());
    size.checked_add(align - 1)
        .map(|s| s & !(align - 1))
        .ok_or_else(|| invalid("布局对齐溢出"))
}
fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
