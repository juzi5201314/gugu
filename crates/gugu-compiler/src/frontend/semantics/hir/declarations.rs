use super::super::traits;
use super::*;

impl Builder<'_, '_> {
    pub(super) fn collect_parameters(&mut self) -> Result<(), Diagnostic> {
        for index in 0..self.identities.origins.len() {
            let origin = self.identities.origins[index];
            let mut parameters = Vec::new();
            if let Some((module, generics)) = self.generic_range(origin) {
                if self.output.definitions[index].kind == hir::DefinitionKind::Trait {
                    parameters.push(type_parameter("Self".into(), false));
                }
                for parameter in generics.as_slice(&self.model.modules[module].arena.generic_params)
                {
                    parameters.push(match parameter.kind {
                        ast::GenericParamKind::Type { name, pack, .. } => {
                            type_parameter(self.model.name(module, name).to_owned(), pack)
                        }
                        ast::GenericParamKind::Comptime { name, ty, .. } => PendingParameter {
                            name: self.model.name(module, name).to_owned(),
                            kind: PendingParameterKind::Comptime { module, ty },
                        },
                    });
                }
            }
            match origin {
                Origin::Named(named) => {
                    if let Some(definition) = self.identities.named_items[named]
                        && let ast::ItemKind::Function(function) =
                            self.model.modules[definition.module].arena.items
                                [definition.item.0 as usize]
                                .kind
                    {
                        parameters.extend(
                            self.model
                                .apits(CallableId {
                                    module: definition.module,
                                    function: function.0,
                                })
                                .map(|id| type_parameter(Model::apit_name(id), false)),
                        );
                    }
                    if let Some(definition) = self.identities.named_items[named]
                        && let Some(implementation) = self
                            .model
                            .traits
                            .implementations
                            .iter()
                            .find(|implementation| implementation.definition == definition)
                    {
                        for name in implementation.parameters.keys() {
                            if !parameters.iter().any(|parameter| parameter.name == *name) {
                                parameters.push(type_parameter(name.clone(), false));
                            }
                        }
                    }
                }
                Origin::Closure(function) => parameters.extend(
                    self.model
                        .apits(function)
                        .map(|id| type_parameter(Model::apit_name(id), false)),
                ),
                Origin::Opaque(id) => parameters.extend(
                    self.model
                        .opaque_context(id)
                        .into_keys()
                        .map(|name| type_parameter(name, false)),
                ),
                Origin::BuiltinTrait(interface) => {
                    parameters.push(type_parameter("Self".into(), false));
                    parameters.extend(
                        self.model.traits.interfaces[interface]
                            .parameters
                            .iter()
                            .cloned()
                            .map(|name| type_parameter(name, false)),
                    );
                }
                Origin::Async { .. } | Origin::LocalStatic { .. } => {}
            }
            self.parameters[index] = parameters;
        }
        for index in 0..self.parameters.len() {
            let owner = hir::DefId(checked_id(index)?);
            let mut formed = Vec::with_capacity(self.parameters[index].len());
            for (position, parameter) in self.parameters[index].clone().into_iter().enumerate() {
                let kind = match parameter.kind {
                    PendingParameterKind::Type { pack } => hir::ParameterKind::Type { pack },
                    PendingParameterKind::Comptime { module, ty } => hir::ParameterKind::Comptime(
                        self.type_id(&self.model.form(module, ty)?, owner)?,
                    ),
                };
                // APIT 的语义临时编号不能写入持久身份；真正的引用使用 owner-local 参数编号。
                let name = if parameter.name.starts_with("$apit:") {
                    format!("$apit:{position}")
                } else {
                    parameter.name
                };
                formed.push(hir::Parameter { name, kind });
            }
            self.output.definitions[index].parameters = formed;
        }
        Ok(())
    }

    fn generic_range(&self, origin: Origin) -> Option<(usize, ast::AstRange<ast::GenericParam>)> {
        let (module, item) = match origin {
            Origin::Named(index) => {
                let definition = self.identities.named_items[index]?;
                (
                    definition.module,
                    &self.model.modules[definition.module].arena.items[definition.item.0 as usize],
                )
            }
            Origin::Closure(function) => {
                return Some((
                    function.module,
                    self.model.modules[function.module].arena.fns[function.function as usize]
                        .generics,
                ));
            }
            _ => return None,
        };
        let generics = match item.kind {
            ast::ItemKind::Function(function) => {
                self.model.modules[module].arena.fns[function.0 as usize].generics
            }
            ast::ItemKind::Struct { generics, .. }
            | ast::ItemKind::Enum { generics, .. }
            | ast::ItemKind::Union { generics, .. }
            | ast::ItemKind::Trait { generics, .. }
            | ast::ItemKind::Impl { generics, .. }
            | ast::ItemKind::TypeAlias { generics, .. } => generics,
            _ => return None,
        };
        Some((module, generics))
    }

    pub(super) fn lower_declarations(&mut self) -> Result<(), Diagnostic> {
        for index in 0..self.identities.origins.len() {
            let owner = hir::DefId(checked_id(index)?);
            let origin = self.identities.origins[index];
            self.lower_signature(owner, origin)?;
            self.lower_obligations(owner, origin)?;
        }
        self.lower_aggregates()?;
        self.lower_interfaces()?;
        self.lower_implementations()?;
        self.lower_opaques()?;
        Ok(())
    }

    fn lower_signature(&mut self, owner: hir::DefId, origin: Origin) -> Result<(), Diagnostic> {
        let ty = match origin {
            Origin::Named(index) => {
                let Some(definition) = self.identities.named_items[index] else {
                    return Ok(());
                };
                let item =
                    &self.model.modules[definition.module].arena.items[definition.item.0 as usize];
                match item.kind {
                    ast::ItemKind::Function(_)
                    | ast::ItemKind::Static { .. }
                    | ast::ItemKind::Const { value: Some(_), .. } => {
                        Some(self.model.value_type(definition)?)
                    }
                    ast::ItemKind::Const { ty: Some(ty), .. }
                    | ast::ItemKind::TypeAlias { ty: Some(ty), .. } => {
                        Some(self.model.form(definition.module, ty)?)
                    }
                    ast::ItemKind::Struct { .. }
                    | ast::ItemKind::Enum { .. }
                    | ast::ItemKind::Union { .. } => {
                        let index = self
                            .model
                            .nominal
                            .iter()
                            .position(|nominal| nominal.definition == definition)
                            .expect("名义声明已形成");
                        Some(Ty::Named(
                            index,
                            self.model.nominal[index]
                                .params
                                .iter()
                                .cloned()
                                .map(Ty::Param)
                                .collect(),
                        ))
                    }
                    _ => None,
                }
            }
            Origin::Closure(function) => self
                .checked
                .bodies
                .iter()
                .flat_map(|body| &body.captures)
                .find(|capture| capture.function == Some(function))
                .map(|capture| capture.signature.clone()),
            Origin::Async { module, expression } => self
                .checked
                .bodies
                .iter()
                .filter(|body| body.definition.module == module)
                .flat_map(|body| &body.captures)
                .find(|capture| capture.function.is_none() && capture.expression == expression)
                .map(|capture| capture.signature.clone()),
            Origin::LocalStatic { module, statement } => {
                let ast::StmtKind::Static { ty, .. } =
                    self.model.modules[module].arena.stmts[statement.0 as usize].kind
                else {
                    unreachable!("local static 来源");
                };
                Some(self.model.form(module, ty)?)
            }
            Origin::Opaque(_) | Origin::BuiltinTrait(_) => None,
        };
        if let Some(ty) = ty {
            let signature = self.type_id(&ty, owner)?;
            self.output.definitions[owner.index()].signature = Some(signature);
        }
        Ok(())
    }

    fn lower_obligations(&mut self, owner: hir::DefId, origin: Origin) -> Result<(), Diagnostic> {
        let Some((module, generics)) = self.generic_range(origin) else {
            return Ok(());
        };
        let parameters = self.model.generic_parameters(module, generics);
        let obligations = self
            .model
            .generic_obligations(module, generics, &parameters)?;
        for obligation in obligations {
            let bound = hir::Obligation::Trait {
                ty: self.type_id(&obligation.ty, owner)?,
                interface: self.trait_ref(&obligation.interface, owner)?,
            };
            self.output.definitions[owner.index()]
                .obligations
                .push(bound);
        }
        for parameter in generics.as_slice(&self.model.modules[module].arena.generic_params) {
            let ast::GenericParamKind::Type { name, bounds, .. } = parameter.kind else {
                continue;
            };
            for bound in bounds.as_slice(&self.model.modules[module].arena.bounds) {
                if let ast::BoundKind::Fn { params, ret } = bound.kind {
                    let signature = self.model.form_kind(
                        module,
                        ast::TyKind::Fn { params, ret },
                        &parameters,
                        &mut Vec::new(),
                    )?;
                    let obligation = hir::Obligation::Callable {
                        ty: self
                            .type_id(&Ty::Param(self.model.name(module, name).to_owned()), owner)?,
                        signature: self.type_id(&signature, owner)?,
                    };
                    self.output.definitions[owner.index()]
                        .obligations
                        .push(obligation);
                }
            }
        }
        Ok(())
    }

    fn lower_aggregates(&mut self) -> Result<(), Diagnostic> {
        for nominal in &self.model.nominal {
            let owner = self.identities.item(nominal.definition);
            let mut variants = Vec::with_capacity(nominal.variants.len());
            for variant in &nominal.variants {
                let fields = variant
                    .fields
                    .iter()
                    .map(|field| {
                        Ok(hir::Field {
                            name: field.name.clone(),
                            ty: self.type_id(&field.ty, owner)?,
                            public: field.public,
                        })
                    })
                    .collect::<Result<_, Diagnostic>>()?;
                variants.push(hir::Variant {
                    name: variant.name.clone(),
                    fields,
                    record: variant.record,
                });
            }
            let repr = nominal.repr;
            let flags = u8::from(repr.c())
                | (u8::from(repr.packed()) << 1)
                | (u8::from(repr.transparent()) << 2);
            debug_assert_eq!(flags & !7, 0, "只有三个布局标志");
            self.output.aggregates.push(hir::Aggregate {
                definition: owner,
                variants,
                representation: hir::Representation {
                    flags,
                    align: repr.align,
                    tag: repr.tag,
                },
            });
        }
        self.output
            .aggregates
            .sort_by_key(|aggregate| aggregate.definition);
        Ok(())
    }

    fn lower_interfaces(&mut self) -> Result<(), Diagnostic> {
        for (index, interface) in self.model.traits.interfaces.iter().enumerate() {
            let owner = self.identities.interfaces[index];
            let members = self.lower_members(&interface.members, owner)?;
            for (ty, bound) in &interface.requirements {
                let obligation = hir::Obligation::Trait {
                    ty: self.type_id(ty, owner)?,
                    interface: self.trait_ref(bound, owner)?,
                };
                self.output.definitions[owner.index()]
                    .obligations
                    .push(obligation);
            }
            self.output.interfaces.push(hir::Interface {
                definition: owner,
                members,
                unsafety: interface.unsafety,
            });
        }
        self.output
            .interfaces
            .sort_by_key(|interface| interface.definition);
        Ok(())
    }

    fn lower_implementations(&mut self) -> Result<(), Diagnostic> {
        for implementation in &self.model.traits.implementations {
            let owner = self.identities.item(implementation.definition);
            let self_ty = self.type_id(&implementation.self_ty, owner)?;
            let interface = implementation
                .interface
                .as_ref()
                .map(|interface| self.trait_ref(interface, owner))
                .transpose()?;
            let members = self.lower_members(&implementation.members, owner)?;
            self.output.implementations.push(hir::Implementation {
                definition: owner,
                self_ty,
                interface,
                members,
                negative: implementation.negative,
            });
        }
        self.output
            .implementations
            .sort_by_key(|implementation| implementation.definition);
        Ok(())
    }

    fn lower_members(
        &mut self,
        members: &BTreeMap<String, traits::Member>,
        owner: hir::DefId,
    ) -> Result<Vec<hir::Member>, Diagnostic> {
        members
            .iter()
            .map(|(name, member)| {
                let definition = member
                    .definition
                    .map(|definition| self.identities.item(definition));
                let scope = definition.unwrap_or(owner);
                let kind = match &member.kind {
                    traits::MemberKind::Method {
                        signature,
                        receiver,
                        default,
                    } => hir::MemberKind::Method {
                        signature: self.type_id(signature, scope)?,
                        receiver: *receiver,
                        default: *default,
                        unsafety: self.model.member_is_unsafe(member),
                    },
                    traits::MemberKind::Type(ty) => hir::MemberKind::Type(
                        ty.as_ref().map(|ty| self.type_id(ty, scope)).transpose()?,
                    ),
                    traits::MemberKind::Const { ty, value } => hir::MemberKind::Constant {
                        ty: self.type_id(ty, scope)?,
                        value: value.as_ref().map(constant_literal),
                    },
                };
                Ok(hir::Member {
                    name: name.clone(),
                    definition,
                    kind,
                })
            })
            .collect()
    }

    fn lower_opaques(&mut self) -> Result<(), Diagnostic> {
        for (index, hidden) in self.checked.hidden_types.iter().enumerate() {
            let owner = self.identities.opaques[index];
            let hidden = hidden
                .as_ref()
                .map(|ty| self.type_id(ty, owner))
                .transpose()?;
            let arguments: Vec<_> = self
                .model
                .opaque_context(index as u32)
                .into_keys()
                .map(Ty::Param)
                .collect();
            let bounds = self.model.opaque_bounds(
                index as u32,
                &arguments,
                &Ty::Opaque(index as u32, arguments.clone()),
            )?;
            for bound in bounds.traits {
                let obligation = hir::Obligation::Trait {
                    ty: self.type_id(&bound.ty, owner)?,
                    interface: self.trait_ref(&bound.interface, owner)?,
                };
                self.output.definitions[owner.index()]
                    .obligations
                    .push(obligation);
            }
            for signature in bounds.functions {
                let ty = self.type_id(&Ty::Opaque(index as u32, arguments.clone()), owner)?;
                let signature = self.type_id(&signature, owner)?;
                self.output.definitions[owner.index()]
                    .obligations
                    .push(hir::Obligation::Callable { ty, signature });
            }
            self.output.opaques.push(hir::Opaque {
                definition: owner,
                hidden,
            });
        }
        self.output.opaques.sort_by_key(|opaque| opaque.definition);
        Ok(())
    }
}

fn type_parameter(name: String, pack: bool) -> PendingParameter {
    PendingParameter {
        name,
        kind: PendingParameterKind::Type { pack },
    }
}
fn constant_literal(value: &super::super::model::ConstantValue) -> hir::Literal {
    match value {
        super::super::model::ConstantValue::Int(value) => hir::Literal::Integer(*value as u128),
        super::super::model::ConstantValue::Float(value) => hir::Literal::Float(*value),
        super::super::model::ConstantValue::Bool(value) => hir::Literal::Bool(*value),
        super::super::model::ConstantValue::String(value) => hir::Literal::String(value.clone()),
    }
}
