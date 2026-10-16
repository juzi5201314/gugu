//! std.sync 契约；backend 和 CLI 只消费已验证对象。
//!
//! 本段把 Ordering、Atomic 合法类型、Mutex/RwLock/Condvar/OnceLock/Cancel 控制块布局、
//! 状态迁移与需求视图固定成带版本的对象，与 `RuntimeRawModel` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。

use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::BTreeMap;
use std::mem::{align_of, offset_of, size_of};

use super::model::RawModelError;
use super::platform::PlatformProfile;

/// 同步契约段的 schema 版本。
pub(crate) const SYNC_SCHEMA: u32 = 1;

/// Ordering 常量编码。
pub const ORDERING_RELAXED: u32 = 0;
pub const ORDERING_ACQUIRE: u32 = 1;
pub const ORDERING_RELEASE: u32 = 2;
pub const ORDERING_ACQ_REL: u32 = 3;
pub const ORDERING_SEQ_CST: u32 = 4;

/// OnceLock / Lazy 状态编码。
pub const ONCE_UNINIT: u32 = 0;
pub const ONCE_INITIALIZING: u32 = 1;
pub const ONCE_READY: u32 = 2;
pub const ONCE_FAILED: u32 = 3;

/// CancelSource / CancelToken 状态编码。
pub const CANCEL_ACTIVE: u32 = 0;
pub const CANCEL_CANCELLED: u32 = 1;

/// Mutex 状态编码。
pub const MUTEX_UNLOCKED: u32 = 0;
pub const MUTEX_LOCKED: u32 = 1;
pub const MUTEX_CONTENDED: u32 = 2;

/// 控制块规范字节数。
pub const SYNC_CONTROL_BYTES: u32 = 64;
/// 控制块规范对齐字节数（cache line）。
pub const SYNC_CONTROL_ALIGN: u32 = 64;

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MutexControl {
    pub state: u64,
    pub owner_coroutine: u64,
    pub wait_q_head: u64,
    pub lock_count: u64,
    pub padding: [u8; 32],
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RwLockControl {
    pub state: u64,
    pub writer_coroutine: u64,
    pub read_q_head: u64,
    pub write_q_head: u64,
    pub padding: [u8; 32],
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CondvarControl {
    pub wait_q_head: u64,
    pub sequence: u64,
    pub padding: [u8; 48],
}

impl Default for CondvarControl {
    fn default() -> Self {
        Self {
            wait_q_head: 0,
            sequence: 0,
            padding: [0; 48],
        }
    }
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OnceControl {
    pub state: u64,
    pub initiator_coroutine: u64,
    pub wait_q_head: u64,
    pub value_ready: u64,
    pub padding: [u8; 32],
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CancelControl {
    pub state: u64,
    pub generation: u64,
    pub wait_q_head: u64,
    pub token_count: u64,
    pub padding: [u8; 32],
}

/// 从优化后 LIR 推导的同步需求。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SyncDemand {
    /// 原子操作数量。
    pub atomic_ops: u32,
    /// 互斥锁操作数量。
    pub mutex_ops: u32,
    /// 读写锁操作数量。
    pub rwlock_ops: u32,
    /// 条件变量操作数量。
    pub condvar_ops: u32,
    /// OnceLock 操作数量。
    pub once_ops: u32,
    /// Lazy 操作数量。
    pub lazy_ops: u32,
    /// 取消操作数量。
    pub cancel_ops: u32,
}

impl SyncDemand {
    /// 所有同步操作调用合计。
    pub const fn total_ops(self) -> u32 {
        self.atomic_ops
            + self.mutex_ops
            + self.rwlock_ops
            + self.condvar_ops
            + self.once_ops
            + self.lazy_ops
            + self.cancel_ops
    }
}

/// 同步 record 字段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SyncFieldLayout {
    /// 字段名称。
    pub name: String,
    /// 字段字节偏移。
    pub offset: u32,
}

/// 同步 record 布局。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SyncRecordLayout {
    /// 结构名称。
    pub name: String,
    /// 总字节数。
    pub bytes: u32,
    /// 对齐要求。
    pub alignment: u32,
    /// 字段列表。
    pub fields: Vec<SyncFieldLayout>,
}

/// 已验证的同步 runtime 契约；版本变化使 RuntimeRawModel 和 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SyncRuntimeContract {
    /// 契约模式版本。
    pub schema: u32,
    /// 平台 profile 标识。
    pub profile: String,
    /// 支持的内存序名称列表。
    pub ordering_names: Vec<String>,
    /// 支持的原子标量类型列表。
    pub atomic_types: Vec<String>,
    /// Once 状态枚举名称列表。
    pub once_states: Vec<String>,
    /// 取消状态枚举名称列表。
    pub cancel_states: Vec<String>,
    /// 互斥锁状态枚举名称列表。
    pub mutex_states: Vec<String>,
    /// 同步原语布局列表。
    pub records: Vec<SyncRecordLayout>,
    /// 同步操作需求。
    pub demand: SyncDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl SyncRuntimeContract {
    /// 返回契约模式版本。
    pub fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回平台 profile 名称。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回注册的原语结构数量。
    pub fn primitive_count(&self) -> u32 {
        u32::try_from(self.records.len()).expect("record 数量适配 u32")
    }

    /// 返回同步需求。
    pub fn demand(&self) -> SyncDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub(crate) fn build(
        demand: SyncDemand,
        profile: PlatformProfile,
    ) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: SYNC_SCHEMA,
            profile: profile.name().to_owned(),
            ordering_names: vec![
                "relaxed".into(),
                "acquire".into(),
                "release".into(),
                "acq_rel".into(),
                "seq_cst".into(),
            ],
            atomic_types: vec![
                "bool".into(),
                "int8".into(),
                "int16".into(),
                "int32".into(),
                "int64".into(),
                "int".into(),
                "uint8".into(),
                "uint16".into(),
                "uint32".into(),
                "uint64".into(),
                "uint".into(),
                "ptr".into(),
            ],
            once_states: vec![
                "uninit".into(),
                "initializing".into(),
                "ready".into(),
                "failed".into(),
            ],
            cancel_states: vec!["active".into(), "cancelled".into()],
            mutex_states: vec!["unlocked".into(), "locked".into(), "contended".into()],
            records: fixed_layouts(),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != SYNC_SCHEMA {
            return Err(RawModelError::new("同步契约 schema 版本不一致"));
        }
        if self.ordering_names != ["relaxed", "acquire", "release", "acq_rel", "seq_cst"] {
            return Err(RawModelError::new("Ordering 目录不一致"));
        }
        if self.once_states != ["uninit", "initializing", "ready", "failed"] {
            return Err(RawModelError::new(
                "Once 状态目录必须是 uninit/initializing/ready/failed",
            ));
        }
        if self.cancel_states != ["active", "cancelled"] {
            return Err(RawModelError::new("Cancel 状态目录必须是 active/cancelled"));
        }
        if self.mutex_states != ["unlocked", "locked", "contended"] {
            return Err(RawModelError::new(
                "Mutex 状态目录必须是 unlocked/locked/contended",
            ));
        }
        if self.records.len() != 5 {
            return Err(RawModelError::new("同步控制块数量必须为 5"));
        }
        for record in &self.records {
            if record.bytes != SYNC_CONTROL_BYTES || record.alignment != SYNC_CONTROL_ALIGN {
                return Err(RawModelError::new(format!(
                    "控制块 `{}` 字节数或对齐与规范不一致",
                    record.name
                )));
            }
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("同步契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 计算契约规范化字节流。
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(self.profile.as_bytes());
        bytes.push(0);
        for ordering in &self.ordering_names {
            bytes.extend_from_slice(ordering.as_bytes());
            bytes.push(0);
        }
        for ty in &self.atomic_types {
            bytes.extend_from_slice(ty.as_bytes());
            bytes.push(0);
        }
        for state in &self.once_states {
            bytes.extend_from_slice(state.as_bytes());
            bytes.push(0);
        }
        for state in &self.cancel_states {
            bytes.extend_from_slice(state.as_bytes());
            bytes.push(0);
        }
        for state in &self.mutex_states {
            bytes.extend_from_slice(state.as_bytes());
            bytes.push(0);
        }
        for record in &self.records {
            bytes.extend_from_slice(record.name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&record.bytes.to_le_bytes());
            bytes.extend_from_slice(&record.alignment.to_le_bytes());
            for field in &record.fields {
                bytes.extend_from_slice(field.name.as_bytes());
                bytes.push(0);
                bytes.extend_from_slice(&field.offset.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.demand.atomic_ops.to_le_bytes());
        bytes.extend_from_slice(&self.demand.mutex_ops.to_le_bytes());
        bytes.extend_from_slice(&self.demand.rwlock_ops.to_le_bytes());
        bytes.extend_from_slice(&self.demand.condvar_ops.to_le_bytes());
        bytes.extend_from_slice(&self.demand.once_ops.to_le_bytes());
        bytes.extend_from_slice(&self.demand.lazy_ops.to_le_bytes());
        bytes.extend_from_slice(&self.demand.cancel_ops.to_le_bytes());
        bytes
    }

    /// 计算契约 BLAKE3 指纹。
    pub fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-sync-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }
}

fn fixed_layouts() -> Vec<SyncRecordLayout> {
    vec![
        SyncRecordLayout {
            name: "MutexControl".into(),
            bytes: u32::try_from(size_of::<MutexControl>()).expect("u32"),
            alignment: u32::try_from(align_of::<MutexControl>()).expect("u32"),
            fields: vec![
                SyncFieldLayout {
                    name: "state".into(),
                    offset: u32::try_from(offset_of!(MutexControl, state)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "owner_coroutine".into(),
                    offset: u32::try_from(offset_of!(MutexControl, owner_coroutine)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "wait_q_head".into(),
                    offset: u32::try_from(offset_of!(MutexControl, wait_q_head)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "lock_count".into(),
                    offset: u32::try_from(offset_of!(MutexControl, lock_count)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "padding".into(),
                    offset: u32::try_from(offset_of!(MutexControl, padding)).expect("u32"),
                },
            ],
        },
        SyncRecordLayout {
            name: "RwLockControl".into(),
            bytes: u32::try_from(size_of::<RwLockControl>()).expect("u32"),
            alignment: u32::try_from(align_of::<RwLockControl>()).expect("u32"),
            fields: vec![
                SyncFieldLayout {
                    name: "state".into(),
                    offset: u32::try_from(offset_of!(RwLockControl, state)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "writer_coroutine".into(),
                    offset: u32::try_from(offset_of!(RwLockControl, writer_coroutine))
                        .expect("u32"),
                },
                SyncFieldLayout {
                    name: "read_q_head".into(),
                    offset: u32::try_from(offset_of!(RwLockControl, read_q_head)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "write_q_head".into(),
                    offset: u32::try_from(offset_of!(RwLockControl, write_q_head)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "padding".into(),
                    offset: u32::try_from(offset_of!(RwLockControl, padding)).expect("u32"),
                },
            ],
        },
        SyncRecordLayout {
            name: "CondvarControl".into(),
            bytes: u32::try_from(size_of::<CondvarControl>()).expect("u32"),
            alignment: u32::try_from(align_of::<CondvarControl>()).expect("u32"),
            fields: vec![
                SyncFieldLayout {
                    name: "wait_q_head".into(),
                    offset: u32::try_from(offset_of!(CondvarControl, wait_q_head)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "sequence".into(),
                    offset: u32::try_from(offset_of!(CondvarControl, sequence)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "padding".into(),
                    offset: u32::try_from(offset_of!(CondvarControl, padding)).expect("u32"),
                },
            ],
        },
        SyncRecordLayout {
            name: "OnceControl".into(),
            bytes: u32::try_from(size_of::<OnceControl>()).expect("u32"),
            alignment: u32::try_from(align_of::<OnceControl>()).expect("u32"),
            fields: vec![
                SyncFieldLayout {
                    name: "state".into(),
                    offset: u32::try_from(offset_of!(OnceControl, state)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "initiator_coroutine".into(),
                    offset: u32::try_from(offset_of!(OnceControl, initiator_coroutine))
                        .expect("u32"),
                },
                SyncFieldLayout {
                    name: "wait_q_head".into(),
                    offset: u32::try_from(offset_of!(OnceControl, wait_q_head)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "value_ready".into(),
                    offset: u32::try_from(offset_of!(OnceControl, value_ready)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "padding".into(),
                    offset: u32::try_from(offset_of!(OnceControl, padding)).expect("u32"),
                },
            ],
        },
        SyncRecordLayout {
            name: "CancelControl".into(),
            bytes: u32::try_from(size_of::<CancelControl>()).expect("u32"),
            alignment: u32::try_from(align_of::<CancelControl>()).expect("u32"),
            fields: vec![
                SyncFieldLayout {
                    name: "state".into(),
                    offset: u32::try_from(offset_of!(CancelControl, state)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "generation".into(),
                    offset: u32::try_from(offset_of!(CancelControl, generation)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "wait_q_head".into(),
                    offset: u32::try_from(offset_of!(CancelControl, wait_q_head)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "token_count".into(),
                    offset: u32::try_from(offset_of!(CancelControl, token_count)).expect("u32"),
                },
                SyncFieldLayout {
                    name: "padding".into(),
                    offset: u32::try_from(offset_of!(CancelControl, padding)).expect("u32"),
                },
            ],
        },
    ]
}

const _: () = {
    assert!(ORDERING_RELAXED == 0);
    assert!(ORDERING_ACQUIRE == 1);
    assert!(ORDERING_RELEASE == 2);
    assert!(ORDERING_ACQ_REL == 3);
    assert!(ORDERING_SEQ_CST == 4);
    assert!(ONCE_UNINIT == 0);
    assert!(ONCE_INITIALIZING == 1);
    assert!(ONCE_READY == 2);
    assert!(ONCE_FAILED == 3);
    assert!(CANCEL_ACTIVE == 0);
    assert!(CANCEL_CANCELLED == 1);
    assert!(MUTEX_UNLOCKED == 0);
    assert!(MUTEX_LOCKED == 1);
    assert!(MUTEX_CONTENDED == 2);
};

/// 内存序。非法编码在 `from_u32` 拒绝。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryOrdering {
    Relaxed = 0,
    Acquire = 1,
    Release = 2,
    AcqRel = 3,
    SeqCst = 4,
}

#[cfg(test)]
impl MemoryOrdering {
    pub(crate) fn from_u32(val: u32) -> Result<Self, RawModelError> {
        match val {
            ORDERING_RELAXED => Ok(Self::Relaxed),
            ORDERING_ACQUIRE => Ok(Self::Acquire),
            ORDERING_RELEASE => Ok(Self::Release),
            ORDERING_ACQ_REL => Ok(Self::AcqRel),
            ORDERING_SEQ_CST => Ok(Self::SeqCst),
            _ => Err(RawModelError::new("未知内存序编码")),
        }
    }
}

/// 合法 Atomic 标量。浮点、字符串和记录类型不是原子标量。
#[cfg(test)]
pub(crate) fn is_legal_atomic_type(ty_name: &str) -> bool {
    matches!(
        ty_name,
        "bool"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "int"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uint"
            | "ptr"
            | "*raw"
            | "*byte"
    )
}

/// 原子内存序状态机：拒绝非法序，并记录 Release/Acquire 与 SeqCst 观察。
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct AtomicStateMachine {
    pub(crate) value: u64,
    pub(crate) release_epoch: u64,
    pub(crate) seq_cst_seq: u64,
    pub(crate) global_clock: u64,
    pub(crate) coroutine_views: BTreeMap<u64, u64>,
}

#[cfg(test)]
impl AtomicStateMachine {
    pub(crate) fn new(initial: u64) -> Self {
        Self {
            value: initial,
            release_epoch: 0,
            seq_cst_seq: 0,
            global_clock: 1,
            coroutine_views: BTreeMap::new(),
        }
    }

    pub(crate) fn load(
        &mut self,
        coroutine: u64,
        order: MemoryOrdering,
    ) -> Result<u64, RawModelError> {
        if matches!(order, MemoryOrdering::Release | MemoryOrdering::AcqRel) {
            return Err(RawModelError::new("load 不接受 Release 或 AcqRel 内存序"));
        }
        if matches!(order, MemoryOrdering::Acquire | MemoryOrdering::SeqCst) {
            let view = self.coroutine_views.entry(coroutine).or_insert(0);
            *view = (*view).max(self.release_epoch);
        }
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(self.value)
    }

    pub(crate) fn store(
        &mut self,
        coroutine: u64,
        val: u64,
        order: MemoryOrdering,
    ) -> Result<(), RawModelError> {
        if matches!(order, MemoryOrdering::Acquire | MemoryOrdering::AcqRel) {
            return Err(RawModelError::new("store 不接受 Acquire 或 AcqRel 内存序"));
        }
        self.value = val;
        self.global_clock += 1;
        if matches!(order, MemoryOrdering::Release | MemoryOrdering::SeqCst) {
            self.release_epoch = self.global_clock;
            self.coroutine_views.insert(coroutine, self.release_epoch);
        }
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(())
    }

    pub(crate) fn compare_exchange(
        &mut self,
        coroutine: u64,
        expected: u64,
        desired: u64,
        success: MemoryOrdering,
        failure: MemoryOrdering,
    ) -> Result<Result<(), u64>, RawModelError> {
        if matches!(failure, MemoryOrdering::Release | MemoryOrdering::AcqRel) {
            return Err(RawModelError::new(
                "compare_exchange 失败序不能是 Release 或 AcqRel",
            ));
        }
        let allowed = match failure {
            MemoryOrdering::Relaxed => true,
            MemoryOrdering::Acquire => matches!(
                success,
                MemoryOrdering::Acquire | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
            ),
            MemoryOrdering::SeqCst => success == MemoryOrdering::SeqCst,
            MemoryOrdering::Release | MemoryOrdering::AcqRel => false,
        };
        if !allowed {
            return Err(RawModelError::new("compare_exchange 失败序不能强于成功序"));
        }
        if self.value == expected {
            self.value = desired;
            self.global_clock += 1;
            if matches!(
                success,
                MemoryOrdering::Release | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
            ) {
                self.release_epoch = self.global_clock;
                self.coroutine_views.insert(coroutine, self.release_epoch);
            }
            if matches!(
                success,
                MemoryOrdering::Acquire | MemoryOrdering::AcqRel | MemoryOrdering::SeqCst
            ) {
                let view = self.coroutine_views.entry(coroutine).or_insert(0);
                *view = (*view).max(self.release_epoch);
            }
            if success == MemoryOrdering::SeqCst {
                self.seq_cst_seq += 1;
            }
            Ok(Ok(()))
        } else {
            let actual = self.value;
            if matches!(failure, MemoryOrdering::Acquire | MemoryOrdering::SeqCst) {
                let view = self.coroutine_views.entry(coroutine).or_insert(0);
                *view = (*view).max(self.release_epoch);
            }
            if failure == MemoryOrdering::SeqCst {
                self.seq_cst_seq += 1;
            }
            Ok(Err(actual))
        }
    }

    pub(crate) fn fence(&mut self, order: MemoryOrdering) -> Result<(), RawModelError> {
        if order == MemoryOrdering::Relaxed {
            return Err(RawModelError::new("fence 不接受 Relaxed 内存序"));
        }
        self.global_clock += 1;
        if order == MemoryOrdering::SeqCst {
            self.seq_cst_seq += 1;
        }
        Ok(())
    }
}
