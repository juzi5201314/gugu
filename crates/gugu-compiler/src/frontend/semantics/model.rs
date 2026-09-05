//! 语义类型和声明身份；名义下标只用于当前模块集合的稠密访问。
use super::super::{
    ParsedModule,
    ast::*,
    intern::Symbol,
    names::{NameResolution, ResolvedTarget},
};
use crate::{Diagnostic, DiagnosticCode};
use std::collections::BTreeMap;
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
    Named(usize, Vec<Ty>),
    Param(String),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Range,
    Chan(Box<Ty>),
    Join(Box<Ty>),
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
    pub(crate) fn primitive(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => Self::Bool,
            "char" => Self::Char,
            "string" => Self::String,
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
}
pub(crate) struct Model<'a> {
    pub(crate) modules: &'a [ParsedModule],
    pub(crate) nominal: Vec<Nominal>,
    names: &'a NameResolution,
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
            Ty::Ref(t) => format!("&{}", self.describe(t)),
            Ty::Ptr(t) => format!("*{}", self.describe(t)),
            Ty::Slice(t) => format!("[{}]", self.describe(t)),
            Ty::Array(t, n) => format!("[{}; {n}]", self.describe(t)),
            Ty::Tuple(ts) => format!("({}{})", list(ts), if ts.len() == 1 { "," } else { "" }),
            Ty::Function(ts, ret) => format!("fn({}) {}", list(ts), self.describe(ret)),
            Ty::Named(def, ts) => {
                let name = &self.nominal[*def].name;
                if ts.is_empty() {
                    name.clone()
                } else {
                    format!("{name}[{}]", list(ts))
                }
            }
            Ty::Param(name) => name.clone(),
            Ty::Range => "Range".into(),
            Ty::Option(t) => format!("Option[{}]", self.describe(t)),
            Ty::Result(t, e) => format!("Result[{}, {}]", self.describe(t), self.describe(e)),
            Ty::Chan(t) => format!("chan[{}]", self.describe(t)),
            Ty::Join(t) => format!("Join[{}]", self.describe(t)),
        }
    }
    pub(crate) fn new(
        modules: &'a [ParsedModule],
        names: &'a NameResolution,
    ) -> Result<Self, Vec<Diagnostic>> {
        let mut model = Self {
            modules,
            nominal: Vec::new(),
            names,
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
                model.nominal.push(Nominal {
                    definition: DefRef {
                        module,
                        item: ItemId(index as u32),
                    },
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
                    let params = generics
                        .as_slice(&parsed.arena.generic_params)
                        .iter()
                        .filter_map(|param| {
                            if let GenericParamKind::Type { name, .. } = param.kind {
                                let name = model.name(module, name).to_owned();
                                Some((name.clone(), Ty::Param(name)))
                            } else {
                                None
                            }
                        })
                        .collect();
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
    fn error(&self, module: usize, message: impl Into<String>) -> Diagnostic {
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
        let owner = arena
            .fns
            .iter()
            .find(|f| f.span.start() <= span.start() && f.span.end() >= span.end());
        let mut params = BTreeMap::new();
        if let Some(owner) = owner {
            for param in owner.generics.as_slice(&arena.generic_params) {
                if let GenericParamKind::Type { name, .. } = param.kind {
                    let name = self.name(module, name).to_owned();
                    params.insert(name.clone(), Ty::Param(name));
                }
            }
        }
        self.form_inner(module, id, &params, &mut Vec::new())
    }
    fn form_inner(
        &self,
        module: usize,
        id: TyId,
        params: &BTreeMap<String, Ty>,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        self.form_kind(
            module,
            self.modules[module].arena.tys[id.0 as usize].kind,
            params,
            stack,
        )
    }

    fn form_kind(
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
                if parts.len() == 1 {
                    if let Some(t) = params.get(parts[0]) {
                        return Ok(t.clone());
                    }
                    if let Some(t) = Ty::primitive(parts[0]) {
                        return Ok(t);
                    }
                }
                let args: Vec<_> = a.paths[path.0 as usize]
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
            _ => return Err(self.error(module, "该类型需要对应语义阶段形成")),
        })
    }

    fn form_arg(
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
                        p.ty.ok_or_else(|| self.error(def.module, "形参类型尚未推断"))
                            .and_then(|t| self.form(def.module, t))
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Ty::Function(
                    ps,
                    Box::new(
                        f.return_ty
                            .map(|t| self.form(def.module, t))
                            .transpose()?
                            .unwrap_or(Ty::Unit),
                    ),
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
    pub(crate) fn fields(&self, ty: &Ty) -> Option<Vec<FieldInfo>> {
        match ty.deref() {
            Ty::Named(i, _) if !self.nominal[*i].is_enum => {
                self.variants(ty).map(|mut vs| vs.remove(0).fields)
            }
            _ => None,
        }
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
    pub(crate) fn constant_type(&self, module: usize, expr: ExprId) -> Result<Ty, Diagnostic> {
        self.constant_type_inner(module, expr, &mut Vec::new())
    }

    fn constant_type_inner(
        &self,
        module: usize,
        expr: ExprId,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        match self.modules[module].arena.exprs[usize::try_from(expr.0).expect("表达式下标")].kind
        {
            ExprKind::Literal(LitKind::Int { .. }) => Ok(Ty::int()),
            ExprKind::Literal(LitKind::Char { .. }) => Ok(Ty::Char),
            ExprKind::Literal(LitKind::ByteChar { .. }) => Ok(Ty::Int {
                signed: false,
                bits: 8,
            }),
            ExprKind::Literal(LitKind::Bool(_)) => Ok(Ty::Bool),
            ExprKind::Literal(LitKind::Float { .. }) => Ok(Ty::Float(64)),
            ExprKind::Literal(LitKind::String { .. } | LitKind::RawString { .. }) => Ok(Ty::String),
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.constant_type_inner(module, inner, stack)
            }
            ExprKind::Binary { lhs, rhs, op } => {
                let left = self.constant_type_inner(module, lhs, stack)?;
                let right = self.constant_type_inner(module, rhs, stack)?;
                if left != right {
                    return Err(self.error(module, "常量操作数类型不一致"));
                }
                Ok(
                    if matches!(
                        op,
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                    ) {
                        Ty::Bool
                    } else {
                        left
                    },
                )
            }
            ExprKind::Path(path) => {
                let def = self.resolve(module, &self.path(module, path))?;
                if stack.contains(&def) {
                    return Err(self.error(module, "常量类型推断形成循环"));
                }
                match self.modules[def.module].arena.items
                    [usize::try_from(def.item.0).expect("项下标")]
                .kind
                {
                    ItemKind::Const { ty: Some(ty), .. } | ItemKind::Static { ty, .. } => {
                        self.form(def.module, ty)
                    }
                    ItemKind::Const {
                        value: Some(value), ..
                    } => {
                        stack.push(def);
                        let ty = self.constant_type_inner(def.module, value, stack);
                        stack.pop();
                        ty
                    }
                    _ => Err(self.error(module, "端点不是常量")),
                }
            }
            _ => Err(self.error(module, "无法形成常量类型")),
        }
    }
    pub(crate) fn constant_int(&self, module: usize, id: ExprId) -> Result<i128, Diagnostic> {
        self.constant(module, id, &mut Vec::new())
    }
    fn constant(
        &self,
        module: usize,
        id: ExprId,
        stack: &mut Vec<DefRef>,
    ) -> Result<i128, Diagnostic> {
        let a = &self.modules[module].arena;
        let fail = || self.error(module, "需要可求值且不溢出的整数常量");
        match a.exprs[id.0 as usize].kind {
            ExprKind::Literal(LitKind::Int { limbs, .. }) => {
                let mut n = 0i128;
                for &limb in limbs.as_slice(&a.int_limbs).iter().rev() {
                    n = n
                        .checked_mul(1i128 << 32)
                        .and_then(|n| n.checked_add(i128::from(limb)))
                        .ok_or_else(fail)?;
                }
                Ok(n)
            }
            ExprKind::Literal(LitKind::Char { value, .. }) => Ok(value as i128),
            ExprKind::Literal(LitKind::ByteChar { value, .. }) => Ok(value as i128),
            ExprKind::Paren(e) => self.constant(module, e, stack),
            ExprKind::Unary {
                op: UnOp::Neg,
                expr,
            } => self
                .constant(module, expr, stack)?
                .checked_neg()
                .ok_or_else(fail),
            ExprKind::Binary { op, lhs, rhs } => {
                let x = self.constant(module, lhs, stack)?;
                let y = self.constant(module, rhs, stack)?;
                match op {
                    BinOp::Add => x.checked_add(y),
                    BinOp::Sub => x.checked_sub(y),
                    BinOp::Mul => x.checked_mul(y),
                    BinOp::Div => x.checked_div(y),
                    BinOp::Rem => x.checked_rem(y),
                    _ => None,
                }
                .ok_or_else(fail)
            }
            ExprKind::Path(path) => {
                let def = self.resolve(module, &self.path(module, path))?;
                if stack.contains(&def) {
                    return Err(self.error(module, "const/static 初始化依赖形成循环"));
                }
                stack.push(def);
                let item = &self.modules[def.module].arena.items[def.item.0 as usize];
                let value = match item.kind {
                    ItemKind::Const { value: Some(v), .. } | ItemKind::Static { value: v, .. } => v,
                    _ => return Err(fail()),
                };
                let result = self.constant(def.module, value, stack);
                stack.pop();
                result
            }
            _ => Err(fail()),
        }
    }
}
fn substitute(ty: &Ty, bindings: &BTreeMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param(s) => bindings.get(s).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Ref(t) => Ty::Ref(Box::new(substitute(t, bindings))),
        Ty::Ptr(t) => Ty::Ptr(Box::new(substitute(t, bindings))),
        Ty::Array(t, n) => Ty::Array(Box::new(substitute(t, bindings)), *n),
        Ty::Slice(t) => Ty::Slice(Box::new(substitute(t, bindings))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| substitute(t, bindings)).collect()),
        Ty::Named(i, args) => Ty::Named(*i, args.iter().map(|t| substitute(t, bindings)).collect()),
        Ty::Option(t) => Ty::Option(Box::new(substitute(t, bindings))),
        Ty::Result(t, e) => Ty::Result(
            Box::new(substitute(t, bindings)),
            Box::new(substitute(e, bindings)),
        ),
        _ => ty.clone(),
    }
}
