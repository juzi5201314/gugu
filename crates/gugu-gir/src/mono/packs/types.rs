use super::*;

impl Expander<'_, '_> {
    pub(super) fn intern(&mut self, ty: hir::Type) -> hir::TypeId {
        if let Some(id) = self.intern.get(&ty) {
            return *id;
        }
        let id = hir::TypeId(self.module.types.len() as u32);
        self.module.types.push(ty.clone());
        self.intern.insert(ty, id);
        id
    }
    pub(super) fn types(&mut self, types: &[Ty]) -> Result<Vec<hir::TypeId>, Diagnostic> {
        types.iter().map(|ty| self.ty(ty)).collect()
    }
    pub(super) fn interface(
        &mut self,
        interface: &crate::semantics::TraitRef,
    ) -> Result<hir::TraitRef, Diagnostic> {
        Ok(hir::TraitRef {
            definition: self.context.identities.interfaces[interface.id],
            arguments: self.types(&interface.arguments)?,
        })
    }
    pub(super) fn ty(&mut self, ty: &Ty) -> Result<hir::TypeId, Diagnostic> {
        use hir::Type as T;
        let formed = match ty {
            Ty::Never => T::Never,
            Ty::Unit => T::Unit,
            Ty::Bool => T::Bool,
            Ty::Char => T::Char,
            Ty::String => T::String,
            Ty::TypeId => T::TypeId,
            Ty::Range => T::Range,
            Ty::Int { signed, bits } => T::Int {
                signed: *signed,
                bits: *bits,
            },
            Ty::Float(bits) => T::Float(*bits),
            Ty::Ref(t) => T::Ref(self.ty(t)?),
            Ty::Ptr(t) => T::Ptr(self.ty(t)?),
            Ty::Slice(t) => T::Slice(self.ty(t)?),
            Ty::Array(t, n) => T::Array(self.ty(t)?, *n),
            Ty::Tuple(ts) => T::Tuple(self.types(ts)?),
            Ty::Function(ps, r) => T::Function {
                parameters: self.types(ps)?,
                result: self.ty(r)?,
            },
            Ty::Callable(c, ts, s) => T::Callable {
                definition: self.context.identities.function(*c),
                arguments: self.types(ts)?,
                signature: self.ty(s)?,
            },
            Ty::Named(i, ts) => T::Named {
                definition: self
                    .context
                    .identities
                    .item(self.context.model.nominal[*i].definition),
                arguments: self.types(ts)?,
            },
            Ty::Param(_) => {
                return (0..self.context.module.types.len())
                    .find_map(|i| {
                        (self.context.semantic_type(hir::TypeId(i as u32)).ok() == Some(ty))
                            .then_some(hir::TypeId(i as u32))
                    })
                    .ok_or_else(|| invalid("pack element has no HIR type"));
            }
            Ty::Projection(t, i, m) => T::Projection {
                self_ty: self.ty(t)?,
                interface: self.interface(i)?,
                member: (0..)
                    .map_while(|index| {
                        self.context
                            .model
                            .interface_member_name(i.id, index)
                            .map(|name| (index, name))
                    })
                    .find(|(_, name)| name == m)
                    .expect("checked member")
                    .0,
            },
            Ty::Opaque(i, ts) => T::Opaque {
                definition: self.context.identities.opaques[*i as usize],
                arguments: self.types(ts)?,
            },
            Ty::Dyn(is) => T::Dyn(
                is.iter()
                    .map(|i| self.interface(i))
                    .collect::<Result<_, _>>()?,
            ),
            Ty::Option(t) => T::Option(self.ty(t)?),
            Ty::Result(t, e) => T::Result(self.ty(t)?, self.ty(e)?),
            Ty::Chan(t) => T::Chan(self.ty(t)?),
            Ty::Join(t) => T::Join(self.ty(t)?),
            Ty::ChanClosed => T::ChanClosed,
            Ty::TrySendErr => T::TrySendErr,
            Ty::TryRecvErr => T::TryRecvErr,
            Ty::Panic => T::Panic,
            Ty::Formatter => T::Formatter,
            Ty::MaybeUninit(t) => T::MaybeUninit(self.ty(t)?),
            Ty::Error | Ty::Var(_) => return Err(invalid("unresolved pack element type")),
        };
        Ok(self.intern(formed))
    }
    pub(super) fn rebind(
        &mut self,
        id: &mut hir::TypeId,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<(), Diagnostic> {
        let original = self.context.semantic_type(*id)?;
        let ty = substitute(original, bindings);
        if &ty != original {
            *id = self.ty(&ty)?;
        }
        Ok(())
    }
}
