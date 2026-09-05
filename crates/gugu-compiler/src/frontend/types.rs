//! 类型形成与目标布局基础。
use super::ast::{AstArena, TyId, TyKind};
use crate::diagnostics::{Diagnostic, DiagnosticCode};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Layout {
    pub(crate) size: u64,
    pub(crate) align: u64,
}

#[derive(Debug, Default)]
pub(crate) struct TypeArena {
    layouts: Vec<Option<Layout>>,
}

impl TypeArena {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn layout(&mut self, arena: &AstArena, ty: TyId) -> Result<Layout, Diagnostic> {
        if let Some(layout) = self.layouts.get(ty.0 as usize).and_then(Option::as_ref) {
            return Ok(*layout);
        }
        let result = match arena.tys.get(ty.0 as usize).map(|t| t.kind) {
            Some(TyKind::Never) => Layout { size: 0, align: 1 },
            Some(TyKind::Infer) | Some(TyKind::Error) => {
                return Err(Diagnostic::error(
                    DiagnosticCode::InvalidType,
                    "类型变量未收敛",
                    None,
                ));
            }
            Some(
                TyKind::Ref(_)
                | TyKind::Ptr(_)
                | TyKind::Fn { .. }
                | TyKind::Dyn(_)
                | TyKind::Chan(_),
            ) => Layout { size: 8, align: 8 },
            Some(TyKind::Slice(_)) => Layout { size: 16, align: 8 },
            Some(TyKind::Tuple(types)) => self.aggregate(arena, types.as_slice(&arena.ty_ids))?,
            Some(TyKind::Array { elem, .. }) => self.layout(arena, elem)?,
            Some(TyKind::Path(path)) => {
                let name = arena
                    .paths
                    .get(path.0 as usize)
                    .map(|_| "named")
                    .unwrap_or("");
                if name.is_empty() {
                    return Err(Diagnostic::error(
                        DiagnosticCode::InvalidType,
                        "未知类型路径",
                        None,
                    ));
                }
                Layout { size: 0, align: 1 }
            }
            Some(TyKind::Impl(_) | TyKind::SourceMacro { .. }) => {
                return Err(Diagnostic::error(
                    DiagnosticCode::InvalidType,
                    "该类型尚未形成",
                    None,
                ));
            }
            None => {
                return Err(Diagnostic::error(
                    DiagnosticCode::InvalidType,
                    "类型编号越界",
                    None,
                ));
            }
        };
        let index = ty.0 as usize;
        if self.layouts.len() <= index {
            self.layouts.resize(index + 1, None);
        }
        self.layouts[index] = Some(result);
        Ok(result)
    }

    fn aggregate(&mut self, arena: &AstArena, types: &[TyId]) -> Result<Layout, Diagnostic> {
        let mut size = 0;
        let mut align = 1;
        for &ty in types {
            let layout = self.layout(arena, ty)?;
            size = align_up(size, layout.align);
            size = size.checked_add(layout.size).ok_or_else(|| {
                Diagnostic::error(DiagnosticCode::InvalidType, "类型大小溢出", None)
            })?;
            align = align.max(layout.align);
        }
        Ok(Layout {
            size: align_up(size, align),
            align,
        })
    }
}

pub(crate) fn form_and_layout(
    modules: &[super::ParsedModule],
) -> Result<Vec<Layout>, Vec<Diagnostic>> {
    let mut layouts = Vec::new();
    let mut diagnostics = Vec::new();
    for module in modules {
        let mut arena = TypeArena::new();
        for index in 0..module.arena.tys.len() {
            match arena.layout(&module.arena, TyId(index as u32)) {
                Ok(layout) => layouts.push(layout),
                Err(error) => diagnostics.push(error),
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(layouts)
    } else {
        Err(diagnostics)
    }
}

fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}
