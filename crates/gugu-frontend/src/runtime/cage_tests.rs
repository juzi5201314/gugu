//! 压缩平面的确定性测试：island 切分、checked 解码、编码、generation 与 FFI 交接。
//!
//! 全部在 `FakePlatform` 上运行：cage 只预留虚拟地址，用例直接构造编码字，因此可以精确
//! 覆盖越界、过期、非 canonical 与关闭态路径。

use super::cage::{CompressionPlane, ForeignPin};
use super::compression_schema::{
    CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract,
};
use super::gc_metadata_contract::GC_ARENA_BYTES;
use super::platform::{FakePlatform, PlatformProfile};
use super::slab::RuntimeSeed;
use crate::target::PointerCompression;

/// 启用态契约：`4` 个 arena 粒度的 cage。
fn cage_contract(cage_bytes: u64) -> CompressionRuntimeContract {
    CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 1,
        },
        CompressionPolicyV1::cage(cage_bytes),
        PointerCompression::x86_64(),
    )
    .expect("启用态契约可构建")
}

/// 在容量足够的确定性平台上预留 `4` 个粒度的 cage。
fn reserved_plane() -> (CompressionPlane, FakePlatform) {
    let mut plane = CompressionPlane::new(&cage_contract(4 * GC_ARENA_BYTES));
    let mut platform = FakePlatform::new(PlatformProfile::Linux, u64::from(u32::MAX));
    plane.reserve(&mut platform).expect("cage 可预留");
    (plane, platform)
}

/// island 按粒度切分：容量不足时拒绝且不推进 bump。
#[test]
fn islands_are_granule_aligned_and_bounded() {
    let (mut plane, _platform) = reserved_plane();
    assert_eq!(plane.island_count(), 0);
    let first = plane.take_island(GC_ARENA_BYTES).expect("第一个 island");
    let second = plane
        .take_island(2 * GC_ARENA_BYTES)
        .expect("第二个 island");
    assert_eq!(second.offset, GC_ARENA_BYTES);
    assert_eq!(second.base, first.base + GC_ARENA_BYTES);
    assert_eq!(second.range, first.range);
    assert_eq!(plane.island_count(), 3);
    assert_eq!(
        plane
            .take_island(GC_ARENA_BYTES + 1)
            .expect_err("非粒度倍数必须拒绝")
            .message(),
        "island 字节数必须是 arena 粒度的整数倍"
    );
    assert_eq!(
        plane
            .take_island(2 * GC_ARENA_BYTES)
            .expect_err("容量不足必须拒绝")
            .message(),
        "cage 容量不足以容纳新的 managed arena"
    );
    assert_eq!(plane.island_count(), 3, "容量拒绝不得推进 bump");
}

/// 解码：空字、未登记 cage、过期 generation、越界 offset 与非 canonical 全覆盖。
#[test]
fn decode_covers_empty_stale_unknown_out_of_range_and_non_canonical() {
    let (mut plane, _platform) = reserved_plane();
    let descriptor = plane.descriptor(0).expect("cage 已登记");
    assert_eq!(plane.decode(0).expect("空字可解码"), None);
    assert_eq!(plane.decode_count(), 0, "空字不计入解码统计");
    let address = descriptor.base + 0x40;
    let word = plane.encode(address).expect("cage 内地址可编码");
    assert_eq!(plane.decode(word).expect("编码字可解码"), Some(address));
    assert_eq!(plane.decode_count(), 1);
    assert_eq!(
        plane
            .decode((9_u64 << 56) | (1_u64 << 32))
            .expect_err("未登记 cage 必须拒绝")
            .message(),
        "压缩引用 cage 未登记"
    );
    let generation = plane.advance_generation(0).expect("generation 可推进");
    assert_eq!(
        plane
            .decode(word)
            .expect_err("旧 generation 必须拒绝")
            .message(),
        "压缩引用 generation 过期"
    );
    let fresh = plane.encode(address).expect("新 generation 下地址可编码");
    assert_ne!(fresh, word);
    assert_eq!(plane.decode(fresh).expect("新编码字可解码"), Some(address));
    let beyond = (u64::from(generation) << 32) | descriptor.len;
    assert_eq!(
        plane
            .decode(beyond)
            .expect_err("越界 offset 必须拒绝")
            .message(),
        "压缩引用 offset 越过 cage 范围"
    );
    assert_eq!(plane.stats().decodes, 2);
    assert_eq!(plane.stats().rejections, 3);
}

/// cage 跨越 canonical 边界时，落在 hole 里的解码必须失败。
#[test]
fn decode_rejects_non_canonical_address() {
    let cage_bytes = 4 * GC_ARENA_BYTES;
    let mut plane = CompressionPlane::new(&cage_contract(cage_bytes));
    let base = (1_u64 << 47) - GC_ARENA_BYTES;
    let mut platform = FakePlatform::with_capacity(
        PlatformProfile::Linux,
        base,
        cage_bytes,
        RuntimeSeed::new(0x51),
    );
    plane.reserve(&mut platform).expect("边界 cage 可预留");
    let crossing = plane
        .encode(base + GC_ARENA_BYTES)
        .expect("hole 边界内的地址仍在 cage 内");
    assert_eq!(
        plane
            .decode(crossing)
            .expect_err("非 canonical 必须拒绝")
            .message(),
        "压缩引用解码出非 canonical 地址"
    );
    let inside = plane
        .encode(base + GC_ARENA_BYTES - 8)
        .expect("边界内地址可编码");
    assert!(plane.decode(inside).is_ok());
}

/// 编码：cage 外地址、越界 offset 与未登记 cage 都拒绝。
#[test]
fn encode_rejects_addresses_outside_the_cage() {
    let (mut plane, _platform) = reserved_plane();
    let descriptor = plane.descriptor(0).expect("cage 已登记");
    for outside in [0, descriptor.base + descriptor.len] {
        assert_eq!(
            plane
                .encode(outside)
                .expect_err("cage 外地址必须拒绝")
                .message(),
            "地址不在任何 cage 内，不能编码为压缩引用"
        );
    }
    assert_eq!(
        plane
            .encode_offset(0, descriptor.len)
            .expect_err("越界 offset 必须拒绝")
            .message(),
        "压缩引用 offset 越过 cage 范围"
    );
    assert_eq!(
        plane
            .encode_offset(7, 0)
            .expect_err("未登记 cage 必须拒绝")
            .message(),
        "压缩引用 cage 未登记"
    );
    let word = plane.encode(descriptor.base).expect("cage 基址可编码");
    assert_eq!(word, u64::from(descriptor.generation) << 32);
    assert_eq!(
        plane.descriptor(7).expect_err("未登记 cage").message(),
        "cage 编号未登记"
    );
}

/// generation 推进到上界后拒绝，未登记 cage 同样拒绝。
#[test]
fn generation_advancement_exhausts_and_rejects() {
    let mut contract = cage_contract(4 * GC_ARENA_BYTES);
    // 把上界收到起点加二：真实上界是 2^24 - 1，测试不必推进 1600 万次。
    contract.generation_max = contract.generation_min + 2;
    let mut plane = CompressionPlane::new(&contract);
    let mut platform = FakePlatform::new(PlatformProfile::Linux, u64::from(u32::MAX));
    plane.reserve(&mut platform).expect("cage 可预留");
    assert_eq!(
        plane.descriptor(0).expect("cage 已登记").generation,
        contract.generation_min
    );
    assert_eq!(
        plane.advance_generation(0).expect("第一次推进"),
        contract.generation_min + 1
    );
    assert_eq!(
        plane.advance_generation(0).expect("第二次推进"),
        contract.generation_min + 2
    );
    assert_eq!(
        plane
            .advance_generation(0)
            .expect_err("耗尽必须拒绝")
            .message(),
        "cage generation 已耗尽"
    );
    assert_eq!(
        plane
            .advance_generation(3)
            .expect_err("未登记 cage 必须拒绝")
            .message(),
        "cage 编号未登记"
    );
}

/// FFI 交接：pin/save/copy/release 全路径与统计六项。
#[test]
fn foreign_handoff_covers_pin_save_copy_and_release() {
    let (mut plane, _platform) = reserved_plane();
    let descriptor = plane.descriptor(0).expect("cage 已登记");
    let address = descriptor.base + 0x80;
    let pin = plane.pin_for_foreign(address).expect("cage 内地址可 pin");
    assert_eq!(plane.active_pin_count(), 1);
    let word = plane.encode(address).expect("cage 内地址可编码");
    assert_eq!(
        plane
            .handoff_word_for_foreign(word)
            .expect_err("压缩字不得直接交接")
            .message(),
        "压缩引用不能直接交给 native code，必须先 resolve+pin"
    );
    let copied = plane
        .copy_for_foreign(descriptor.base, &[1, 2, 3])
        .expect("cage 内地址可拷贝");
    assert_eq!(copied, vec![1, 2, 3]);
    assert_eq!(
        plane.save_for_foreign(pin).expect("活动 lease 可保存"),
        address
    );
    let forged = ForeignPin { sequence: 0, ..pin };
    assert_eq!(
        plane
            .save_for_foreign(forged)
            .expect_err("伪造 lease 必须拒绝")
            .message(),
        "FFI 保存缺少活动 pin lease"
    );
    plane.release_for_foreign(pin).expect("lease 可释放");
    assert_eq!(plane.active_pin_count(), 0);
    assert_eq!(
        plane
            .save_for_foreign(pin)
            .expect_err("已释放 lease 不得保存")
            .message(),
        "FFI 保存缺少活动 pin lease"
    );
    assert_eq!(
        plane
            .release_for_foreign(pin)
            .expect_err("重复释放必须拒绝")
            .message(),
        "FFI pin lease 已释放或不存在"
    );
    assert_eq!(
        plane
            .copy_for_foreign(0, &[0])
            .expect_err("cage 外地址不得拷贝")
            .message(),
        "cage 外地址不能复制给 native code"
    );
    assert_eq!(
        plane
            .pin_for_foreign(0)
            .expect_err("cage 外地址不得 pin")
            .message(),
        "cage 外地址不能建立 FFI pin"
    );
    let stats = plane.stats();
    assert_eq!(stats.foreign_pins, 1);
    assert_eq!(stats.foreign_saves, 1);
    assert_eq!(stats.foreign_copies, 1);
    assert_eq!(
        stats.foreign_rejections, 3,
        "直接交接、伪造 lease 与释放后保存各计一次拒绝"
    );
    assert_eq!(stats.decodes, 0);
}

/// generation 推进使旧 pin 失效。
#[test]
fn stale_generation_pin_cannot_be_saved() {
    let (mut plane, _platform) = reserved_plane();
    let address = plane.descriptor(0).expect("cage 已登记").base + 0x40;
    let pin = plane.pin_for_foreign(address).expect("cage 内地址可 pin");
    plane.advance_generation(0).expect("generation 可推进");
    assert_eq!(
        plane
            .save_for_foreign(pin)
            .expect_err("过期 generation 必须拒绝")
            .message(),
        "FFI 保存的 cage generation 已过期"
    );
}

/// 平台容量放不下 cage 时预留失败且不登记半成品 cage。
#[test]
fn reserve_fails_when_platform_capacity_is_short() {
    let mut plane = CompressionPlane::new(&cage_contract(4 * GC_ARENA_BYTES));
    let mut platform = FakePlatform::with_capacity(
        PlatformProfile::Linux,
        0x1000_0000,
        2 * GC_ARENA_BYTES,
        RuntimeSeed::new(0x21),
    );
    assert!(plane.reserve(&mut platform).is_err(), "容量不足必须拒绝");
    assert!(plane.descriptor(0).is_err(), "失败不得登记半成品 cage");
    // 非 canonical 基址必须拒绝：2^47 已越过正半区。
    let mut plane = CompressionPlane::new(&cage_contract(4 * GC_ARENA_BYTES));
    let mut platform = FakePlatform::with_capacity(
        PlatformProfile::Linux,
        1_u64 << 47,
        4 * GC_ARENA_BYTES,
        RuntimeSeed::new(0x22),
    );
    assert_eq!(
        plane
            .reserve(&mut platform)
            .expect_err("非 canonical 基址必须拒绝")
            .message(),
        "cage 预留落在非 canonical 地址空间"
    );
}

/// 关闭 profile：reserve 是 no-op，其余 cage 操作全部拒绝。
#[test]
fn disabled_profile_rejects_every_cage_operation() {
    let contract = CompressionRuntimeContract::build(
        CompressionDemand::default(),
        CompressionPolicyV1::disabled(),
        PointerCompression::x86_64(),
    )
    .expect("关闭态契约可构建");
    let mut plane = CompressionPlane::new(&contract);
    let mut platform = FakePlatform::new(PlatformProfile::Linux, u64::from(u32::MAX));
    plane
        .reserve(&mut platform)
        .expect("关闭态 reserve 是 no-op");
    assert!(!plane.enabled());
    assert!(plane.descriptor(0).is_err(), "关闭态不得登记 cage");
    assert_eq!(plane.island_count(), 0);
    assert_eq!(plane.active_pin_count(), 0);
    assert_eq!(
        plane
            .take_island(GC_ARENA_BYTES)
            .expect_err("关闭态不得切 island")
            .message(),
        "未启用 cage profile 时不能切分 managed arena"
    );
    assert_eq!(
        plane.encode(0x1000).expect_err("关闭态不可编码").message(),
        "未启用 cage profile 时不能编码压缩引用"
    );
    assert_eq!(
        plane
            .encode_offset(0, 0)
            .expect_err("关闭态不可按 offset 编码")
            .message(),
        "压缩引用 cage 未登记"
    );
    assert_eq!(
        plane.decode(0x40).expect_err("关闭态不可解码").message(),
        "压缩引用 cage 未登记"
    );
    assert_eq!(
        plane
            .pin_for_foreign(0x1000)
            .expect_err("关闭态不得 pin")
            .message(),
        "未启用 cage profile 时不存在压缩 FFI 交接"
    );
    assert_eq!(
        plane
            .copy_for_foreign(0x1000, &[1])
            .expect_err("关闭态不得拷贝")
            .message(),
        "cage 外地址不能复制给 native code"
    );
    let stats = plane.stats();
    assert_eq!(stats.rejections, 1, "关闭态的解码尝试同样计入拒绝");
    assert_eq!(stats.decodes, 0);
    assert_eq!(stats.foreign_pins, 0);
    assert_eq!(stats.foreign_saves, 0);
    assert_eq!(stats.foreign_copies, 0);
    assert_eq!(stats.foreign_rejections, 0);
}
