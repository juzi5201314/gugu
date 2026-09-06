//! 语义类型和声明身份；名义下标只用于当前模块集合的稠密访问。
use super::super::{
    ParsedModule,
    ast::*,
    intern::Symbol,
    names::{NameResolution, ResolvedTarget},
};
use crate::{Diagnostic, DiagnosticCode};
use std::collections::BTreeMap;
mod attributes;
pub(crate) use attributes::Representation;
mod constants;
mod lang;
pub(super) use constants::ConstantValue;
pub(crate) use lang::MemoryIntrinsic;
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize)]
pub(crate) enum Ty {
    Error,
    Var(u32),
    Never,
    Unit,
    Bool,
    Int { signed: bool, bits: u16 },
    Float(u16),
    Char,
    String,
    Ref(Box<Ty>),
    Ptr(Box<Ty>),
    Slice(Box<Ty>),
    Array(Box<Ty>, u64),
    Tuple(Vec<Ty>),
    Function(Vec<Ty>, Box<Ty>),
    Callable(CallableId, Vec<Ty>, Box<Ty>),
    Named(usize, Vec<Ty>),
    Param(String),
    Projection(Box<Ty>, super::traits::TraitRef, String),
    Opaque(u32, Vec<Ty>),
    Dyn(Vec<super::traits::TraitRef>),
    TypeId,
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Range,
    Chan(Box<Ty>),
    Join(Box<Ty>),
    MaybeUninit(Box<Ty>),
}

/// 函数项和闭包的身份来自模块内稠密 FnDecl 编号，不使用地址。
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct CallableId {
    pub(crate) module: usize,
    pub(crate) function: u32,
}
impl Ty {
    pub(crate) fn int() -> Self {
        Self::Int {
            signed: true,
            bits: 64,
        }
    }
    pub(crate) fn deref(&self) -> &Self {
        match self {
            Self::Ref(t) => t.deref(),
            _ => self,
        }
    }
    pub(crate) fn signature(&self) -> Option<(&[Ty], &Ty)> {
        match self {
            Self::Function(params, ret) => Some((params, ret)),
            Self::Callable(_, _, signature) => signature.signature(),
            _ => None,
        }
    }
    pub(crate) fn primitive(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => Self::Bool,
            "char" => Self::Char,
            "string" => Self::String,
            "TypeId" => Self::TypeId,
            "int" | "i64" | "isize" => Self::int(),
            "uint" | "u64" | "usize" => Self::Int {
                signed: false,
                bits: 64,
            },
            "byte" | "u8" => Self::Int {
                signed: false,
                bits: 8,
            },
            "float" | "f64" => Self::Float(64),
            "f32" => Self::Float(32),
            "i8" => Self::Int {
                signed: true,
                bits: 8,
            },
            "i16" => Self::Int {
                signed: true,
                bits: 16,
            },
            "i32" => Self::Int {
                signed: true,
                bits: 32,
            },
            "i128" => Self::Int {
                signed: true,
                bits: 128,
            },
            "u16" => Self::Int {
                signed: false,
                bits: 16,
            },
            "u32" => Self::Int {
                signed: false,
                bits: 32,
            },
            "u128" => Self::Int {
                signed: false,
                bits: 128,
            },
            "Range" => Self::Range,
            _ => return None,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DefRef {
    pub(crate) module: usize,
    pub(crate) item: ItemId,
}
#[derive(Clone, Debug)]
pub(crate) struct FieldInfo {
    pub(crate) name: String,
    pub(crate) ty: Ty,
    pub(crate) public: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Constructor {
    pub(crate) name: String,
    pub(crate) fields: Vec<FieldInfo>,
    pub(crate) record: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Nominal {
    pub(crate) definition: DefRef,
    pub(crate) name: String,
    pub(crate) params: Vec<String>,
    pub(crate) variants: Vec<Constructor>,
    pub(crate) is_enum: bool,
    pub(crate) repr: Representation,
}
pub(crate) struct Model<'a> {
    pub(crate) modules: &'a [ParsedModule],
    pub(crate) nominal: Vec<Nominal>,
    pub(super) traits: super::traits::Traits,
    pub(super) opaques: super::opaque::Opaques,
    pub(super) foreign: super::foreign::Foreign,
    names: &'a NameResolution,
    // 模块/FnDecl 编号稠密；匿名闭包没有具名 ItemId，不参与函数地址的初始化依赖。
    function_items: Vec<Vec<Option<ItemId>>>,
}
impl<'a> Model<'a> {
    pub(super) fn name_fingerprint(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_derive_key("gugu-semantic-names-v1");
        for definition in &self.names.definitions {
            hash.update(&definition.stable_key);
        }
        for import in &self.names.imports {
            hash.update(
                &u64::try_from(import.module.index())
                    .expect("模块编号")
                    .to_le_bytes(),
            );
            hash.update(&(import.alias.len() as u64).to_le_bytes());
            hash.update(import.alias.as_bytes());
            hash.update(
                format!(
                    "{:?}:{:?}:{}",
                    import.namespace, import.target, import.public
                )
                .as_bytes(),
            );
        }
        *hash.finalize().as_bytes()
    }
    pub(super) fn describe(&self, ty: &Ty) -> String {
        let list = |types: &[Ty]| {
            types
                .iter()
                .map(|ty| self.describe(ty))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match ty {
            Ty::Error => "<错误类型>".into(),
            Ty::Var(_) => "_".into(),
            Ty::Never => "!".into(),
            Ty::Unit => "()".into(),
            Ty::Bool => "bool".into(),
            Ty::Int {
                signed: true,
                bits: 64,
            } => "int".into(),
            Ty::Int {
                signed: false,
                bits: 64,
            } => "uint".into(),
            Ty::Int { signed, bits } => format!("{}{bits}", if *signed { 'i' } else { 'u' }),
            Ty::Float(64) => "float".into(),
            Ty::Float(bits) => format!("f{bits}"),
            Ty::Char => "char".into(),
            Ty::String => "string".into(),
            Ty::TypeId => "TypeId".into(),
            Ty::Opaque(_, _) => "impl Trait".into(),
            Ty::Dyn(bounds) => format!(
                "dyn {}",
                bounds
                    .iter()
                    .map(|bound| self.traits.interfaces[bound.id].name.as_str())
                    .collect::<Vec<_>>()
                    .join(" + ")
            ),
            Ty::Ref(t) => format!("&{}", self.describe(t)),
            Ty::Ptr(t) => format!("*{}", self.describe(t)),
            Ty::MaybeUninit(t) => format!("MaybeUninit[{}]", self.describe(t)),
            Ty::Slice(t) => format!("[{}]", self.describe(t)),
            Ty::Array(t, n) => format!("[{}; {n}]", self.describe(t)),
            Ty::Tuple(ts) => format!("({}{})", list(ts), if ts.len() == 1 { "," } else { "" }),
            Ty::Function(ts, ret) => format!("fn({}) {}", list(ts), self.describe(ret)),
            Ty::Callable(id, arguments, signature) => {
                let function = &self.modules[id.module].arena.fns[id.function as usize];
                match function.name {
                    Some(name) => format!(
                        "函数项 {}[{}]: {}",
                        self.name(id.module, name),
                        list(arguments),
                        self.describe(signature)
                    ),
                    None => format!("闭包: {}", self.describe(signature)),
                }
            }
            Ty::Named(def, ts) => {
                let name = &self.nominal[*def].name;
                if ts.is_empty() {
                    name.clone()
                } else {
                    format!("{name}[{}]", list(ts))
                }
            }
            Ty::Param(name) => name.clone(),
            Ty::Projection(ty, interface, name) => format!(
                "<{} as {}>::{name}",
                self.describe(ty),
                self.traits.interfaces[interface.id].name
            ),
            Ty::Range => "Range".into(),
            Ty::Option(t) => format!("Option[{}]", self.describe(t)),
            Ty::Result(t, e) => format!("Result[{}, {}]", self.describe(t), self.describe(e)),
            Ty::Chan(t) => format!("chan[{}]", self.describe(t)),
            Ty::Join(t) => format!("Join[{}]", self.describe(t)),
        }
    }
    pub(super) fn function_definition(&self, id: CallableId) -> Option<DefRef> {
        self.function_items[id.module][id.function as usize].map(|item| DefRef {
            module: id.module,
            item,
        })
    }
    pub(crate) fn new(
        modules: &'a [ParsedModule],
        names: &'a NameResolution,
    ) -> Result<Self, Vec<Diagnostic>> {
        let mut model = Self {
            modules,
            nominal: Vec::new(),
            names,
            traits: super::traits::Traits::default(),
            opaques: super::opaque::Opaques::default(),
            foreign: super::foreign::Foreign::default(),
            function_items: modules
                .iter()
                .map(|module| {
                    let mut functions = vec![None; module.arena.fns.len()];
                    for (index, item) in module.arena.items.iter().enumerate() {
                        if let ItemKind::Function(function) = item.kind {
                            debug_assert!((function.0 as usize) < functions.len());
                            functions[function.0 as usize] = Some(ItemId(index as u32));
                        }
                    }
                    functions
                })
                .collect(),
        };
        for (module, m) in modules.iter().enumerate() {
            for (index, item) in m.arena.items.iter().enumerate() {
                if !m.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                let generics = match item.kind {
                    ItemKind::Struct { generics, .. }
                    | ItemKind::Enum { generics, .. }
                    | ItemKind::Union { generics, .. } => generics,
                    _ => continue,
                };
                let params = generics
                    .as_slice(&m.arena.generic_params)
                    .iter()
                    .map(|g| match g.kind {
                        GenericParamKind::Type { name, .. }
                        | GenericParamKind::Comptime { name, .. } => {
                            model.name(module, name).to_owned()
                        }
                    })
                    .collect();
                let definition = DefRef {
                    module,
                    item: ItemId(index as u32),
                };
                let repr = model
                    .representation(definition)
                    .map_err(|error| vec![error])?;
                model.nominal.push(Nominal {
                    definition,
                    repr,
                    name: item
                        .name
                        .map(|s| model.name(module, s).to_owned())
                        .unwrap_or_default(),
                    params,
                    variants: Vec::new(),
                    is_enum: matches!(item.kind, ItemKind::Enum { .. }),
                });
            }
        }
        model.collect_foreign()?;
        model.collect_opaques().map_err(|error| vec![error])?;
        model.collect_traits().map_err(|error| vec![error])?;
        model
            .validate_opaque_bounds()
            .map_err(|error| vec![error])?;
        for index in 0..model.nominal.len() {
            let def = model.nominal[index].definition;
            let m = &modules[def.module];
            let item = &m.arena.items[def.item.0 as usize];
            let params = model.nominal[index]
                .params
                .iter()
                .map(|s| (s.clone(), Ty::Param(s.clone())))
                .collect();
            let field = |f: &Field, pos: usize| -> Result<FieldInfo, Diagnostic> {
                Ok(FieldInfo {
                    name: f
                        .name
                        .map(|s| model.name(def.module, s).to_owned())
                        .unwrap_or_else(|| pos.to_string()),
                    ty: model.form_inner(def.module, f.ty, &params, &mut Vec::new())?,
                    public: f.visibility == Visibility::Pub,
                })
            };
            let fields = |range: AstRange<Field>| -> Result<Vec<FieldInfo>, Diagnostic> {
                range
                    .as_slice(&m.arena.fields)
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| m.configured.field_active(range.start as usize + i))
                    .map(|(i, f)| field(f, i))
                    .collect()
            };
            let variants: Result<Vec<Constructor>, Diagnostic> = match &item.kind {
                ItemKind::Struct {
                    body: StructBody::Record(range),
                    ..
                }
                | ItemKind::Union { fields: range, .. } => fields(*range).map(|fields| {
                    vec![Constructor {
                        name: model.nominal[index].name.clone(),
                        fields,
                        record: true,
                    }]
                }),
                ItemKind::Struct {
                    body: StructBody::Newtype(f),
                    ..
                } => field(f, 0).map(|f| {
                    vec![Constructor {
                        name: model.nominal[index].name.clone(),
                        fields: vec![f],
                        record: false,
                    }]
                }),
                ItemKind::Enum { variants, .. } => variants
                    .as_slice(&m.arena.variants)
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| m.configured.variant_active(variants.start as usize + i))
                    .map(|(_, v)| {
                        Ok(Constructor {
                            name: model.name(def.module, v.name).to_owned(),
                            fields: match v.kind {
                                VariantKind::Unit => Vec::new(),
                                VariantKind::Tuple(r) | VariantKind::Struct(r) => fields(r)?,
                            },
                            record: matches!(v.kind, VariantKind::Struct(_)),
                        })
                    })
                    .collect(),
                _ => unreachable!(),
            };
            model.nominal[index].variants = variants.map_err(|e| vec![e])?;
        }
        for (module, parsed) in modules.iter().enumerate() {
            for (index, item) in parsed.arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                if let ItemKind::TypeAlias {
                    generics,
                    ty: Some(ty),
                } = item.kind
                {
                    let mut params = model.parameters_at(module, &item.span);
                    params.extend(model.generic_parameters(module, generics));
                    model
                        .form_inner(
                            module,
                            ty,
                            &params,
                            &mut vec![DefRef {
                                module,
                                item: ItemId(index as u32),
                            }],
                        )
                        .map_err(|error| vec![error])?;
                }
            }
        }
        model.validate_traits().map_err(|error| vec![error])?;
        Ok(model)
    }
    pub(crate) fn name(&self, module: usize, symbol: Symbol) -> &str {
        self.modules[module].tokens.intern.get_str(symbol)
    }
    pub(crate) fn path(&self, module: usize, path: PathId) -> Vec<&str> {
        let a = &self.modules[module].arena;
        a.paths[path.0 as usize]
            .segments
            .as_slice(&a.segments)
            .iter()
            .map(|s| self.name(module, s.name))
            .collect()
    }
    pub(super) fn error(&self, module: usize, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(
            DiagnosticCode::InvalidType,
            message,
            Some(self.modules[module].file.eof_span.clone()),
        )
    }
    pub(crate) fn resolve(&self, module: usize, path: &[&str]) -> Result<DefRef, Diagnostic> {
        let Some(first) = path.first() else {
            return Err(self.error(module, "空名称路径"));
        };
        if path.len() == 1 {
            for (i, item) in self.modules[module].arena.items.iter().enumerate() {
                if self.modules[module]
                    .configured
                    .item_active(ItemId(i as u32))
                    && (matches!(item.kind, ItemKind::Trait { .. })
                        || self
                            .traits
                            .owners
                            .get(module)
                            .and_then(|owners| owners.get(i))
                            .copied()
                            .flatten()
                            .is_none())
                    && item.name.is_some_and(|s| self.name(module, s) == *first)
                {
                    return Ok(DefRef {
                        module,
                        item: ItemId(i as u32),
                    });
                }
            }
        }
        for import in &self.names.imports {
            if import.module.index() != module || import.alias != *first {
                continue;
            }
            match import.target {
                ResolvedTarget::Module(target) if path.len() > 1 => {
                    let def = self.resolve(target.index(), &path[1..])?;
                    if self.modules[def.module].arena.items[def.item.0 as usize].visibility
                        != Visibility::Pub
                    {
                        return Err(self.error(module, "不能访问私有声明"));
                    }
                    return Ok(def);
                }
                ResolvedTarget::Def(id) if path.len() == 1 => {
                    let d = &self.names.definitions[id.index()];
                    if let Some((i, _)) = self.modules[d.module.index()]
                        .arena
                        .items
                        .iter()
                        .enumerate()
                        .find(|(_, i)| i.span == d.span)
                    {
                        return Ok(DefRef {
                            module: d.module.index(),
                            item: ItemId(i as u32),
                        });
                    }
                }
                _ => {}
            }
        }
        Err(self.error(module, format!("未解析名称 `{}`", path.join("."))))
    }
    pub(crate) fn form(&self, module: usize, id: TyId) -> Result<Ty, Diagnostic> {
        let arena = &self.modules[module].arena;
        let span = &arena.tys[usize::try_from(id.0).expect("类型下标")].span;
        let params = self.parameters_at(module, span);
        self.form_inner(module, id, &params, &mut Vec::new())
    }

    pub(super) fn parameters_at(&self, module: usize, span: &crate::Span) -> BTreeMap<String, Ty> {
        let arena = &self.modules[module].arena;
        let mut params = self.associated_scope(module, span);
        // parser 先分配内层 FnDecl，逆序遍历使内层同名参数覆盖外层。
        let owners = arena.fns.iter().enumerate().rev().filter(|(_, function)| {
            function.span.start() <= span.start() && function.span.end() >= span.end()
        });
        for (index, owner) in owners {
            for id in self.apits(CallableId {
                module,
                function: index as u32,
            }) {
                let name = Self::apit_name(id);
                params.insert(name.clone(), Ty::Param(name));
            }
            for param in owner.generics.as_slice(&arena.generic_params) {
                if let GenericParamKind::Type { name, .. } = param.kind {
                    let name = self.name(module, name).to_owned();
                    params.insert(name.clone(), Ty::Param(name));
                }
            }
        }
        params
    }

    pub(super) fn callable_context(&self, id: CallableId) -> BTreeMap<String, Ty> {
        let function = &self.modules[id.module].arena.fns[id.function as usize];
        let mut context = self.parameters_at(id.module, &function.span);
        // 关联类型路径是由 Self 推导的查找缓存，不构成独立实例参数。
        context.retain(|name, _| !name.starts_with("Self::"));
        context
    }

    pub(super) fn form_argument(
        &self,
        module: usize,
        argument: GenericArg,
    ) -> Result<Ty, Diagnostic> {
        match argument {
            GenericArg::Type(ty) => self.form(module, ty),
            GenericArg::Expr(expression) => {
                let expression = &self.modules[module].arena.exprs[expression.0 as usize];
                let ExprKind::Path(path) = expression.kind else {
                    return Err(self.error(module, "泛型位置要求类型实参"));
                };
                self.form_kind(
                    module,
                    TyKind::Path(path),
                    &self.parameters_at(module, &expression.span),
                    &mut Vec::new(),
                )
            }
        }
    }
    pub(super) fn form_inner(
        &self,
        module: usize,
        id: TyId,
        params: &BTreeMap<String, Ty>,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        if matches!(
            self.modules[module].arena.tys[id.0 as usize].kind,
            TyKind::Impl(_)
        ) {
            return self.form_opaque(module, id, params);
        }
        self.form_kind(
            module,
            self.modules[module].arena.tys[id.0 as usize].kind,
            params,
            stack,
        )
    }

    pub(super) fn form_kind(
        &self,
        module: usize,
        kind: TyKind,
        params: &BTreeMap<String, Ty>,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        let a = &self.modules[module].arena;
        Ok(match kind {
            TyKind::Never => Ty::Never,
            TyKind::Infer | TyKind::Error => return Err(self.error(module, "类型尚未唯一收敛")),
            TyKind::Ref(t) => Ty::Ref(Box::new(self.form_inner(module, t, params, stack)?)),
            TyKind::Ptr(t) => Ty::Ptr(Box::new(self.form_inner(module, t, params, stack)?)),
            TyKind::Slice(t) => Ty::Ref(Box::new(Ty::Slice(Box::new(
                self.form_inner(module, t, params, stack)?,
            )))),
            TyKind::Array { elem, len } => Ty::Array(
                Box::new(self.form_inner(module, elem, params, stack)?),
                u64::try_from(self.constant_int(module, len)?)
                    .map_err(|_| self.error(module, "数组长度必须非负"))?,
            ),
            TyKind::Tuple(ts) => {
                let ts: Vec<_> = ts
                    .as_slice(&a.ty_ids)
                    .iter()
                    .map(|&t| self.form_inner(module, t, params, stack))
                    .collect::<Result<_, _>>()?;
                if ts.is_empty() {
                    Ty::Unit
                } else {
                    Ty::Tuple(ts)
                }
            }
            TyKind::Fn { params: ps, ret } => Ty::Function(
                ps.as_slice(&a.ty_ids)
                    .iter()
                    .map(|&t| self.form_inner(module, t, params, stack))
                    .collect::<Result<_, _>>()?,
                Box::new(
                    ret.map(|t| self.form_inner(module, t, params, stack))
                        .transpose()?
                        .unwrap_or(Ty::Unit),
                ),
            ),
            TyKind::Path(path) => {
                let parts = self.path(module, path);
                if let Some(ty) = params.get(&parts.join("::")) {
                    return Ok(ty.clone());
                }
                if let Some(ty) = self.projected_type(module, path, params, stack)? {
                    return Ok(ty);
                }
                if parts.len() == 1 {
                    if let Some(t) = params.get(parts[0]) {
                        return Ok(t.clone());
                    }
                    if let Some(t) = Ty::primitive(parts[0]) {
                        return Ok(t);
                    }
                }
                let args: Vec<_> = a.paths[path.0 as usize]
                    .segments
                    .as_slice(&a.segments)
                    .last()
                    .expect("路径至少有一段")
                    .args
                    .as_slice(&a.generic_args)
                    .iter()
                    .map(|arg| self.form_arg(module, *arg, params, stack))
                    .collect::<Result<_, _>>()?;
                match (parts.as_slice(), args.as_slice()) {
                    (["Option"], [t]) => return Ok(Ty::Option(Box::new(t.clone()))),
                    (["Result"], [t, e]) => {
                        return Ok(Ty::Result(Box::new(t.clone()), Box::new(e.clone())));
                    }
                    (["Join"], [t]) => return Ok(Ty::Join(Box::new(t.clone()))),
                    _ => {}
                }
                if self.external_path(module, &parts).as_deref() == Some("std.mem.MaybeUninit") {
                    let [inner] = args.as_slice() else {
                        return Err(self.error(module, "MaybeUninit 需要一个类型实参"));
                    };
                    return Ok(Ty::MaybeUninit(Box::new(inner.clone())));
                }
                let def = self.resolve(module, &parts)?;
                if let Some((i, n)) = self
                    .nominal
                    .iter()
                    .enumerate()
                    .find(|(_, n)| n.definition == def)
                {
                    if args.len() != n.params.len() {
                        return Err(self.error(module, "泛型实参数量不符"));
                    }
                    Ty::Named(i, args)
                } else {
                    if stack.contains(&def) {
                        return Err(self.error(module, "透明别名形成循环"));
                    }
                    stack.push(def);
                    let ItemKind::TypeAlias {
                        ty: Some(ty),
                        generics,
                    } = self.modules[def.module].arena.items[def.item.0 as usize].kind
                    else {
                        return Err(self.error(module, "该声明不是类型"));
                    };
                    let mut bound = BTreeMap::new();
                    let gs = generics.as_slice(&self.modules[def.module].arena.generic_params);
                    if gs.len() != args.len() {
                        return Err(self.error(module, "别名实参数量不符"));
                    }
                    for (g, t) in gs.iter().zip(args) {
                        if let GenericParamKind::Type { name, .. } = g.kind {
                            bound.insert(self.name(def.module, name).to_owned(), t);
                        }
                    }
                    let ty = self.form_inner(def.module, ty, &bound, stack)?;
                    stack.pop();
                    ty
                }
            }
            TyKind::Chan(args) => {
                let [arg] = args.as_slice(&a.generic_args) else {
                    return Err(self.error(module, "chan 需要一个元素类型"));
                };
                Ty::Chan(Box::new(self.form_arg(module, *arg, params, stack)?))
            }
            TyKind::Dyn(paths) => self.form_dyn(module, paths, params)?,
            _ => return Err(self.error(module, "该类型需要对应语义阶段形成")),
        })
    }

    pub(super) fn form_arg(
        &self,
        module: usize,
        arg: GenericArg,
        params: &BTreeMap<String, Ty>,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        match arg {
            GenericArg::Type(t) => self.form_inner(module, t, params, stack),
            GenericArg::Expr(expr) => {
                match self.modules[module].arena.exprs[expr.0 as usize].kind {
                    ExprKind::Path(path) => {
                        self.form_kind(module, TyKind::Path(path), params, stack)
                    }
                    _ => Err(self.error(module, "此泛型位置需要类型")),
                }
            }
        }
    }
    pub(crate) fn value_type(&self, def: DefRef) -> Result<Ty, Diagnostic> {
        let m = &self.modules[def.module];
        match m.arena.items[def.item.0 as usize].kind {
            ItemKind::Function(id) => {
                let f = &m.arena.fns[id.0 as usize];
                let ps = f
                    .params
                    .as_slice(&m.arena.params)
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| m.configured.param_active(f.params.start as usize + i))
                    .map(|(_, p)| {
                        if let Some(ty) = p.ty {
                            self.form(def.module, ty)
                        } else if self.is_receiver(def.module, p) {
                            self.parameters_at(def.module, &f.span)
                                .get("Self")
                                .cloned()
                                .ok_or_else(|| self.error(def.module, "self 只能用于关联方法"))
                        } else {
                            Err(self.error(def.module, "形参类型尚未推断"))
                        }
                    })
                    .collect::<Result<_, _>>()?;
                let arguments = self
                    .callable_context(CallableId {
                        module: def.module,
                        function: id.0,
                    })
                    .into_values()
                    .collect();
                Ok(Ty::Callable(
                    CallableId {
                        module: def.module,
                        function: id.0,
                    },
                    arguments,
                    Box::new(Ty::Function(
                        ps,
                        Box::new(
                            f.return_ty
                                .map(|ty| self.form(def.module, ty))
                                .transpose()?
                                .unwrap_or(Ty::Unit),
                        ),
                    )),
                ))
            }
            ItemKind::Static { ty, .. } | ItemKind::Const { ty: Some(ty), .. } => {
                self.form(def.module, ty)
            }
            ItemKind::Const {
                value: Some(expr), ..
            } => self.constant_type(def.module, expr),
            _ => Err(self.error(def.module, "声明不是值")),
        }
    }
    pub(crate) fn variants(&self, ty: &Ty) -> Option<Vec<Constructor>> {
        let field = |t: Ty| FieldInfo {
            name: "0".into(),
            ty: t,
            public: true,
        };
        match ty.deref() {
            Ty::Named(i, args) => {
                let n = &self.nominal[*i];
                let bindings = n.params.iter().cloned().zip(args.iter().cloned()).collect();
                Some(
                    n.variants
                        .iter()
                        .map(|v| Constructor {
                            name: v.name.clone(),
                            record: v.record,
                            fields: v
                                .fields
                                .iter()
                                .map(|f| FieldInfo {
                                    name: f.name.clone(),
                                    ty: substitute(&f.ty, &bindings),
                                    public: f.public,
                                })
                                .collect(),
                        })
                        .collect(),
                )
            }
            Ty::Option(t) => Some(vec![
                Constructor {
                    name: "Some".into(),
                    fields: vec![field((**t).clone())],
                    record: false,
                },
                Constructor {
                    name: "None".into(),
                    fields: vec![],
                    record: false,
                },
            ]),
            Ty::Result(t, e) => Some(vec![
                Constructor {
                    name: "Ok".into(),
                    fields: vec![field((**t).clone())],
                    record: false,
                },
                Constructor {
                    name: "Err".into(),
                    fields: vec![field((**e).clone())],
                    record: false,
                },
            ]),
            _ => None,
        }
    }
    pub(crate) fn find_field(&self, ty: &Ty, name: &str) -> Option<(usize, Ty, bool)> {
        let Ty::Named(index, arguments) = ty.deref() else {
            return None;
        };
        let nominal = &self.nominal[*index];
        if nominal.is_enum {
            return None;
        }
        let (index, field) = nominal.variants[0]
            .fields
            .iter()
            .enumerate()
            .find(|(_, field)| field.name == name)?;
        let bindings = nominal
            .params
            .iter()
            .cloned()
            .zip(arguments.iter().cloned())
            .collect();
        Some((index, substitute(&field.ty, &bindings), field.public))
    }
    pub(crate) fn def_module(&self, ty: &Ty) -> Option<usize> {
        match ty.deref() {
            Ty::Named(i, _) => Some(self.nominal[*i].definition.module),
            _ => None,
        }
    }
    pub(crate) fn constructor(
        &self,
        module: usize,
        path: &[&str],
        expected: Option<&Ty>,
    ) -> Result<Option<(Ty, Constructor)>, Diagnostic> {
        let Some(name) = path.last() else {
            return Ok(None);
        };
        let qualified = if path.len() > 1 {
            self.resolve(module, &path[..path.len() - 1]).ok()
        } else {
            None
        };
        let whole = self.resolve(module, path).ok();
        let mut candidates = Vec::new();
        if let Some(ty) = expected {
            let visible = match ty.deref() {
                Ty::Named(index, _) => {
                    let nominal = &self.nominal[*index];
                    if nominal.is_enum {
                        path.len() == 1
                            && (nominal.definition.module == module
                                || self.resolve(module, &[nominal.name.as_str()]).ok()
                                    == Some(nominal.definition))
                            || qualified == Some(nominal.definition)
                    } else {
                        whole == Some(nominal.definition)
                    }
                }
                Ty::Option(_) => path.len() == 1 || path == ["Option", *name],
                Ty::Result(_, _) => path.len() == 1 || path == ["Result", *name],
                _ => false,
            };
            if visible {
                if let Some(variant) = self
                    .variants(ty)
                    .and_then(|vs| vs.into_iter().find(|v| v.name == *name))
                {
                    return Ok(Some((ty.deref().clone(), variant)));
                }
            }
        }
        for (index, nominal) in self.nominal.iter().enumerate() {
            let visible = if nominal.is_enum {
                qualified == Some(nominal.definition)
                    || path.len() == 1
                        && (nominal.definition.module == module
                            || self.resolve(module, &[nominal.name.as_str()]).ok()
                                == Some(nominal.definition))
            } else {
                whole == Some(nominal.definition)
            };
            if !visible || !nominal.params.is_empty() {
                continue;
            }
            if let Some(variant) = nominal
                .variants
                .iter()
                .find(|v| v.name == *name || !nominal.is_enum)
            {
                candidates.push((Ty::Named(index, Vec::new()), variant.clone()));
            }
        }
        match candidates.len() {
            0 => Ok(None),
            1 => Ok(candidates.pop()),
            _ => Err(self.error(module, "构造器名称不唯一，必须使用类型限定")),
        }
    }
}
pub(super) fn substitute(ty: &Ty, bindings: &BTreeMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param(s) => bindings.get(s).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Projection(ty, interface, name) => Ty::Projection(
            Box::new(substitute(ty, bindings)),
            interface.substitute(bindings),
            name.clone(),
        ),
        Ty::Ref(t) => Ty::Ref(Box::new(substitute(t, bindings))),
        Ty::Ptr(t) => Ty::Ptr(Box::new(substitute(t, bindings))),
        Ty::Array(t, n) => Ty::Array(Box::new(substitute(t, bindings)), *n),
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
