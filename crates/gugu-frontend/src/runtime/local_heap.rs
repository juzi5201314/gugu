//! 编译器与共享协议共用的 managed block 身份和对象头编码常量。
//!
//! LocalHeap 的分配、扫描、搬迁与回收已经下沉到 `resources/runtime/*.gg`；这里不保留
//! 宿主执行算法，只保留消息/codec/后端仍需的稳定身份和值编码。

use super::local_heap_schema::HEAP_BLOCKS_PER_ARENA;
use super::slab::RawInvariant;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct ManagedBlockId(pub(crate) u32);

impl ManagedBlockId {
    pub(crate) fn new(descriptor: u32, block: u32) -> Result<Self, RawInvariant> {
        if block >= HEAP_BLOCKS_PER_ARENA {
            return Err(RawInvariant::new("arena 的 block 下标越过契约上界"));
        }
        let value = descriptor
            .checked_mul(HEAP_BLOCKS_PER_ARENA)
            .and_then(|base| base.checked_add(block))
            .ok_or_else(|| RawInvariant::new("managed block 身份溢出"))?;
        Ok(Self(value))
    }

    pub(crate) const fn arena(self) -> u32 {
        self.0 / HEAP_BLOCKS_PER_ARENA
    }

    pub(crate) const fn block(self) -> u32 {
        self.0 % HEAP_BLOCKS_PER_ARENA
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct BlockRef {
    pub(crate) id: ManagedBlockId,
    pub(crate) generation: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapArenaKind {
    Nursery = 0,
    Old = 1,
    Resource = 2,
    Pinned = 3,
    Large = 4,
}

impl HeapArenaKind {
    pub(crate) const COUNT: usize = 5;
}

pub(crate) const GENERATION_NURSERY: u8 = 0;
pub(crate) const GENERATION_AGING: u8 = 1;
pub(crate) const GENERATION_OLD: u8 = 2;
pub(crate) const GENERATION_IMMORTAL: u8 = 3;

pub(crate) const CONTROL_TYPE_MASK: u64 = 0xffff_ffff;
pub(crate) const CONTROL_GENERATION_SHIFT: u32 = 36;
pub(crate) const CONTROL_REPRESENTATION_SHIFT: u32 = 43;
pub(crate) const REPRESENTATION_LOCAL_DIRECT: u64 = 0;
pub(crate) const REPRESENTATION_TURN_REGION: u64 = 1;
pub(crate) const REPRESENTATION_SHARED_HANDLE: u64 = 2;
pub(crate) const REPRESENTATION_COMPRESSED_REF: u64 = 3;
pub(crate) const OBJECT_HEADER_BYTES: u64 = 16;

/// 后端和 metadata verifier 共用的 header 镜像；不执行对象操作。
pub(crate) const HEADER_ENCODING_MIRROR: [(&str, u64); 8] = [
    ("OBJECT_HEADER_BYTES", OBJECT_HEADER_BYTES),
    ("CONTROL_TYPE_MASK", CONTROL_TYPE_MASK),
    ("CONTROL_GENERATION_SHIFT", CONTROL_GENERATION_SHIFT as u64),
    (
        "CONTROL_REPRESENTATION_SHIFT",
        CONTROL_REPRESENTATION_SHIFT as u64,
    ),
    ("REPRESENTATION_LOCAL_DIRECT", REPRESENTATION_LOCAL_DIRECT),
    ("REPRESENTATION_TURN_REGION", REPRESENTATION_TURN_REGION),
    ("REPRESENTATION_SHARED_HANDLE", REPRESENTATION_SHARED_HANDLE),
    (
        "REPRESENTATION_COMPRESSED_REF",
        REPRESENTATION_COMPRESSED_REF,
    ),
];
