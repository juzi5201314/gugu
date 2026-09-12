//! channel / Join / select 等待契约；backend 和 CLI 只消费已验证对象。
//!
//! 本段把等待源种类、wait-node 字段、FIFO 队列、select scratch class、`SelectTxn` 相位
//! 与 winner 编码固定成带版本的对象，与 `scheduler_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。生产环境的
//! `select_rng` 种子按 BLAKE3 从闭世界启动熵派生；确定性测试注入固定状态，禁止读 OS 熵。

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::mem::{align_of, offset_of, size_of};

use super::RAW_CLASS_LADDER;
use super::channel::ChannelControl;
use super::model::RawModelError;
use super::platform::PlatformProfile;
use super::wait::{SelectScratchCache, SelectTxn, WaitNode};

/// `case_count <= 8` 时走同时 `try_lock` 路径。
pub(crate) const INLINE_SELECT_CASES: u32 = 8;
/// processor-local scratch cache 的 0 号 class 是内联 8 个 `u64`。
pub(crate) const INLINE_SCRATCH_WORDS: u32 = 8;
/// scratch 溢出 class：1, 2, 4, …, 1024 个 word。
pub(crate) const SCRATCH_WORD_CLASSES: [u32; 11] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];
/// wait-node 走 raw class 阶梯的典型尺寸。
pub(crate) const WAIT_NODE_CLASSES: [u32; 2] = [64, 128];
/// wait-node 的规范字节数。
pub(crate) const WAIT_NODE_BYTES: u32 = 64;
/// processor-local select scratch cache 累计字节上限。
pub(crate) const SELECT_SCRATCH_CACHE_BYTES: u64 = 65536;
/// 等待契约段的 schema 版本。
pub(crate) const WAIT_SCHEMA: u32 = 1;
/// winner：尚未提交。
pub(crate) const WINNER_UNSET: u64 = 0;
/// winner：default 臂。
pub(crate) const WINNER_DEFAULT: u64 = 1;
/// winner 编码里 case 的起点：`2 + case_index`。
pub(crate) const WINNER_CASE_BASE: u64 = 2;
/// `SelectTxn` 相位：正在登记。
pub(crate) const SELECT_PHASE_BUILDING: u64 = 0;
/// `SelectTxn` 相位：已经武装，可被 waker 提交。
pub(crate) const SELECT_PHASE_ARMED: u64 = 1;

/// 从优化后 LIR 推导的等待需求，不是运行时对象数量。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WaitDemand {
    /// `RuntimeCall::ChannelNew` 次数。
    pub channel_new: u32,
    /// `RuntimeCall::ChannelClose` 次数。
    pub channel_close: u32,
    /// `RuntimeCall::ChannelSend` 次数。
    pub channel_send: u32,
    /// `RuntimeCall::ChannelReceive` 次数。
    pub channel_receive: u32,
    /// `RuntimeCall::ChannelTrySend` 次数。
    pub channel_try_send: u32,
    /// `RuntimeCall::ChannelTryRecv` 次数。
    pub channel_try_recv: u32,
    /// `RuntimeCall::JoinWait` 次数。
    pub join_wait: u32,
    /// `RuntimeCall::SelectCommit` 次数。
    pub select_commit: u32,
    /// 无 case 且无 default 的 never select 次数。
    pub never_select: u32,
    /// `SafepointKind::Select` 次数。
    pub select_safepoints: u32,
}

impl WaitDemand {
    /// channel 相关调用合计。
    pub const fn channel_ops(self) -> u32 {
        self.channel_new
            + self.channel_close
            + self.channel_send
            + self.channel_receive
            + self.channel_try_send
            + self.channel_try_recv
    }
}

/// 一个固定 runtime 字段，offset 相对其所属 record。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WaitFieldLayout {
    /// 字段名。
    pub name: String,
    /// 字节偏移。
    pub offset: u32,
}

/// compiler 与 typed visitor 共用的等待 record 布局。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WaitRecordLayout {
    /// record 名。
    pub name: String,
    /// record 字节数。
    pub bytes: u32,
    /// record 对齐。
    pub alignment: u32,
    /// 声明顺序的字段偏移。
    pub fields: Vec<WaitFieldLayout>,
}

/// 已验证的等待 runtime 契约；版本变化使 RuntimeRawModel 和 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WaitRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// 同时 `try_lock` 路径的 case 上限。
    pub inline_select_cases: u32,
    /// processor-local 0 号 class 的内联 word 数。
    pub inline_scratch_words: u32,
    /// scratch 溢出 class（word）。
    pub scratch_word_classes: Vec<u32>,
    /// wait-node 可用的 raw class 字节。
    pub wait_node_classes: Vec<u32>,
    /// wait-node 规范字节数。
    pub wait_node_bytes: u32,
    /// processor-local scratch cache 累计上限。
    pub select_scratch_cache_bytes: u64,
    /// 来源 profile 名。
    pub profile: String,
    /// 等待源种类目录。
    pub source_kinds: Vec<String>,
    /// wait-node 字段目录；禁止裸栈指针。
    pub node_fields: Vec<String>,
    /// `SelectTxn` 相位名。
    pub select_phases: Vec<String>,
    /// winner 编码名。
    pub winner_codes: Vec<String>,
    /// 固定 record 目录。
    pub records: Vec<WaitRecordLayout>,
    /// 上游 LIR 需求。
    pub demand: WaitDemand,
}

impl WaitRuntimeContract {
    /// 返回内部 schema 版本。
    pub fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回内联 select case 上限。
    pub fn inline_select_cases(&self) -> u32 {
        self.inline_select_cases
    }

    /// 返回 scratch class 数量（不含 0 号内联 class）。
    pub fn scratch_class_count(&self) -> u32 {
        u32::try_from(self.scratch_word_classes.len()).expect("class 数量适配 u32")
    }

    /// 返回 wait-node class 数量。
    pub fn wait_node_class_count(&self) -> u32 {
        u32::try_from(self.wait_node_classes.len()).expect("class 数量适配 u32")
    }

    /// 返回 scratch cache 字节上限。
    pub fn select_scratch_cache_bytes(&self) -> u64 {
        self.select_scratch_cache_bytes
    }

    pub(crate) fn build(
        demand: WaitDemand,
        profile: PlatformProfile,
    ) -> Result<Self, RawModelError> {
        let contract = Self {
            schema: WAIT_SCHEMA,
            inline_select_cases: INLINE_SELECT_CASES,
            inline_scratch_words: INLINE_SCRATCH_WORDS,
            scratch_word_classes: SCRATCH_WORD_CLASSES.to_vec(),
            wait_node_classes: WAIT_NODE_CLASSES.to_vec(),
            wait_node_bytes: WAIT_NODE_BYTES,
            select_scratch_cache_bytes: profile.constants().select_scratch_cache_bytes,
            profile: profile.name().to_owned(),
            source_kinds: vec!["channel".into(), "join".into(), "never".into()],
            node_fields: vec![
                "coroutine_index".into(),
                "coroutine_generation".into(),
                "wait_generation".into(),
                "case_index".into(),
                "payload_offset".into(),
                "result_offset".into(),
                "next".into(),
                "flags".into(),
            ],
            select_phases: vec!["building".into(), "armed".into()],
            winner_codes: vec!["unset".into(), "default".into(), "case".into()],
            records: fixed_layouts(),
            demand,
        };
        contract.verify()?;
        Ok(contract)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != WAIT_SCHEMA
            || self.inline_select_cases != INLINE_SELECT_CASES
            || self.inline_scratch_words != INLINE_SCRATCH_WORDS
            || self.scratch_word_classes != SCRATCH_WORD_CLASSES
            || self.wait_node_classes != WAIT_NODE_CLASSES
            || self.wait_node_bytes != WAIT_NODE_BYTES
        {
            return Err(RawModelError::new(
                "等待契约的 select/scratch/wait-node 常量与登记值不一致",
            ));
        }
        let profile = PlatformProfile::ALL
            .into_iter()
            .find(|profile| profile.name() == self.profile)
            .ok_or_else(|| RawModelError::new("等待契约引用未登记的 profile"))?;
        if self.select_scratch_cache_bytes != profile.constants().select_scratch_cache_bytes
            || self.select_scratch_cache_bytes != SELECT_SCRATCH_CACHE_BYTES
        {
            return Err(RawModelError::new(
                "select scratch cache 上限与 profile 常量不一致",
            ));
        }
        if self.source_kinds != ["channel", "join", "never"] {
            return Err(RawModelError::new(
                "等待源种类目录必须是 channel/join/never",
            ));
        }
        if self.node_fields.iter().any(|field| {
            field.contains("pointer") || field.contains("ptr") || field.contains("rsp")
        }) {
            return Err(RawModelError::new("wait-node 禁止保存裸栈指针"));
        }
        if self.winner_codes != ["unset", "default", "case"]
            || WINNER_UNSET != 0
            || WINNER_DEFAULT != 1
            || WINNER_CASE_BASE != 2
        {
            return Err(RawModelError::new("winner 编码必须是 UNSET/DEFAULT/case"));
        }
        if self.select_phases != ["building", "armed"]
            || SELECT_PHASE_BUILDING != 0
            || SELECT_PHASE_ARMED != 1
        {
            return Err(RawModelError::new("SelectTxn 相位必须是 Building/Armed"));
        }
        for class in &self.wait_node_classes {
            if !RAW_CLASS_LADDER.contains(class) {
                return Err(RawModelError::new("wait-node class 不在 raw class 阶梯"));
            }
        }
        if u32::try_from(size_of::<WaitNode>()).expect("WaitNode 适配 u32") != WAIT_NODE_BYTES {
            return Err(RawModelError::new("WaitNode 布局字节数与契约不一致"));
        }
        if u32::try_from(size_of::<SelectTxn>()).expect("SelectTxn 适配 u32") != 32 {
            return Err(RawModelError::new(
                "SelectTxn 描述符必须是 CoroutineCold.select_scratch 的 32 字节",
            ));
        }
        if u32::try_from(size_of::<SelectScratchCache>()).expect("cache 适配 u32") != 64 {
            return Err(RawModelError::new(
                "SelectScratchCache 必须是 64 字节 0 号 class",
            ));
        }
        if self.records != fixed_layouts() {
            return Err(RawModelError::new("等待 record 布局与 machine 布局不一致"));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("等待契约可序列化")
    }

    /// 等待契约的域隔离内容身份。
    pub fn fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-wait-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "wait schema={} inline-select-cases={} scratch-classes={} wait-node-classes={} scratch-cache-bytes={} profile={}",
            self.schema,
            self.inline_select_cases,
            self.scratch_class_count(),
            self.wait_node_class_count(),
            self.select_scratch_cache_bytes,
            self.profile,
        )
        .expect("String写入");
        writeln!(output, "wait-source-kinds {}", self.source_kinds.join(",")).expect("String写入");
        writeln!(output, "wait-node-fields {}", self.node_fields.join(",")).expect("String写入");
        writeln!(
            output,
            "wait-select phases={} winners={}",
            self.select_phases.join(","),
            self.winner_codes.join(","),
        )
        .expect("String写入");
        for record in &self.records {
            writeln!(
                output,
                "wait-record {} bytes={} align={}",
                record.name, record.bytes, record.alignment
            )
            .expect("String写入");
            for field in &record.fields {
                writeln!(
                    output,
                    "wait-field {}.{} offset={}",
                    record.name, field.name, field.offset
                )
                .expect("String写入");
            }
        }
        writeln!(
            output,
            "wait-demand channel-new={} channel-close={} channel-send={} channel-receive={} channel-try-send={} channel-try-recv={} join-wait={} select-commit={} never-select={} select-safepoints={}",
            self.demand.channel_new,
            self.demand.channel_close,
            self.demand.channel_send,
            self.demand.channel_receive,
            self.demand.channel_try_send,
            self.demand.channel_try_recv,
            self.demand.join_wait,
            self.demand.select_commit,
            self.demand.never_select,
            self.demand.select_safepoints,
        )
        .expect("String写入");
        output
    }
}

fn fixed_layouts() -> Vec<WaitRecordLayout> {
    macro_rules! record {
        ($ty:ty; $($field:ident),+ $(,)?) => {
            WaitRecordLayout {
                name: stringify!($ty).to_owned(),
                bytes: u32::try_from(size_of::<$ty>()).expect("record适配u32"),
                alignment: u32::try_from(align_of::<$ty>()).expect("alignment适配u32"),
                fields: vec![$(WaitFieldLayout {
                    name: stringify!($field).to_owned(),
                    offset: u32::try_from(offset_of!($ty, $field)).expect("offset适配u32"),
                }),+],
            }
        };
    }
    vec![
        record!(ChannelControl; source_id, generation, capacity, closed, head, tail, len, send_q_head, recv_q_head, reservation_generation, ring_index, ring_generation, padding),
        record!(WaitNode; coroutine_index, coroutine_generation, wait_generation, case_index, payload_offset, result_offset, next, flags),
        record!(SelectTxn; phase_winner, case_count, scratch_handle, wait_block),
        record!(SelectScratchCache; inline_words),
    ]
}
