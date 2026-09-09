use super::{ConcreteCall, Diagnostic, MonoWorldV1, Protocol, id, index, invalid, layout};
use crate::frontend::{
    gir::body::{Callee, GirBody, Terminator},
    hir,
    mono::instantiate::CallSite,
    semantics::Ty,
};

pub(super) fn bind(
    body: &GirBody,
    world: &MonoWorldV1,
    layouts: &mut layout::Builder<'_, '_>,
) -> Result<Vec<ConcreteCall>, Diagnostic> {
    let mut calls = Vec::new();
    for block in &body.blocks {
        let Terminator::Call { callee, site, .. } = &block.terminator else {
            continue;
        };
        if calls.iter().any(|call: &ConcreteCall| call.site == *site) {
            continue;
        }
        let target = layouts
            .instance
            .call_targets
            .iter()
            .find(|(candidate, _)| candidate == site)
            .map(|(_, key)| *key)
            .or_else(|| match callee {
                Callee::Dispatch(dispatch) => layouts
                    .instance
                    .call_targets
                    .iter()
                    .find(|(candidate, _)| *candidate == CallSite::Dispatch(*dispatch))
                    .map(|(_, key)| *key),
                _ => None,
            });
        let (ty, protocol) = if let Some(target) = target {
            let instance = world
                .instances
                .iter()
                .find(|instance| crate::frontend::mono::digest_of(&instance.mono_key) == target)
                .ok_or_else(|| invalid("调用签名缺少闭合实例"))?;
            let signature = layouts.context.module.definitions[index(instance.definition)]
                .signature
                .ok_or_else(|| invalid("调用目标缺少函数签名"))?;
            (
                layouts
                    .context
                    .type_at(signature, &instance.substitutions)?,
                None,
            )
        } else if let Callee::Dispatch(dispatch) | Callee::Dynamic(dispatch) = callee {
            let owner = layouts
                .context
                .module
                .owners
                .iter()
                .find(|owner| owner.definition == body.owner)
                .ok_or_else(|| invalid("具体调用没有 owner"))?;
            let dispatch = &owner.dispatches[index(*dispatch)];
            if dispatch.dynamic {
                continue;
            }
            let mut bindings = layouts.instance.substitutions.clone();
            let self_ty = layouts.context.type_at(dispatch.self_ty, &bindings)?;
            bindings.insert(
                "Self".to_owned(),
                crate::frontend::mono::universe::concrete(layouts.context, &self_ty)?,
            );
            let ty = layouts.context.type_at(dispatch.signature, &bindings)?;
            (ty, protocol(layouts.context.module, dispatch)?)
        } else {
            continue;
        };
        let ty = crate::frontend::mono::universe::concrete(layouts.context, &ty)?;
        let signature = match ty {
            Ty::Callable(_, _, signature) => *signature,
            signature => signature,
        };
        let Ty::Function(parameters, result) = signature else {
            return Err(invalid("已选调用签名不是函数"));
        };
        let parameters = parameters
            .iter()
            .map(|ty| layouts.intern(ty))
            .collect::<Result<_, _>>()?;
        let result = layouts.intern(&result)?;
        calls.push(ConcreteCall {
            site: *site,
            target,
            protocol,
            parameters,
            result,
        });
    }
    calls.sort_by_key(|call| call.site);
    debug_assert!(id(calls.len()) <= id(body.blocks.len()));
    Ok(calls)
}

fn protocol(
    module: &hir::Module,
    dispatch: &hir::Dispatch,
) -> Result<Option<Protocol>, Diagnostic> {
    if dispatch.function.is_some() {
        return Err(invalid("已选静态方法没有闭合调用目标"));
    }
    let interface = dispatch
        .interface
        .as_ref()
        .ok_or_else(|| invalid("内建派发缺少 trait 身份"))?;
    let name = &module.definitions[interface.definition.index()].name;
    let protocol = match (name.as_str(), dispatch.member) {
        ("Clone", _) => Protocol::Clone,
        ("Try", Some(2)) => Protocol::TryBranch,
        ("Try", Some(3)) => Protocol::TryFromValue,
        ("Try", Some(4)) => Protocol::TryFromError,
        ("IntoIter", Some(2)) => Protocol::IntoIter,
        ("Iter", Some(1)) => Protocol::IterNext,
        _ => return Err(invalid("内建 trait 方法没有登记 lowering")),
    };
    Ok(Some(protocol))
}
