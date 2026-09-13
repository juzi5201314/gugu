//! 栈图契约段：根种类、safepoint kind、寄存器编号与 section 常量。
//!
//! 本段把栈图规范的种类名、safepoint kind 数值、通用寄存器位号、flags 位定义与
//! section header 常量固定成带版本的对象，与 `scheduler_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。逻辑函数、
//! 安全点与根字数的需求视图来自优化后 LIR 的栈图推导；真实 `code_rva`、
//! `pc_offset`、`frame_size` 与寄存器掩码由后端在分配后填充，本段只固定其
//! 编码规则，不登记具体机器布局。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::model::RawModelError;

/// 栈图契约段的 schema 版本。
pub(crate) const STACKMAP_SCHEMA: u32 = 1;
/// 栈图 section 的主版本；decoder 拒收 version 1 记录。
pub(crate) const STACKMAP_SECTION_VERSION: u16 = 2;
/// 栈图 section 魔数。
pub(crate) const STACKMAP_MAGIC: &[u8; 8] = b"GUGUSM01";
/// 栈图 section 指针宽度（字节）。
pub(crate) const STACKMAP_POINTER_SIZE: u8 = 8;
/// 栈图 section 字节序标记：1 表示小端。
pub(crate) const STACKMAP_ENDIAN: u8 = 1;

/// 逻辑根种类名：顺序即判别值 0..4。
pub(crate) const ROOT_KIND_NAMES: [&str; 5] = [
    "heap-direct",
    "heap-interior",
    "shared-handle",
    "compressed-ref",
    "stack-interior",
];

/// safepoint kind 名（下标即数值）：0=CallReturn、1=PollResume、2=SuspendResume、
/// 3=ForeignBridge、4=MorestackEntry。
pub(crate) const SAFEPOINT_KIND_NAMES: [&str; 5] = [
    "call-return",
    "poll-resume",
    "suspend-resume",
    "foreign-bridge",
    "morestack-entry",
];

/// 通用寄存器位号：bit0..14 对应 rax、rbx、rcx、rdx、rsi、rdi、rbp、r8、r9、r10、
/// r11、r12、r13、r14、r15；bit15 保留为 0。r14 与 r15 为 runtime 保留寄存器，
/// 普通函数的对应位必须为 0。
pub(crate) const REGISTER_NAMES: [&str; 15] = [
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "r8", "r9", "r10", "r11", "r12", "r13", "r14",
    "r15",
];
/// 普通函数禁止占用的保留寄存器位。
pub(crate) const RESERVED_REGISTER_BITS: u16 = (1 << 13) | (1 << 14);

/// 从优化后 LIR 推导的栈图需求视图，不是机器布局的行数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StackMapDemand {
    /// 逻辑函数记录数。
    pub functions: u32,
    /// 逻辑安全点记录数。
    pub safepoints: u32,
    /// 全局去重后的 map 记录数。
    pub maps: u32,
    /// `CallReturn` 记录数。
    pub call_return: u32,
    /// `PollResume` 记录数。
    pub poll_resume: u32,
    /// `SuspendResume` 记录数。
    pub suspend_resume: u32,
    /// `ForeignBridge` 记录数（含 dirty）。
    pub foreign_bridge: u32,
    /// `MorestackEntry` 记录数。
    pub morestack_entry: u32,
    /// 纯分配操作站点数（无独立记录，供后端核对）。
    pub alloc_sites: u32,
    /// 屏障操作站点数（无独立记录，供后端核对）。
    pub barrier_sites: u32,
    /// 五类根的字数合计。
    pub root_words: u32,
    /// 存在 unwind 边因而携带落地摘要的函数数。
    pub functions_with_landing: u32,
}

/// 已验证的栈图 runtime 契约；版本变化使 RuntimeRawModel 和 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StackMapRuntimeContract {
    /// 契约段 schema 版本。
    pub schema: u32,
    /// 根种类名（顺序即判别值）。
    pub root_kinds: Vec<String>,
    /// safepoint kind 名（下标即数值）。
    pub safepoint_kinds: Vec<String>,
    /// 通用寄存器名（下标即位号）。
    pub registers: Vec<String>,
    /// section 魔数文本。
    pub magic: String,
    /// section 主版本。
    pub section_version: u16,
    /// section 指针宽度。
    pub pointer_size: u8,
    /// section 字节序标记。
    pub endian: u8,
    /// 上游 LIR 需求。
    pub demand: StackMapDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl StackMapRuntimeContract {
    /// 返回上游需求视图。
    pub(crate) fn demand(&self) -> StackMapDemand {
        self.demand
    }

    pub(crate) fn build(demand: StackMapDemand) -> Result<Self, RawModelError> {
        let contract = Self {
            schema: STACKMAP_SCHEMA,
            root_kinds: ROOT_KIND_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            safepoint_kinds: SAFEPOINT_KIND_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            registers: REGISTER_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            magic: String::from_utf8_lossy(STACKMAP_MAGIC).into_owned(),
            section_version: STACKMAP_SECTION_VERSION,
            pointer_size: STACKMAP_POINTER_SIZE,
            endian: STACKMAP_ENDIAN,
            demand,
            fingerprint: [0; 32],
        };
        contract.verify()?;
        let fingerprint = contract.compute_fingerprint();
        let mut contract = contract;
        contract.fingerprint = fingerprint;
        Ok(contract)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != STACKMAP_SCHEMA {
            return Err(RawModelError::new("栈图契约 schema 版本不一致"));
        }
        if self.root_kinds != ROOT_KIND_NAMES {
            return Err(RawModelError::new("栈图根种类目录必须是五类固定顺序"));
        }
        if self.safepoint_kinds != SAFEPOINT_KIND_NAMES {
            return Err(RawModelError::new("栈图 safepoint kind 目录未登记"));
        }
        if self.registers != REGISTER_NAMES {
            return Err(RawModelError::new("栈图寄存器编号与规范不一致"));
        }
        if self.magic.as_bytes() != STACKMAP_MAGIC
            || self.section_version != STACKMAP_SECTION_VERSION
            || self.pointer_size != STACKMAP_POINTER_SIZE
            || self.endian != STACKMAP_ENDIAN
        {
            return Err(RawModelError::new("栈图 section 常量与规范不一致"));
        }
        let counted = self
            .demand
            .call_return
            .saturating_add(self.demand.poll_resume)
            .saturating_add(self.demand.suspend_resume)
            .saturating_add(self.demand.foreign_bridge)
            .saturating_add(self.demand.morestack_entry);
        if counted != self.demand.safepoints {
            return Err(RawModelError::new("栈图 kind 分类计数与安全点总数不一致"));
        }
        if self.demand.maps > self.demand.safepoints {
            return Err(RawModelError::new("去重 map 数量不得超过安全点数量"));
        }
        if self.demand.functions_with_landing > self.demand.functions {
            return Err(RawModelError::new("落地函数数量不得超过函数总数"));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("栈图契约可序列化")
    }

    /// 栈图契约的域隔离内容身份。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-stackmap-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "stackmap schema={} section-version={} functions={} safepoints={} maps={} roots={} fingerprint={}",
            self.schema,
            self.section_version,
            self.demand.functions,
            self.demand.safepoints,
            self.demand.maps,
            self.demand.root_words,
            hex(&self.fingerprint),
        )
        .expect("String写入");
        writeln!(output, "stackmap-root-kinds {}", self.root_kinds.join(",")).expect("String写入");
        writeln!(
            output,
            "stackmap-safepoint-kinds {}",
            self.safepoint_kinds.join(",")
        )
        .expect("String写入");
        writeln!(
            output,
            "stackmap-kinds call-return={} poll-resume={} suspend-resume={} foreign-bridge={} morestack-entry={} alloc-sites={} barrier-sites={} functions-with-landing={}",
            self.demand.call_return,
            self.demand.poll_resume,
            self.demand.suspend_resume,
            self.demand.foreign_bridge,
            self.demand.morestack_entry,
            self.demand.alloc_sites,
            self.demand.barrier_sites,
            self.demand.functions_with_landing,
        )
        .expect("String写入");
        output
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}
