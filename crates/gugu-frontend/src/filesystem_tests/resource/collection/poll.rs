//! 固定 PollResume 栈图驱动真实 dispatcher，不替换根枚举或收集算法。
use super::*;
use crate::runtime::stackmap_codec::{CodeLayout, SafepointLayout, encode};

#[test]
fn pressure_poll_dispatches_gc_once_and_preserves_stack_resource() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    harness.clock.extend([1000, 1125]);
    let (mut memory, _) = graph_memory();
    let nursery = 4194304;
    let arena_bytes = 2097152;
    memory.resize((nursery + arena_bytes + 65536) as usize, 0);
    let meta = nursery + arena_bytes;
    for block in 0..64 {
        put32(&mut memory, meta + 55576 + block * 64 + 56, 6);
    }
    put(&mut memory, PROCESSOR + 3520, nursery + 16);
    put(&mut memory, PROCESSOR + 3528, nursery + 32768);
    put(&mut memory, PROCESSOR + 3592, nursery);
    // minor 现在实际消费 staging；空表仍须是启动期已登记的有效页。
    put(&mut memory, PROCESSOR + 3576, 262144);
    put(&mut memory, PROCESSOR + 3920, arena_bytes * 2 + 131072);
    put(&mut memory, PROCESSOR + 3928, 40960);
    put(&mut memory, PROCESSOR + 3960, 32768);
    put(&mut memory, PROCESSOR + 3952, 1);
    put(&mut memory, PROCESSOR + 3888, 1);
    put(&mut memory, PROCESSOR + 3912, 1);
    put(&mut memory, PROCESSOR + 3616, 1);
    put(&mut memory, PROCESSOR + 4064, 57344);
    put(&mut memory, COORDINATOR + 104, 73728);
    put(&mut memory, COORDINATOR + 112, 90112);
    put(&mut memory, COORDINATOR + 120, 106496);
    put(&mut memory, COORDINATOR + 136, HOT);
    let section_at = 40960;
    let frame = 45056;
    let base = 1048576;
    let functions = [CodeLayout {
        code_rva: 4096,
        code_size: 64,
        frame_size: 24,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: false,
    }];
    let point = SafepointLayout {
        pc_offset: 8,
        kind: 1,
        dirty: false,
        copy_allowed: true,
        scan_allowed: true,
        slots: [vec![1], vec![0], vec![0], vec![0], vec![0]],
        registers: [0; 5],
        typed: vec![],
        function: 0,
        slot_count: 3,
    };
    let section = encode(&functions, &[(0, point)]).unwrap();
    memory[section_at..section_at + section.len()].copy_from_slice(&section);
    put(&mut memory, 36864 + 64, section.len() as u64);
    put(&mut memory, frame - 8, base + 4104);
    put(&mut memory, frame, ARENA + 16);
    put(&mut memory, frame + 24, base + 12288);
    assert_eq!(
        harness.run(
            "collect",
            "gc_dispatch",
            &[PROCESSOR + 3616, frame - 8],
            &mut memory
        ),
        [0]
    );
    assert!(harness.clock.is_empty());
    assert_eq!(word(&memory, COORDINATOR + 80), 1);
    assert_eq!(word(&memory, COORDINATOR + 144), 1);
    assert_eq!(word(&memory, COORDINATOR + 160), 125);
    assert_eq!(word(&memory, PROCESSOR + 3904), 1);
    assert_eq!(word(&memory, PROCESSOR + 3912), 1);
    assert_eq!(word(&memory, CONTROL), 1);
    assert_eq!(harness.submitted, [vec![SECOND_CONTROL, 43, 1]]);
    let before = memory.clone();
    assert_eq!(
        harness.run(
            "collect",
            "gc_dispatch",
            &[PROCESSOR + 3616, frame - 8],
            &mut memory
        ),
        [0]
    );
    assert_eq!(memory, before, "同一 episode 不再次收集或计时");
}

fn put32(memory: &mut [u8], at: u64, value: u32) {
    memory[at as usize..at as usize + 4].copy_from_slice(&value.to_le_bytes());
}
