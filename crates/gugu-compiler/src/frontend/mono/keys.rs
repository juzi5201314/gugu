//! `StableTypeKey`、`MonoKey` 的规范编码与 interner。
//!
//! 编码遵循 GBC1：小端定宽整数、`u16` 枚举 tag、长度前缀序列；全部经
//! `gugu-mono-v1` 域哈希。session-local 的 `DefId`/`TyId` 不进入编码，名义类型只
//! 携带 `Definition.key` 稳定键。

use crate::{
    Diagnostic, DiagnosticCode,
    frontend::{
        hir::{self, Module},
        semantics::{CallableId, DefRef, Identities, Model, TraitRef, Ty},
    },
    target::TargetName,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// 调用 ABI：Gugu 内部或 C。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum MonoCallAbi {
    Gugu,
    C,
}

/// 实例化的定义种类；与 HIR `DefinitionKind` 对应，仅供计划与摘要投影使用，
/// 不进入 `MonoKey`（definition 稳定键本身已区分种类）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum MonoKind {
    Function,
    Closure,
    Async,
    StaticInit,
    GlobalAsm,
}

/// 单态化实例的规范身份；字段与规范一一对应。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MonoKey {
    pub definition: [u8; 32],
    pub type_arguments: Vec<Vec<u8>>,
    pub const_arguments: Vec<Vec<u8>>,
    pub selected_impls: Vec<[u8; 32]>,
    pub call_abi: MonoCallAbi,
    pub target: String,
    pub harness_mode: bool,
    pub instrumentation_mode: u32,
}

impl MonoKey {
    /// GBC1 规范字节；相同字段值必须逐字节一致。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.definition);
        encode_byte_sequences(&mut out, &self.type_arguments);
        encode_byte_sequences(&mut out, &self.const_arguments);
        out.extend_from_slice(
            &u64::try_from(self.selected_impls.len())
                .unwrap()
                .to_le_bytes(),
        );
        for key in &self.selected_impls {
            out.extend_from_slice(key);
        }
        let abi = match self.call_abi {
            MonoCallAbi::Gugu => 0u8,
            MonoCallAbi::C => 1,
        };
        out.push(abi);
        encode_string(&mut out, &self.target);
        out.push(u8::from(self.harness_mode));
        out.extend_from_slice(&self.instrumentation_mode.to_le_bytes());
        out
    }

    /// `gugu-mono-v1` 域摘要。
    pub(crate) fn digest(&self) -> [u8; 32] {
        hash_domain("gugu-mono-v1", &self.canonical_bytes())
    }
}

/// 稳定类型键：类型规范字节的 `gugu-mono-v1` 域摘要。
pub(crate) type StableTypeKey = [u8; 32];

/// 摘要 interner：digest -> 规范字节；同摘要不同字节是内部错误，不得合并。
#[derive(Default)]
pub(crate) struct MonoInterner {
    keys: HashMap<[u8; 32], Vec<u8>>,
    types: HashMap<StableTypeKey, Vec<u8>>,
}

impl MonoInterner {
    /// 登记 `MonoKey`，返回域摘要；冲突立即报错停止。
    pub(crate) fn intern_key(&mut self, key: &MonoKey) -> Result<[u8; 32], Diagnostic> {
        let canonical = key.canonical_bytes();
        let digest = key.digest();
        intern(&mut self.keys, digest, canonical, "MonoKey")
    }

    /// 登记规范类型字节，返回稳定类型键。
    pub(crate) fn intern_type(&mut self, canonical: Vec<u8>) -> Result<StableTypeKey, Diagnostic> {
        let digest = hash_domain("gugu-mono-v1", &canonical);
        intern(&mut self.types, digest, canonical, "StableTypeKey")
    }
}

fn intern(
    table: &mut HashMap<[u8; 32], Vec<u8>>,
    digest: [u8; 32],
    canonical: Vec<u8>,
    kind: &str,
) -> Result<[u8; 32], Diagnostic> {
    match table.get(&digest) {
        Some(existing) if *existing == canonical => Ok(digest),
        Some(_) => Err(Diagnostic::error(
            DiagnosticCode::DefinitionHashCollision,
            format!("{kind} 摘要冲突：相同 digest 的规范字节不同"),
            None,
        )),
        None => {
            table.insert(digest, canonical);
            Ok(digest)
        }
    }
}

pub(crate) fn hash_domain(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

fn encode_string(out: &mut Vec<u8>, value: &str) {
    encode_u64(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn encode_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn encode_byte_sequences(out: &mut Vec<u8>, sequences: &[Vec<u8>]) {
    encode_u64(out, sequences.len() as u64);
    for sequence in sequences {
        encode_u64(out, sequence.len() as u64);
        out.extend_from_slice(sequence);
    }
}

/// 类型编码与单态化收集共享的只读上下文。
pub(crate) struct MonoContext<'a> {
    pub model: &'a Model<'a>,
    pub checked: &'a crate::frontend::semantics::CheckedSemantics,
    pub identities: &'a Identities,
    pub module: &'a Module,
    pub target: TargetName,
    pub harness: bool,
    /// DefId -> `CheckedBody` 下标；闭包/协程/外部声明为 `None`。
    pub body_of: Vec<Option<usize>>,
    /// DefId -> 声明 `DefRef`；匿名/合成定义为 `None`。
    pub definition_ref: Vec<Option<DefRef>>,
    /// DefId -> 源模块下标。
    pub definition_module: Vec<usize>,
}

impl<'a> MonoContext<'a> {
    pub(crate) fn new(
        model: &'a Model<'a>,
        checked: &'a crate::frontend::semantics::CheckedSemantics,
        identities: &'a Identities,
        module: &'a Module,
        target: TargetName,
        harness: bool,
    ) -> Self {
        let mut body_of = vec![None; module.definitions.len()];
        let mut definition_ref = vec![None; module.definitions.len()];
        let mut definition_module = vec![0usize; module.definitions.len()];
        for (module_index, table) in identities.items.iter().enumerate() {
            for (item, definition) in table.iter().enumerate() {
                if let Some(definition) = definition {
                    let index = definition.index();
                    definition_module[index] = module_index;
                    definition_ref[index] = Some(DefRef {
                        module: module_index,
                        item: crate::frontend::ast::ItemId(item as u32),
                    });
                }
            }
        }
        for (index, body) in checked.bodies.iter().enumerate() {
            if let Some(definition) =
                identities.items[body.definition.module][body.definition.item.0 as usize]
            {
                body_of[definition.index()] = Some(index);
            }
        }
        Self {
            model,
            checked,
            identities,
            module,
            target,
            harness,
            body_of,
            definition_ref,
            definition_module,
        }
    }

    pub(crate) fn definition_key(&self, definition: hir::DefId) -> [u8; 32] {
        self.module.definitions[definition.index()].key
    }

    pub(crate) fn item_key(&self, definition: DefRef) -> [u8; 32] {
        self.definition_key(self.identities.item(definition))
    }

    pub(crate) fn function_key(&self, id: CallableId) -> [u8; 32] {
        self.definition_key(self.identities.function(id))
    }

    /// `DefRef` -> `DefId`。
    pub(crate) fn definition_of(&self, reference: &DefRef) -> Option<hir::DefId> {
        self.identities.items[reference.module][reference.item.0 as usize]
    }

    /// 规范类型字节：具体类型不得含参数、投影或未收敛变量。
    pub(crate) fn encode_type(&self, ty: &Ty) -> Result<Vec<u8>, Diagnostic> {
        let mut out = Vec::new();
        self.encode_type_into(ty, &mut out)?;
        Ok(out)
    }

    fn encode_type_into(&self, ty: &Ty, out: &mut Vec<u8>) -> Result<(), Diagnostic> {
        let invalid = || {
            Err(Diagnostic::error(
                DiagnosticCode::MonoDivergence,
                "具体类型不能包含泛型参数、投影或未收敛类型变量",
                None,
            ))
        };
        match ty {
            Ty::Error | Ty::Var(_) | Ty::Param(_) | Ty::Projection(..) => invalid()?,
            Ty::Never => out.extend_from_slice(&1u16.to_le_bytes()),
            Ty::Unit => out.extend_from_slice(&2u16.to_le_bytes()),
            Ty::Bool => out.extend_from_slice(&3u16.to_le_bytes()),
            Ty::Int { signed, bits } => {
                out.extend_from_slice(&4u16.to_le_bytes());
                out.push(u8::from(*signed));
                out.extend_from_slice(&bits.to_le_bytes());
            }
            Ty::Float(bits) => {
                out.extend_from_slice(&5u16.to_le_bytes());
                out.extend_from_slice(&bits.to_le_bytes());
            }
            Ty::Char => out.extend_from_slice(&6u16.to_le_bytes()),
            Ty::String => out.extend_from_slice(&7u16.to_le_bytes()),
            Ty::TypeId => out.extend_from_slice(&8u16.to_le_bytes()),
            Ty::Range => out.extend_from_slice(&9u16.to_le_bytes()),
            Ty::Ref(inner) => {
                out.extend_from_slice(&10u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
            Ty::Ptr(inner) => {
                out.extend_from_slice(&11u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
            Ty::Slice(inner) => {
                out.extend_from_slice(&12u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
            Ty::Array(inner, length) => {
                out.extend_from_slice(&13u16.to_le_bytes());
                encode_u64(out, *length);
                self.encode_type_into(inner, out)?;
            }
            Ty::Tuple(parts) => {
                out.extend_from_slice(&14u16.to_le_bytes());
                encode_u64(out, parts.len() as u64);
                for part in parts {
                    self.encode_type_into(part, out)?;
                }
            }
            Ty::Function(parameters, result) => {
                out.extend_from_slice(&15u16.to_le_bytes());
                encode_u64(out, parameters.len() as u64);
                for parameter in parameters {
                    self.encode_type_into(parameter, out)?;
                }
                self.encode_type_into(result, out)?;
            }
            Ty::Callable(id, arguments, signature) => {
                out.extend_from_slice(&16u16.to_le_bytes());
                out.extend_from_slice(&self.function_key(*id));
                encode_u64(out, arguments.len() as u64);
                for argument in arguments {
                    self.encode_type_into(argument, out)?;
                }
                self.encode_type_into(signature, out)?;
            }
            Ty::Named(index, arguments) => {
                let nominal = self
                    .model
                    .nominal
                    .get(*index)
                    .ok_or_else(|| internal("名义类型索引越界"))?;
                out.extend_from_slice(&17u16.to_le_bytes());
                out.extend_from_slice(&self.item_key(nominal.definition));
                out.push(nominal.repr.flags());
                encode_u64(out, nominal.repr.align);
                match nominal.repr.tag {
                    Some((signed, bits)) => {
                        out.push(1);
                        out.push(u8::from(signed));
                        out.extend_from_slice(&bits.to_le_bytes());
                    }
                    None => out.push(0),
                }
                encode_u64(out, arguments.len() as u64);
                for argument in arguments {
                    self.encode_type_into(argument, out)?;
                }
            }
            Ty::Opaque(id, arguments) => {
                let definition = self
                    .identities
                    .opaques
                    .get(*id as usize)
                    .ok_or_else(|| internal("opaque 类型索引越界"))?;
                out.extend_from_slice(&18u16.to_le_bytes());
                out.extend_from_slice(&self.definition_key(*definition));
                encode_u64(out, arguments.len() as u64);
                for argument in arguments {
                    self.encode_type_into(argument, out)?;
                }
            }
            Ty::Dyn(interfaces) => {
                out.extend_from_slice(&19u16.to_le_bytes());
                encode_u64(out, interfaces.len() as u64);
                for interface in interfaces {
                    self.encode_trait_ref_into(interface, out)?;
                }
            }
            Ty::Option(inner) => {
                out.extend_from_slice(&20u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
            Ty::Result(value, error) => {
                out.extend_from_slice(&21u16.to_le_bytes());
                self.encode_type_into(value, out)?;
                self.encode_type_into(error, out)?;
            }
            Ty::Chan(inner) => {
                out.extend_from_slice(&22u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
            Ty::Join(inner) => {
                out.extend_from_slice(&23u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
            Ty::MaybeUninit(inner) => {
                out.extend_from_slice(&24u16.to_le_bytes());
                self.encode_type_into(inner, out)?;
            }
        }
        Ok(())
    }

    /// 规范 trait 引用：接口稳定键 + 实参。
    pub(crate) fn encode_trait_ref_into(
        &self,
        interface: &TraitRef,
        out: &mut Vec<u8>,
    ) -> Result<(), Diagnostic> {
        let definition = self
            .identities
            .interfaces
            .get(interface.id)
            .ok_or_else(|| internal("接口索引越界"))?;
        out.extend_from_slice(&self.definition_key(*definition));
        encode_u64(out, interface.arguments.len() as u64);
        for argument in &interface.arguments {
            self.encode_type_into(argument, out)?;
        }
        Ok(())
    }
}

/// 规范类型树节点数：叶计 1，构造器计 1 并递归；u64 饱和。
pub(crate) fn type_structure_size(ty: &Ty) -> u64 {
    fn size(ty: &Ty) -> u64 {
        let children = |types: &[Ty]| types.iter().map(size).fold(0u64, u64::saturating_add);
        match ty {
            Ty::Ref(t)
            | Ty::Ptr(t)
            | Ty::Slice(t)
            | Ty::Array(t, _)
            | Ty::Option(t)
            | Ty::Chan(t)
            | Ty::Join(t)
            | Ty::MaybeUninit(t) => 1u64.saturating_add(size(t)),
            Ty::Tuple(ts) => 1u64.saturating_add(children(ts)),
            Ty::Function(ts, ret) => 1u64.saturating_add(children(ts)).saturating_add(size(ret)),
            Ty::Callable(_, args, sig) => 1u64
                .saturating_add(children(args))
                .saturating_add(size(sig)),
            Ty::Named(_, args) | Ty::Opaque(_, args) => 1u64.saturating_add(children(args)),
            Ty::Dyn(interfaces) => 1u64.saturating_add(
                interfaces
                    .iter()
                    .map(|interface| 1u64.saturating_add(children(&interface.arguments)))
                    .fold(0u64, u64::saturating_add),
            ),
            Ty::Result(t, e) => 1u64.saturating_add(size(t)).saturating_add(size(e)),
            _ => 1,
        }
    }
    size(ty)
}

/// 以声明签名为模式，从实际签名解出泛型实参绑定。
///
/// 声明侧 `Param` 按名绑定；结构不匹配视为单态化内部不一致。
pub(crate) fn unify(
    declared: &Ty,
    actual: &Ty,
    bindings: &mut BTreeMap<String, Ty>,
) -> Result<(), Diagnostic> {
    let mismatch = || {
        Err(Diagnostic::error(
            DiagnosticCode::MonoDivergence,
            format!("实例化时声明签名与实际签名结构不一致：{declared:?} vs {actual:?}"),
            None,
        ))
    };
    match (declared, actual) {
        (Ty::Param(name), actual) => {
            bindings.insert(name.clone(), actual.clone());
            Ok(())
        }
        (Ty::Ref(a), Ty::Ref(b)) | (Ty::Ptr(a), Ty::Ptr(b)) | (Ty::Slice(a), Ty::Slice(b)) => {
            unify(a, b, bindings)
        }
        (Ty::Array(a, la), Ty::Array(b, lb)) if la == lb => unify(a, b, bindings),
        // 参数包：声明 Tuple[模式...] 对齐较短实元列表；单元素包接受裸值。
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            let (pattern, actual) = if a.len() >= b.len() { (a, b) } else { (b, a) };
            for (left, right) in pattern.iter().zip(actual) {
                unify(left, right, bindings)?;
            }
            Ok(())
        }
        (Ty::Tuple(a), other) if a.len() == 1 => unify(&a[0], other, bindings),
        (other, Ty::Tuple(b)) if b.len() == 1 => unify(other, &b[0], bindings),
        (Ty::Function(ap, ar), Ty::Function(bp, br)) if ap.len() == bp.len() => {
            for (left, right) in ap.iter().zip(bp) {
                unify(left, right, bindings)?;
            }
            unify(ar, br, bindings)
        }
        (Ty::Callable(ai, aa, asig), Ty::Callable(bi, ba, bsig)) if ai == bi => {
            if aa.len() != ba.len() {
                return mismatch();
            }
            for (left, right) in aa.iter().zip(ba) {
                unify(left, right, bindings)?;
            }
            unify(asig, bsig, bindings)
        }
        (Ty::Named(ai, aa), Ty::Named(bi, ba)) if ai == bi && aa.len() == ba.len() => {
            for (left, right) in aa.iter().zip(ba) {
                unify(left, right, bindings)?;
            }
            Ok(())
        }
        (Ty::Opaque(ai, aa), Ty::Opaque(bi, ba)) if ai == bi && aa.len() == ba.len() => {
            for (left, right) in aa.iter().zip(ba) {
                unify(left, right, bindings)?;
            }
            Ok(())
        }
        (Ty::Dyn(a), Ty::Dyn(b)) if a.len() == b.len() => {
            for (left, right) in a.iter().zip(b) {
                if left.id != right.id || left.arguments.len() != right.arguments.len() {
                    return mismatch();
                }
                for (x, y) in left.arguments.iter().zip(&right.arguments) {
                    unify(x, y, bindings)?;
                }
            }
            Ok(())
        }
        (Ty::Option(a), Ty::Option(b))
        | (Ty::Chan(a), Ty::Chan(b))
        | (Ty::Join(a), Ty::Join(b))
        | (Ty::MaybeUninit(a), Ty::MaybeUninit(b)) => unify(a, b, bindings),
        (Ty::Result(av, ae), Ty::Result(bv, be)) => {
            unify(av, bv, bindings)?;
            unify(ae, be, bindings)
        }
        (a, b) if a == b => Ok(()),
        _ => mismatch(),
    }
}

fn internal(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
