//! 栈图 section 编解码与 walker 的确定性回归；不读真实镜像，只用合成布局。
//!
//! 覆盖往返、版本拒收、去重字典序、位图互斥、保留位拒绝、表重叠与越界拒绝、
//! 空世界合法，以及函数/安全点二分查找、五类根扫描、落地选择、复制输入组装与
//! bridge 帧校验。全部是进程内、确定性、快速测试。

use super::model::RawModelError;
use super::stackmap::{
    CageDescriptor, HandleSlot, LandingRecord, WalkFunction, WalkMap, WalkSafepoint, WalkWorld,
    copy_input, find_function, find_safepoint, scan_roots, select_landing, verify_bridge_frame,
};
use super::stackmap_codec::{CodeLayout, SafepointLayout, decode, encode};

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
        cages: &[],
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

#[test]
fn walker_scans_five_root_kinds() {
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
    let cages = vec![CageDescriptor {
        base: 0x5000,
        len: 0x1000,
        generation: 3,
    }];
    let world = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles: &handles,
        cages: &cages,
        landings: &[],
    };
    // 字布局：direct 非空、interior 带增量、handle 槽 2、压缩引用 cage0 世代 3 偏移 0x40、
    // stack 非空。
    let compressed = (0u64 << 56) | (3u64 << 32) | 0x40u64;
    let words = vec![0x1111, 0x2222_0001, 2, compressed, 0x5555];
    let roots = scan_roots(&world, 0, &words).expect("五类根可扫描");
    assert_eq!(roots.len(), 5, "五类根各一个：{roots:?}");
    // 过期 handle 代际不得解析：槽 1 的 generation 为 0。
    let stale = vec![0x1111, 0x2222_0001, 1, compressed, 0x5555];
    assert!(
        scan_roots(&world, 0, &stale).is_err(),
        "handle 槽 1 代际为 0 必须拒绝"
    );
    // 压缩引用代际过期不得解码。
    let aged = (0u64 << 56) | (9u64 << 32) | 0x40u64;
    let aged_words = vec![0x1111, 0x2222_0001, 2, aged, 0x5555];
    assert!(scan_roots(&world, 0, &aged_words).is_err());
    // 越过 cage 范围不得解码。
    let outside = (0u64 << 56) | (3u64 << 32) | 0x2000u64;
    let outside_words = vec![0x1111, 0x2222_0001, 2, outside, 0x5555];
    assert!(scan_roots(&world, 0, &outside_words).is_err());
    // 空 direct 字容忍跳过。
    let sparse = vec![0, 0x2222_0001, 2, compressed, 0x5555];
    assert_eq!(scan_roots(&world, 0, &sparse).expect("空值跳过").len(), 4);
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
        cages: &[],
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
    let overflow: Result<(), RawModelError> =
        verify_bridge_frame(u64::MAX, 16, 32).map_err(|error| error);
    assert!(overflow.is_err());
}
