//! raw slab 的 dense size class 表与 stride 除法常量。
//!
//! `RuntimeSizeClass` 按 dense id 索引，不使用映射容器；每个 class 记录 payload、stride、
//! 对齐、每 span 的 slot 数与 metadata bytes、link 复用能力、clear/poison 策略和所属
//! adapter domain。只有 layout、drop、scan 与 owner 规则完全相同的记录才允许物理复用
//! free list，`verify` 拒绝同 stride 但策略不同的 class。

use serde::{Deserialize, Serialize};

use super::RAW_SLAB_PAGE_BYTES;
use super::slab::{MemoryDomainId, RawInvariant};

/// slot 头部保留字节；link 与状态字占用这段区域，payload 从其后开始。
pub(crate) const SLOT_HEADER_BYTES: u32 = 8;

/// intrusive link 所需的字节数（编码后的 64-bit link）。
pub(crate) const LINK_BYTES: u32 = 8;

/// 复用 slot 前必须清除的字段位。
pub(crate) struct ClearField;
impl ClearField {
    /// pointer 或 descriptor 字段。
    pub(crate) const POINTER: u32 = 1;
    /// length 或 capacity 字段。
    pub(crate) const LENGTH: u32 = 2;
    /// integrity secret 索引或 token 副本。
    pub(crate) const SECRET: u32 = 4;
    /// resource 状态或 lease 计数。
    pub(crate) const RESOURCE_STATE: u32 = 8;
}

/// 一个 raw size class 的 dense 编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub(crate) struct RuntimeSizeClassId(u16);

impl RuntimeSizeClassId {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u16 {
        self.0
    }

    /// 由编号原值还原。
    pub(crate) const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// 返回作为表下标的编号。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// class 的 drop/scan 不变量；不同的策略禁止共享 free list。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum DropScanPolicy {
    /// managed plane 的对象；raw slab 不使用。
    Managed,
    /// 不含 managed pointer 的 raw 记录。
    RawNoPointers,
    /// 由 lease 计数与 close 状态线性化的 resource slot。
    ResourceLease,
}

/// 一个 raw slab size class 的完整描述。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RuntimeSizeClass {
    pub(crate) id: RuntimeSizeClassId,
    pub(crate) payload_bytes: u32,
    pub(crate) slot_stride: u32,
    pub(crate) alignment: u32,
    pub(crate) slots_per_span: u32,
    pub(crate) metadata_bytes: u32,
    /// slot 前置区域能否承载 free-list link。
    pub(crate) link_usable: bool,
    pub(crate) clear_mask: u32,
    pub(crate) poison: bool,
    pub(crate) policy: DropScanPolicy,
    pub(crate) domain: MemoryDomainId,
}

impl RuntimeSizeClass {
    /// 返回该 class 的 stride 除法常量。
    pub(crate) fn division(&self) -> StrideDivision {
        StrideDivision::new(self.slot_stride)
    }

    /// 返回一个 slot 的 payload 起始偏移。
    pub(crate) const fn payload_offset(&self) -> u32 {
        SLOT_HEADER_BYTES
    }

    /// 返回给定 slot 编号在 span 内的字节偏移。
    pub(crate) fn slot_offset(&self, index: u32) -> u64 {
        u64::from(index) * u64::from(self.slot_stride)
    }

    /// 由 span 内字节偏移精确反推 slot 编号。
    pub(crate) fn exact_index(&self, offset: u64) -> u32 {
        u32::try_from(offset / u64::from(self.slot_stride)).expect("slot 编号适配 u32")
    }
}

/// 非二次幂 stride 的 reciprocal 除法常量。
///
/// `reciprocal` 取 `ceil(2^64 / stride)`；对本文档规定的使用域（span 内偏移，最大
/// `RAW_SLAB_PAGE_BYTES`，且 stride ≤ `u32::MAX`），当 `n * (reciprocal * stride - 2^64)
/// < 2^64` 时 `index_of(n)` 与精确除法等价，`verify` 以穷举方式确认这一点。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StrideDivision {
    reciprocal: u64,
    shift: u32,
}

impl StrideDivision {
    /// 为给定 stride 生成常量；stride 为 0 时使用 1 以避免除零。
    pub(crate) fn new(stride: u32) -> Self {
        let divisor = u128::from(stride.max(1));
        let reciprocal = (1_u128 << 64).div_ceil(divisor);
        Self {
            reciprocal: u64::try_from(reciprocal).unwrap_or(u64::MAX),
            shift: 64,
        }
    }

    /// 返回 reciprocal 常量。
    pub(crate) const fn reciprocal(&self) -> u64 {
        self.reciprocal
    }

    /// 返回移位位数。
    pub(crate) const fn shift(&self) -> u32 {
        self.shift
    }

    /// 用乘法与移位求 `offset / stride`；调用者必须已用 `verify` 确认使用域。
    pub(crate) fn index_of(&self, offset: u64) -> u64 {
        ((u128::from(offset) * u128::from(self.reciprocal)) >> self.shift) as u64
    }

    /// 在使用域与边界样本上穷举校验常量与精确除余等价。
    pub(crate) fn verify(&self, stride: u32) -> Result<(), RawInvariant> {
        let stride = u64::from(stride);
        if stride == 0 {
            return Err(RawInvariant::new("size class stride 不能为 0"));
        }
        let domain = RAW_SLAB_PAGE_BYTES.max(stride + 1);
        for offset in 0..=domain {
            if self.index_of(offset) != offset / stride {
                return Err(RawInvariant::new(format!(
                    "stride {stride} 的 reciprocal 除法在偏移 {offset} 偏离精确除法"
                )));
            }
            if self.index_of(offset) * stride > offset {
                return Err(RawInvariant::new(format!(
                    "stride {stride} 的 reciprocal 除法在偏移 {offset} 越过 slot 边界"
                )));
            }
        }
        for sample in [
            0,
            1,
            stride - 1,
            stride,
            stride + 1,
            u64::from(u32::MAX) - 1,
            u64::from(u32::MAX),
        ] {
            if self.index_of(sample) != sample / stride {
                return Err(RawInvariant::new(format!(
                    "stride {stride} 的 reciprocal 除法在边界样本 {sample} 偏离精确除法"
                )));
            }
        }
        Ok(())
    }
}

/// 按 dense id 索引的 raw size class 表。
///
/// 表是编译期阶梯加显式追加的固定序列；下标访问与规范排序都依赖稠密编号，因此不使用
/// 任何映射容器，`verify` 会拒绝非稠密编号。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RuntimeSizeClassTable {
    classes: Vec<RuntimeSizeClass>,
}

impl RuntimeSizeClassTable {
    /// 由规范阶梯构造 raw plane 的 class 表。
    pub(crate) fn ladder(domain: MemoryDomainId) -> Result<Self, RawInvariant> {
        let classes = super::RAW_CLASS_LADDER
            .iter()
            .enumerate()
            .map(|(index, stride)| RuntimeSizeClass {
                id: RuntimeSizeClassId(u16::try_from(index).expect("阶梯长度适配 u16")),
                payload_bytes: stride - SLOT_HEADER_BYTES,
                slot_stride: *stride,
                alignment: (*stride).min(64),
                slots_per_span: u32::try_from(RAW_SLAB_PAGE_BYTES / u64::from(*stride))
                    .expect("每 span slot 数适配 u32"),
                metadata_bytes: u32::try_from(RAW_SLAB_PAGE_BYTES % u64::from(*stride))
                    .expect("span metadata 适配 u32"),
                link_usable: *stride >= LINK_BYTES,
                clear_mask: ClearField::POINTER
                    | ClearField::LENGTH
                    | ClearField::SECRET
                    | ClearField::RESOURCE_STATE,
                poison: true,
                policy: DropScanPolicy::RawNoPointers,
                domain,
            })
            .collect();
        let table = Self { classes };
        table.verify()?;
        Ok(table)
    }

    /// 用显式 class 序列构造表；编号必须由调用者稠密分配。
    pub(crate) fn from_classes(classes: Vec<RuntimeSizeClass>) -> Result<Self, RawInvariant> {
        let table = Self { classes };
        table.verify()?;
        Ok(table)
    }

    /// 返回规范顺序的 class 序列。
    pub(crate) fn classes(&self) -> &[RuntimeSizeClass] {
        &self.classes
    }

    /// 按稠密编号取 class。
    pub(crate) fn get(&self, id: RuntimeSizeClassId) -> Option<&RuntimeSizeClass> {
        self.classes.get(id.index())
    }

    /// 返回能容纳给定 payload 与对齐要求的最小 class。
    pub(crate) fn lookup(&self, payload_bytes: u32, alignment: u32) -> Option<&RuntimeSizeClass> {
        self.classes
            .iter()
            .find(|class| class.payload_bytes >= payload_bytes && class.alignment >= alignment)
    }

    /// 返回 class 表的规范编码，供指纹使用。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.classes.len() * 32);
        bytes.extend_from_slice(&(self.classes.len() as u32).to_le_bytes());
        for class in &self.classes {
            bytes.extend_from_slice(&class.id.raw().to_le_bytes());
            bytes.extend_from_slice(&class.payload_bytes.to_le_bytes());
            bytes.extend_from_slice(&class.slot_stride.to_le_bytes());
            bytes.extend_from_slice(&class.alignment.to_le_bytes());
            bytes.extend_from_slice(&class.slots_per_span.to_le_bytes());
            bytes.extend_from_slice(&class.metadata_bytes.to_le_bytes());
            bytes.push(u8::from(class.link_usable));
            bytes.extend_from_slice(&class.clear_mask.to_le_bytes());
            bytes.push(u8::from(class.poison));
            bytes.push(class.policy as u8);
            bytes.push(class.domain.raw());
        }
        bytes
    }

    /// 校验稠密编号、阶梯一致性、link 能力、策略唯一性与 stride 除法常量。
    pub(crate) fn verify(&self) -> Result<(), RawInvariant> {
        if self.classes.is_empty() {
            return Err(RawInvariant::new("raw size class 表不能为空"));
        }
        for (index, class) in self.classes.iter().enumerate() {
            if class.id.index() != index {
                return Err(RawInvariant::new(format!(
                    "size class 编号不稠密：下标 {index} 存放 {}",
                    class.id.raw()
                )));
            }
            if class.payload_bytes == 0 || class.slot_stride < class.payload_bytes {
                return Err(RawInvariant::new("size class 的 payload 超出 slot stride"));
            }
            if class.slot_stride <= SLOT_HEADER_BYTES {
                return Err(RawInvariant::new("size class 的 stride 不足 slot 头部"));
            }
            if !class.alignment.is_power_of_two() || class.slot_stride % class.alignment != 0 {
                return Err(RawInvariant::new("size class 的 alignment 非法"));
            }
            let expected_slots = u32::try_from(RAW_SLAB_PAGE_BYTES / u64::from(class.slot_stride))
                .expect("每 span slot 数适配 u32");
            let expected_metadata =
                u32::try_from(RAW_SLAB_PAGE_BYTES % u64::from(class.slot_stride))
                    .expect("span metadata 适配 u32");
            if class.slots_per_span != expected_slots || class.metadata_bytes != expected_metadata {
                return Err(RawInvariant::new(
                    "size class 的每 span slot 数或 metadata bytes 与页布局不一致",
                ));
            }
            if class.link_usable != (class.slot_stride >= LINK_BYTES) {
                return Err(RawInvariant::new(
                    "size class 的 link 复用能力与 stride 不一致",
                ));
            }
            if matches!(class.policy, DropScanPolicy::Managed) {
                return Err(RawInvariant::new("raw size class 不能登记 managed 策略"));
            }
            class.division().verify(class.slot_stride)?;
        }
        for (index, class) in self.classes.iter().enumerate() {
            for other in &self.classes[index + 1..] {
                if class.slot_stride == other.slot_stride
                    && (class.policy != other.policy
                        || class.clear_mask != other.clear_mask
                        || class.poison != other.poison
                        || class.domain != other.domain)
                {
                    return Err(RawInvariant::new(format!(
                        "stride {} 的 size class 之间 drop/scan 或 owner 规则不同",
                        class.slot_stride
                    )));
                }
            }
        }
        Ok(())
    }
}
