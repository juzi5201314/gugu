use super::*;
use crate::{Diagnostic, DiagnosticCode};
mod body;

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::InvalidType,
        format!("HIR 冻结失败：{message}"),
        None,
    )
}

impl Module {
    pub(in crate::frontend) fn verify(&self) -> Result<(), Diagnostic> {
        if !self
            .sources
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
            || !self
                .definitions
                .windows(2)
                .all(|pair| pair[0].key < pair[1].key)
            || !self
                .owners
                .windows(2)
                .all(|pair| pair[0].definition < pair[1].definition)
        {
            return Err(invalid("源码、定义及 owner 身份不是规范唯一序列"));
        }
        for (index, expansion) in self.expansions.iter().enumerate() {
            if expansion.parent as usize > index
                || expansion.call.expansion != expansion.parent
                || !self.location(&expansion.call)
                || !self.location(&expansion.definition)
                || self
                    .sources
                    .get(expansion.source as usize)
                    .is_none_or(|source| source.hash != expansion.hash)
            {
                return Err(invalid("展开记录没有合法的父上下文及源码摘要"));
            }
        }
        // 每个定义只有一个父节点，三色状态使归属校验为 O(定义数)，不逐节点重走整条祖先链。
        for (index, definition) in self.definitions.iter().enumerate() {
            if definition.parent.is_some_and(|parent| !self.definition(parent) || parent.index() == index)
                || definition.location.as_ref().is_some_and(|location| !self.location(location))
                || definition.signature.is_some_and(|ty| !self.ty(ty))
                || definition.parameters.iter().any(|parameter| matches!(parameter.kind, ParameterKind::Comptime(ty) if !self.ty(ty)))
                || definition.obligations.iter().any(|bound| !self.obligation(bound)) {
                return Err(invalid("定义签名、位置或泛型约束越界"));
            }
        }
        self.verify_parents()?;
        for (index, ty) in self.types.iter().enumerate() {
            if !self.type_shape(ty, index) {
                return Err(invalid("驻留类型包含未形成引用或子类型循环"));
            }
        }
        self.verify_declarations()?;
        for owner in &self.owners {
            self.verify_owner(owner)?;
        }
        if let Some(entry) = self.entry {
            if !self.definition(entry)
                || self.definitions[entry.index()].kind != DefinitionKind::Function
                || !self.owners.iter().any(|owner| owner.definition == entry)
            {
                return Err(invalid("入口没有已验证函数 body"));
            }
        }
        for initialization in &self.initialization {
            if !self.definition(initialization.definition) {
                return Err(invalid("初始化顺序引用未知定义"));
            }
        }
        for linkage in &self.linkage {
            if !self.definition(linkage.definition)
                || linkage
                    .export_name
                    .iter()
                    .chain(&linkage.import_name)
                    .chain(&linkage.section)
                    .any(|name| name.is_empty() || name.contains('\0'))
            {
                return Err(invalid("链接记录没有有效的声明身份或名称"));
            }
        }
        Ok(())
    }

    fn verify_declarations(&self) -> Result<(), Diagnostic> {
        for aggregate in &self.aggregates {
            if !self.definition(aggregate.definition)
                || !matches!(
                    self.definitions[aggregate.definition.index()].kind,
                    DefinitionKind::Struct | DefinitionKind::Enum | DefinitionKind::Union
                )
                || aggregate.representation.flags & !7 != 0
                || !aggregate.representation.align.is_power_of_two()
                || aggregate
                    .variants
                    .iter()
                    .flat_map(|variant| &variant.fields)
                    .any(|field| !self.ty(field.ty))
            {
                return Err(invalid("聚合字段或布局属性不完整"));
            }
        }
        for interface in &self.interfaces {
            if !self.definition(interface.definition)
                || self.definitions[interface.definition.index()].kind != DefinitionKind::Trait
                || !self.members(&interface.members)
            {
                return Err(invalid("接口成员表不完整"));
            }
        }
        for implementation in &self.implementations {
            if !self.definition(implementation.definition)
                || !self.ty(implementation.self_ty)
                || implementation
                    .interface
                    .as_ref()
                    .is_some_and(|interface| !self.trait_ref(interface))
                || !self.members(&implementation.members)
            {
                return Err(invalid("实现头或关联成员不完整"));
            }
        }
        for opaque in &self.opaques {
            if !self.definition(opaque.definition) || opaque.hidden.is_some_and(|ty| !self.ty(ty)) {
                return Err(invalid("隐藏类型引用越界"));
            }
        }
        Ok(())
    }

    fn members(&self, members: &[Member]) -> bool {
        members.windows(2).all(|pair| pair[0].name < pair[1].name)
            && members.iter().all(|member| {
                member
                    .definition
                    .is_none_or(|definition| self.definition(definition))
                    && match &member.kind {
                        MemberKind::Method { signature, .. } => self.ty(*signature),
                        MemberKind::Type(ty) => ty.is_none_or(|ty| self.ty(ty)),
                        MemberKind::Constant { ty, .. } => self.ty(*ty),
                    }
            })
    }

    fn definition(&self, id: DefId) -> bool {
        id.index() < self.definitions.len()
    }
    fn ty(&self, id: TypeId) -> bool {
        id.index() < self.types.len()
    }
    fn location(&self, location: &Location) -> bool {
        location.start <= location.end
            && self
                .sources
                .get(location.source as usize)
                .is_some_and(|source| location.end <= source.length)
            && (location.expansion == 0
                || self
                    .expansions
                    .get(location.expansion as usize - 1)
                    .is_some_and(|expansion| expansion.source == location.source))
    }
    fn trait_ref(&self, interface: &TraitRef) -> bool {
        self.definition(interface.definition)
            && self.definitions[interface.definition.index()].kind == DefinitionKind::Trait
            && interface.arguments.iter().all(|&ty| self.ty(ty))
    }
    fn obligation(&self, obligation: &Obligation) -> bool {
        match obligation {
            Obligation::Trait { ty, interface } => self.ty(*ty) && self.trait_ref(interface),
            Obligation::Callable { ty, signature } => self.ty(*ty) && self.ty(*signature),
            Obligation::Equal { left, right } => self.ty(*left) && self.ty(*right),
        }
    }
    fn type_shape(&self, ty: &Type, index: usize) -> bool {
        let child = |ty: TypeId| ty.index() < index;
        let children = |types: &[TypeId]| types.iter().copied().all(child);
        match ty {
            Type::Never
            | Type::Unit
            | Type::Bool
            | Type::Char
            | Type::String
            | Type::TypeId
            | Type::Range => true,
            Type::Int { bits, .. } => matches!(bits, 8 | 16 | 32 | 64 | 128),
            Type::Float(bits) => matches!(bits, 32 | 64),
            Type::Ref(ty)
            | Type::Ptr(ty)
            | Type::Slice(ty)
            | Type::Array(ty, _)
            | Type::Option(ty)
            | Type::Chan(ty)
            | Type::Join(ty)
            | Type::MaybeUninit(ty) => child(*ty),
            Type::Tuple(types) => children(types),
            Type::Function { parameters, result } => children(parameters) && child(*result),
            Type::Callable {
                definition,
                arguments,
                signature,
            } => self.definition(*definition) && children(arguments) && child(*signature),
            Type::Named {
                definition,
                arguments,
            }
            | Type::Opaque {
                definition,
                arguments,
            } => self.definition(*definition) && children(arguments),
            Type::Parameter { owner, index } => self
                .definitions
                .get(owner.index())
                .is_some_and(|definition| (*index as usize) < definition.parameters.len()),
            Type::Projection {
                self_ty,
                interface,
                member,
            } => {
                child(*self_ty)
                    && self.trait_ref(interface)
                    && children(&interface.arguments)
                    && self
                        .interfaces
                        .iter()
                        .find(|entry| entry.definition == interface.definition)
                        .is_some_and(|entry| (*member as usize) < entry.members.len())
            }
            Type::Dyn(interfaces) => interfaces
                .iter()
                .all(|interface| self.trait_ref(interface) && children(&interface.arguments)),
            Type::Result(value, error) => child(*value) && child(*error),
        }
    }
}

impl Module {
    fn verify_parents(&self) -> Result<(), Diagnostic> {
        let mut states = vec![0u8; self.definitions.len()];
        let mut trail = Vec::new();
        for start in 0..states.len() {
            if states[start] == 2 {
                continue;
            }
            let mut next = Some(DefId(start as u32));
            while let Some(id) = next {
                match states[id.index()] {
                    2 => break,
                    1 => return Err(invalid("定义归属形成循环")),
                    _ => {}
                }
                states[id.index()] = 1;
                trail.push(id);
                next = self.definitions[id.index()].parent;
            }
            for id in trail.drain(..) {
                states[id.index()] = 2;
            }
        }
        Ok(())
    }
}
