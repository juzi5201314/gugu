use super::*;

const CHANNEL: u64 = 512;
const PAYLOAD: u64 = 3072;

fn channel_program() -> Compilation {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("../fixtures/channel_waits.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

fn channel_memory() -> Vec<u8> {
    let mut memory = request_memory();
    put(&mut memory, PROCESSOR + 4048, 9216);
    put(&mut memory, CHANNEL + 72, 8);
    put(&mut memory, PAYLOAD, 9);
    memory
}

#[test]
fn rendezvous_close_does_not_redeliver_previous_payload() {
    let output = channel_program();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = channel_memory();
    harness.on_park = Some(|harness, memory| {
        harness.run("channel", "channel_close", &[CHANNEL], memory);
    });
    assert_eq!(
        harness.run(
            "channel",
            "channel_receive",
            &[CHANNEL, PAYLOAD, 0],
            &mut memory
        ),
        [0]
    );
    assert_eq!(word(&memory, PAYLOAD), 9);
    assert_eq!(word(&memory, CHANNEL + 64), 0);
    assert_eq!(harness.woken, [HOT]);
}

#[test]
fn rendezvous_delivery_remains_successful_when_channel_closes_before_resume() {
    let output = channel_program();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = channel_memory();
    put(&mut memory, PAYLOAD + 8, 42);
    harness.on_park = Some(|harness, memory| {
        assert_eq!(
            harness.run(
                "channel",
                "channel_try_send",
                &[CHANNEL, PAYLOAD + 8, 0],
                memory
            ),
            [1]
        );
        harness.run("channel", "channel_close", &[CHANNEL], memory);
    });
    assert_eq!(
        harness.run(
            "channel",
            "channel_receive",
            &[CHANNEL, PAYLOAD, 0],
            &mut memory
        ),
        [1]
    );
    assert_eq!(word(&memory, PAYLOAD), 42);
    assert_eq!(
        harness.run(
            "channel",
            "channel_receive",
            &[CHANNEL, PAYLOAD, 0],
            &mut memory
        ),
        [0]
    );
}

#[test]
fn rendezvous_sender_woken_by_close_reports_failure() {
    let output = channel_program();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = channel_memory();
    harness.on_park = Some(|harness, memory| {
        harness.run("channel", "channel_close", &[CHANNEL], memory);
    });
    assert_eq!(
        harness.run(
            "channel",
            "channel_send",
            &[CHANNEL, PAYLOAD, 0],
            &mut memory
        ),
        [0]
    );
    assert_eq!(word(&memory, CHANNEL + 56), 0);
}

#[test]
fn channel_close_wakes_every_queued_sender_and_receiver() {
    let output = channel_program();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = channel_memory();
    for (head, first, second, hot) in [(56, 9216, 9280, 128), (64, 9344, 9408, 256)] {
        put(&mut memory, CHANNEL + head, first);
        put(&mut memory, first, hot);
        put(&mut memory, first + 48, second);
        put(&mut memory, second, hot + 64);
    }
    harness.run("channel", "channel_close", &[CHANNEL], &mut memory);
    assert_eq!(harness.woken, [128, 192, 256, 320]);
    assert_eq!(word(&memory, CHANNEL + 56), 0);
    assert_eq!(word(&memory, CHANNEL + 64), 0);
}
