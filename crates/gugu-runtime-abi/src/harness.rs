//! 压缩引用解码的稳定 fixture；GC 分配、标记、屏障和回收执行体全部位于 Gugu 镜像。

use super::cage::{CompressionPlane, is_canonical};
use super::cage_control::CageControlRecord;
use super::compression_schema::{
    CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract,
};
use super::gc_metadata_contract::GC_ARENA_BYTES;
use super::platform::{FakePlatform, PlatformProfile};
use super::slab::{RawInvariant, RuntimeSeed};
use crate::target::PointerCompression;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodeReferenceFixture {
    pub control: Vec<u8>,
    pub words: Vec<(u64, Option<u64>)>,
    pub decodes: u64,
    pub rejections: u64,
}

impl DecodeReferenceFixture {
    pub fn new() -> Result<Self, RawInvariant> {
        let contract = CompressionRuntimeContract::build(
            CompressionDemand {
                decode_sites: 1,
                compressed_root_slots: 1,
            },
            CompressionPolicyV1::cage(4 * GC_ARENA_BYTES),
            PointerCompression::x86_64(),
        )
        .map_err(|error| RawInvariant::new(error.message()))?;
        let base = super::cage_control::CAGE_CANONICAL_LIMIT - GC_ARENA_BYTES;
        let mut platform = FakePlatform::with_capacity(
            PlatformProfile::Linux,
            base,
            contract.cage_bytes(),
            RuntimeSeed::new(0x52),
        );
        let mut plane = CompressionPlane::new(&contract);
        plane.reserve(&mut platform)?;
        let record = CageControlRecord::from_plane(&plane)?;
        let descriptor = plane
            .cage_descriptor()
            .ok_or_else(|| RawInvariant::new("预留后缺少 cage 描述"))?;
        let shift_id = u32::try_from(contract.cage_id_shift()).expect("偏移适配 u32");
        let shift_generation =
            u32::try_from(contract.cage_generation_shift()).expect("偏移适配 u32");
        let pack = |id: u64, generation: u64, offset: u64| {
            (id << shift_id) | (generation << shift_generation) | offset
        };
        let generation = u64::from(descriptor.generation);
        let headroom = super::cage_control::CAGE_CANONICAL_LIMIT - descriptor.base;
        let candidates = [
            pack(0, generation, 0x40),
            contract.null_word(),
            pack(1, generation, 0x40),
            pack(0, generation + 1, 0x40),
            pack(0, generation, descriptor.len),
            pack(0, generation, headroom),
        ];
        let words = candidates
            .into_iter()
            .map(|word| {
                let decoded = plane.decode(word).ok().flatten();
                (word, decoded)
            })
            .collect::<Vec<_>>();
        if words.iter().any(|(_, address)| {
            address.is_some_and(|address| !is_canonical(address, contract.canonical_bits()))
        }) {
            return Err(RawInvariant::new("参照解码给出了非 canonical 地址"));
        }
        let stats = plane.stats();
        Ok(Self {
            control: record.to_bytes(),
            words,
            decodes: stats.decodes,
            rejections: stats.rejections,
        })
    }
}
