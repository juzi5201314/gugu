//! 协程布局、栈策略与换栈代码的同源契约；backend和CLI只消费已验证对象。

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::mem::offset_of;

use super::context::ContextSwitchCode;
use super::coroutine::{
    CoroutineCold, CoroutineContext, CoroutineHot, CoroutineSlot, MorestackScratch, StackDescriptor,
};
use super::model::RawModelError;
use super::stack::{
    STACK_ARENA_BYTES, STACK_CACHE_CLASSES, STACK_CACHE_LIMIT, STACK_CACHE_LOW, STACK_CLASSES,
    STACK_REFILL_LIMIT, STACK_SPAN_BYTES,
};

/// 从真实LIR推导的协程机器边界需求，不是运行时协程数量。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CoroutineDemand {
    /// 已保留的协程创建操作数。
    pub creation_sites: u32,
    /// 需要入口stack check的具体实例数。
    pub checked_entries: u32,
    /// 显式挂起或切换statepoint数。
    pub suspend_points: u32,
}

/// 一个固定runtime字段，offset相对其所属record。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CoroutineFieldLayout {
    /// 字段名。
    pub name: String,
    /// 字节偏移。
    pub offset: u32,
}

/// compiler、typed visitor、调试器与汇编共用的record布局。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CoroutineRecordLayout {
    /// record名。
    pub name: String,
    /// record字节数。
    pub bytes: u32,
    /// record对齐。
    pub alignment: u32,
    /// 声明顺序的字段偏移。
    pub fields: Vec<CoroutineFieldLayout>,
}

/// 栈arena与有界cache的固定策略。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StackPolicy {
    /// payload arena字节数，不包含两端guard。
    pub arena_bytes: u64,
    /// 单span字节数。
    pub span_bytes: u64,
    /// arena两端各自的guard字节数。
    pub guard_bytes: u64,
    /// slot的二次幂尺寸阶梯。
    pub classes: Vec<u64>,
    /// processor-local cache拥有的class数。
    pub cache_classes: u32,
    /// 本地cache高水位。
    pub cache_limit_bytes: u64,
    /// 超过高水位后归还到此水位。
    pub cache_low_bytes: u64,
    /// 一次refill的传输上限。
    pub refill_bytes: u64,
    /// 新建和增长后的容量下界。
    pub initial_floor_bytes: u64,
    /// 初始frame和增长保留的headroom。
    pub headroom_bytes: u64,
    /// 常规收缩需要的连续完整GC观察窗。
    pub shrink_windows: u32,
}

impl StackPolicy {
    fn fixed() -> Self {
        Self {
            arena_bytes: u64::try_from(STACK_ARENA_BYTES).expect("arena适配u64"),
            span_bytes: u64::try_from(STACK_SPAN_BYTES).expect("span适配u64"),
            guard_bytes: 4096,
            classes: STACK_CLASSES
                .iter()
                .map(|bytes| u64::try_from(*bytes).expect("class适配u64"))
                .collect(),
            cache_classes: u32::try_from(STACK_CACHE_CLASSES).expect("7个class"),
            cache_limit_bytes: u64::try_from(STACK_CACHE_LIMIT).expect("cache适配u64"),
            cache_low_bytes: u64::try_from(STACK_CACHE_LOW).expect("cache适配u64"),
            refill_bytes: u64::try_from(STACK_REFILL_LIMIT).expect("refill适配u64"),
            initial_floor_bytes: 2048,
            headroom_bytes: 512,
            shrink_windows: 4,
        }
    }
}

/// 已验证的协程runtime契约；版本变化使RuntimeRawModel和action key失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CoroutineRuntimeContract {
    /// 内部schema版本。
    pub schema: u32,
    /// 从CoroutineHot基址读取stack_check的绝对偏移。
    pub stack_check_offset: u32,
    /// 固定record目录。
    pub records: Vec<CoroutineRecordLayout>,
    /// 固定栈策略。
    pub stack: StackPolicy,
    /// x86_64换栈实际代码。
    pub context: ContextSwitchCode,
    /// 上游LIR需求。
    pub demand: CoroutineDemand,
}

/// 从`CoroutineHot`基址读取`stack_check`的绝对偏移；backend热路与契约共用这一处来源。
pub(crate) const fn stack_check_offset() -> u32 {
    (offset_of!(CoroutineSlot, stack) + offset_of!(StackDescriptor, stack_check)) as u32
}

impl CoroutineRuntimeContract {
    pub(crate) fn build(demand: CoroutineDemand) -> Result<Self, RawModelError> {
        let result = Self {
            schema: 1,
            stack_check_offset: stack_check_offset(),
            records: fixed_layouts(),
            stack: StackPolicy::fixed(),
            context: ContextSwitchCode::fixed(),
            demand,
        };
        result.verify()?;
        Ok(result)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != 1
            || self.stack_check_offset != 64
            || self.records != fixed_layouts()
            || self.stack != StackPolicy::fixed()
            || self.context != ContextSwitchCode::fixed()
        {
            return Err(RawModelError::new(
                "协程布局、栈策略或context代码与runtime/backend schema不匹配",
            ));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("协程契约可序列化")
    }

    /// 协程契约的域隔离内容身份。
    pub fn fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-coroutine-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "coroutine schema={} stack-check-offset={} context-bytes={} restore-offset={}",
            self.schema,
            self.stack_check_offset,
            self.context.bytes.len(),
            self.context.restore_offset
        )
        .expect("String写入");
        for record in &self.records {
            writeln!(
                output,
                "coroutine-layout {} bytes={} align={}",
                record.name, record.bytes, record.alignment
            )
            .expect("String写入");
            for field in &record.fields {
                writeln!(
                    output,
                    "coroutine-field {}.{} offset={}",
                    record.name, field.name, field.offset
                )
                .expect("String写入");
            }
        }
        writeln!(output, "stack-arena payload={} span={} guards=2x{} initial-floor={} cache-limit={} cache-low={} refill={}", self.stack.arena_bytes, self.stack.span_bytes, self.stack.guard_bytes, self.stack.initial_floor_bytes, self.stack.cache_limit_bytes, self.stack.cache_low_bytes, self.stack.refill_bytes).expect("String写入");
        writeln!(
            output,
            "coroutine-demand create={} checked-entries={} suspend={}",
            self.demand.creation_sites, self.demand.checked_entries, self.demand.suspend_points
        )
        .expect("String写入");
        output
    }
}

fn fixed_layouts() -> Vec<CoroutineRecordLayout> {
    macro_rules! record {
        ($ty:ty; $($field:ident),+ $(,)?) => {
            CoroutineRecordLayout {
                name: stringify!($ty).to_owned(),
                bytes: u32::try_from(size_of::<$ty>()).expect("record适配u32"),
                alignment: u32::try_from(align_of::<$ty>()).expect("alignment适配u32"),
                fields: vec![$(CoroutineFieldLayout { name: stringify!($field).to_owned(), offset: u32::try_from(offset_of!($ty, $field)).expect("offset适配u32") }),+],
            }
        };
    }
    vec![
        record!(CoroutineHot; state, run_link_next, current_processor, preferred_processor, preferred_processor_id, wait_word, cold_index, run_batch_len),
        record!(StackDescriptor; stack_check, stack_low, stack_high, capacity, recent_high_water, last_grow_gc_epoch, low_use_gc_cycles, flags),
        record!(CoroutineSlot; hot, stack),
        record!(CoroutineContext; rsp, rip, rbx, rbp, r12, r13),
        record!(MorestackScratch; return_pc, gpr, xmm),
        record!(CoroutineCold; id, context, morestack_scratch, wait_record, foreign_bridge, join_state, coroutine_locals, panic_state, select_rng, select_scratch, gc_scan_epoch),
    ]
}
