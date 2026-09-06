use super::*;

impl Builder<'_, '_> {
    pub(super) fn type_id(
        &mut self,
        ty: &Ty,
        owner: hir::DefId,
    ) -> Result<hir::TypeId, Diagnostic> {
        if let Some(&id) = self.type_cache[owner.index()].get(ty) {
            return Ok(id);
        }
        let formed = match ty {
            Ty::Error | Ty::Var(_) => return Err(self.error(owner, "未收敛类型不能进入 HIR")),
            Ty::Never => hir::Type::Never,
            Ty::Unit => hir::Type::Unit,
            Ty::Bool => hir::Type::Bool,
            Ty::Char => hir::Type::Char,
            Ty::String => hir::Type::String,
            Ty::TypeId => hir::Type::TypeId,
            Ty::Range => hir::Type::Range,
            Ty::Int { signed, bits } => hir::Type::Int {
                signed: *signed,
                bits: *bits,
            },
            Ty::Float(bits) => hir::Type::Float(*bits),
            Ty::Ref(inner) => hir::Type::Ref(self.type_id(inner, owner)?),
            Ty::Ptr(inner) => hir::Type::Ptr(self.type_id(inner, owner)?),
            Ty::Slice(inner) => hir::Type::Slice(self.type_id(inner, owner)?),
            Ty::Array(inner, count) => hir::Type::Array(self.type_id(inner, owner)?, *count),
            Ty::Tuple(types) => hir::Type::Tuple(self.type_ids(types, owner)?),
            Ty::Function(parameters, result) => hir::Type::Function {
                parameters: self.type_ids(parameters, owner)?,
                result: self.type_id(result, owner)?,
            },
            Ty::Callable(callable, arguments, signature) => hir::Type::Callable {
                definition: self.identities.function(*callable),
                arguments: self.type_ids(arguments, owner)?,
                signature: self.type_id(signature, owner)?,
            },
            Ty::Named(index, arguments) => hir::Type::Named {
                definition: self.identities.item(self.model.nominal[*index].definition),
                arguments: self.type_ids(arguments, owner)?,
            },
            Ty::Param(name) => {
                let (scope, index) = self.parameter(owner, name).ok_or_else(|| {
                    self.error(owner, &format!("泛型参数 `{name}` 没有稳定声明身份"))
                })?;
                hir::Type::Parameter {
                    owner: scope,
                    index,
                }
            }
            Ty::Projection(self_ty, interface, member) => {
                let position = self.model.traits.interfaces[interface.id]
                    .members
                    .keys()
                    .position(|name| name == member)
                    .ok_or_else(|| self.error(owner, "关联投影没有已选择成员"))?;
                hir::Type::Projection {
                    self_ty: self.type_id(self_ty, owner)?,
                    interface: self.trait_ref(interface, owner)?,
                    member: checked_id(position)?,
                }
            }
            Ty::Opaque(index, arguments) => hir::Type::Opaque {
                definition: self.identities.opaques[*index as usize],
                arguments: self.type_ids(arguments, owner)?,
            },
            Ty::Dyn(interfaces) => hir::Type::Dyn(
                interfaces
                    .iter()
                    .map(|interface| self.trait_ref(interface, owner))
                    .collect::<Result<_, _>>()?,
            ),
            Ty::Option(inner) => hir::Type::Option(self.type_id(inner, owner)?),
            Ty::Result(value, error) => {
                hir::Type::Result(self.type_id(value, owner)?, self.type_id(error, owner)?)
            }
            Ty::Chan(inner) => hir::Type::Chan(self.type_id(inner, owner)?),
            Ty::Join(inner) => hir::Type::Join(self.type_id(inner, owner)?),
            Ty::MaybeUninit(inner) => hir::Type::MaybeUninit(self.type_id(inner, owner)?),
        };
        let id = if let Some(&id) = self.type_intern.get(&formed) {
            id
        } else {
            let id = hir::TypeId(checked_id(self.output.types.len())?);
            self.type_intern.insert(formed.clone(), id);
            self.output.types.push(formed);
            id
        };
        self.type_cache[owner.index()].insert(ty.clone(), id);
        Ok(id)
    }

    pub(super) fn type_ids(
        &mut self,
        types: &[Ty],
        owner: hir::DefId,
    ) -> Result<Vec<hir::TypeId>, Diagnostic> {
        types.iter().map(|ty| self.type_id(ty, owner)).collect()
    }

    pub(super) fn trait_ref(
        &mut self,
        interface: &super::super::traits::TraitRef,
        owner: hir::DefId,
    ) -> Result<hir::TraitRef, Diagnostic> {
        Ok(hir::TraitRef {
            definition: self.identities.interfaces[interface.id],
            arguments: self.type_ids(&interface.arguments, owner)?,
        })
    }

    fn parameter(&self, mut owner: hir::DefId, name: &str) -> Option<(hir::DefId, u32)> {
        loop {
            if let Some(index) = self.parameters[owner.index()]
                .iter()
                .position(|parameter| parameter.name == name)
            {
                debug_assert!(index < u32::MAX as usize);
                return Some((owner, index as u32));
            }
            owner = self.output.definitions[owner.index()].parent?;
        }
    }
}
