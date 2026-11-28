//! 编码固定两帧栈图，实际 Gugu walker 消费字节；不替换查表或根枚举算法。
use super::*;
use crate::runtime::stackmap_codec::{CodeLayout, SafepointLayout, TypedRoot, encode};

const PAYLOAD: u64 = 36864;
const SECTION: u64 = 40960;
const FRAME: u64 = 45056;
const CALLER: u64 = FRAME + 32;
const GPR: u64 = 49152;
const CODE_BASE: u64 = 1048576;
const LOAD_BIAS: u64 = 4096;

pub(super) fn run_all() {
    callback_poll_parent_keeps_exact_roots_during_yield();
    foreign_return_maps_preserve_caller_resource_during_native_wait();
    parked_stack_preserves_file_until_coroutine_leaves_registry();
}

fn callback_poll_parent_keeps_exact_roots_during_yield() {
    let mut harness = runtime_harness();
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    install_caller(&mut memory, 1, false);
    verify_live(&mut harness, &mut memory);
    remove_roots(&mut harness, &mut memory);
}

fn foreign_return_maps_preserve_caller_resource_during_native_wait() {
    for dirty in [false, true] {
        let mut harness = runtime_harness();
        harness.gc_config = CONFIG;
        harness.block_budget = 32768;
        let (mut memory, _) = graph_memory();
        install_caller(&mut memory, 3, dirty);
        assert_eq!(scan(&mut harness, &mut memory, 3), 3);
        assert_eq!(word(&memory, ROOTS + 16), 2);
        assert_eq!(word(&memory, ROOTS + 160), CALLER);
        harness.run(
            "collect",
            "gc_collect_major",
            &[PROCESSOR, ROOTS, 3],
            &mut memory,
        );
        assert_eq!(word(&memory, CONTROL), 1);
        assert_eq!(harness.submitted, [vec![SECOND_CONTROL, 43, 1]]);
        remove_roots(&mut harness, &mut memory);
        harness.run(
            "collect",
            "gc_collect_major",
            &[PROCESSOR, ROOTS, 3],
            &mut memory,
        );
        assert_eq!(word(&memory, CONTROL), 0);
        assert_eq!(
            harness.submitted,
            [vec![SECOND_CONTROL, 43, 1], vec![CONTROL, 42, 1]]
        );
    }
}

fn parked_stack_preserves_file_until_coroutine_leaves_registry() {
    let mut harness = runtime_harness();
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let nodes = 90112;
    let parked = 96000;
    let cold = 96256;
    let newborn = 97024;
    let scratch = 73728;
    put(&mut memory, PROCESSOR + 4000, nodes);
    put(&mut memory, PROCESSOR + 4032, 3);
    put(&mut memory, PROCESSOR + 4072, 0xfeed);
    for (index, hot) in [HOT, parked, newborn].into_iter().enumerate() {
        let node = nodes + index as u64 * 16;
        put(&mut memory, node, if index == 2 { 0 } else { node + 16 });
        put(&mut memory, node + 8, hot);
    }
    put(&mut memory, parked + 48, cold);
    put(&mut memory, cold + 8, CALLER);
    put(&mut memory, cold + 16, CODE_BASE + 0x2010);
    put(&mut memory, newborn + 48, 97280);
    put(&mut memory, 97280 + 16, 0xfeed);
    assert_eq!(
        harness.run(
            "collect",
            "gc_append_parked_roots",
            &[PROCESSOR, HOT, ROOTS, scratch],
            &mut memory
        ),
        [1]
    );
    assert_eq!(word(&memory, ROOTS + 96), CALLER);
    assert_eq!(word(&memory, ROOTS + 112), ARENA + 20);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 3],
        &mut memory,
    );
    assert_eq!(word(&memory, CONTROL), 1);
    assert_eq!(harness.submitted, [vec![SECOND_CONTROL, 43, 1]]);
    harness.run(
        "scheduler",
        "unregister_coroutine",
        &[PROCESSOR, parked],
        &mut memory,
    );
    put(&mut memory, ROOTS, 0);
    put(&mut memory, ROOTS + 8, 0);
    assert_eq!(
        harness.run(
            "collect",
            "gc_append_parked_roots",
            &[PROCESSOR, HOT, ROOTS, scratch],
            &mut memory
        ),
        [0]
    );
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 3],
        &mut memory,
    );
    assert_eq!(word(&memory, CONTROL), 0);
    assert_eq!(
        harness.submitted,
        [vec![SECOND_CONTROL, 43, 1], vec![CONTROL, 42, 1]]
    );
}

pub(super) fn verify_compiled_spills(output: &Compilation) {
    let plan = output.image_plan().expect("文件 API 镜像");
    let section = crate::runtime::stackmap_codec::decode_tables(plan.x64_stackmap_section())
        .expect("实际机器栈图可解码");
    let mut rooted_points = 0;
    let mut call_points = 0;
    for point in &section.safepoints {
        if !matches!(point.kind, 0..=2) {
            continue;
        }
        if point.kind == 0 {
            call_points += 1;
        }
        let map = &section.maps[point.map_index as usize];
        assert_eq!(
            map.registers, [0; 5],
            "CallReturn/PollResume/Suspend 根必须落栈"
        );
        if !map.typed.is_empty() || map.slots.iter().any(|slots| !slots.is_empty()) {
            rooted_points += 1;
        }
    }
    assert!(
        call_points > 0 && rooted_points > 0,
        "文件调用必须产生有根 CallReturn 图：call={call_points}, rooted={rooted_points}"
    );
}

pub(super) fn install(memory: &mut [u8]) {
    install_caller(memory, 0, false);
}

fn install_caller(memory: &mut [u8], kind: u8, dirty: bool) {
    let functions = [0x1000, 0x2000].map(|code_rva| CodeLayout {
        code_rva,
        code_size: 64,
        frame_size: 24,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: false,
    });
    let first = SafepointLayout {
        pc_offset: 8,
        kind: 2,
        dirty: false,
        copy_allowed: true,
        scan_allowed: true,
        slots: std::array::from_fn(|_| vec![0]),
        registers: [1, 0, 0, 0, 0],
        typed: vec![TypedRoot {
            word: 0,
            root_kind: 0,
            init_byte: 16,
        }],
        function: 0,
        slot_count: 3,
    };
    let caller = SafepointLayout {
        pc_offset: 16,
        kind,
        dirty,
        registers: [0; 5],
        typed: Vec::new(),
        function: 1,
        slots: [vec![0], vec![1], vec![0], vec![0], vec![0]],
        ..first.clone()
    };
    let section = encode(&functions, &[(0, first), (1, caller)]).expect("合法两帧栈图");
    assert!(section.len() < (FRAME - SECTION) as usize);
    memory[SECTION as usize..SECTION as usize + section.len()].copy_from_slice(&section);
    put(memory, STATE + 112, PAYLOAD);
    put(memory, STATE + 120, LOAD_BIAS);
    put(memory, PAYLOAD + 48, SECTION - LOAD_BIAS);
    put(memory, PAYLOAD + 56, CODE_BASE - LOAD_BIAS);
    put(memory, PAYLOAD + 64, section.len() as u64);
    put(memory, FRAME, ARENA + 16);
    put(memory, FRAME + 16, 1);
    put(memory, FRAME + 24, CODE_BASE + 0x2010);
    put(memory, CALLER, ARENA + 20); // 容器的内部指针。
    put(memory, CALLER + 24, CODE_BASE + 0x3000); // 非 managed 入口哨兵。
    put(memory, GPR, ARENA + 16);
    // 未登记槽即使看起来是文件资源指针也不得变成根。
    put(memory, FRAME + 8, ARENA + 80);
    put(memory, CALLER + 8, ARENA + 80);
}

fn scan(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>, capacity: u64) -> u64 {
    memory[ROOTS as usize..ROOTS as usize + 192].fill(0);
    harness.run(
        "roots",
        "gc_walk_roots",
        &[ROOTS, capacity, FRAME, CODE_BASE + 0x1008, 2, GPR, 1],
        memory,
    )[0]
}

pub(super) fn verify_live(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    // 容量不足只截断 written，needed 仍记录完整根数，不将截断输出交给 GC。
    assert_eq!(scan(harness, memory, 2), 3);
    assert_eq!(word(memory, ROOTS + 8), 2);
    assert_eq!(scan(harness, memory, 3), 3);
    assert_eq!(word(memory, ROOTS + 8), 3);
    assert_eq!(word(memory, ROOTS + 16), 2);
    assert_eq!(word(memory, ROOTS + 24), 0x3000);
    for (index, (slot, kind, value)) in [
        (GPR, 0, ARENA + 16),
        (FRAME, 0, ARENA + 16),
        (CALLER, 1, ARENA + 20),
    ]
    .into_iter()
    .enumerate()
    {
        let record = ROOTS + 96 + index as u64 * 32;
        assert_eq!(
            (
                word(memory, record),
                word(memory, record + 8),
                word(memory, record + 16)
            ),
            (slot, kind, value)
        );
    }
}

pub(super) fn remove_roots(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    put(memory, FRAME + 16, 0); // 保留旧指针字，仅撤销初始化标志。
    put(memory, GPR, 0);
    put(memory, CALLER, 0);
    assert_eq!(scan(harness, memory, 3), 0);
    assert_eq!(word(memory, ROOTS + 8), 0);
    assert_eq!(word(memory, ROOTS + 16), 2);
    assert_eq!(word(memory, FRAME), ARENA + 16);
    assert_eq!(word(memory, FRAME + 8), ARENA + 80);
}
