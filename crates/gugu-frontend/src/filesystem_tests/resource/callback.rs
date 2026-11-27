//! 回调完成发布只观察已结清的协程根、登记和栈；队列机器协议另有生产序列回归。
use super::*;

const CHILD: u64 = 256;
const COLD: u64 = 2560;
const NODE: u64 = 1536;
const REGISTRY: u64 = 6000;
const ROOT_TABLE: u64 = 8192;

pub(super) fn run_runtime_contracts() {
    callback_storage_reuses_retired_records_and_pressure_releases_mapping();
    callback_waiter_executes_fifo_without_releasing_outer_credit();
    callback_completion_follows_root_and_stack_retirement();
}

fn callback_storage_reuses_retired_records_and_pressure_releases_mapping() {
    let mut harness = runtime_harness();
    harness.block_budget = 2048;
    let mut memory = vec![0; 131072];
    let storage = 16384;
    let cold = storage + 128;
    let node = storage + 960;
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, PROCESSOR + 4008, node);
    put(&mut memory, PROCESSOR + 4064, ROOT_TABLE);
    put(&mut memory, ROOT_TABLE, 8);
    put(&mut memory, node + 8, storage);
    put(&mut memory, storage, 16);
    put(&mut memory, storage + 48, cold);
    put(&mut memory, storage + 80, 65536);
    put(&mut memory, storage + 88, 4096);
    put(&mut memory, cold + 528, storage);
    put(&mut memory, PROCESSOR + 3848, 4096);
    for _ in 0..2 {
        assert_eq!(
            harness.run(
                "scheduler",
                "prepare_child",
                &[PROCESSOR, 0xfeed, NODE, 0, 0, 1],
                &mut memory
            ),
            [storage]
        );
        assert_eq!(word(&memory, PROCESSOR + 4008), 0);
        assert_eq!(word(&memory, PROCESSOR + 4000), node);
        assert_eq!(word(&memory, PROCESSOR + 3848), 0);
        assert_eq!(word(&memory, PROCESSOR + 3880), 4096);
        assert_eq!(word(&memory, ROOT_TABLE + 8), 1);
        put(&mut memory, PROCESSOR + 4032, 1);
        harness.run("scheduler", "finish_hot", &[storage, 0, 0], &mut memory);
        assert_eq!(word(&memory, ROOT_TABLE + 8), 0);
        assert_eq!(word(&memory, PROCESSOR + 3880), 0);
        assert_eq!(word(&memory, PROCESSOR + 3848), 4096);
    }
    assert!(harness.reserved.is_empty());
    assert!(harness.committed.is_empty());
    // 平台边界使用虚拟 reservation；退役阶段不再解引用栈，decommit 映射固定 backing。
    let reserve = 1u64 << 30;
    let high = reserve + 65536;
    put(&mut memory, storage + 80, high);
    harness.decommit_alias = Some((high - 4096, 61440));
    for (offset, value) in [
        (3864, reserve),
        (3872, 4096),
        (3920, reserve + 4096),
        (3928, 8192),
        (3936, 2),
        (3888, 2),
    ] {
        put(&mut memory, PROCESSOR + offset, value);
    }
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(harness.released, [(65536, reserve), (storage, 4096)]);
    for offset in [3848, 3864, 3872, 3880, 3920, 3928, 3936, 4008] {
        assert_eq!(word(&memory, PROCESSOR + offset), 0, "offset {offset}");
    }
}

#[test]
fn runtime_transactions_defer_poll_but_user_loop_keeps_scheduler_entry() {
    use crate::backend::x64::mangle;
    let output = Compiler::shared().compile_full_for_test(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../gugu-compiler/tests/fixtures/runtime_poll.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let machine = output.x64_fragments().unwrap();
    let harness = ResourceHarness::new(&output);
    let user = super::super::named(&output, "user_loop");
    let fragment = machine.find(&mangle::mangle_function(user)).unwrap();
    assert!(
        fragment
            .relocations
            .iter()
            .any(|r| r.target == mangle::mangle_runtime("safepoint_slow"))
    );
    assert!(
        !fragment
            .relocations
            .iter()
            .any(|r| r.target == mangle::mangle_runtime("runtime_poll_defer"))
    );
    let runtime = harness.body("fs_platform", "service_pool");
    let fragment = machine.find(&mangle::mangle_function(runtime)).unwrap();
    assert!(
        fragment
            .relocations
            .iter()
            .any(|r| r.target == mangle::mangle_runtime("runtime_poll_defer"))
    );
    assert!(
        !fragment
            .relocations
            .iter()
            .any(|r| r.target == mangle::mangle_runtime("safepoint_slow"))
    );
}

#[test]
fn linux_callback_adapters_roundtrip_scalar_mixed_and_large_values() {
    verify_adapters(TargetName::X86_64Linux);
}

#[test]
fn windows_callback_adapters_roundtrip_scalar_mixed_and_large_values() {
    verify_adapters(TargetName::X86_64Windows);
}

fn verify_adapters(target: TargetName) {
    {
        let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../gugu-compiler/tests/fixtures/callback.gg"),
            target,
        ));
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        let mut harness = ResourceHarness::new(&output);
        for export in output.lir.as_ref().unwrap().exports() {
            let mut memory = vec![0; 8192];
            let packet = 1024;
            let base = packet + crate::lir::exports::DATA_OFFSET;
            let expected: &[u64] = match export.name.as_str() {
                "callback_add" => {
                    put(&mut memory, base, 17);
                    put(&mut memory, base + 16, 25);
                    &[42]
                }
                "callback_mixed" => &[19, 0x400921fb54442d18],
                "callback_large" => &[23, 29, 31],
                other => panic!("未登记导出 {other}"),
            };
            if export.name != "callback_add" {
                for (index, &value) in expected.iter().enumerate() {
                    put(&mut memory, base + index as u64 * 8, value);
                }
            }
            let adapter = harness.instances[&export.adapter];
            assert!(harness.evaluate(adapter, &[packet], &mut memory).is_empty());
            for (index, &value) in expected.iter().enumerate() {
                assert_eq!(
                    word(&memory, packet + export.result_offset + index as u64 * 8),
                    value,
                    "{target:?} {}",
                    export.name
                );
            }
        }
    }
}

fn callback_waiter_executes_fifo_without_releasing_outer_credit() {
    let mut harness = runtime_harness();
    harness.block_budget = 32768;
    let mut memory = request_memory();
    put(&mut memory, CANCEL, 0);
    for slot in 0..8 {
        put(&mut memory, REQUEST + slot * 128 + 104, 12000 + slot * 128);
        put(&mut memory, REQUEST + slot * 128 + 112, 0);
    }
    put(&mut memory, CHILD + 48, COLD);
    let wait = COLD + 272;
    put(&mut memory, POOL, CHILD);
    put(&mut memory, POOL + 8, CHILD);
    put(&mut memory, POOL + 32, 1);
    put(&mut memory, wait + 8, 0);
    put(&mut memory, wait + 16, 44);
    put(&mut memory, wait + 24, 9000);
    put(&mut memory, wait + 32, 16);
    put(&mut memory, COLD + 464, CANCEL);
    put(&mut memory, 64 + 48, NODE);
    put(&mut memory, NODE + 8, 2);
    put(&mut memory, NODE + 16, 0xfeed);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, NODE + 8), 2);
    assert_eq!(word(&memory, POOL + 32), 1, "独立外部线程不能绕过 credit");
    assert!(harness.started.is_empty());
    put(&mut memory, NODE + 400, 1);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    let req = NODE + 256;
    assert_eq!(word(&memory, NODE + 8), 4);
    assert_eq!(word(&memory, req + 104), CHILD);
    assert_eq!(word(&memory, req + 16), 44);
    assert_eq!(word(&memory, req + 24), 9000);
    assert_eq!(word(&memory, POOL + 32), 0);
    assert!(harness.started.is_empty());
    assert!(harness.woken.is_empty());
    put(&mut memory, CANCEL, 1);
    put(&mut memory, NODE + 16, 0); // managed 回调先结束，借用请求仍在执行。
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, req + 120), 1);
    assert_eq!(word(&memory, NODE + 8), 4);
    assert!(harness.woken.is_empty());
    put(&mut memory, req + 64, 7);
    put(&mut memory, NODE + 8, 5);
    harness.on_callback = Some(|args, memory| {
        assert_eq!(args, [64, NODE, 3]);
        assert_eq!(word(memory, NODE + 256 + 104), 0, "先结清借用请求");
        assert_eq!(word(memory, COLD + 272 + 80), 7);
        assert_eq!(word(memory, COLD + 272 + 88), u64::MAX);
        put(memory, 64 + 48, 0);
        put(memory, NODE + 8, 3);
        0
    });
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.woken, [CHILD]);
    assert_eq!(word(&memory, NODE + 8), 3);
    for slot in 0..8 {
        assert_eq!(
            word(&memory, REQUEST + slot * 128 + 104),
            12000 + slot * 128
        );
    }
    assert!(harness.started.is_empty());
    assert!(harness.released.is_empty());
}

fn callback_completion_follows_root_and_stack_retirement() {
    let mut harness = runtime_harness();
    let mut memory = request_memory();
    put(&mut memory, CHILD + 16, PROCESSOR);
    put(&mut memory, CHILD + 48, COLD);
    put(&mut memory, CHILD + 88, 4096);
    put(&mut memory, COLD + 504, 1);
    put(&mut memory, COLD + 520, NODE);
    put(&mut memory, NODE + 8, 2);
    put(&mut memory, PROCESSOR + 4000, REGISTRY);
    put(&mut memory, PROCESSOR + 4032, 1);
    put(&mut memory, PROCESSOR + 4064, ROOT_TABLE);
    put(&mut memory, PROCESSOR + 3880, 4096);
    put(&mut memory, REGISTRY + 8, CHILD);
    put(&mut memory, ROOT_TABLE, 1);
    put(&mut memory, ROOT_TABLE + 8, 1);
    put(&mut memory, ROOT_TABLE + 16, CHILD);
    harness.on_callback = Some(|args, memory| {
        assert_eq!(args, [64, NODE, 3]);
        assert_eq!(word(memory, COLD + 520), 0, "先撤销协程中的节点借用");
        assert_eq!(word(memory, PROCESSOR + 4000), 0, "先退出活跃登记");
        assert_eq!(word(memory, PROCESSOR + 4008), REGISTRY, "先退役栈登记");
        assert_eq!(word(memory, PROCESSOR + 4032), 0);
        assert_eq!(word(memory, PROCESSOR + 3880), 0);
        assert_eq!(word(memory, ROOT_TABLE + 8), 0, "先结清动态根");
        assert_eq!(word(memory, ROOT_TABLE + 16), 0);
        assert_eq!(word(memory, CHILD), 16);
        assert_eq!(word(memory, COLD + 376), 42);
        let calls = word(memory, NODE);
        put(memory, NODE, calls + 1);
        if calls == 0 { 3 } else { 0 }
    });
    harness.run("scheduler", "finish_hot", &[CHILD, 42, 0], &mut memory);
    assert_eq!(word(&memory, NODE), 2, "锁忙一次后重试完成发布");
}
