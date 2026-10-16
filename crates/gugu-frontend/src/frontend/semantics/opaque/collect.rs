use super::super::super::ast::{
    AstArena, ExprId, ExprKind, GenericArg, ItemId, ItemKind, TyId, TyKind,
};
use super::super::model::{CallableId, DefRef, Model};
use super::{Definition, Origin};
use crate::{Diagnostic, DiagnosticCode};

impl Model<'_> {
    pub(in super::super) fn collect_opaques(&mut self) -> Result<(), Diagnostic> {
        self.opaques.by_type = self
            .modules
            .iter()
            .map(|module| vec![None; module.arena.tys.len()])
            .collect();
        for (module, parsed) in self.modules.iter().enumerate() {
            let arena = &parsed.arena;
            let mut active = vec![false; arena.fns.len()];
            for (index, item) in arena.items.iter().enumerate() {
                if parsed.configured.item_active(ItemId(index as u32))
                    && let ItemKind::Function(id) = item.kind
                {
                    active[id.0 as usize] = true;
                }
            }
            for (index, expression) in arena.exprs.iter().enumerate() {
                if parsed.configured.expr_active(ExprId(index as u32))
                    && let ExprKind::Closure(id) = expression.kind
                {
                    active[id.0 as usize] = true;
                }
            }
            let mut origins = vec![None; arena.tys.len()];
            for (index, function) in arena.fns.iter().enumerate() {
                if function.extern_abi.is_some() || !active[index] {
                    continue;
                }
                let owner = CallableId {
                    module,
                    function: index as u32,
                };
                for (index, param) in function.params.as_slice(&arena.params).iter().enumerate() {
                    if !parsed
                        .configured
                        .param_active(function.params.start as usize + index)
                    {
                        continue;
                    }
                    if let Some(ty) = param.ty {
                        self.opaque_positions(module, ty, Origin::Parameter(owner), &mut origins);
                    }
                }
                if let Some(ty) = function.return_ty {
                    self.opaque_positions(module, ty, Origin::Return(owner), &mut origins);
                }
            }
            for (index, item) in arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                if let ItemKind::TypeAlias { ty: Some(ty), .. } = item.kind {
                    self.opaque_positions(
                        module,
                        ty,
                        Origin::Alias(DefRef {
                            module,
                            item: ItemId(index as u32),
                        }),
                        &mut origins,
                    );
                }
            }
            for (index, ty) in arena.tys.iter().enumerate() {
                let TyKind::Impl(bounds) = ty.kind else {
                    continue;
                };
                let Some(origin) = origins[index] else {
                    continue;
                };
                let id = u32::try_from(self.opaques.definitions.len()).map_err(|_| {
                    Diagnostic::error(
                        DiagnosticCode::InvalidType,
                        "不透明声明数量超过 u32 上限",
                        Some(ty.span.clone()),
                    )
                })?;
                self.opaques.by_type[module][index] = Some(id);
                self.opaques.definitions.push(Definition {
                    module,
                    ty: TyId(index as u32),
                    origin,
                    bounds,
                });
            }
        }
        Ok(())
    }
    fn opaque_positions(
        &self,
        module: usize,
        id: TyId,
        origin: Origin,
        origins: &mut [Option<Origin>],
    ) {
        let arena = &self.modules[module].arena;
        match arena.tys[id.0 as usize].kind {
            TyKind::Impl(_) => origins[id.0 as usize] = Some(origin),
            TyKind::Ref(ty)
            | TyKind::Ptr(ty)
            | TyKind::Slice(ty)
            | TyKind::Array { elem: ty, .. } => self.opaque_positions(module, ty, origin, origins),
            TyKind::Tuple(types) => {
                for &ty in types.as_slice(&arena.ty_ids) {
                    self.opaque_positions(module, ty, origin, origins);
                }
            }
            TyKind::Path(path) => {
                for segment in arena.paths[path.0 as usize]
                    .segments
                    .as_slice(&arena.segments)
                {
                    if self.name(module, segment.name) == "Vec" {
                        continue;
                    }
                    for argument in segment.args.as_slice(&arena.generic_args) {
                        if let GenericArg::Type(ty) = *argument {
                            self.opaque_positions(module, ty, origin, origins);
                        }
                    }
                }
            }
            // 胖函数签名与通道元素要求可点名类型，不能引入新的匿名泛型。
            TyKind::Fn { .. } | TyKind::Chan(_) | TyKind::Dyn(_) => {}
            _ => {}
        }
    }
}

pub(super) fn contains_self_or_impl(model: &Model<'_>, module: usize, id: TyId) -> bool {
    let arena: &AstArena = &model.modules[module].arena;
    match arena.tys[id.0 as usize].kind {
        TyKind::Impl(_) => true,
        TyKind::Path(path) => path_uses_self_or_impl(model, module, path),
        TyKind::Ref(ty) | TyKind::Ptr(ty) | TyKind::Slice(ty) | TyKind::Array { elem: ty, .. } => {
            contains_self_or_impl(model, module, ty)
        }
        TyKind::Tuple(types) => types
            .as_slice(&arena.ty_ids)
            .iter()
            .any(|&ty| contains_self_or_impl(model, module, ty)),
        TyKind::Fn { params, ret } => {
            params
                .as_slice(&arena.ty_ids)
                .iter()
                .any(|&ty| contains_self_or_impl(model, module, ty))
                || ret.is_some_and(|ty| contains_self_or_impl(model, module, ty))
        }
        TyKind::Chan(args) => args
            .as_slice(&arena.generic_args)
            .iter()
            .any(|arg| argument_uses_self_or_impl(model, module, *arg)),
        TyKind::Dyn(paths) => paths
            .as_slice(&arena.path_ids)
            .iter()
            .any(|&path| path_uses_self_or_impl(model, module, path)),
        _ => false,
    }
}

fn path_uses_self_or_impl(
    model: &Model<'_>,
    module: usize,
    path: super::super::super::ast::PathId,
) -> bool {
    let arena = &model.modules[module].arena;
    arena.paths[path.0 as usize]
        .segments
        .as_slice(&arena.segments)
        .iter()
        .any(|segment| {
            model.name(module, segment.name) == "Self"
                || segment
                    .args
                    .as_slice(&arena.generic_args)
                    .iter()
                    .any(|arg| argument_uses_self_or_impl(model, module, *arg))
        })
}

fn argument_uses_self_or_impl(model: &Model<'_>, module: usize, argument: GenericArg) -> bool {
    match argument {
        GenericArg::Type(ty) => contains_self_or_impl(model, module, ty),
        GenericArg::Expr(expression) => {
            match model.modules[module].arena.exprs[expression.0 as usize].kind {
                ExprKind::Path(path) => path_uses_self_or_impl(model, module, path),
                _ => false,
            }
        }
    }
}
