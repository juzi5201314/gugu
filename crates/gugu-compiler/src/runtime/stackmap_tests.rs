//! 栈图 section 编解码与 walker 的确定性回归；不读真实镜像，只用合成布局。
//!
//! 覆盖往返、版本拒收、去重字典序、位图互斥、保留位拒绝、表重叠与越界拒绝、
//! 空世界合法，以及函数/安全点二分查找、五类根扫描、落地选择、复制输入组装与
//! bridge 帧校验。全部是进程内、确定性、快速测试。

use super::cage::{CompressionPlane, is_canonical};
use super::compression_schema::{
    CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract,
};
use super::gc_metadata_contract::GC_ARENA_BYTES;
use super::platform::{FakePlatform, PlatformProfile};
use super::slab::RuntimeSeed;
use super::stackmap::{
    HandleSlot, LandingRecord, WalkFunction, WalkMap, WalkSafepoint, WalkWorld, copy_input,
    find_function, find_safepoint, scan_roots, select_landing, verify_bridge_frame,
};
use super::stackmap_codec::{CodeLayout, SafepointLayout, decode, encode};
use crate::target::PointerCompression;

fn function(code_rva: u64, code_size: u32) -> CodeLayout {
    CodeLayout {
        code_rva,
        code_size,
        frame_size: 24,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: true,
        has_stack_interior: true,
    }
}

fn layout(
    function: u32,
    pc_offset: u32,
    kind: u8,
    slot_count: u32,
    word: u64,
) -> (u32, SafepointLayout) {
    let lanes = (slot_count.div_ceil(8) as usize).div_ceil(8).max(1);
    let mut slots: [Vec<u64>; 5] = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    slots[0] = vec![word; lanes];
    for bitmap in slots.iter_mut().skip(1) {
        *bitmap = vec![0; lanes];
    }
    (
        function,
        SafepointLayout {
            pc_offset,
            kind,
            dirty: false,
            copy_allowed: true,
            scan_allowed: true,
            slots,
            registers: [0; 5],
            function,
            slot_count,
        },
    )
}

#[test]
fn codec_round_trip_and_empty_world() {
    let functions = vec![function(0x1000, 64), function(0x2000, 64)];
    let safepoints = vec![
        layout(0, 8, 0, 64, 0b101),
        layout(0, 16, 1, 64, 0b010),
        layout(1, 4, 2, 64, 0b001),
    ];
    let bytes = encode(&functions, &safepoints).expect("编码合成布局");
    let (functions_count, safepoints_count, maps) = decode(&bytes).expect("解码合成布局");
    assert_eq!((functions_count, safepoints_count), (2, 3));
    assert!((1..=3).contains(&maps), "去重 map 非空且不超过安全点数");
    // 空世界合法：三表计数为 0。
    let empty = encode(&[], &[]).expect("空世界可编码");
    assert_eq!(decode(&empty).expect("空世界可解码"), (0, 0, 0));
}

#[test]
fn codec_preserves_fifth_register_root() {
    let functions = vec![function(0x1000, 64)];
    let mut fifth = layout(0, 8, 0, 64, 0);
    fifth.1.registers[4] = 1 << 2;
    let bytes = encode(&functions, std::slice::from_ref(&fifth)).expect("第五类寄存器根可编码");
    let map_data = u64::from_le_bytes(bytes[56..64].try_into().expect("map data 偏移")) as usize;
    assert_eq!(
        u16::from_le_bytes(
            bytes[map_data + 12..map_data + 14]
                .try_into()
                .expect("第五类掩码")
        ),
        1 << 2,
        "第五类寄存器掩码必须写入 map header"
    );
    assert_eq!(decode(&bytes).expect("第五类寄存器根可解码"), (1, 1, 1));

    let mut overlap = fifth;
    overlap.1.registers[0] = 1 << 2;
    assert!(
        encode(&functions, std::slice::from_ref(&overlap)).is_err(),
        "五类寄存器掩码重叠必须拒绝"
    );
}

#[test]
fn codec_rejects_version_one_and_unsorted_maps() {
    let functions = vec![function(0x1000, 64)];
    let safepoints = vec![layout(0, 8, 0, 64, 0b001)];
    let mut bytes = encode(&functions, &safepoints).expect("编码合成布局");
    // version 1 必须拒收。
    bytes[8] = 1;
    bytes[9] = 0;
    assert!(decode(&bytes).is_err(), "decoder 必须拒收 version 1");
}

#[test]
fn codec_rejects_reserved_bits_overlap_and_out_of_range() {
    let functions = vec![function(0x1000, 64)];
    // 寄存器 bit15 必须为 0。
    let mut bad = layout(0, 8, 0, 64, 0b001);
    bad.1.registers[0] = 1 << 15;
    assert!(encode(&functions, std::slice::from_ref(&bad)).is_err());
    // 普通函数占用保留寄存器必须拒绝。
    let mut reserved = layout(0, 8, 0, 64, 0b001);
    reserved.1.registers[1] = 1 << 13;
    assert!(encode(&functions, std::slice::from_ref(&reserved)).is_err());
    // 五类槽位图不互斥必须拒绝。
    let mut overlap = layout(0, 8, 0, 64, 0b011);
    overlap.1.slots[1] = vec![0b001];
    assert!(encode(&functions, std::slice::from_ref(&overlap)).is_err());
    // 安全点偏移越过函数代码必须拒绝。
    let outside = layout(0, 64, 0, 64, 0b001);
    assert!(encode(&functions, std::slice::from_ref(&outside)).is_err());
    // 函数表重叠必须拒绝。
    let overlapping = vec![function(0x1000, 64), function(0x1020, 64)];
    assert!(encode(&overlapping, &[]).is_err());
    // `MorestackEntry` 的 `slot_count` 必须为 0。
    let morestack = layout(0, 8, 4, 64, 0b001);
    assert!(encode(&functions, std::slice::from_ref(&morestack)).is_err());
    // dirty 只能与 kind 3 同时出现。
    let mut dirty = layout(0, 8, 0, 64, 0b001);
    dirty.1.dirty = true;
    assert!(encode(&functions, std::slice::from_ref(&dirty)).is_err());
    // 截断的 section 必须拒绝。
    let bytes = encode(&functions, &[layout(0, 8, 0, 64, 0b001)]).expect("编码");
    assert!(decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn walker_finds_functions_and_safepoints_by_binary_search() {
    let functions = vec![
        WalkFunction {
            code_rva: 0x1000,
            code_size: 64,
            frame_size: 24,
        },
        WalkFunction {
            code_rva: 0x2000,
            code_size: 64,
            frame_size: 24,
        },
    ];
    let safepoints = vec![
        WalkSafepoint {
            function: 0,
            pc_offset: 8,
            kind: 0,
            map: 0,
        },
        WalkSafepoint {
            function: 0,
            pc_offset: 16,
            kind: 1,
            map: 0,
        },
        WalkSafepoint {
            function: 1,
            pc_offset: 4,
            kind: 2,
            map: 0,
        },
    ];
    let maps = vec![WalkMap::default()];
    let world = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles: &[],
        landings: &[],
    };
    assert_eq!(find_function(&world, 0x1008).expect("函数内"), 0);
    assert_eq!(find_function(&world, 0x2004).expect("第二函数"), 1);
    assert!(find_function(&world, 0x1800).is_err(), "函数间隙不得猜测");
    assert_eq!(find_safepoint(&world, 0, 16).expect("精确偏移"), 1);
    assert!(
        find_safepoint(&world, 0, 12).is_err(),
        "失配偏移不得退回相邻记录"
    );
}

/// 构造启用 cage profile 的契约；需求计数与用例声明的解码点/压缩根槽一致。
fn compression_contract(
    cage_bytes: u64,
    decode_sites: u32,
    compressed_root_slots: u32,
) -> CompressionRuntimeContract {
    CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites,
            compressed_root_slots,
        },
        CompressionPolicyV1::cage(cage_bytes),
        PointerCompression::x86_64(),
    )
    .expect("压缩契约可构建")
}

/// 在确定性平台上预留 `cage_bytes` 的平面。
fn reserved_plane(cage_bytes: u64) -> CompressionPlane {
    let contract = compression_contract(cage_bytes, 1, 1);
    let mut plane = CompressionPlane::new(&contract);
    let mut platform = FakePlatform::new(PlatformProfile::Linux, u64::from(u32::MAX));
    plane.reserve(&mut platform).expect("cage 可预留");
    plane
}

/// 在一个五类根各一槽的安全点上扫描；压缩槽的字由调用方给出。
fn scan_compressed_slot(
    word: u64,
    plane: &mut CompressionPlane,
) -> Result<Vec<super::stackmap::ScannedRoot>, super::model::RawModelError> {
    let functions = vec![WalkFunction {
        code_rva: 0x1000,
        code_size: 64,
        frame_size: 24,
    }];
    let safepoints = vec![WalkSafepoint {
        function: 0,
        pc_offset: 8,
        kind: 0,
        map: 0,
    }];
    let maps = vec![WalkMap {
        direct: vec![0],
        interior: vec![1],
        handle: vec![2],
        compressed: vec![3],
        stack: vec![4],
    }];
    let handles = vec![
        HandleSlot {
            payload: 0,
            generation: 0,
        },
        HandleSlot {
            payload: 0,
            generation: 0,
        },
        HandleSlot {
            payload: 0xabcd,
            generation: 7,
        },
    ];
    let world = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles: &handles,
        landings: &[],
    };
    // 字布局：direct 非空、interior 带增量、handle 槽 2、压缩引用槽 3、stack 非空。
    let words = vec![0x1111, 0x2222_0001, 2, word, 0x5555];
    scan_roots(&world, 0, &words, plane)
}

#[test]
fn walker_scans_five_root_kinds() {
    let mut plane = reserved_plane(4 * GC_ARENA_BYTES);
    let descriptor = plane.descriptor(0).expect("cage 已登记");
    let compressed = plane
        .encode(descriptor.base + 0x40)
        .expect("cage 内地址可编码");
    let roots = scan_compressed_slot(compressed, &mut plane).expect("五类根可扫描");
    assert_eq!(roots.len(), 5, "五类根各一个：{roots:?}");
    assert_eq!(
        roots[3],
        super::stackmap::ScannedRoot::Compressed {
            offset: 3,
            target: descriptor.base + 0x40,
        },
        "压缩根必须解码到 cage 内完整地址"
    );
    assert_eq!(plane.decode_count(), 1, "压缩根解码计入统计");
    // 过期 handle 代际不得解析：槽 1 的 generation 为 0。
    let functions = vec![WalkFunction {
        code_rva: 0x1000,
        code_size: 64,
        frame_size: 24,
    }];
    let safepoints = vec![WalkSafepoint {
        function: 0,
        pc_offset: 8,
        kind: 0,
        map: 0,
    }];
    let maps = vec![WalkMap {
        direct: vec![0],
        interior: vec![1],
        handle: vec![2],
        compressed: vec![3],
        stack: vec![4],
    }];
    let handles = vec![
        HandleSlot {
            payload: 0,
            generation: 0,
        },
        HandleSlot {
            payload: 0,
            generation: 0,
        },
        HandleSlot {
            payload: 0xabcd,
            generation: 7,
        },
    ];
    let world = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles: &handles,
        landings: &[],
    };
    let stale = vec![0x1111, 0x2222_0001, 1, compressed, 0x5555];
    assert!(
        scan_roots(&world, 0, &stale, &mut plane).is_err(),
        "handle 槽 1 代际为 0 必须拒绝"
    );
    // 空压缩字不解码也不计数。
    assert_eq!(
        scan_compressed_slot(0, &mut plane).expect("空值跳过").len(),
        4
    );
    assert_eq!(plane.decode_count(), 1, "空字不改变解码计数");
    // 空 direct 字容忍跳过。
    assert_eq!(
        scan_compressed_slot(compressed, &mut plane)
            .expect("空值跳过")
            .len(),
        5
    );
}

/// 未登记 cage 的压缩字必须被拒绝，而不是按位域猜测。
#[test]
fn walker_rejects_unregistered_compression_cage() {
    let mut plane = reserved_plane(4 * GC_ARENA_BYTES);
    let word = (9_u64 << 56) | (1_u64 << 32) | 0x40;
    let error = scan_compressed_slot(word, &mut plane).expect_err("未登记 cage 必须拒绝");
    assert_eq!(error.message(), "压缩引用 cage 未登记");
    assert_eq!(plane.stats().rejections, 1);
    assert_eq!(plane.decode_count(), 0);
}

/// generation 推进后旧压缩字全部过期。
#[test]
fn walker_rejects_stale_compression_generation() {
    let mut plane = reserved_plane(4 * GC_ARENA_BYTES);
    let descriptor = plane.descriptor(0).expect("cage 已登记");
    let stale = plane
        .encode(descriptor.base + 0x40)
        .expect("cage 内地址可编码");
    let advanced = plane.advance_generation(0).expect("generation 可推进");
    assert_ne!(advanced, descriptor.generation);
    let error = scan_compressed_slot(stale, &mut plane).expect_err("过期 generation 必须拒绝");
    assert_eq!(error.message(), "压缩引用 generation 过期");
    let fresh = plane
        .encode(descriptor.base + 0x40)
        .expect("新 generation 可编码");
    assert!(scan_compressed_slot(fresh, &mut plane).is_ok());
}

/// offset 越界必须在解码前被拒绝。
#[test]
fn walker_rejects_out_of_range_compression_offset() {
    let mut plane = reserved_plane(4 * GC_ARENA_BYTES);
    let descriptor = plane.descriptor(0).expect("cage 已登记");
    assert_eq!(
        plane
            .encode_offset(0, descriptor.len)
            .expect_err("越界 offset 必须拒绝")
            .message(),
        "压缩引用 offset 越过 cage 范围"
    );
    let word = (u64::from(descriptor.generation) << 32) | descriptor.len;
    let error = scan_compressed_slot(word, &mut plane).expect_err("越界 offset 必须拒绝");
    assert_eq!(error.message(), "压缩引用 offset 越过 cage 范围");
}

/// cage 跨越 canonical 边界时，落在 canonical hole 里的解码必须失败。
#[test]
fn walker_rejects_non_canonical_compression_decode() {
    let cage_bytes = 4 * GC_ARENA_BYTES;
    let base = (1_u64 << 47) - GC_ARENA_BYTES;
    let contract = compression_contract(cage_bytes, 1, 1);
    let mut plane = CompressionPlane::new(&contract);
    let mut platform = FakePlatform::with_capacity(
        PlatformProfile::Linux,
        base,
        cage_bytes,
        RuntimeSeed::new(0x51),
    );
    plane.reserve(&mut platform).expect("边界 cage 可预留");
    assert_eq!(plane.descriptor(0).expect("cage 已登记").base, base);
    assert!(is_canonical(base, contract.canonical_bits()));
    assert!(!is_canonical(1_u64 << 47, contract.canonical_bits()));
    let crossing = plane
        .encode(base + GC_ARENA_BYTES)
        .expect("洞内地址仍在 cage 内可编码");
    let error = scan_compressed_slot(crossing, &mut plane).expect_err("非 canonical 必须拒绝");
    assert_eq!(error.message(), "压缩引用解码出非 canonical 地址");
    let inside = plane
        .encode(base + GC_ARENA_BYTES - 8)
        .expect("边界内地址可编码");
    assert!(scan_compressed_slot(inside, &mut plane).is_ok());
}

/// 关闭 cage profile 时既不能编码，也不能解码压缩槽。
#[test]
fn walker_rejects_compressed_slot_without_cage_profile() {
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
    assert!(plane.descriptor(0).is_err(), "关闭态不得登记 cage");
    assert_eq!(
        plane.encode(0x1000).expect_err("关闭态不可编码").message(),
        "未启用 cage profile 时不能编码压缩引用"
    );
    let error = scan_compressed_slot(0x40, &mut plane).expect_err("关闭态压缩槽必须失败");
    assert_eq!(error.message(), "压缩引用 cage 未登记");
}

#[test]
fn walker_selects_innermost_landing_and_builds_copy_input() {
    let landings = vec![
        LandingRecord {
            pc_start: 0,
            pc_end: 64,
            landing_pc: 60,
            cleanup: 1,
        },
        LandingRecord {
            pc_start: 8,
            pc_end: 24,
            landing_pc: 20,
            cleanup: 2,
        },
    ];
    let world = WalkWorld {
        functions: &[],
        safepoints: &[],
        maps: &[],
        handles: &[],
        landings: &landings,
    };
    assert_eq!(
        select_landing(&world, 12).expect("最内层").cleanup,
        2,
        "重叠范围选择最窄覆盖"
    );
    assert_eq!(select_landing(&world, 40).expect("外层").cleanup, 1);
    assert!(select_landing(&world, 80).is_err(), "无覆盖不得猜测");
    // 复制输入只收栈根并按字节换算排序。
    let roots = vec![
        super::stackmap::ScannedRoot::Direct(0),
        super::stackmap::ScannedRoot::Stack(2),
        super::stackmap::ScannedRoot::Stack(0),
    ];
    let image = copy_input(&roots, 32).expect("复制输入");
    assert_eq!(image.stack_roots, vec![0, 16]);
    assert!(copy_input(&roots, 8).is_err(), "越界槽表必须拒绝");
    // bridge 帧必须落在已用范围内。
    verify_bridge_frame(8, 16, 32).expect("帧范围内");
    assert!(verify_bridge_frame(8, 32, 32).is_err());
    assert!(verify_bridge_frame(u64::MAX, 16, 32).is_err());
}
