//! 栈尺寸策略、精确 relocation 传输与迟滞收缩；不解析机器 stack map。

use super::coroutine::{COLD_COMPACTED, CoroutineContext, CoroutineState, StackDescriptor};
use super::provider::{FaultClass, ProviderError};
use super::startup_schema::FatalKind;

pub(crate) const STACK_ARENA_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const STACK_SPAN_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const STACK_SPANS: usize = 128;
pub(crate) const STACK_CLASSES: [usize; 13] = [
    512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152,
];
pub(crate) const STACK_CACHE_CLASSES: usize = 7;
pub(crate) const STACK_CACHE_LIMIT: usize = 64 * 1024;
pub(crate) const STACK_CACHE_LOW: usize = 32 * 1024;
pub(crate) const STACK_REFILL_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StackError {
    Overflow,
    Platform(ProviderError),
    Invariant(&'static str),
}

impl StackError {
    pub(crate) fn fatal(self) -> FatalKind {
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

pub(crate) fn class_ceil(required: usize, limit: usize) -> Result<usize, StackError> {
    required
        .max(512)
        .checked_next_power_of_two()
        .filter(|bytes| *bytes <= limit)
        .ok_or(StackError::Overflow)
}

pub(crate) fn initial_capacity(entry_required: usize, limit: usize) -> Result<usize, StackError> {
    let required = entry_required
        .checked_add(512)
        .ok_or(StackError::Overflow)?;
    class_ceil(required.max(2048), limit)
}

pub(crate) fn growth_capacity(
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
pub(crate) fn shrink_capacity(
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

pub(crate) fn record_resize(descriptor: &mut StackDescriptor, epoch: u32, grew: bool) {
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
pub(crate) struct StackImage {
    pub(crate) bytes: Vec<u8>,
    /// 从已使用范围起点计算的8字节 StackInterior slot，严格升序且不重叠。
    pub(crate) stack_roots: Vec<u32>,
    /// context中rbx/rbp/r12/r13四个寄存器的StackInterior位；其它位非法。
    pub(crate) stack_registers: u8,
}

impl StackImage {
    pub(crate) fn relocate(
        &self,
        context: CoroutineContext,
        old: &StackDescriptor,
        new_low: usize,
        new_capacity: usize,
    ) -> Result<(Self, CoroutineContext), StackError> {
        let used = old
            .used(context.rsp)
            .map_err(|_| StackError::Invariant("复制栈的rsp越界"))?;
        if used != self.bytes.len() || used > new_capacity || self.stack_registers & !15 != 0 {
            return Err(StackError::Invariant(
                "栈复制范围或寄存器map与context不一致",
            ));
        }
        let high = new_low
            .checked_add(new_capacity)
            .ok_or(StackError::Overflow)?;
        let new_rsp = high - used;
        let relocate = |value: usize| -> Result<usize, StackError> {
            if value == 0 {
                return Ok(0);
            }
            if !(old.stack_low..old.stack_high).contains(&value) {
                return Err(StackError::Invariant("StackInterior不在旧栈allocation内"));
            }
            high.checked_sub(old.stack_high - value)
                .filter(|value| *value >= new_low && *value < high)
                .ok_or(StackError::Invariant("StackInterior在新栈allocation之外"))
        };
        let mut previous_end = 0;
        for offset in &self.stack_roots {
            let offset = usize::try_from(*offset).expect("u32 offset适配目标");
            if offset < previous_end
                || !offset.is_multiple_of(8)
                || offset.checked_add(8).is_none_or(|end| end > used)
            {
                return Err(StackError::Invariant(
                    "StackInterior slot越界、重叠或未对齐",
                ));
            }
            previous_end = offset + 8;
            let word =
                u64::from_le_bytes(self.bytes[offset..offset + 8].try_into().expect("机器字"));
            relocate(usize::try_from(word).map_err(|_| StackError::Invariant("指针宽度不匹配"))?)?;
        }
        let mut saved = [context.rbx, context.rbp, context.r12, context.r13];
        for (index, word) in saved.iter_mut().enumerate() {
            if self.stack_registers & (1 << index) != 0 {
                *word = relocate(*word)?;
            }
        }
        let mut bytes = self.bytes.clone();
        for offset in &self.stack_roots {
            let offset = usize::try_from(*offset).expect("offset适配目标");
            let word =
                usize::from_le_bytes(bytes[offset..offset + 8].try_into().expect("x86_64机器字"));
            bytes[offset..offset + 8].copy_from_slice(&relocate(word)?.to_le_bytes());
        }
        Ok((
            Self {
                bytes,
                stack_roots: self.stack_roots.clone(),
                stack_registers: self.stack_registers,
            },
            CoroutineContext {
                rsp: new_rsp,
                rbx: saved[0],
                rbp: saved[1],
                r12: saved[2],
                r13: saved[3],
                ..context
            },
        ))
    }
}
