//! HIR 是实例边的唯一 owner 来源；类型替换与 impl 选择仍复用语义层。

use super::keys::{MonoCallAbi, MonoContext, hash_domain};
use crate::frontend::hir;
use crate::frontend::semantics::{Model, TraitRef, Ty, substitute};
use crate::{Diagnostic, DiagnosticCode};
use std::collections::{BTreeMap, BTreeSet};

impl MonoContext<'_> {
    /// HIR TypeId 连续且子类型先驻留；每个类型只恢复一次语义工作值。
    pub(crate) fn semantic_type(&self, id: hir::TypeId) -> Result<&Ty, Diagnostic> {
        let index = id.index();
        debug_assert!(index < self.semantic_types.len());
        if let Some(ty) = self.semantic_types[index].get() {
            return Ok(ty);
        }
        let ty = self.restore_type(&self.module.types[index])?;
        self.semantic_types[index]
            .set(ty)
            .expect("类型表按子类型先行构造，不重入同一类型");
        Ok(self.semantic_types[index]
            .get()
            .expect("刚刚驻留的语义类型"))
    }

    fn restore_type(&self, ty: &hir::Type) -> Result<Ty, Diagnostic> {
        let boxed = |id| self.semantic_type(id).map(|ty| Box::new(ty.clone()));
        Ok(match ty {
            hir::Type::Never => Ty::Never,
            hir::Type::Unit => Ty::Unit,
            hir::Type::Bool => Ty::Bool,
            hir::Type::Char => Ty::Char,
            hir::Type::String => Ty::String,
            hir::Type::TypeId => Ty::TypeId,
            hir::Type::Range => Ty::Range,
            hir::Type::Int { signed, bits } => Ty::Int {
                signed: *signed,
                bits: *bits,
            },
            hir::Type::Float(bits) => Ty::Float(*bits),
            hir::Type::Ref(inner) => Ty::Ref(boxed(*inner)?),
            hir::Type::Ptr(inner) => Ty::Ptr(boxed(*inner)?),
            hir::Type::Slice(inner) => Ty::Slice(boxed(*inner)?),
            hir::Type::Array(inner, length) => Ty::Array(boxed(*inner)?, *length),
            hir::Type::Tuple(parts) => Ty::Tuple(self.semantic_types(parts)?),
            hir::Type::Function { parameters, result } => {
                Ty::Function(self.semantic_types(parameters)?, boxed(*result)?)
            }
            hir::Type::Callable {
                definition,
                arguments,
                signature,
            } => Ty::Callable(
                self.callable_of[definition.index()]
                    .ok_or_else(|| invalid("函数类型没有 callable 身份"))?,
                self.semantic_types(arguments)?,
                boxed(*signature)?,
            ),
            hir::Type::Named {
                definition,
                arguments,
            } => Ty::Named(
                self.nominal_of[definition.index()]
                    .ok_or_else(|| invalid("名义类型没有语义声明"))?,
                self.semantic_types(arguments)?,
            ),
            hir::Type::Parameter { owner, index } => {
                Ty::Param(self.parameter_name(*owner, *index)?)
            }
            hir::Type::Projection {
                self_ty,
                interface,
                member,
            } => {
                let interface = self.semantic_trait(interface)?;
                let name = self
                    .model
                    .interface_member_name(interface.id, *member)
                    .ok_or_else(|| invalid("关联类型缺少已选择成员"))?;
                Ty::Projection(boxed(*self_ty)?, interface, name)
            }
            hir::Type::Opaque {
                definition,
                arguments,
            } => Ty::Opaque(
                self.opaque_of[definition.index()]
                    .ok_or_else(|| invalid("不透明类型没有语义声明"))?,
                self.semantic_types(arguments)?,
            ),
            hir::Type::Dyn(interfaces) => Ty::Dyn(
                interfaces
                    .iter()
                    .map(|interface| self.semantic_trait(interface))
                    .collect::<Result<_, _>>()?,
            ),
            hir::Type::Option(inner) => Ty::Option(boxed(*inner)?),
            hir::Type::Result(value, error) => Ty::Result(boxed(*value)?, boxed(*error)?),
            hir::Type::Chan(inner) => Ty::Chan(boxed(*inner)?),
            hir::Type::Join(inner) => Ty::Join(boxed(*inner)?),
            hir::Type::MaybeUninit(inner) => Ty::MaybeUninit(boxed(*inner)?),
        })
    }

    fn semantic_types(&self, ids: &[hir::TypeId]) -> Result<Vec<Ty>, Diagnostic> {
        ids.iter()
            .map(|&id| self.semantic_type(id).cloned())
            .collect()
    }

    pub(crate) fn semantic_trait(&self, interface: &hir::TraitRef) -> Result<TraitRef, Diagnostic> {
        Ok(TraitRef {
            id: self.interface_of[interface.definition.index()]
                .ok_or_else(|| invalid("接口引用没有语义声明"))?,
            arguments: self.semantic_types(&interface.arguments)?,
        })
    }

    pub(crate) fn type_at(
        &self,
        id: hir::TypeId,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<Ty, Diagnostic> {
        self.model
            .normalize(&substitute(self.semantic_type(id)?, bindings), &[])
    }

    fn parameter_name(&self, owner: hir::DefId, index: u32) -> Result<String, Diagnostic> {
        let parameters = &self.module.definitions[owner.index()].parameters;
        let index = usize::try_from(index).expect("u32 参数编号适配目标宿主");
        let name = &parameters[index].name;
        if !name.starts_with("$apit:") {
            return Ok(name.clone());
        }
        let callable =
            self.callable_of[owner.index()].ok_or_else(|| invalid("APIT 缺少函数 owner"))?;
        let ordinal = parameters[..index]
            .iter()
            .filter(|parameter| parameter.name.starts_with("$apit:"))
            .count();
        self.model
            .apits(callable)
            .nth(ordinal)
            .map(Model::apit_name)
            .ok_or_else(|| invalid("APIT 的 HIR 参数与语义声明不一致"))
    }

    pub(crate) fn parameter_context(&self, definition: hir::DefId) -> BTreeMap<String, Ty> {
        if let Some(callable) = self.callable_of[definition.index()] {
            return self.model.callable_context(callable);
        }
        let item = &self.module.definitions[definition.index()];
        if matches!(
            item.kind,
            hir::DefinitionKind::Async | hir::DefinitionKind::LocalStatic
        ) && let Some(parent) = item.parent
        {
            return self.parameter_context(parent);
        }
        BTreeMap::new()
    }

    pub(crate) fn call_abi(&self, definition: hir::DefId) -> MonoCallAbi {
        let Some(callable) = self.callable_of[definition.index()] else {
            return MonoCallAbi::Gugu;
        };
        if self.model.modules[callable.module].arena.fns
            [usize::try_from(callable.function).expect("FnId 适配宿主")]
        .extern_abi
        .is_some()
        {
            MonoCallAbi::C
        } else {
            MonoCallAbi::Gugu
        }
    }

    pub(crate) fn selected_impls(
        &self,
        mut definition: hir::DefId,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<Vec<[u8; 32]>, Diagnostic> {
        let mut selected = BTreeSet::new();
        loop {
            let declaration = &self.module.definitions[definition.index()];
            if declaration.kind == hir::DefinitionKind::Impl {
                selected.insert(declaration.key);
            }
            for obligation in &declaration.obligations {
                if let hir::Obligation::Trait { ty, interface } = obligation {
                    let ty = self.type_at(*ty, bindings)?;
                    let interface = self.semantic_trait(interface)?.substitute(bindings);
                    if let Some((index, _)) = self.model.select_impl(&ty, &interface, &[])? {
                        selected.insert(
                            self.item_key(self.model.traits.implementations[index].definition),
                        );
                    }
                }
            }
            let Some(parent) = declaration.parent else {
                break;
            };
            definition = parent;
        }
        Ok(selected.into_iter().collect())
    }

    pub(crate) fn signature_bytes(
        &self,
        definition: hir::DefId,
        bindings: &BTreeMap<String, Ty>,
    ) -> Result<Vec<u8>, Diagnostic> {
        let signature = self.module.definitions[definition.index()].signature;
        let mut ty = signature.map_or(Ok(Ty::Unit), |id| self.type_at(id, bindings))?;
        if let Ty::Callable(_, _, signature) = ty {
            ty = *signature;
        }
        let mut bytes = self.encode_type(&ty)?;
        let abi = match self.call_abi(definition) {
            MonoCallAbi::Gugu => 0u16,
            MonoCallAbi::C => 1,
        };
        bytes.extend_from_slice(&abi.to_le_bytes());
        Ok(bytes)
    }

    pub(crate) fn body_fingerprint(&self, definition: hir::DefId) -> [u8; 32] {
        let Some(owner) = self.owner_of[definition.index()] else {
            return hash_domain("gugu-mono-body-v1", &[]);
        };
        let owner = &self.module.owners[owner];
        let location = &owner.scopes[0].location;
        let source =
            &self.sources.snapshots()[usize::try_from(location.source).expect("来源编号适配宿主")];
        let start = usize::try_from(location.start).expect("源码偏移适配宿主");
        let end = usize::try_from(location.end).expect("源码偏移适配宿主");
        let mut hash = blake3::Hasher::new();
        hash.update(b"gugu-mono-body-v1\0");
        hash.update(&hash_domain(
            "gugu-mono-source-v1",
            &source.bytes()[start..end],
        ));
        // 宏生成节点可能来自其它快照；按内容摘要排序，不记录 source ID 或位置偏移。
        let mut generated = BTreeSet::new();
        for expression in &owner.expressions {
            if expression.location.source != location.source {
                let source = &self.sources.snapshots()
                    [usize::try_from(expression.location.source).expect("来源编号适配宿主")];
                generated.insert(source.content_hash());
            }
        }
        for digest in generated {
            hash.update(&digest);
        }
        *hash.finalize().as_bytes()
    }
}

/// comptime 实参复用 evaluator 的规范值树，按 GBC1 编码而非 AST 或 ConstId。
pub(crate) fn encode_constant(
    context: &MonoContext<'_>,
    value: &crate::frontend::semantics::comptime::eval::ConstantValue,
) -> Result<Vec<u8>, Diagnostic> {
    use crate::frontend::semantics::comptime::eval::ConstantValue as Value;
    fn bytes(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(
            &u64::try_from(value.len())
                .expect("常量长度适配 GBC1")
                .to_le_bytes(),
        );
        out.extend_from_slice(value);
    }
    fn encode(
        context: &MonoContext<'_>,
        value: &Value,
        out: &mut Vec<u8>,
    ) -> Result<(), Diagnostic> {
        let tag: u16 = match value {
            Value::Unit => 0,
            Value::Int(_) => 1,
            Value::Float(_) => 2,
            Value::Bool(_) => 3,
            Value::String(_) => 4,
            Value::Array(_) => 5,
            Value::Tuple(_) => 6,
            Value::Struct(_) => 7,
            Value::Type(_) => 8,
            Value::ParsedSource(_) | Value::ResultOk(_) | Value::ResultErr(_) => {
                return Err(invalid("源码宏工作值不能成为单态化实参"));
            }
        };
        out.extend_from_slice(&tag.to_le_bytes());
        match value {
            Value::Unit => {}
            Value::Int(value) => out.extend_from_slice(&value.to_le_bytes()),
            Value::Float(bits) => out.extend_from_slice(&bits.to_le_bytes()),
            Value::Bool(value) => out.push(u8::from(*value)),
            Value::String(value) => bytes(out, value.as_bytes()),
            Value::Type(ty) => bytes(out, &context.encode_type(ty)?),
            Value::Array(values) | Value::Tuple(values) => {
                out.extend_from_slice(
                    &u64::try_from(values.len())
                        .expect("常量元素数适配 GBC1")
                        .to_le_bytes(),
                );
                for value in values {
                    encode(context, value, out)?;
                }
            }
            Value::Struct(fields) => {
                let mut fields: Vec<_> = fields
                    .iter()
                    .map(|(name, value)| {
                        let mut key = Vec::new();
                        bytes(&mut key, name.as_bytes());
                        (key, value)
                    })
                    .collect();
                fields.sort_unstable_by(|left, right| left.0.cmp(&right.0));
                out.extend_from_slice(
                    &u64::try_from(fields.len())
                        .expect("字段数适配 GBC1")
                        .to_le_bytes(),
                );
                for (name, value) in fields {
                    out.extend_from_slice(&name);
                    encode(context, value, out)?;
                }
            }
            Value::ParsedSource(_) | Value::ResultOk(_) | Value::ResultErr(_) => {
                unreachable!("前面已拒绝源码宏工作值")
            }
        }
        Ok(())
    }
    let mut bytes = Vec::new();
    encode(context, value, &mut bytes)?;
    Ok(bytes)
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
