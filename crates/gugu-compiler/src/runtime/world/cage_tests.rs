//! cage profile 的世界级接入：island arena、压缩根 map、root slice/seed 与 FFI 闸门。
//!
//! 全部在进程内运行：cage 与 managed arena 的物理页由 extent 替身按页提交，不读镜像、
//! 不起子进程。

use super::RawWorld;
use super::heap_impl::ManagedPlacement;
use super::heap_tests::{configured_world, gc_contract, gc_contract_with};
use crate::runtime::cage::ForeignPin;
use crate::runtime::compression_schema::{CompressionDemand, CompressionPolicyV1};
use crate::runtime::gc_metadata_contract::GC_ARENA_BYTES;
use crate::runtime::gc_metadata_schema::GcRootKindV1;
use crate::runtime::slab::MemoryDomainId;
use crate::runtime::{RawPlanePolicyV1, RuntimeRawContractV1};

/// 启用 cage profile 的已验证契约；cage 容量按 GC arena 粒度给出。
fn cage_contract(cage_bytes: u64) -> RuntimeRawContractV1 {
    gc_contract_with(
        RawPlanePolicyV1 {
            compression: CompressionPolicyV1::cage(cage_bytes),
            ..RawPlanePolicyV1::default()
        },
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 1,
        },
    )
}

/// 配置一个启用 cage profile 的单 owner world。
fn cage_world(cage_bytes: u64) -> RawWorld {
    let contract = cage_contract(cage_bytes);
    configured_world(&contract, 11, 1, 64)
}

/// managed arena 按 island 从 cage 切出：基址落在 cage 内且与 range 偏移一致。
#[test]
fn managed_arena_bases_are_cage_islands() {
    let cage_bytes = 4 * GC_ARENA_BYTES;
    let mut world = cage_world(cage_bytes);
    let cage = *world
        .provider_ranges()
        .iter()
        .find(|range| range.domain == MemoryDomainId::MANAGED_LOCAL)
        .expect("cage 预留必须登记在 MANAGED_LOCAL domain");
    assert_eq!(cage.bytes, cage_bytes);
    assert!(cage.base.is_multiple_of(GC_ARENA_BYTES));
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    let plane = world.compression().expect("压缩平面已配置");
    let (cage_id, _) = plane.cage_of(address).expect("nursery 地址落在 cage 内");
    assert_eq!(
        plane.descriptor(cage_id).expect("cage 已登记").base,
        cage.base
    );
    let arena = *world
        .managed_arenas()
        .first()
        .expect("managed arena 已登记");
    assert!(arena.base.is_multiple_of(GC_ARENA_BYTES));
    let range_offset = world
        .extents
        .arena_range_offset(arena.extent_arena)
        .expect("arena 必须有 range 偏移");
    assert_eq!(
        arena.base,
        cage.base + range_offset,
        "island 基址必须等于 cage 基址加 range 偏移"
    );
    assert!(plane.cage_of(arena.base).is_some());
}

/// 关闭 profile 时保留等价 full-pointer 语义：无 cage、压缩根被拒绝、mark 照常推进。
#[test]
fn closed_profile_keeps_full_pointer_semantics() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 12, 1, 64);
    let plane = world.compression().expect("压缩平面已配置");
    assert!(!plane.enabled());
    assert!(plane.descriptor(0).is_err(), "关闭态不得登记 cage");
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    assert!(
        world
            .compression()
            .expect("平面")
            .cage_of(address)
            .is_none(),
        "关闭态没有 cage 承载 managed 地址"
    );
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 1)
        .expect("普通根可登记");
    world.set_managed_root(slot, address).expect("根可写");
    assert_eq!(
        world
            .register_compressed_root(1, address)
            .expect_err("关闭态不得登记压缩根")
            .message(),
        "未启用 cage profile 时不能登记压缩根"
    );
    assert_eq!(
        world
            .register_managed_root(GcRootKindV1::CompressedRef, 1)
            .expect_err("关闭态不得登记压缩根槽")
            .message(),
        "未启用 cage profile 时不能登记压缩根"
    );
    let report = world
        .run_mark_pass(&[0])
        .expect("full-pointer 语义下 mark pass 可执行");
    assert!(report.marked > 0, "普通根必须标记到对象");
}

/// 压缩根参与 root slice 校验与 mark seeding：解码成功后才解析 owner。
#[test]
fn compressed_root_participates_in_snapshot_and_seeding() {
    let mut world = cage_world(4 * GC_ARENA_BYTES);
    let object = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    let slot = world
        .register_compressed_root(1, object)
        .expect("压缩根可登记");
    let word = world.compressed_root_word(slot).expect("压缩字可读");
    assert_ne!(word, object, "槽里必须是编码字而不是完整地址");
    let decoded = world
        .compression_mut()
        .expect("平面")
        .decode(word)
        .expect("编码字可解码")
        .expect("非空编码字解码为地址");
    assert_eq!(decoded, object);
    let report = world
        .run_mark_pass(&[0])
        .expect("压缩根驱动的 mark pass 可执行");
    assert!(report.marked > 0, "压缩根必须 seed 到对象");
    assert!(
        world.compression_stats().expect("统计存在").decodes >= 1,
        "root slice 与 seed 的解码计入统计"
    );
}

/// minor 搬迁后压缩根必须按新 payload 重新编码。
#[test]
fn minor_relocation_reencodes_compressed_root() {
    let mut world = cage_world(4 * GC_ARENA_BYTES);
    let object = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    let slot = world
        .register_compressed_root(1, object)
        .expect("压缩根可登记");
    let before = world.compressed_root_word(slot).expect("压缩字可读");
    world.collect_minor(0).expect("minor cycle 可执行");
    let after = world.compressed_root_word(slot).expect("压缩字可读");
    assert_ne!(after, before, "搬迁后压缩字必须按新 payload 重新编码");
    let target = world
        .compression_mut()
        .expect("平面")
        .decode(after)
        .expect("新编码字可解码")
        .expect("非空编码字解码为地址");
    assert_ne!(target, object, "minor cycle 必须移动 nursery 对象");
    assert_eq!(world.owner_of(target).expect("新 payload 属于 owner 0"), 0);
}

/// `MANAGED_SHARED` 地址永不成岛：压缩根不能绕过 handle resolve。
#[test]
fn shared_payload_addresses_are_not_encodable() {
    let contract = cage_contract(4 * GC_ARENA_BYTES);
    let mut world = configured_world(&contract, 15, 2, 64);
    world.allocate_shared_object(1, 16).expect("共享对象可分配");
    let shared = *world
        .provider_ranges()
        .iter()
        .find(|range| range.domain == MemoryDomainId::MANAGED_SHARED)
        .expect("共享 arena 已预留");
    assert!(
        world
            .compression()
            .expect("平面")
            .cage_of(shared.base)
            .is_none(),
        "共享 arena 不得落在 cage 内"
    );
    assert_eq!(
        world
            .register_compressed_root(1, shared.base)
            .expect_err("共享地址不能编码为压缩根")
            .message(),
        "地址不在任何 cage 内，不能编码为压缩引用"
    );
}

/// FFI 交接：活动 lease 才可保存，generation 推进与释放都使旧 lease 失效。
#[test]
fn foreign_handoff_requires_active_pin_lease() {
    let mut world = cage_world(4 * GC_ARENA_BYTES);
    let object = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old)
        .expect("old 对象可分配");
    let forged = ForeignPin {
        cage: 0,
        offset: 0x40,
        generation: 1,
        sequence: 0,
    };
    assert_eq!(
        world
            .save_for_foreign(forged)
            .expect_err("无 pin 不得保存")
            .message(),
        "FFI 保存缺少活动 pin lease"
    );
    let pin = world.pin_for_foreign(0, object).expect("FFI pin 可建立");
    let saved = world.save_for_foreign(pin).expect("活动 lease 可保存");
    assert_eq!(saved, object, "old 对象不晋升，保存地址与请求地址一致");
    let copied = world
        .copy_for_foreign(saved, &[1, 2, 3, 4])
        .expect("cage 内地址可拷贝");
    assert_eq!(copied, vec![1, 2, 3, 4]);
    assert_eq!(
        world
            .copy_for_foreign(0, &[0])
            .expect_err("cage 外地址不得拷贝")
            .message(),
        "cage 外地址不能复制给 native code"
    );
    world
        .compression_mut()
        .expect("平面")
        .advance_generation(0)
        .expect("generation 可推进");
    assert_eq!(
        world
            .save_for_foreign(pin)
            .expect_err("过期 generation 不得保存")
            .message(),
        "FFI 保存的 cage generation 已过期"
    );
    world
        .release_for_foreign(0, saved, pin)
        .expect("lease 可释放");
    assert_eq!(
        world
            .save_for_foreign(pin)
            .expect_err("已释放 lease 不得保存")
            .message(),
        "FFI 保存缺少活动 pin lease"
    );
    assert_eq!(
        world
            .release_for_foreign(0, saved, pin)
            .expect_err("重复释放必须拒绝")
            .message(),
        "FFI pin lease 已释放或不存在"
    );
    let stats = world.compression_stats().expect("统计存在");
    assert!(stats.foreign_pins >= 1 && stats.foreign_saves >= 1 && stats.foreign_copies >= 1);
    assert_eq!(
        stats.foreign_rejections, 3,
        "仅非法保存计入 foreign_rejections；copy 域外拒绝与重复释放不改变它"
    );
}

/// island 数必须与 LocalHeap arena 数一致：没有 arena 绕过 cage 直接预留。
#[test]
fn island_count_matches_managed_arenas() {
    let contract = cage_contract(4 * GC_ARENA_BYTES);
    let mut world = configured_world(&contract, 17, 2, 64);
    for owner in 0..2 {
        world
            .allocate_managed(owner, 1, 8, ManagedPlacement::Nursery)
            .expect("nursery 对象可分配");
    }
    let plane = world.compression().expect("平面");
    let islands = plane.island_count();
    assert_eq!(
        islands as usize,
        world.managed_arenas().len(),
        "每个 LocalHeap arena 都必须来自一个 island"
    );
    assert_eq!(islands, 2);
    for arena in world.managed_arenas() {
        assert!(
            plane.cage_of(arena.base).is_some(),
            "arena 基址必须落在 cage 内"
        );
        assert!(arena.base.is_multiple_of(GC_ARENA_BYTES));
    }
    let stats = world.compression_stats().expect("统计存在");
    assert_eq!(stats.decodes, 0);
    assert_eq!(stats.foreign_pins, 0);
}
