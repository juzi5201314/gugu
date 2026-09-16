//! MarkMailbox、owner credit 与终止检测的契约段；backend 与 CLI 只消费已验证对象。
//!
//! 本段把「每 owner 单 consumer MarkMailbox」「cycle/topology/generation 身份」「跨 owner
//! mark ticket 的字段集合」「credit acquire/consume/return 状态机」「root snapshot gate 的
//! 参与者目录」与「7 项收敛条件到九个 credit 来源的绑定」固定成带版本的对象，与
//! `barrier_schema`/`pacing_schema`/`local_heap_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! 参数是编译期实现门禁，不是用户可观察的时序：契约只登记容量、目录、绑定与不变量，
//! 不登记宿主地址、线程数或回收时刻。credit 池的容量上界是「常驻 message node 容量加根槽数」
//! ——任何在飞的 mark ticket 都占一个 non-moving node，根 seed 不占 node 但每个根槽每 cycle
//! 至多一次——因此池耗尽就是真实契约违约。

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::mem::{align_of, offset_of, size_of};

use super::barrier_schema::MessageFamilyTag;
use super::model::{FieldKind, MessageFieldSchema, MessageSchemaV1, RawModelError};
use super::pacing_schema::CREDIT_SOURCE_NAMES;

/// mark 契约段的 schema 版本。
pub(crate) const MARK_SCHEMA: u32 = 1;

/// 内建 mark profile 名。
pub(crate) const MARK_PROFILE_NAME: &str = "mosaic-mark";
/// mark profile 的 revision；任何参数或绑定变化都必须递增。
pub(crate) const MARK_PROFILE_REVISION: u32 = 1;

/// owner credit 的 cost unit 名。
pub(crate) const MARK_CREDIT_UNIT: &str = "mark-credit";
/// 每个 MarkMailbox 的 consumer 数量；单 consumer 是 queue-page grace 的前提。
pub(crate) const MARK_MAILBOX_CONSUMERS: u32 = 1;
/// MarkMailbox 的 shard 数量；与 owner inbox 保持一致。
pub(crate) const MARK_MAILBOX_SHARDS: u32 = super::OWNER_INBOX_SHARDS;
/// credit id 中 owner 字段的位宽。
pub(crate) const MARK_CREDIT_OWNER_BITS: u32 = 8;
/// credit id 中 counter 字段的位宽。
pub(crate) const MARK_CREDIT_COUNTER_BITS: u32 = 24;
/// `MarkTicket` 的规范字段数。
pub(crate) const MARK_TICKET_FIELDS: u32 = 14;

/// mark cycle 的状态名；顺序即状态机推进顺序。
pub(crate) const MARK_CYCLE_STATES: [&str; 6] = [
    "idle",
    "snapshot",
    "marking",
    "converging",
    "remark",
    "complete",
];
/// owner credit 的三个转移名；顺序即 acquire → consume → return。
pub(crate) const MARK_CREDIT_TRANSITIONS: [&str; 3] = ["acquire", "consume", "return"];
/// cycle 终止检测的七个收敛条件；顺序即判定顺序，全部为 0 才允许 remark。
pub(crate) const MARK_CONVERGENCE_CONDITIONS: [&str; 7] = [
    "local-worklist",
    "published-batch",
    "mailbox",
    "barrier-buffer",
    "producer-epoch",
    "forwarding-work",
    "pending-credit",
];
/// root snapshot gate 的参与者目录；顺序即确认顺序。
pub(crate) const MARK_SNAPSHOT_PARTICIPANTS: [&str; 6] = [
    "producer-stop-epoch",
    "remote-consumer",
    "root-slice",
    "region-registry",
    "handle-access-guard",
    "local-worklist",
];

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

/// 每条收敛条件绑定的 credit 来源。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MarkConditionBinding {
    /// 收敛条件名；与 `MARK_CONVERGENCE_CONDITIONS` 同序同值。
    pub condition: String,
    /// 该条件观测的 credit 来源；每一项都必须出现在 `credit_sources` 中。
    pub sources: Vec<String>,
}

/// 一个 mark record 字段的固定布局。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MarkRecordField {
    /// 字段名。
    pub name: String,
    /// 字段字节偏移。
    pub offset: u32,
    /// 字段字节数。
    pub bytes: u32,
}

/// 一个 mark record 的固定布局；由 `mark.gg` 逐字段交叉校验。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MarkRecordLayout {
    /// record 名。
    pub name: String,
    /// record 字节数。
    pub bytes: u32,
    /// record 对齐。
    pub alignment: u32,
    /// 声明顺序的字段偏移与尺寸。
    pub fields: Vec<MarkRecordField>,
}

/// MarkMailbox 头部布局；每 owner 一个，单 consumer。
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MarkMailboxHead {
    /// owner 的稳定 descriptor。
    pub owner_descriptor: u64,
    /// 头部所属的 cycle epoch。
    pub cycle_epoch: u64,
    /// 发布时的 producer topology epoch。
    pub topology_epoch: u32,
    /// 已发布但尚未消费的 ticket 数。
    pub pending: u32,
    /// 已消费的 ticket 数。
    pub consumed: u64,
    /// 已转发的 ticket 数。
    pub forwarded: u64,
    /// 最近一次消费的 credit 稠密编号。
    pub last_credit: u32,
    /// consumer slot 数量；固定为 1。
    pub consumer_slots: u32,
    /// 对齐填充，使头部独占一条 cache line。
    pub padding: [u8; 16],
}

/// owner credit 头部布局；每 owner 一个。
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MarkCreditHead {
    /// owner 的稳定 descriptor。
    pub owner_descriptor: u64,
    /// 头部所属的 cycle epoch。
    pub cycle_epoch: u64,
    /// 本 cycle 授权的 credit 上界。
    pub granted: u64,
    /// 已 acquire 的 credit 累计数。
    pub issued: u64,
    /// 已 acquire 但尚未 consume 的 credit 数。
    pub in_flight: u64,
    /// 已 consume 但尚未归还的 credit 数。
    pub done: u64,
    /// 已归还的 credit 累计数。
    pub returned: u64,
    /// 对齐填充。
    pub padding: [u8; 8],
}

/// cycle 终止记录布局；每 cycle 一条，登记七个收敛条件的观测值。
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MarkTerminationRecord {
    /// 该记录的 cycle epoch。
    pub cycle_epoch: u64,
    /// 该记录发布时的 topology epoch。
    pub topology_epoch: u32,
    /// 记录时的 cycle 状态判别值。
    pub state: u32,
    /// 七个收敛条件的观测值；全为 0 表示可以 remark。
    pub conditions: [u64; 7],
}

/// 从优化后 LIR 与冻结世界推导的 mark 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MarkDemand {
    /// 触发 mark 的根站点数；与 GC metadata 的 root range 数同源。
    pub root_sites: u32,
    /// 可能产生跨 owner mark 工作的屏障站点数。
    pub barrier_sites: u32,
    /// 需要跨 owner ticket 的 shared 站点数；与 LocalHeap 的 shared 站点同源。
    pub ticket_sites: u32,
    /// 可能产生跨 block edge delta 的站点数。
    pub edge_delta_sites: u32,
}

impl MarkDemand {
    /// 返回需求视图的稳定指纹；字段顺序即编码顺序。
    pub(crate) fn fingerprint(self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&self.root_sites.to_le_bytes());
        bytes.extend_from_slice(&self.barrier_sites.to_le_bytes());
        bytes.extend_from_slice(&self.ticket_sites.to_le_bytes());
        bytes.extend_from_slice(&self.edge_delta_sites.to_le_bytes());
        *blake3::Hasher::new_derive_key("gugu-mark-demand-v1")
            .update(&bytes)
            .finalize()
            .as_bytes()
    }
}

/// 已验证的 mark runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MarkRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// credit 的 cost unit 名。
    pub credit_unit: String,
    /// 每个 mailbox 的 consumer 数量；必须为 1。
    pub mailbox_consumers: u32,
    /// mailbox 的 shard 数量；必须与 owner inbox 一致。
    pub mailbox_shards: u32,
    /// credit id 的 owner 字段位宽。
    pub credit_owner_bits: u32,
    /// credit id 的 counter 字段位宽。
    pub credit_counter_bits: u32,
    /// 本 cycle 授权的 credit 池上界。
    pub credit_pool: u64,
    /// cycle 状态目录。
    pub cycle_states: Vec<String>,
    /// credit 转移目录。
    pub credit_transitions: Vec<String>,
    /// credit 来源目录；与 pacing 契约同源。
    pub credit_sources: Vec<String>,
    /// 每个收敛条件绑定的 credit 来源。
    pub conditions: Vec<MarkConditionBinding>,
    /// root snapshot gate 的参与者目录。
    pub snapshot_participants: Vec<String>,
    /// 固定 record 目录。
    pub records: Vec<MarkRecordLayout>,
    /// `MarkTicket` 的字段集合；禁止携带地址。
    pub(crate) ticket_fields: MessageSchemaV1,
    /// 上游需求视图。
    pub demand: MarkDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl MarkRuntimeContract {
    /// 由需求视图与 credit 池上界构建契约。
    pub(crate) fn build(demand: MarkDemand, credit_pool: u64) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: MARK_SCHEMA,
            profile: MARK_PROFILE_NAME.to_owned(),
            profile_revision: MARK_PROFILE_REVISION,
            credit_unit: MARK_CREDIT_UNIT.to_owned(),
            mailbox_consumers: MARK_MAILBOX_CONSUMERS,
            mailbox_shards: MARK_MAILBOX_SHARDS,
            credit_owner_bits: MARK_CREDIT_OWNER_BITS,
            credit_counter_bits: MARK_CREDIT_COUNTER_BITS,
            credit_pool,
            cycle_states: names(&MARK_CYCLE_STATES),
            credit_transitions: names(&MARK_CREDIT_TRANSITIONS),
            credit_sources: CREDIT_SOURCE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            conditions: condition_bindings(),
            snapshot_participants: names(&MARK_SNAPSHOT_PARTICIPANTS),
            records: fixed_layouts(),
            ticket_fields: MessageSchemaV1::mark_ticket(),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回内部 schema 版本。
    pub(crate) const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 profile 名。
    pub(crate) fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub(crate) const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回 credit cost unit 名。
    pub(crate) fn credit_unit(&self) -> &str {
        &self.credit_unit
    }

    /// 返回 mailbox consumer 数量。
    pub(crate) const fn mailbox_consumers(&self) -> u32 {
        self.mailbox_consumers
    }

    /// 返回 mailbox shard 数量。
    pub(crate) const fn mailbox_shards(&self) -> u32 {
        self.mailbox_shards
    }

    /// 返回 credit id 的 owner 位宽。
    pub(crate) const fn credit_owner_bits(&self) -> u32 {
        self.credit_owner_bits
    }

    /// 返回 credit id 的 counter 位宽。
    pub(crate) const fn credit_counter_bits(&self) -> u32 {
        self.credit_counter_bits
    }

    /// 返回本 cycle 授权的 credit 池上界。
    pub(crate) const fn credit_pool(&self) -> u64 {
        self.credit_pool
    }

    /// 返回 credit 来源目录长度。
    pub(crate) fn credit_source_count(&self) -> u32 {
        u32::try_from(self.credit_sources.len()).expect("来源数量适配 u32")
    }

    /// 返回收敛条件数量。
    pub(crate) fn condition_count(&self) -> u32 {
        u32::try_from(self.conditions.len()).expect("条件数量适配 u32")
    }

    /// 返回 root snapshot 参与者数量。
    pub(crate) fn snapshot_participant_count(&self) -> u32 {
        u32::try_from(self.snapshot_participants.len()).expect("参与者数量适配 u32")
    }

    /// 返回 record 数量。
    pub(crate) fn record_count(&self) -> u32 {
        u32::try_from(self.records.len()).expect("record 数量适配 u32")
    }

    /// 返回 `MarkTicket` 字段数。
    pub(crate) fn ticket_field_count(&self) -> u32 {
        u32::try_from(self.ticket_fields.fields.len()).expect("字段数量适配 u32")
    }

    /// 返回 `MarkTicket` 字段集合。
    pub(crate) const fn ticket_fields(&self) -> &MessageSchemaV1 {
        &self.ticket_fields
    }

    /// 返回上游需求视图。
    pub(crate) const fn demand(&self) -> MarkDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub(crate) const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 校验契约：常量与 profile 身份、目录、条件绑定、record 布局与 ticket 字段。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        self.verify_constants()?;
        self.verify_catalogs()?;
        self.verify_conditions()?;
        self.verify_layouts()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("mark 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 校验固定参数、profile 身份与 credit id 位宽关系。
    fn verify_constants(&self) -> Result<(), RawModelError> {
        if self.schema != MARK_SCHEMA
            || self.profile != MARK_PROFILE_NAME
            || self.profile_revision != MARK_PROFILE_REVISION
            || self.credit_unit != MARK_CREDIT_UNIT
            || self.mailbox_consumers != MARK_MAILBOX_CONSUMERS
            || self.mailbox_shards != MARK_MAILBOX_SHARDS
            || self.credit_owner_bits != MARK_CREDIT_OWNER_BITS
            || self.credit_counter_bits != MARK_CREDIT_COUNTER_BITS
        {
            return Err(RawModelError::new(
                "mark profile 参数与登记值不一致，或参数未随 revision 变化",
            ));
        }
        if self.mailbox_consumers != 1 {
            return Err(RawModelError::new(
                "MarkMailbox 必须是每 owner 单 consumer，才能配合 queue-page grace",
            ));
        }
        if self.credit_owner_bits + self.credit_counter_bits != 32 {
            return Err(RawModelError::new(
                "credit id 的 owner 与 counter 位宽必须合计 32",
            ));
        }
        if self.credit_pool == 0 {
            return Err(RawModelError::new("credit 池上界必须为正"));
        }
        Ok(())
    }

    /// 校验 cycle 状态、credit 转移、来源与 snapshot 参与者目录。
    fn verify_catalogs(&self) -> Result<(), RawModelError> {
        if self.cycle_states != MARK_CYCLE_STATES {
            return Err(RawModelError::new("mark cycle 状态目录与规范不一致"));
        }
        if self.credit_transitions != MARK_CREDIT_TRANSITIONS {
            return Err(RawModelError::new("owner credit 转移目录与规范不一致"));
        }
        if self.credit_sources != CREDIT_SOURCE_NAMES {
            return Err(RawModelError::new(
                "owner credit 来源目录与 pacing 契约不一致",
            ));
        }
        if self.snapshot_participants != MARK_SNAPSHOT_PARTICIPANTS {
            return Err(RawModelError::new("root snapshot 参与者目录与规范不一致"));
        }
        Ok(())
    }

    /// 校验七个收敛条件与它们的来源绑定：并集必须恰好覆盖 `credit_sources`。
    fn verify_conditions(&self) -> Result<(), RawModelError> {
        if self.conditions.len() != MARK_CONVERGENCE_CONDITIONS.len() {
            return Err(RawModelError::new("收敛条件数量与规范不一致"));
        }
        let mut referenced: Vec<&str> = Vec::new();
        for (binding, expected) in self.conditions.iter().zip(MARK_CONVERGENCE_CONDITIONS) {
            if binding.condition != expected {
                return Err(RawModelError::new("收敛条件名与规范不一致"));
            }
            if binding.sources.is_empty() {
                return Err(RawModelError::new("收敛条件必须绑定至少一个 credit 来源"));
            }
            for source in &binding.sources {
                if !self.credit_sources.contains(source) {
                    return Err(RawModelError::new("收敛条件绑定了未登记的 credit 来源"));
                }
                referenced.push(source.as_str());
            }
        }
        referenced.sort_unstable();
        referenced.dedup();
        let mut catalog: Vec<&str> = self.credit_sources.iter().map(String::as_str).collect();
        catalog.sort_unstable();
        if referenced != catalog {
            return Err(RawModelError::new(
                "收敛条件来源并集必须恰好覆盖全部 credit 来源，既不遗漏也不重复绑定",
            ));
        }
        Ok(())
    }

    /// 校验 record 布局与 `MarkTicket` 字段集合。
    fn verify_layouts(&self) -> Result<(), RawModelError> {
        if self.records != fixed_layouts() {
            return Err(RawModelError::new("mark record 布局与 machine 布局不一致"));
        }
        if self.ticket_fields.schema != 1
            || self.ticket_fields.family() != MessageFamilyTag::MarkTicket
        {
            return Err(RawModelError::new(
                "MarkTicket 字段集合的 schema 或消息族不匹配",
            ));
        }
        self.ticket_fields
            .verify_family(MessageFamilyTag::MarkTicket)
            .map_err(|error| RawModelError::new(error.message().to_owned()))?;
        if self.ticket_field_count() != MARK_TICKET_FIELDS {
            return Err(RawModelError::new("MarkTicket 字段数与登记值不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        push_text(&mut bytes, &self.profile);
        bytes.extend_from_slice(&self.profile_revision.to_le_bytes());
        push_text(&mut bytes, &self.credit_unit);
        bytes.extend_from_slice(&self.mailbox_consumers.to_le_bytes());
        bytes.extend_from_slice(&self.mailbox_shards.to_le_bytes());
        bytes.extend_from_slice(&self.credit_owner_bits.to_le_bytes());
        bytes.extend_from_slice(&self.credit_counter_bits.to_le_bytes());
        bytes.extend_from_slice(&self.credit_pool.to_le_bytes());
        push_names(&mut bytes, &self.cycle_states);
        push_names(&mut bytes, &self.credit_transitions);
        push_names(&mut bytes, &self.credit_sources);
        for binding in &self.conditions {
            push_text(&mut bytes, &binding.condition);
            push_names(&mut bytes, &binding.sources);
        }
        push_names(&mut bytes, &self.snapshot_participants);
        for record in &self.records {
            push_text(&mut bytes, &record.name);
            bytes.extend_from_slice(&record.bytes.to_le_bytes());
            bytes.extend_from_slice(&record.alignment.to_le_bytes());
            for field in &record.fields {
                push_text(&mut bytes, &field.name);
                bytes.extend_from_slice(&field.offset.to_le_bytes());
                bytes.extend_from_slice(&field.bytes.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.ticket_fields.canonical_bytes());
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes
    }

    /// mark 契约的域隔离内容身份。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-mark-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回固定文本 dump；不含地址与宿主信息。
    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "mark schema={} profile={} revision={} credit-unit={}",
            self.schema, self.profile, self.profile_revision, self.credit_unit,
        )
        .expect("String写入");
        writeln!(output, "mark-cycle-states {}", self.cycle_states.join(",")).expect("String写入");
        writeln!(
            output,
            "mark-conditions {}",
            self.conditions
                .iter()
                .map(|binding| binding.condition.as_str())
                .collect::<Vec<_>>()
                .join(",")
        )
        .expect("String写入");
        for binding in &self.conditions {
            writeln!(
                output,
                "mark-condition-sources {} {}",
                binding.condition,
                binding.sources.join(",")
            )
            .expect("String写入");
        }
        writeln!(
            output,
            "mark-credit-sources {}",
            self.credit_sources.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "mark-credit-transitions {}",
            self.credit_transitions.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "mark-snapshot-participants {}",
            self.snapshot_participants.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "mark-mailbox consumers={} shards={}",
            self.mailbox_consumers, self.mailbox_shards
        )
        .expect("String写入");
        for record in &self.records {
            writeln!(
                output,
                "mark-record {} bytes={} align={}",
                record.name, record.bytes, record.alignment
            )
            .expect("String写入");
            for field in &record.fields {
                writeln!(
                    output,
                    "mark-field {}.{} offset={} bytes={}",
                    record.name, field.name, field.offset, field.bytes
                )
                .expect("String写入");
            }
        }
        writeln!(
            output,
            "mark-ticket-fields {}",
            self.ticket_fields.fields.len()
        )
        .expect("String写入");
        writeln!(
            output,
            "mark-demand root-sites={} barrier-sites={} ticket-sites={} edge-delta-sites={}",
            self.demand.root_sites,
            self.demand.barrier_sites,
            self.demand.ticket_sites,
            self.demand.edge_delta_sites,
        )
        .expect("String写入");
        writeln!(output, "mark-fingerprint {}", hex_lower(self.fingerprint)).expect("String写入");
        output
    }
}

/// 把名称目录展开成 `Vec<String>`。
fn names(directory: &[&str]) -> Vec<String> {
    directory.iter().map(|name| (*name).to_owned()).collect()
}

/// 收敛条件到 credit 来源的固定绑定表；顺序即 `MARK_CONVERGENCE_CONDITIONS`。
fn condition_bindings() -> Vec<MarkConditionBinding> {
    [
        ("local-worklist", &["mark-worklist"][..]),
        ("published-batch", &["producer-staging"][..]),
        ("mailbox", &["mark-mailbox"][..]),
        (
            "barrier-buffer",
            &["barrier-buffer", "card-mark-batch", "edge-delta"][..],
        ),
        ("producer-epoch", &["producer-staging"][..]),
        (
            "forwarding-work",
            &["forwarding-work", "pending-return"][..],
        ),
        ("pending-credit", &["mark-credit"][..]),
    ]
    .into_iter()
    .map(|(condition, sources)| MarkConditionBinding {
        condition: condition.to_owned(),
        sources: sources.iter().map(|source| (*source).to_owned()).collect(),
    })
    .collect()
}

/// 返回 mark 固定 record 目录；字段偏移与尺寸全部由 machine 布局派生。
fn fixed_layouts() -> Vec<MarkRecordLayout> {
    macro_rules! record {
        ($ty:ty; $($field:ident),+ $(,)?) => {
            MarkRecordLayout {
                name: stringify!($ty).to_owned(),
                bytes: u32::try_from(size_of::<$ty>()).expect("record适配u32"),
                alignment: u32::try_from(align_of::<$ty>()).expect("alignment适配u32"),
                fields: vec![$(MarkRecordField {
                    name: stringify!($field).to_owned(),
                    offset: u32::try_from(offset_of!($ty, $field)).expect("offset适配u32"),
                    bytes: field_size(|value: &$ty| value.$field),
                }),+],
            }
        };
    }
    vec![
        record!(MarkMailboxHead;
            owner_descriptor, cycle_epoch, topology_epoch, pending, consumed, forwarded,
            last_credit, consumer_slots, padding,
        ),
        record!(MarkCreditHead;
            owner_descriptor, cycle_epoch, granted, issued, in_flight, done, returned, padding,
        ),
        record!(MarkTerminationRecord; cycle_epoch, topology_epoch, state, conditions),
    ]
}

/// 用字段投影派生该字段的机器字节数，避免手写尺寸与结构体定义漂移。
fn field_size<T, F>(_project: fn(&T) -> F) -> u32 {
    u32::try_from(size_of::<F>()).expect("字段尺寸适配u32")
}

fn push_text(bytes: &mut Vec<u8>, text: &str) {
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(0);
}

fn push_names(bytes: &mut Vec<u8>, names: &[String]) {
    bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for name in names {
        push_text(bytes, name);
    }
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}
