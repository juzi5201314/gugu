use super::{ConcreteCall, Diagnostic, MonoWorldV1, Protocol, id, index, invalid, layout};
use crate::{
    gir::body::{Callee, GirBody, Terminator},
    hir,
    mono::instantiate::CallSite,
    semantics::Ty,
};

pub(super) fn bind(
    body: &GirBody,
    world: &MonoWorldV1,
    digests: &[[u8; 32]],
    instance: &super::InstanceSummaryV1,
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
        let target = instance
            .call_targets
            .iter()
            .find(|(candidate, _)| candidate == site)
            .map(|(_, key)| *key)
            .or_else(|| match callee {
                Callee::Dispatch(dispatch) => instance
                    .call_targets
                    .iter()
                    .find(|(candidate, _)| *candidate == CallSite::Dispatch(*dispatch))
                    .map(|(_, key)| *key),
                _ => None,
            });
        let (ty, protocol) = if let Some(target) = target {
            // 摘要表按实例摘要有序且与 `world.instances` 同位：点查不重算 mono 摘要。
            let position = digests
                .binary_search(&target)
                .map_err(|_| invalid("调用签名缺少闭合实例"))?;
            let instance = &world.instances[position];
            let signature = layouts.context.module.definitions[index(instance.definition)]
                .signature
                .ok_or_else(|| invalid("调用目标缺少函数签名"))?;
            let ty = layouts
                .context
                .type_at(signature, &instance.substitutions)?;
            // 静态初始化器不是函数定义：它的正文是静态值的构造式，调用形状就是「无参、返回该值」。
            let ty = match callee {
                Callee::StaticInit(_) => Ty::Function(Vec::new(), Box::new(ty)),
                _ => ty,
            };
            (ty, None)
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
            let mut bindings = instance.substitutions.clone();
            let self_ty = layouts.context.type_at(dispatch.self_ty, &bindings)?;
            bindings.insert(
                "Self".to_owned(),
                crate::mono::universe::concrete(layouts.context, &self_ty)?.into_owned(),
            );
            let ty = layouts.context.type_at(dispatch.signature, &bindings)?;
            let location = crate::mono::instantiate::call_location(owner, *site);
            (
                ty,
                protocol(
                    layouts.context.module,
                    dispatch,
                    location,
                    layouts.context.sources,
                )?,
            )
        } else {
            continue;
        };
        let ty = crate::mono::universe::concrete(layouts.context, &ty)?.into_owned();
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
    location: &hir::Location,
    sources: &crate::SourceMap,
) -> Result<Option<Protocol>, Diagnostic> {
    let invalid = |message| invalid(message).with_span(location.span(sources));
    if dispatch.function.is_some() {
        return Err(invalid("已选静态方法没有闭合调用目标"));
    }
    let interface = dispatch
        .interface
        .as_ref()
        .ok_or_else(|| invalid("内建派发缺少 trait 身份"))?;
    let name = &module.definitions[interface.definition.index()].name;
    let protocol = match (name.as_str(), dispatch.member) {
        ("Eq", _) => Protocol::Eq,
        ("Ord", _) => Protocol::Cmp,
        ("Clone", _) => Protocol::Clone,
        ("Try", Some(2)) => Protocol::TryBranch,
        ("Try", Some(3)) => Protocol::TryFromValue,
        ("Try", Some(4)) => Protocol::TryFromError,
        ("IntoIter", Some(2)) => Protocol::IntoIter,
        ("Iter", Some(1)) => Protocol::IterNext,
        ("Hash", _) => {
            return Err(invalid(
                "该内建类型暂不支持调用 Hash.hash；请显式馈送语义字段",
            ));
        }
        _ => return Err(invalid("内建 trait 方法没有登记 lowering")),
    };
    Ok(Some(protocol))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_builtin_protocol_reports_the_call_expression() {
        let source = "fn main() {\n    value.print(out)\n}";
        let sources = crate::SourceMap::new(vec![
            crate::SourceSnapshot::from_str("main.gg", source).unwrap(),
        ])
        .unwrap();
        let start = source.find("value.print").unwrap() as u32;
        let location = hir::Location {
            source: 0,
            start,
            end: start + "value.print(out)".len() as u32,
            expansion: 0,
        };
        let mut module = hir::Module::default();
        module.definitions.push(hir::Definition {
            key: [0; 32],
            module: 0,
            name: "Print".into(),
            parent: None,
            location: None,
            kind: hir::DefinitionKind::Builtin,
            signature: None,
            parameters: Vec::new(),
            obligations: Vec::new(),
            public: true,
            track_caller: false,
        });
        let dispatch = hir::Dispatch {
            function: None,
            arguments: Vec::new(),
            implementation: None,
            interface: Some(hir::TraitRef {
                definition: hir::DefId(0),
                arguments: Vec::new(),
            }),
            member: Some(0),
            member_name: Some("print".into()),
            dereferences: 0,
            borrow: true,
            implicit_receiver: true,
            self_ty: hir::TypeId(0),
            signature: hir::TypeId(0),
            dynamic: false,
        };
        let error = protocol(&module, &dispatch, &location, &sources).unwrap_err();
        assert_eq!(error.code(), crate::DiagnosticCode::LirInvariant);
        assert_eq!(error.message(), "内建 trait 方法没有登记 lowering");
        let span = error.span().expect("协议错误必须有调用点");
        assert_eq!((span.line(), span.column()), (2, 5));
        assert_eq!(
            &source[span.start() as usize..span.end() as usize],
            "value.print(out)"
        );
    }
}
