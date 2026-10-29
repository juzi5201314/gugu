//! 调度调优 profile：target 描述符把它的摘要纳入目标身份，runtime 调度契约从它派生常量。

/// 高争用元数据的填充粒度。
pub const QUEUE_PAD_BYTES: u64 = 128;
/// x86_64 两目标的 cache line 字节数。
pub const CACHE_LINE_BYTES: u64 = 64;

/// local deque 的编码变体；profile 选定一种，release 镜像不生成运行时 mode 分支。
///
/// 当前 profile 选择 `Classic64`；`Packed55` 由调度参照实现登记，供后续 profile 校准选择。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalDequeMode {
    /// 经典 64-bit ticket 变体 `Classic64Deque`。
    Classic64,
    /// 单 `u64` 打包 55-bit steal ticket 与 9-bit 距离的 `Packed55Deque`。
    Packed55,
}

/// 调优 profile 的 revision；任何字段变更都属于 backend schema 变更。
pub const TUNING_PROFILE_REVISION: u32 = 1;

/// 调度调优参数；[`RUNTIME_TUNING_PROFILE`] 是唯一来源，其余常量从这里派生。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTuningProfile {
    /// local deque 的编码变体。
    pub deque_mode: LocalDequeMode,
    /// 本地队列容量。
    pub local_capacity: u32,
    /// remote batch head 分片数。
    pub remote_shards: u32,
    /// 单个 batch 的 item 上限。
    pub batch_max: u32,
    /// external service 的 tick 间隔。
    pub service_interval: u32,
    /// 一次 external service 至多写入 local 的项数。
    pub service_batch: u32,
    /// 高争用元数据的填充粒度。
    pub queue_pad_bytes: u64,
    /// 缓存行字节数。
    pub cache_line_bytes: u64,
    /// profile revision。
    pub revision: u32,
}

/// 当前登记的调度调优 profile。
pub const RUNTIME_TUNING_PROFILE: RuntimeTuningProfile = RuntimeTuningProfile {
    deque_mode: LocalDequeMode::Classic64,
    local_capacity: 256,
    remote_shards: 8,
    batch_max: 128,
    service_interval: 61,
    service_batch: 128,
    queue_pad_bytes: QUEUE_PAD_BYTES,
    cache_line_bytes: CACHE_LINE_BYTES,
    revision: TUNING_PROFILE_REVISION,
};

impl RuntimeTuningProfile {
    /// 域隔离的内容身份。
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-runtime-tuning-profile-v1");
        hasher.update(&[match self.deque_mode {
            LocalDequeMode::Classic64 => 0,
            LocalDequeMode::Packed55 => 1,
        }]);
        hasher.update(&self.local_capacity.to_le_bytes());
        hasher.update(&self.remote_shards.to_le_bytes());
        hasher.update(&self.batch_max.to_le_bytes());
        hasher.update(&self.service_interval.to_le_bytes());
        hasher.update(&self.service_batch.to_le_bytes());
        hasher.update(&self.queue_pad_bytes.to_le_bytes());
        hasher.update(&self.cache_line_bytes.to_le_bytes());
        hasher.update(&self.revision.to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}
