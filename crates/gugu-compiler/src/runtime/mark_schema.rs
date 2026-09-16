//! MarkMailbox、owner credit 与终止检测的契约段；backend 与 CLI 只消费已验证对象。
//!
//! 本段把「每 owner 单 consumer MarkMailbox」「cycle/topology/generation 身份」「跨 owner
//! mark ticket 的字段集合」「credit acquire/consume/return 状态机」「root snapshot gate 的
//! 参与者目录」与「7 项收敛条件到 credit 来源的绑定」固定成带版本的对象，与
//! `barrier_schema`/`pacing_schema`/`local_heap_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! 参数是编译期实现门禁，不是用户可观察的时序：契约只登记容量、目录、绑定与不变量，
//! 不登记宿主地址、线程数或回收时刻。

use super::model::{FieldKind, MessageFieldSchema};

/// mark 契约段的 schema 版本。
pub(crate) const MARK_SCHEMA: u32 = 1;

/// `MarkTicket` 的规范字段数。
pub(crate) const MARK_TICKET_FIELDS: u32 = 14;

/// 登记 `MarkTicket` 的字段集合：只允许稳定 arena descriptor、对象偏移、source block、
/// cycle/topology epoch、credit 与 bytes，任何地址字段都在 verifier 中被拒绝。
pub(crate) fn mark_ticket_fields() -> Vec<MessageFieldSchema> {
    let mut fields = vec![
        MessageFieldSchema::new("bytes", FieldKind::Bytes),
        MessageFieldSchema::new("credit", FieldKind::Credit),
        MessageFieldSchema::new("cycle_epoch", FieldKind::Epoch),
        MessageFieldSchema::new("family", FieldKind::KindTag),
        MessageFieldSchema::new("integrity", FieldKind::Integrity),
        MessageFieldSchema::new("object_offset", FieldKind::UnitIndex),
        MessageFieldSchema::new("source_block", FieldKind::SourceBlock),
        MessageFieldSchema::new("state", FieldKind::MessageState),
        MessageFieldSchema::new("target.domain", FieldKind::OwnerDomain),
        MessageFieldSchema::new("target.generation", FieldKind::Generation),
        MessageFieldSchema::new("target.owner_id", FieldKind::OwnerId),
        MessageFieldSchema::new("target.route_key", FieldKind::RouteKey),
        MessageFieldSchema::new("target_arena", FieldKind::DescriptorIndex),
        MessageFieldSchema::new("topology_epoch", FieldKind::Epoch),
    ];
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    fields
}
