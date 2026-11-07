use super::*;
const PIN_PAGE: u64 = 73728;
const PINNED: u64 = 0x8000000000;

#[test]
fn pinned_old_field_survives_minor_after_young_store() {
    const OLD: u64 = 2097152;
    const YOUNG: u64 = 4194304;
    const BARRIER: u64 = 98304;
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    memory.resize((YOUNG + 2097152 + 65536) as usize, 0);
    memory.copy_within(
        ARENA as usize..(ARENA + 2097152 + 65536) as usize,
        OLD as usize,
    );
    put(&mut memory, COORDINATOR + 48, OLD);
    put(&mut memory, PROCESSOR + 3592, YOUNG);
    put(&mut memory, PROCESSOR + 3520, YOUNG + 32);
    put(&mut memory, PROCESSOR + 3528, YOUNG + 32768);
    put(&mut memory, PROCESSOR + 3576, BARRIER);
    put(&mut memory, OLD, 1 | (2 << 36) | PINNED);
    put(&mut memory, OLD + 16, YOUNG + 16);
    put(&mut memory, OLD + 24, 0);
    put(&mut memory, OLD + 2097152 + 24, 1);
    put(&mut memory, YOUNG, 0);
    put(&mut memory, YOUNG + 8, 8);
    put(&mut memory, YOUNG + 16, 73);
    let meta = YOUNG + 2097152;
    put(&mut memory, meta + 24, 1);
    memory[(meta + 16) as usize..(meta + 20) as usize].copy_from_slice(&1u32.to_le_bytes());
    for block in 1..64 {
        let state = meta + 55576 + block * 64 + 56;
        memory[state as usize..state as usize + 4].copy_from_slice(&6u32.to_le_bytes());
    }
    // 真实写屏障先入 staging；minor 必须 flush 再扫描 old 对象字段。
    harness.run(
        "barrier",
        "gc_write_barrier",
        &[OLD + 16, 0, YOUNG + 16],
        &mut memory,
    );
    assert_eq!(word(&memory, BARRIER), 1);
    put(&mut memory, ROOTS + 8, 0);
    assert_eq!(
        harness.run(
            "collect",
            "gc_collect_minor",
            &[PROCESSOR, ROOTS, 1],
            &mut memory
        ),
        [0]
    );
    let moved = word(&memory, OLD + 16);
    assert!(
        moved >= OLD && moved < OLD + 2097152,
        "年轻引用必须在清空 nursery 前被更新：{moved}"
    );
    assert_eq!(word(&memory, moved), 73);
    assert_eq!(word(&memory, OLD) & PINNED, PINNED);
    assert_eq!(word(&memory, BARRIER), 0);
    assert_eq!(memory[(OLD + 2097152 + 34840) as usize], 0);
}

fn pin_compilation() -> Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../../gugu-compiler/tests/fixtures/pin.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

#[test]
fn pin_callback_cleanup_and_acquisition_keep_precise_roots() {
    use crate::lir::{
        body::{Op, Terminator},
        stackmap,
    };
    let output = pin_compilation();
    let harness = ResourceHarness::new(&output);
    let prepare = harness.body("pinning", "pin_prepare").instance;
    let acquire = harness.body("pinning", "pin_acquire").instance;
    let release = harness.body("pinning", "pin_release").instance;
    let mut checked = 0;
    let mut release_calls = 0;
    let mut exits = 0;
    for body in output.lir.as_ref().unwrap().bodies() {
        if body.instructions.iter().any(|instruction| matches!(&instruction.op, Op::Call(call) if call.target == CallTarget::Instance(acquire)))
            || body.blocks.iter().any(|block| matches!(&block.terminator, Terminator::Invoke { call, .. } if call.target == CallTarget::Instance(acquire))) {
            verify_pin_cleanup(body, acquire, release);
        }
        let anchors = stackmap::anchors(body).unwrap();
        for (block_index, block) in body.blocks.iter().enumerate() {
            for index in block.instructions.clone() {
                if let Op::Call(call) = &body.instructions[index as usize].op {
                    if call.target == CallTarget::Instance(prepare)
                        || call.target == CallTarget::Instance(acquire)
                    {
                        let anchor = anchors
                            .iter()
                            .find(|point| {
                                point.block == block_index as u32 && point.instruction == index
                            })
                            .expect("pin 准备/登记调用必须有根图");
                        assert!(
                            anchor.roots.words() > 0,
                            "{} 的 pin 准备/登记不能只剩裸整数",
                            body.name
                        );
                        checked += 1;
                    }
                    if call.target == CallTarget::Instance(release) {
                        release_calls += 1;
                        exits |= cleanup_exits(body, block_index);
                    }
                }
            }
            if let Terminator::Invoke { call, .. } = &block.terminator {
                if call.target == CallTarget::Instance(prepare)
                    || call.target == CallTarget::Instance(acquire)
                {
                    let anchor = anchors
                        .iter()
                        .find(|point| {
                            point.block == block_index as u32
                                && point.instruction == block.instructions.end
                        })
                        .expect("pin Invoke 必须有根图");
                    assert!(
                        anchor.roots.words() > 0,
                        "{} 的 pin Invoke 不能只剩裸整数",
                        body.name
                    );
                    checked += 1;
                }
                if call.target == CallTarget::Instance(release) {
                    release_calls += 1;
                    exits |= cleanup_exits(body, block_index);
                }
            }
        }
    }
    assert!(checked >= 2, "必须检查实际 prepare/acquire 调用点");
    assert!(
        release_calls >= 2,
        "正常返回与 panic 展开都需要 release 调用：{release_calls}"
    );
    assert_eq!(exits, 3, "release 后必须既能正常返回，也能继续 panic 展开");
}

fn verify_pin_cleanup(body: &Body, acquire: [u8; 32], release: [u8; 32]) {
    use crate::lir::body::{Op, Terminator};
    let effect = |target: &CallTarget| {
        if *target == CallTarget::Instance(acquire) {
            1i8
        } else if *target == CallTarget::Instance(release) {
            -1
        } else {
            0
        }
    };
    // 每个 pin 实例只持有自身的一条租约；嵌套调用由各自实例验证。
    let mut seen = vec![[false; 2]; body.blocks.len()];
    let mut pending = vec![(body.entry, 0i8, Vec::<String>::new())];
    let mut exits = 0;
    while let Some((id, mut leases, mut path)) = pending.pop() {
        path.push(format!("b{}", id.0));
        assert!((0..=1).contains(&leases), "pin 租约重复取得或释放");
        if std::mem::replace(&mut seen[id.index()][leases as usize], true) {
            continue;
        }
        let block = &body.blocks[id.index()];
        for instruction in &body.instructions[crate::lir::body::range(&block.instructions)] {
            if let Op::Call(call) = &instruction.op {
                leases += effect(&call.target);
            }
            assert!((0..=1).contains(&leases), "pin 租约重复取得或释放");
        }
        match &block.terminator {
            Terminator::Return { .. } | Terminator::ResumePanic { .. } => {
                assert_eq!(leases, 0, "{} 出口遗漏 pin release：{path:?}", body.name);
                exits += 1;
            }
            Terminator::Invoke {
                call,
                normal,
                unwind,
                ..
            } => {
                let mut normal_path = path.clone();
                normal_path.push(format!("normal({})", effect(&call.target)));
                pending.push((
                    body.edges[normal.index()].to,
                    leases + effect(&call.target),
                    normal_path,
                ));
                path.push(format!("unwind({})", effect(&call.target)));
                // acquire 正常返回才取得租约；release 一进入便履行清理义务。
                // 本检查验证每条路径恰好调用一次，释放效果由 runtime 状态测试验证。
                pending.push((
                    body.edges[unwind.index()].to,
                    leases + effect(&call.target).min(0),
                    path,
                ));
            }
            _ => {
                for edge in &body.edges {
                    if edge.from == id {
                        pending.push((edge.to, leases, path.clone()));
                    }
                }
            }
        }
    }
    assert!(exits > 0);
}

fn cleanup_exits(body: &Body, start: usize) -> u8 {
    use crate::lir::body::Terminator;
    let mut seen = vec![false; body.blocks.len()];
    let mut pending = vec![start];
    let mut exits = 0;
    while let Some(index) = pending.pop() {
        if std::mem::replace(&mut seen[index], true) {
            continue;
        }
        match body.blocks[index].terminator {
            Terminator::Return { .. } => exits |= 1,
            Terminator::ResumePanic { .. } => exits |= 2,
            _ => {}
        }
        for edge in &body.edges {
            if edge.from.index() == index {
                pending.push(edge.to.index());
            }
        }
    }
    exits
}

#[test]
fn pin_pages_unlink_middle_head_and_tail_once() {
    let output = pin_compilation();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let pages = [73728, 77824, 81920];
    let payloads = [ARENA + 16, ARENA + 48, ARENA + 80];
    put(&mut memory, COORDINATOR + 168, pages[0]);
    for index in 0usize..3 {
        let page = pages[index];
        put(
            &mut memory,
            page,
            pages.get(index + 1).copied().unwrap_or(0),
        );
        put(
            &mut memory,
            page + 8,
            index.checked_sub(1).map_or(0, |previous| pages[previous]),
        );
        put(&mut memory, page + 16, 1);
        put(&mut memory, page + 32, payloads[index]);
        put(&mut memory, page + 40, 1);
        let header = payloads[index] - 16;
        let control = word(&memory, header);
        put(&mut memory, header, control | PINNED);
    }
    for offset in [3920, 3928] {
        put(&mut memory, PROCESSOR + offset, 8192 + 3 * 4096);
    }
    put(&mut memory, PROCESSOR + 3936, 5);
    for index in [1, 0, 2] {
        harness.run(
            "pinning",
            "pin_release",
            &[pages[index] + 32, payloads[index]],
            &mut memory,
        );
        assert_eq!(word(&memory, payloads[index] - 16) & PINNED, 0);
        if index == 1 {
            assert_eq!(word(&memory, pages[0]), pages[2]);
            assert_eq!(word(&memory, pages[2] + 8), pages[0]);
        }
        if index == 0 {
            assert_eq!(word(&memory, COORDINATOR + 168), pages[2]);
        }
    }
    assert_eq!(word(&memory, COORDINATOR + 168), 0);
    assert_eq!(
        harness.released,
        [(pages[1], 4096), (pages[0], 4096), (pages[2], 4096)]
    );
    for offset in [3920, 3928, 3968, 3976] {
        assert_eq!(word(&memory, PROCESSOR + offset), 8192);
    }
    assert_eq!(word(&memory, PROCESSOR + 3936), 2);
}

#[test]
fn nested_pin_keeps_object_graph_until_last_release() {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../../gugu-compiler/tests/fixtures/pin.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.reserve_result = PIN_PAGE;
    harness.block_budget = 32768;
    let (mut memory, meta) = graph_memory();
    put(&mut memory, 768, 1);
    put(&mut memory, 768 + 8, ARENA);
    put(&mut memory, 768 + 16, 2097152);
    let first = harness.run("pinning", "pin_acquire", &[ARENA + 16], &mut memory)[0];
    let second = harness.run("pinning", "pin_acquire", &[ARENA + 24], &mut memory)[0];
    assert_eq!(first, PIN_PAGE + 32);
    assert_eq!(second, first, "内部引用与对象首地址共享同一固定计数");
    assert_eq!(word(&memory, first + 8), 2);
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 21], &mut memory),
        [2]
    );
    assert_eq!(word(&memory, ARENA) & PINNED, PINNED);
    assert_eq!(harness.reserved, [(PIN_PAGE, 4096)]);
    put(&mut memory, ROOTS + 8, 0);
    for remaining in [2, 1] {
        assert_eq!(
            harness.run(
                "collect",
                "gc_collect_major",
                &[PROCESSOR, ROOTS, 1],
                &mut memory
            ),
            [0]
        );
        assert_eq!(
            word(&memory, meta + 24) & 0b101,
            0b101,
            "pin 强根必须同时保留字段可达资源"
        );
        assert!(!harness.released.contains(&(CONTROL, 4096)));
        assert_eq!(word(&memory, first + 8), remaining);
        harness.run("pinning", "pin_release", &[first, ARENA + 24], &mut memory);
    }
    assert_eq!(word(&memory, ARENA) & PINNED, 0);
    assert_eq!(word(&memory, COORDINATOR + 168), 0);
    assert_eq!(
        harness
            .released
            .iter()
            .filter(|entry| **entry == (PIN_PAGE, 4096))
            .count(),
        1
    );
    assert_eq!(
        harness.run(
            "collect",
            "gc_collect_major",
            &[PROCESSOR, ROOTS, 1],
            &mut memory
        ),
        [0]
    );
    assert_eq!(word(&memory, meta + 24) & 0b101, 0);
    assert_eq!(
        harness.submitted,
        [vec![SECOND_CONTROL, 43, 1], vec![CONTROL, 42, 1]]
    );
    for control in [SECOND_CONTROL, CONTROL] {
        harness.run(
            "resource_state",
            "complete_close",
            &[control, 0],
            &mut memory,
        );
    }
    assert_eq!(
        harness
            .released
            .iter()
            .filter(|entry| **entry == (CONTROL, 4096))
            .count(),
        1
    );
}
