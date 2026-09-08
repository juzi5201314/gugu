//! 闭世界类型元数据：编号只存在于当前 universe，片段继续保存稳定类型键。
use crate::frontend::mono::keys::{StableTypeKey, hash_domain};
use crate::{Diagnostic, DiagnosticCode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Shape {
    Unit,
    Bool,
    Char,
    Int { signed: bool, bits: u16 },
    Float(u16),
    TypeId,
    Tuple(Vec<StableTypeKey>),
    Array(StableTypeKey, u64),
    Struct(Vec<(String, StableTypeKey)>),
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TypeRecord {
    pub key: StableTypeKey,
    pub canonical: Vec<u8>,
    pub name: String,
    pub layout: Option<(u64, u64)>,
    pub children: Vec<StableTypeKey>,
    pub shape: Shape,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TypeUniverse {
    pub records: Vec<TypeRecord>,
    pub vtables: Vec<VtableReference>,
    pub fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct VtableReference {
    pub interface: [u8; 32],
    pub concrete: u32,
}

impl TypeUniverse {
    pub(crate) fn type_id(&self, key: &StableTypeKey) -> Option<u32> {
        self.records
            .binary_search_by_key(key, |record| record.key)
            .ok()
            .map(|i| i as u32)
    }

    pub(crate) fn record(&self, key: &StableTypeKey) -> Result<&TypeRecord, Diagnostic> {
        self.type_id(key)
            .map(|id| &self.records[id as usize])
            .ok_or_else(|| invalid("冻结后不能发现新类型或无 TypeId 的类型"))
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        hash_domain(
            "gugu-type-universe-v1",
            &serde_json::to_vec(&(&self.records, &self.vtables)).expect("类型表可序列化"),
        )
    }

    pub(crate) fn verify(&self) -> Result<(), Diagnostic> {
        if self.records.len() > u32::MAX as usize
            || self.fingerprint != self.fingerprint()
            || !self
                .records
                .windows(2)
                .all(|pair| pair[0].key < pair[1].key)
            || !self.vtables.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(invalid("type universe 编号、顺序或指纹不合法"));
        }
        for record in &self.records {
            if record.key != hash_domain("gugu-mono-v1", &record.canonical)
                || record.canonical.len() < 2
                || matches!(
                    u16::from_le_bytes([record.canonical[0], record.canonical[1]]),
                    1 | 24
                )
                || record.name.is_empty()
                || record
                    .layout
                    .is_some_and(|(size, align)| !align.is_power_of_two() || size % align != 0)
                || !record.children.windows(2).all(|pair| pair[0] < pair[1])
                || record
                    .children
                    .iter()
                    .any(|key| self.type_id(key).is_none())
            {
                return Err(invalid("类型 descriptor 身份、布局或子类型引用不合法"));
            }
        }
        if self
            .vtables
            .iter()
            .any(|vtable| vtable.concrete as usize >= self.records.len())
        {
            return Err(invalid("vtable 的具体类型引用越界"));
        }
        Ok(())
    }
}

pub(crate) fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::LateComptime, message, None)
}
