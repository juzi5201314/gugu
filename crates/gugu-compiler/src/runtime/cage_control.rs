//! cage 控制记录：机器解码序列读取的固定布局。
//!
//! `DecodeCompressedRef` 的 lowering 通过 `Operand::Rip(RelocTarget::CageControl, offset)`
//! 直接读取本记录的字段；字段表 [`CAGE_CONTROL_FIELDS`] 是机器序列与 Rust 记录之间的
//! 唯一契约，`CompressionRuntimeContract::verify` 逐项核对它与 `offset_of!`/`size_of`。

use crate::runtime::cage::CompressionPlane;
use crate::runtime::slab::RawInvariant;

/// 48 位 canonical 正半区上界：地址必须严格小于它才是 canonical 正半区地址。
pub(crate) const CAGE_CANONICAL_LIMIT: u64 = 1 << 47;

/// world 只登记一个 cage，见压缩契约。
pub(crate) const CAGE_CONTROL_ID: u8 = 0;

/// 控制记录字段表：`(名, offset, size)`；顺序即机器解码序列的读取顺序。
///
/// offset/size 与 `CageControlRecord` 的 `repr(C)` 布局一致，由契约 verifier 用
/// `offset_of!`/`size_of` 逐项核对。
pub(crate) const CAGE_CONTROL_FIELDS: [(&str, u32, u32); 7] = [
    ("generation", 0, 4),
    ("cage_id", 4, 1),
    ("base", 8, 8),
    ("len", 16, 8),
    ("canonical_headroom", 24, 8),
    ("decodes", 32, 8),
    ("rejections", 40, 8),
];

/// 机器解码序列读取的控制记录。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct CageControlRecord {
    /// 当前 generation。
    pub generation: u32,
    /// cage 编号。
    pub cage_id: u8,
    /// 显式 padding，保证字段表与 `repr(C)` 布局稳定。
    pub(crate) padding: [u8; 3],
    /// cage 基址。
    pub base: u64,
    /// cage 字节数；offset 必须严格小于它。
    pub len: u64,
    /// `CAGE_CANONICAL_LIMIT - base`：offset 必须严格小于它才落在 canonical 正半区。
    pub canonical_headroom: u64,
    /// 成功解码计数。
    pub decodes: u64,
    /// 拒绝计数。
    pub rejections: u64,
}

impl CageControlRecord {
    /// 从参照平面读取控制记录；没有已预留 cage 时报错。
    pub(crate) fn from_plane(plane: &CompressionPlane) -> Result<Self, RawInvariant> {
        let cage = plane
            .cage_descriptor()
            .ok_or_else(|| RawInvariant::new("控制记录需要已预留的 cage"))?;
        let stats = plane.stats();
        Ok(Self {
            generation: cage.generation,
            cage_id: CAGE_CONTROL_ID,
            padding: [0; 3],
            base: cage.base,
            len: cage.len,
            canonical_headroom: CAGE_CANONICAL_LIMIT.saturating_sub(cage.base),
            decodes: stats.decodes,
            rejections: stats.rejections,
        })
    }

    /// 返回记录的 `repr(C)` 小端字节；机器序列按同一布局读取。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(std::mem::size_of::<Self>());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.push(self.cage_id);
        bytes.extend_from_slice(&self.padding);
        bytes.extend_from_slice(&self.base.to_le_bytes());
        bytes.extend_from_slice(&self.len.to_le_bytes());
        bytes.extend_from_slice(&self.canonical_headroom.to_le_bytes());
        bytes.extend_from_slice(&self.decodes.to_le_bytes());
        bytes.extend_from_slice(&self.rejections.to_le_bytes());
        bytes
    }

    /// 按名查字段 `(offset, size)`。
    pub fn field(name: &str) -> Option<(u32, u32)> {
        CAGE_CONTROL_FIELDS
            .iter()
            .find(|(field, ..)| *field == name)
            .map(|(_, offset, size)| (*offset, *size))
    }

    /// 返回记录的字节数。
    pub fn byte_len() -> u32 {
        u32::try_from(std::mem::size_of::<Self>()).expect("控制记录尺寸适配 u32")
    }

    /// 返回记录的对齐。
    pub fn byte_align() -> u32 {
        u32::try_from(std::mem::align_of::<Self>()).expect("控制记录对齐适配 u32")
    }
}
