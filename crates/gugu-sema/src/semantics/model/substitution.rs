use super::*;
pub fn substitute(ty: &Ty, bindings: &BTreeMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param(s) => bindings.get(s).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Projection(ty, interface, name) => Ty::Projection(
            Box::new(substitute(ty, bindings)),
            interface.substitute(bindings),
            name.clone(),
        ),
        Ty::Ref(t) => Ty::Ref(Box::new(substitute(t, bindings))),
        Ty::Ptr(t) => Ty::Ptr(Box::new(substitute(t, bindings))),
        Ty::Array(t, n) => Ty::Array(Box::new(substitute(t, bindings)), n.substitute(bindings)),
        Ty::Const(n) => Ty::Const(n.substitute(bindings)),
        Ty::Slice(t) => Ty::Slice(Box::new(substitute(t, bindings))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| substitute(t, bindings)).collect()),
        Ty::Named(i, args) => Ty::Named(*i, args.iter().map(|t| substitute(t, bindings)).collect()),
        Ty::Opaque(id, arguments) => Ty::Opaque(
            *id,
            arguments
                .iter()
                .map(|ty| substitute(ty, bindings))
                .collect(),
        ),
        Ty::Dyn(interfaces) => Ty::Dyn(
            interfaces
                .iter()
                .map(|interface| interface.substitute(bindings))
                .collect(),
        ),
        Ty::Option(t) => Ty::Option(Box::new(substitute(t, bindings))),
        Ty::Result(t, e) => Ty::Result(
            Box::new(substitute(t, bindings)),
            Box::new(substitute(e, bindings)),
        ),
        Ty::Chan(t) => Ty::Chan(Box::new(substitute(t, bindings))),
        Ty::Join(t) => Ty::Join(Box::new(substitute(t, bindings))),
        Ty::MaybeUninit(t) => Ty::MaybeUninit(Box::new(substitute(t, bindings))),
        Ty::ChanClosed | Ty::TrySendErr | Ty::TryRecvErr | Ty::Panic => ty.clone(),
        Ty::Function(params, ret) => Ty::Function(
            params.iter().map(|ty| substitute(ty, bindings)).collect(),
            Box::new(substitute(ret, bindings)),
        ),
        Ty::Callable(id, arguments, signature) => Ty::Callable(
            *id,
            arguments
                .iter()
                .map(|ty| substitute(ty, bindings))
                .collect(),
            Box::new(substitute(signature, bindings)),
        ),
        _ => ty.clone(),
    }
}
