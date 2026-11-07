//! 固定 park/close 等待事件；队列登记、游标操作租约、EOF 和关闭均执行真实 LIR。
use super::*;
const ALIAS: u64 = 2048;

fn setup(output: &Compilation) -> (ResourceHarness<'_>, Vec<u8>) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = memory();
    put(&mut memory, HOT + 48, COLD);
    put(&mut memory, CONTROL + 72, 2);
    put(&mut memory, ALIAS, CONTROL);
    harness.run("resource", "acquire_control", &[CONTROL], &mut memory);
    assert_eq!(
        harness.run("resource", "begin_directory", &[CELL], &mut memory),
        [42]
    );
    assert_eq!(word(&memory, CONTROL + 80), 1);
    (harness, memory)
}
fn assert_waiting(memory: &[u8]) {
    assert_eq!(word(memory, CONTROL + 80), 1, "等待者不得同时取得游标");
    assert_eq!(word(memory, CONTROL + 144), HOT);
    assert_eq!(word(memory, CONTROL + 152), HOT);
}
fn publish_eof(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    assert_waiting(memory);
    harness.run("resource", "exhaust_directory", &[CELL], memory);
    harness.run("resource", "end_directory", &[CELL], memory);
}
fn finish_inflight(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    assert_eq!(word(memory, CONTROL + 8) & 2, 2, "close 先禁止新操作");
    assert!(
        harness.submitted.is_empty(),
        "已有操作未结束时不能关闭 OS 句柄"
    );
    harness.run("resource", "end_directory", &[CELL], memory);
    assert_eq!(harness.submitted, vec![vec![CONTROL, 42, 2]]);
}
fn request_close(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    assert_waiting(memory);
    harness.on_close_wait = Some(finish_inflight);
    assert_eq!(harness.run("resource", "close", &[CELL], memory), [0]);
}
fn release_aliases(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    harness.run("resource", "release_control", &[CONTROL], memory);
    assert!(harness.reclaimed.is_empty(), "尚有目录副本");
    harness.run("resource", "release_control", &[CONTROL], memory);
    assert_eq!(harness.released, vec![(CONTROL, 4096)]);
    harness.run("native_memory", "verify_drained", &[PROCESSOR], memory);
}

#[test]
fn directory_waiter_observes_shared_eof_then_closed_state() {
    let output = compile();
    let (mut harness, mut memory) = setup(&output);
    harness.on_park = Some(publish_eof);
    assert_eq!(
        harness.run("resource", "begin_directory", &[ALIAS], &mut memory),
        [42]
    );
    assert_eq!(harness.woken, [HOT]);
    assert_eq!(word(&memory, CONTROL + 80), 1);
    assert_eq!(
        harness.run("resource", "directory_exhausted", &[ALIAS], &mut memory),
        [1]
    );
    assert_eq!(word(&memory, CONTROL + 144), 0);
    assert_eq!(word(&memory, CONTROL + 152), 0);
    harness.run("resource", "end_directory", &[ALIAS], &mut memory);
    assert_eq!(harness.run("resource", "close", &[CELL], &mut memory), [0]);
    assert_eq!(harness.run("resource", "close", &[ALIAS], &mut memory), [0]);
    assert_eq!(
        harness.run("resource", "begin_directory", &[ALIAS], &mut memory),
        [u64::MAX]
    );
    assert_eq!(harness.submitted, vec![vec![CONTROL, 42, 2]]);
    release_aliases(&mut harness, &mut memory);
}

#[test]
fn closing_directory_waits_for_active_read_and_rejects_queued_reader() {
    let output = compile();
    let (mut harness, mut memory) = setup(&output);
    harness.on_park = Some(request_close);
    assert_eq!(
        harness.run("resource", "begin_directory", &[ALIAS], &mut memory),
        [u64::MAX]
    );
    assert_eq!(word(&memory, CONTROL + 80), 0);
    assert_eq!(harness.woken, [HOT]);
    assert_eq!(word(&memory, CONTROL + 144), 0);
    assert_eq!(word(&memory, CONTROL + 152), 0);
    assert!(harness.on_park.is_none() && harness.on_close_wait.is_none());
    assert_eq!(harness.run("resource", "close", &[ALIAS], &mut memory), [0]);
    assert_eq!(harness.submitted.len(), 1);
    release_aliases(&mut harness, &mut memory);
}
