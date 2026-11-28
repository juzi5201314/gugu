use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, put, word},
};
use crate::{
    lir::body::{CallTarget, RuntimeCall},
    runtime::RuntimeMachineOp,
};

const SAMPLE: u64 = 512;
const GROUP: u64 = 640;
const PANIC: u64 = 768;
const CASE_STATE: u64 = 896;

#[expect(clippy::ptr_arg, reason = "签名需与 call_hook 一致")]
fn scheduler(
    h: &mut Harness<'_>,
    call: &CallTarget,
    args: &[u64],
    memory: &mut Vec<u8>,
) -> Option<Vec<u64>> {
    match call {
        CallTarget::Instance(key) => match h.names.get(key).copied() {
            Some(("std/runtime/scheduler.gg", "subtree_group_new")) => Some(vec![GROUP]),
            Some(("std/runtime/scheduler.gg", "subtree_group_attach")) => Some(vec![]),
            Some(("std/runtime/scheduler.gg", "subtree_group_pending")) => Some(vec![0]),
            Some(("std/runtime/scheduler.gg", "subtree_group_panics")) => {
                Some(vec![word(memory, CASE_STATE + 8)])
            }
            _ => None,
        },
        CallTarget::Runtime(RuntimeCall::Spawn) => Some(vec![1]),
        CallTarget::Runtime(RuntimeCall::JoinWait) => {
            put(memory, args[1], SAMPLE);
            Some(vec![if word(memory, CASE_STATE) == 0 { 1 } else { 2 }])
        }
        CallTarget::Runtime(RuntimeCall::JoinPanic) => Some(vec![word(memory, CASE_STATE)]),
        _ => None,
    }
}

#[expect(clippy::ptr_arg, reason = "签名需与 machine_hook 一致")]
fn report(
    h: &mut Harness<'_>,
    op: RuntimeMachineOp,
    args: &[u64],
    memory: &mut Vec<u8>,
) -> Vec<u64> {
    assert_eq!(op, RuntimeMachineOp::ReportWrite);
    assert_eq!(args[0], 1);
    let bytes = &memory[args[1] as usize..(args[1] + args[2]) as usize];
    h.calls
        .push((0, bytes.iter().map(|byte| u64::from(*byte)).collect()));
    vec![args[2]]
}

#[test]
fn benchmark_samples_use_direct_results_and_preserve_reports() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file_bench(
        "bench.gg",
        include_str!("fixtures/bench_results.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut h = Harness::new(&output);
    h.clock_step = 1_000_000;
    let mut memory = vec![0; 65536];
    let entry = h
        .names
        .iter()
        .find_map(|(key, &(path, name))| {
            let body = h.bodies[key];
            (path == "__gugu_harness.gg"
                && name == "__gugu_bwrap_0"
                && body.params(body.entry).len() == 1)
                .then_some(body)
        })
        .expect("生成 benchmark 包装器");
    assert!(entry.signature.sret.is_none(), "协程只返回托管样本指针");
    let result = h.run(entry, &[], &mut memory);
    let [sample] = result.as_slice() else {
        panic!("benchmark 必须只返回一条直接车道")
    };
    let expected = [9, 1, 1_000_000, 1_000_000, 1_000_000, 64];
    for (index, value) in expected.into_iter().enumerate() {
        assert_eq!(word(&memory, sample + index as u64 * 8), value);
        put(&mut memory, SAMPLE + index as u64 * 8, value);
    }
    h.call_hook = Some(scheduler);
    h.machine_hook = Some(report);
    put(&mut memory, PANIC, PANIC + 16);
    put(&mut memory, PANIC + 8, 4);
    memory[(PANIC + 16) as usize..(PANIC + 20) as usize].copy_from_slice(b"boom");
    for (panic, descendants, expected, failed) in [
        (
            0,
            0,
            "bench bench.measured ... samples=9 iters=1 median=1000000ns/iter min=1000000 max=1000000 thpt=64000B/s\n",
            0,
        ),
        (
            PANIC,
            0,
            "bench bench.measured ... FAILED\n    panic: boom\n",
            1,
        ),
        (
            0,
            1,
            "bench bench.measured ... FAILED\n    未处理的分离协程 panic: 1\n",
            1,
        ),
    ] {
        h.calls.clear();
        put(&mut memory, CASE_STATE, panic);
        put(&mut memory, CASE_STATE + 8, descendants);
        let result = h.invoke("__gugu_harness.gg", "main", &[], &mut memory);
        assert_eq!(result, [failed, failed]);
        let bytes: Vec<u8> = h
            .calls
            .iter()
            .flat_map(|(_, bytes)| bytes.iter().map(|byte| u8::try_from(*byte).unwrap()))
            .collect();
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }
}
