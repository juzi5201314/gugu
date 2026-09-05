use super::super::super::ast::{
    AstRange, ExprKind, FnBody, GenericArg, ItemId, ItemKind, PathId, TyId, TyKind,
};
use super::super::model::{DefRef, Model, Ty};
use super::{Implementation, Interface, Member, MemberKind, Owner, TraitRef};
use crate::Diagnostic;
use std::collections::BTreeMap;

impl Model<'_> {
    pub(in super::super) fn collect_traits(&mut self) -> Result<(), Diagnostic> {
        self.traits.owners = self
            .modules
            .iter()
            .map(|m| vec![None; m.arena.items.len()])
            .collect();
        self.language_interfaces();
        for (module, parsed) in self.modules.iter().enumerate() {
            for (index, item) in parsed.arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                if let ItemKind::Trait {
                    generics,
                    items,
                    unsafety,
                } = item.kind
                {
                    let definition = DefRef {
                        module,
                        item: ItemId(index as u32),
                    };
                    let name = self
                        .name(module, item.name.expect("trait 有名字"))
                        .to_owned();
                    if self
                        .traits
                        .interfaces
                        .iter()
                        .any(|t| t.definition.is_none() && t.name == name)
                    {
                        return Err(self.trait_error(definition, "不能重新声明语言认识的 trait"));
                    }
                    let id = self.traits.interfaces.len();
                    let members = self.member_shells(module, items)?;
                    self.traits.interfaces.push(Interface {
                        definition: Some(definition),
                        name,
                        parameters: generics
                            .as_slice(&parsed.arena.generic_params)
                            .iter()
                            .filter_map(|g| match g.kind {
                                super::super::super::ast::GenericParamKind::Type {
                                    name, ..
                                } => Some(self.name(module, name).to_owned()),
                                _ => None,
                            })
                            .collect(),
                        members,
                        unsafety,
                        requirements: Vec::new(),
                    });
                    self.set_owner(definition, items, Owner::Interface(id));
                }
            }
        }
        for (module, parsed) in self.modules.iter().enumerate() {
            for (index, item) in parsed.arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                if let ItemKind::Impl {
                    generics,
                    self_ty,
                    trait_ty,
                    items,
                    negative,
                    ..
                } = item.kind
                {
                    let definition = DefRef {
                        module,
                        item: ItemId(index as u32),
                    };
                    let mut params = self.generic_parameters(module, generics);
                    self.implicit_parameters(module, self_ty, false, &mut params);
                    if let Some(ty) = trait_ty {
                        self.implicit_parameters(module, ty, false, &mut params);
                    }
                    let self_ty = self.form_inner(module, self_ty, &params, &mut Vec::new())?;
                    let interface = trait_ty
                        .map(|id| {
                            let TyKind::Path(path) = parsed.arena.tys[id.0 as usize].kind else {
                                return Err(self.trait_error(definition, "impl 要求 trait 路径"));
                            };
                            self.trait_ref(module, path, &params)
                        })
                        .transpose()?;
                    let obligations = self.generic_obligations(module, generics, &params)?;
                    let members = self.member_shells(module, items)?;
                    let id = self.traits.implementations.len();
                    self.traits.implementations.push(Implementation {
                        definition,
                        self_ty,
                        interface,
                        parameters: params,
                        obligations,
                        members,
                        negative,
                    });
                    self.set_owner(definition, items, Owner::Implementation(id));
                }
            }
        }
        // 先形成全部关联类型，再形成签名，声明顺序不影响 Self::Output。
        for id in 0..self.traits.implementations.len() {
            self.complete_members(Owner::Implementation(id), true)?;
        }
        for id in 0..self.traits.interfaces.len() {
            if self.traits.interfaces[id].definition.is_some() {
                self.complete_members(Owner::Interface(id), false)?;
            }
        }
        for id in 0..self.traits.implementations.len() {
            self.complete_members(Owner::Implementation(id), false)?;
        }
        Ok(())
    }
    fn set_owner(&mut self, definition: DefRef, items: AstRange<ItemId>, owner: Owner) {
        let arena = &self.modules[definition.module].arena;
        debug_assert!((definition.item.0 as usize) < self.traits.owners[definition.module].len());
        self.traits.owners[definition.module][definition.item.0 as usize] = Some(owner);
        for item in items.as_slice(&arena.item_ids) {
            self.traits.owners[definition.module][item.0 as usize] = Some(owner);
        }
    }
    fn member_shells(
        &self,
        module: usize,
        items: AstRange<ItemId>,
    ) -> Result<BTreeMap<String, Member>, Diagnostic> {
        let parsed = &self.modules[module];
        let mut members = BTreeMap::new();
        for &item in items.as_slice(&parsed.arena.item_ids) {
            if !parsed.configured.item_active(item) {
                continue;
            }
            let definition = DefRef { module, item };
            let node = &parsed.arena.items[item.0 as usize];
            let name = self
                .name(module, node.name.expect("关联项有名字"))
                .to_owned();
            let kind = match node.kind {
                ItemKind::TypeAlias { .. } => MemberKind::Type(None),
                ItemKind::Const { .. } => MemberKind::Const {
                    ty: Ty::Error,
                    value: None,
                },
                ItemKind::Function(_) => MemberKind::Method {
                    signature: Ty::Error,
                    receiver: false,
                    default: false,
                },
                _ => return Err(self.trait_error(definition, "不支持的关联项")),
            };
            if members
                .insert(
                    name,
                    Member {
                        definition: Some(definition),
                        kind,
                    },
                )
                .is_some()
            {
                return Err(self.trait_error(definition, "关联项不能重名"));
            }
        }
        Ok(members)
    }
    fn complete_members(&mut self, owner: Owner, types_only: bool) -> Result<(), Diagnostic> {
        let members = match owner {
            Owner::Interface(id) => &self.traits.interfaces[id].members,
            Owner::Implementation(id) => &self.traits.implementations[id].members,
        };
        let definitions: Vec<_> = members
            .iter()
            .filter_map(|(name, member)| member.definition.map(|d| (name.clone(), d)))
            .collect();
        for (name, definition) in definitions {
            let item = &self.modules[definition.module].arena.items[definition.item.0 as usize];
            let kind = match item.kind {
                ItemKind::TypeAlias { ty, generics }
                    if types_only || matches!(owner, Owner::Interface(_)) =>
                {
                    if generics.len != 0 {
                        return Err(self.trait_error(definition, "关联类型不能引入类型参数"));
                    }
                    MemberKind::Type(ty.map(|ty| self.form(definition.module, ty)).transpose()?)
                }
                ItemKind::Function(id) if !types_only => {
                    let f = &self.modules[definition.module].arena.fns[id.0 as usize];
                    let self_ty = self
                        .parameters_at(definition.module, &f.span)
                        .get("Self")
                        .cloned()
                        .expect("关联函数有 Self 上下文");
                    for (index, param) in f
                        .params
                        .as_slice(&self.modules[definition.module].arena.params)
                        .iter()
                        .enumerate()
                    {
                        if self.is_receiver(definition.module, param) {
                            let receiver_ty = param
                                .ty
                                .map(|ty| self.form(definition.module, ty))
                                .transpose()?
                                .unwrap_or_else(|| self_ty.clone());
                            if index != 0
                                || receiver_ty != self_ty
                                    && receiver_ty != Ty::Ref(Box::new(self_ty.clone()))
                            {
                                return Err(self.trait_error(
                                    definition,
                                    "self 必须为首参，类型只能是 Self 或 &Self",
                                ));
                            }
                        }
                    }
                    let receiver = f
                        .params
                        .as_slice(&self.modules[definition.module].arena.params)
                        .first()
                        .is_some_and(|p| self.is_receiver(definition.module, p));
                    let ty = self.value_type(definition)?;
                    let (params, ret) = ty.signature().expect("函数签名");
                    MemberKind::Method {
                        signature: Ty::Function(params.to_vec(), Box::new(ret.clone())),
                        receiver,
                        default: !matches!(f.body, FnBody::None),
                    }
                }
                ItemKind::Const { ty, value } if !types_only => {
                    let ty = ty
                        .map(|ty| self.form(definition.module, ty))
                        .transpose()?
                        .or_else(|| {
                            value
                                .and_then(|value| self.constant_type(definition.module, value).ok())
                        })
                        .ok_or_else(|| self.trait_error(definition, "关联常量需要可确定的类型"))?;
                    let value = value
                        .map(|value| self.constant_value(definition.module, value, &ty))
                        .transpose()?;
                    MemberKind::Const { ty, value }
                }
                _ => continue,
            };
            let members = match owner {
                Owner::Interface(id) => &mut self.traits.interfaces[id].members,
                Owner::Implementation(id) => &mut self.traits.implementations[id].members,
            };
            members.get_mut(&name).expect("已有成员").kind = kind;
        }
        Ok(())
    }
    fn implicit_parameters(
        &self,
        module: usize,
        id: TyId,
        argument: bool,
        params: &mut BTreeMap<String, Ty>,
    ) {
        let arena = &self.modules[module].arena;
        match arena.tys[id.0 as usize].kind {
            TyKind::Path(path) => self.implicit_path(module, path, argument, params),
            TyKind::Ref(ty)
            | TyKind::Ptr(ty)
            | TyKind::Slice(ty)
            | TyKind::Array { elem: ty, .. } => self.implicit_parameters(module, ty, true, params),
            TyKind::Tuple(types) => {
                for &ty in types.as_slice(&arena.ty_ids) {
                    self.implicit_parameters(module, ty, true, params);
                }
            }
            TyKind::Chan(args) => self.implicit_arguments(module, args, params),
            _ => {}
        }
    }
    fn implicit_arguments(
        &self,
        module: usize,
        args: AstRange<GenericArg>,
        params: &mut BTreeMap<String, Ty>,
    ) {
        let arena = &self.modules[module].arena;
        for arg in args.as_slice(&arena.generic_args) {
            match *arg {
                GenericArg::Type(ty) => self.implicit_parameters(module, ty, true, params),
                GenericArg::Expr(expr) => {
                    if let ExprKind::Path(path) = arena.exprs[expr.0 as usize].kind {
                        self.implicit_path(module, path, true, params);
                    }
                }
            }
        }
    }
    fn implicit_path(
        &self,
        module: usize,
        path: PathId,
        argument: bool,
        params: &mut BTreeMap<String, Ty>,
    ) {
        let parts = self.path(module, path);
        if argument
            && parts.len() == 1
            && Ty::primitive(parts[0]).is_none()
            && self.resolve(module, &parts).is_err()
            && !matches!(parts[0], "Option" | "Result" | "Join" | "Self")
        {
            params
                .entry(parts[0].to_owned())
                .or_insert_with(|| Ty::Param(parts[0].to_owned()));
        }
        let arena = &self.modules[module].arena;
        for segment in arena.paths[path.0 as usize]
            .segments
            .as_slice(&arena.segments)
        {
            self.implicit_arguments(module, segment.args, params);
        }
    }
    fn language_interfaces(&mut self) {
        let self_ty = Ty::Param("Self".into());
        let rhs = Ty::Param("Rhs".into());
        for (name, method) in [
            ("Add", "add"),
            ("Sub", "sub"),
            ("Mul", "mul"),
            ("Div", "div"),
            ("Rem", "rem"),
            ("BitAnd", "bitand"),
            ("BitOr", "bitor"),
            ("BitXor", "bitxor"),
            ("Shl", "shl"),
            ("Shr", "shr"),
        ] {
            let id = self.traits.interfaces.len();
            let output = Ty::Projection(
                Box::new(self_ty.clone()),
                TraitRef {
                    id,
                    arguments: vec![rhs.clone()],
                },
                "Output".into(),
            );
            self.add_language_interface(
                name,
                vec!["Rhs".into()],
                vec![
                    ("Output", MemberKind::Type(None)),
                    (
                        method,
                        method_kind(vec![self_ty.clone(), rhs.clone()], output),
                    ),
                ],
                false,
            );
            self.add_language_interface(
                &format!("{name}Assign"),
                vec!["Rhs".into()],
                vec![(
                    &format!("{method}_assign"),
                    method_kind(
                        vec![Ty::Ref(Box::new(self_ty.clone())), rhs.clone()],
                        Ty::Unit,
                    ),
                )],
                false,
            );
        }
        for (name, method, args, ret) in [
            (
                "Eq",
                "eq",
                vec![
                    Ty::Ref(Box::new(self_ty.clone())),
                    Ty::Ref(Box::new(self_ty.clone())),
                ],
                Ty::Bool,
            ),
            (
                "Ord",
                "cmp",
                vec![
                    Ty::Ref(Box::new(self_ty.clone())),
                    Ty::Ref(Box::new(self_ty.clone())),
                ],
                Ty::int(),
            ),
            (
                "Clone",
                "clone",
                vec![Ty::Ref(Box::new(self_ty.clone()))],
                self_ty.clone(),
            ),
        ] {
            self.add_language_interface(
                name,
                Vec::new(),
                vec![(method, method_kind(args, ret))],
                false,
            );
        }
        for name in ["StableHash", "StableOrd"] {
            self.add_language_interface(name, Vec::new(), Vec::new(), true);
        }
        self.add_language_interface("Fn", Vec::new(), Vec::new(), false);
        self.add_language_interface(
            "Any",
            Vec::new(),
            vec![(
                "type_of",
                method_kind(vec![Ty::Ref(Box::new(self_ty.clone()))], Ty::TypeId),
            )],
            false,
        );
        let id = self.traits.interfaces.len();
        let output = Ty::Projection(
            Box::new(self_ty.clone()),
            TraitRef {
                id,
                arguments: Vec::new(),
            },
            "Output".into(),
        );
        self.add_language_interface(
            "Index",
            Vec::new(),
            vec![
                ("Output", MemberKind::Type(None)),
                (
                    "index",
                    method_kind(
                        vec![Ty::Ref(Box::new(self_ty.clone())), Ty::int()],
                        output.clone(),
                    ),
                ),
                (
                    "index_set",
                    method_kind(
                        vec![Ty::Ref(Box::new(self_ty.clone())), Ty::int(), output],
                        Ty::Unit,
                    ),
                ),
            ],
            false,
        );
        let id = self.traits.interfaces.len();
        let projection = |name: &str| {
            Ty::Projection(
                Box::new(self_ty.clone()),
                TraitRef {
                    id,
                    arguments: Vec::new(),
                },
                name.into(),
            )
        };
        let value = projection("Value");
        let error = projection("Error");
        self.add_language_interface(
            "Try",
            Vec::new(),
            vec![
                ("Value", MemberKind::Type(None)),
                ("Error", MemberKind::Type(None)),
                (
                    "branch",
                    method_kind(
                        vec![self_ty.clone()],
                        Ty::Result(Box::new(value.clone()), Box::new(error.clone())),
                    ),
                ),
                (
                    "from_value",
                    MemberKind::Method {
                        signature: Ty::Function(vec![value], Box::new(self_ty.clone())),
                        receiver: false,
                        default: false,
                    },
                ),
                (
                    "from_error",
                    MemberKind::Method {
                        signature: Ty::Function(vec![error], Box::new(self_ty.clone())),
                        receiver: false,
                        default: false,
                    },
                ),
            ],
            false,
        );
        let id = self.traits.interfaces.len();
        let item = Ty::Projection(
            Box::new(self_ty.clone()),
            TraitRef {
                id,
                arguments: Vec::new(),
            },
            "Item".into(),
        );
        self.add_language_interface(
            "Iter",
            Vec::new(),
            vec![
                ("Item", MemberKind::Type(None)),
                (
                    "next",
                    method_kind(
                        vec![Ty::Ref(Box::new(self_ty.clone()))],
                        Ty::Option(Box::new(item)),
                    ),
                ),
            ],
            false,
        );
        let id = self.traits.interfaces.len();
        let iterator = Ty::Projection(
            Box::new(self_ty.clone()),
            TraitRef {
                id,
                arguments: Vec::new(),
            },
            "Iter".into(),
        );
        self.add_language_interface(
            "IntoIter",
            Vec::new(),
            vec![
                ("Item", MemberKind::Type(None)),
                ("Iter", MemberKind::Type(None)),
                (
                    "into_iter",
                    method_kind(vec![self_ty.clone()], iterator.clone()),
                ),
            ],
            false,
        );
        let iter_id = self
            .traits
            .interfaces
            .iter()
            .position(|interface| interface.name == "Iter" && interface.definition.is_none())
            .expect("Iter 已登记");
        let iter = TraitRef {
            id: iter_id,
            arguments: Vec::new(),
        };
        self.traits.interfaces[id]
            .requirements
            .push((iterator.clone(), iter.clone()));
        // IntoIter 的关联等式使泛型 for 与具体 for 使用相同的 Item 类型。
        self.traits.projection_equalities.push((
            Ty::Projection(Box::new(iterator), iter, "Item".into()),
            Ty::Projection(
                Box::new(self_ty),
                TraitRef {
                    id,
                    arguments: Vec::new(),
                },
                "Item".into(),
            ),
        ));
    }
    fn add_language_interface(
        &mut self,
        name: &str,
        parameters: Vec<String>,
        members: Vec<(&str, MemberKind)>,
        unsafety: bool,
    ) {
        self.traits.interfaces.push(Interface {
            definition: None,
            name: name.into(),
            parameters,
            unsafety,
            requirements: Vec::new(),
            members: members
                .into_iter()
                .map(|(name, kind)| {
                    (
                        name.into(),
                        Member {
                            definition: None,
                            kind,
                        },
                    )
                })
                .collect(),
        });
    }
}
fn method_kind(params: Vec<Ty>, ret: Ty) -> MemberKind {
    MemberKind::Method {
        signature: Ty::Function(params, Box::new(ret)),
        receiver: true,
        default: false,
    }
}
