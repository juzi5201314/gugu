//! 栈尺寸策略、共享 relocation 传输与迟滞收缩；架构专属消费方位于子模块。

use super::coroutine::{COLD_COMPACTED, CoroutineContext, CoroutineState, StackDescriptor};
use super::provider::{FaultClass, ProviderError};
use super::startup_schema::FatalKind;

#[path = "stack_a64.rs"]
pub mod a64;

pub const STACK_ARENA_BYTES: usize = 256 * 1024 * 1024;
pub const STACK_SPAN_BYTES: usize = 2 * 1024 * 1024;
pub const STACK_SPANS: usize = 128;
pub const STACK_CLASSES: [usize; 13] = [
    512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152,
];
pub const STACK_CACHE_CLASSES: usize = 7;
pub const STACK_CACHE_LIMIT: usize = 64 * 1024;
pub const STACK_CACHE_LOW: usize = 32 * 1024;
pub const STACK_REFILL_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StackError {
    Overflow,
    Platform(ProviderError),
    Invariant(&'static str),
}

impl StackError {
    pub fn fatal(self) -> FatalKind {
        match self {
            Self::Overflow => FatalKind::StackOverflow,
            Self::Platform(error) if error.fault_class() != FaultClass::RuntimeInvariant => {
                FatalKind::OutOfMemory
            }
            Self::Platform(_) | Self::Invariant(_) => FatalKind::RuntimeInvariant,
        }
    }
}

impl std::fmt::Display for StackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overflow => f.write_str("协程所需尺寸阶梯超过逻辑栈上限"),
            Self::Platform(error) => write!(f, "协程栈平台分配失败：{error}"),
            Self::Invariant(message) => f.write_str(message),
        }
    }
}

impl From<ProviderError> for StackError {
    fn from(error: ProviderError) -> Self {
        Self::Platform(error)
    }
}

pub fn class_ceil(required: usize, limit: usize) -> Result<usize, StackError> {
    required
        .max(512)
        .checked_next_power_of_two()
        .filter(|bytes| *bytes <= limit)
        .ok_or(StackError::Overflow)
}

pub fn initial_capacity(entry_required: usize, limit: usize) -> Result<usize, StackError> {
    let required = entry_required
        .checked_add(512)
        .ok_or(StackError::Overflow)?;
    class_ceil(required.max(2048), limit)
}

pub fn growth_capacity(
    old: usize,
    used: usize,
    frame: usize,
    limit: usize,
) -> Result<usize, StackError> {
    let double = old.checked_mul(2).ok_or(StackError::Overflow)?;
    let required = used
        .checked_add(frame)
        .and_then(|value| value.checked_add(512))
        .ok_or(StackError::Overflow)?;
    class_ceil(double.max(required).max(2048), limit)
}

/// 只在取得 scan lock 前判定；采样在独占 system-stack 操作内完成。
pub fn shrink_capacity(
    descriptor: &mut StackDescriptor,
    state: CoroutineState,
    used: usize,
    epoch: u32,
    pressure: bool,
) -> Option<usize> {
    if !matches!(state, CoroutineState::Runnable | CoroutineState::Waiting) {
        return None;
    }
    let high_water = descriptor.recent_high_water.max(used);
    descriptor.recent_high_water = used;
    let quiet = epoch.checked_sub(descriptor.last_grow_gc_epoch)?;
    let enough_epochs = if pressure { quiet >= 1 } else { quiet >= 4 };
    if quiet == 0 || high_water.checked_add(512)? > descriptor.capacity / 4 {
        descriptor.low_use_gc_cycles = 0;
        return None;
    }
    descriptor.low_use_gc_cycles = descriptor.low_use_gc_cycles.saturating_add(1);
    if !enough_epochs || !pressure && descriptor.low_use_gc_cycles < 4 {
        return None;
    }
    let (headroom, floor) = if state == CoroutineState::Waiting {
        (256, 512)
    } else {
        (512, 2048)
    };
    let capacity = class_ceil(used.checked_add(headroom)?.max(floor), descriptor.capacity).ok()?;
    (capacity < descriptor.capacity).then_some(capacity)
}

pub fn record_resize(descriptor: &mut StackDescriptor, epoch: u32, grew: bool) {
    descriptor.low_use_gc_cycles = 0;
    if grew {
        descriptor.last_grow_gc_epoch = epoch;
        descriptor.flags &= !COLD_COMPACTED;
    } else if descriptor.capacity < 2048 {
        descriptor.flags |= COLD_COMPACTED;
    }
}

/// 已使用字节的确定性传输对象。root offsets 由上游精确 scanner 提供，不能猜测整数或raw pointer。
#[derive(Debug, Default)]
pub struct StackImage {
    pub bytes: Vec<u8>,
    /// 从已使用范围起点计算的8字节 StackInterior slot，严格升序且不重叠。
    pub stack_roots: Vec<u32>,
    /// context中rbx/rbp/r12/r13四个寄存器的StackInterior位；其它位非法。
    pub stack_registers: u8,
}

impl StackImage {
    pub fn relocate(
        &self,
        context: CoroutineContext,
        old: &StackDescriptor,
        new_low: usize,
        new_capacity: usize,
    ) -> Result<(Self, CoroutineContext), StackError> {
        let relocation = Relocation::new(old, context.rsp, new_low, new_capacity)?;
        if relocation.used != self.bytes.len() || self.stack_registers & !15 != 0 {
            return Err(StackError::Invariant(
                "栈复制范围或寄存器map与context不一致",
            ));
        }
        let bytes = relocation.copy(&self.bytes, &self.stack_roots)?;
        let mut saved = [context.rbx, context.rbp, context.r12, context.r13];
        for (index, word) in saved.iter_mut().enumerate() {
            if self.stack_registers & (1 << index) != 0 {
                *word = relocation.pointer(*word)?;
            }
        }
        Ok((
            Self {
                bytes,
                stack_roots: self.stack_roots.clone(),
                stack_registers: self.stack_registers,
            },
            CoroutineContext {
                rsp: relocation.sp,
                rbx: saved[0],
                rbp: saved[1],
                r12: saved[2],
                r13: saved[3],
                ..context
            },
        ))
    }
}

struct Relocation<'a> {
    old: &'a StackDescriptor,
    low: usize,
    high: usize,
    sp: usize,
    used: usize,
}

impl<'a> Relocation<'a> {
    fn new(
        old: &'a StackDescriptor,
        sp: usize,
        low: usize,
        capacity: usize,
    ) -> Result<Self, StackError> {
        let used = old
            .used(sp)
            .map_err(|_| StackError::Invariant("复制栈的rsp越界"))?;
        if used > capacity {
            return Err(StackError::Invariant(
                "栈复制范围或寄存器map与context不一致",
            ));
        }
        let high = low.checked_add(capacity).ok_or(StackError::Overflow)?;
        Ok(Self {
            old,
            low,
            high,
            sp: high - used,
            used,
        })
    }

    fn pointer(&self, value: usize) -> Result<usize, StackError> {
        if value == 0 {
            return Ok(0);
        }
        if !(self.old.stack_low..self.old.stack_high).contains(&value) {
            return Err(StackError::Invariant("StackInterior不在旧栈allocation内"));
        }
        self.high
            .checked_sub(self.old.stack_high - value)
            .filter(|value| *value >= self.low && *value < self.high)
            .ok_or(StackError::Invariant("StackInterior在新栈allocation之外"))
    }

    fn copy(&self, bytes: &[u8], offsets: &[u32]) -> Result<Vec<u8>, StackError> {
        let mut end = 0;
        let mut result = bytes.to_vec();
        if bytes.len() != self.used {
            return Err(StackError::Invariant(
                "栈复制范围或寄存器map与context不一致",
            ));
        }
        for offset in offsets {
            let offset = *offset as usize;
            if offset < end
                || !offset.is_multiple_of(8)
                || offset.checked_add(8).is_none_or(|end| end > bytes.len())
            {
                return Err(StackError::Invariant(
                    "StackInterior slot越界、重叠或未对齐",
                ));
            }
            end = offset + 8;
            let word = u64::from_le_bytes(bytes[offset..end].try_into().expect("机器字"));
            let word =
                usize::try_from(word).map_err(|_| StackError::Invariant("指针宽度不匹配"))?;
            result[offset..end].copy_from_slice(&(self.pointer(word)? as u64).to_le_bytes());
        }
        Ok(result)
    }
}
