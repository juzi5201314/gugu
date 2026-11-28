//! 进程内执行生产 panic 分派与完成路径，仅替换报告和上下文切换边界。
use super::harness::{Harness, put, runtime_bodies, word};
use crate::backend::x64::unwind::{self, Landing, UnwindFunction};
use crate::runtime::{
    RuntimeMachineOp,
    coroutine::CoroutineCold,
    stackmap_codec::{self, CodeLayout, SafepointLayout},
};
use std::mem::offset_of;

const CORE: &str = "std/runtime/core.gg";
const STATE: u64 = 128;
const HOT: u64 = 1024;
const COLD: u64 = 1536;
const GROUP: u64 = 2304;
const PAYLOAD: u64 = 2560;
const PROCESSOR: u64 = 8192;
const MESSAGE: u64 = 3000;
const PATH: u64 = 3024;
const BASE: u64 = 1048576;
const PANIC_ARGS: [u64; 6] = [MESSAGE, 4, PATH, 7, 3, 5];

#[derive(Debug)]
struct RuntimeExit(u64);

#[expect(clippy::ptr_arg, reason = "签名需与 machine_hook 一致")]
fn machine(
    h: &mut Harness<'_>,
    op: RuntimeMachineOp,
    args: &[u64],
    memory: &mut Vec<u8>,
) -> Vec<u64> {
    match op {
        RuntimeMachineOp::ContextRestore => {
            assert_eq!(args, [STATE + 40, HOT, PROCESSOR]);
            assert_eq!(word(memory, STATE + 104), 0, "完成展开后必须允许后续 panic");
            assert!(
                memory[(STATE + 704) as usize..(STATE + 752) as usize]
                    .iter()
                    .all(|byte| *byte == 0)
            );
            h.calls.push((0, args.to_vec()));
            vec![]
        }
        RuntimeMachineOp::ReportWrite => {
            assert_eq!(args[0], 2);
            h.calls.push((1, args.to_vec()));
            vec![args[2]]
        }
        RuntimeMachineOp::ProcessExit => std::panic::panic_any(RuntimeExit(args[0])),
        other => panic!("未登记的 panic 机器边界 {other:?}"),
    }
}

fn panic_metadata(memory: &mut [u8]) {
    let code = CodeLayout {
        code_rva: 4096,
        code_size: 64,
        frame_size: 8,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: true,
        has_stack_interior: false,
    };
    let point = SafepointLayout {
        pc_offset: 8,
        kind: 0,
        dirty: false,
        copy_allowed: true,
        scan_allowed: true,
        slots: std::array::from_fn(|_| vec![0]),
        registers: [0; 5],
        typed: vec![],
        function: 0,
        slot_count: 1,
    };
    let maps = stackmap_codec::encode(&[code], &[(0, point)]).unwrap();
    let unwind = unwind::encode(
        super::TargetName::X86_64Linux,
        &[UnwindFunction::new(4096, 64, 8, &[], 0, 1, false)],
        &[Landing {
            pc_start: 0,
            pc_end: 8,
            landing_pc: 32,
            cleanup: u32::MAX,
        }],
        &[vec![]],
    )
    .unwrap();
    memory[4096..4096 + maps.len()].copy_from_slice(&maps);
    memory[6144..6144 + unwind.len()].copy_from_slice(&unwind);
    for (offset, value) in [
        (48, 4096),
        (56, BASE),
        (64, maps.len() as u64),
        (72, 6144),
        (80, unwind.len() as u64),
    ] {
        put(memory, PAYLOAD + offset, value);
    }
}

fn setup() -> (Harness<'static>, Vec<u8>) {
    let (hir, bodies) = runtime_bodies();
    let mut h = Harness::from_bodies(hir, bodies);
    h.machine_hook = Some(machine);
    h.heap = 24576;
    let mut memory = vec![0; 65536];
    for (at, value) in [
        (STATE, HOT),
        (STATE + 96, PROCESSOR),
        (STATE + 112, PAYLOAD),
        (HOT + 48, COLD),
        (PROCESSOR + 4048, 16384),
        (PROCESSOR + 4056, 24576),
        (PROCESSOR + 3840, 24576),
        (PROCESSOR + 4088, HOT),
    ] {
        put(&mut memory, at, value);
    }
    memory[MESSAGE as usize..MESSAGE as usize + 4].copy_from_slice(b"boom");
    memory[PATH as usize..PATH as usize + 7].copy_from_slice(b"test.gg");
    panic_metadata(&mut memory);
    (h, memory)
}

fn dispatch(h: &mut Harness<'_>, memory: &mut Vec<u8>) {
    let args: Vec<_> = PANIC_ARGS.into_iter().chain([BASE + 4104, STATE]).collect();
    assert_eq!(
        h.invoke(CORE, "panic_dispatch", &args, memory),
        [BASE + 4128]
    );
    assert_eq!(word(memory, STATE + 104), 1);
    for (index, expected) in PANIC_ARGS.into_iter().enumerate() {
        assert_eq!(word(memory, STATE + 704 + index as u64 * 8), expected);
    }
}

#[test]
fn completed_panics_release_dispatch_state_for_later_coroutines() {
    let (mut h, mut memory) = setup();
    let mut records = Vec::new();
    for (index, (waiter, group, result)) in [(HOT, 0, 1), (0, GROUP, 2), (0, 0, 3), (HOT, 0, 1)]
        .into_iter()
        .enumerate()
    {
        // 冷记录位于元数据之后的保留区，每个场景使用独立协程。
        let cold = 32768 + index as u64 * std::mem::size_of::<CoroutineCold>() as u64;
        put(&mut memory, HOT + 48, cold);
        put(
            &mut memory,
            cold + offset_of!(CoroutineCold, join_waiter) as u64,
            waiter,
        );
        put(
            &mut memory,
            cold + offset_of!(CoroutineCold, subtree_group) as u64,
            group,
        );
        put(
            &mut memory,
            cold + offset_of!(CoroutineCold, payload_state) as u64,
            7,
        );
        dispatch(&mut h, &mut memory);
        let args: Vec<_> = PANIC_ARGS.into_iter().chain([STATE]).collect();
        assert_eq!(h.invoke(CORE, "panic_unwind", &args, &mut memory), [0]);
        assert_eq!(
            word(
                &memory,
                cold + offset_of!(CoroutineCold, payload_state) as u64
            ),
            1
        );
        assert_eq!(
            word(
                &memory,
                cold + offset_of!(CoroutineCold, panic_result) as u64
            ),
            result
        );
        assert_eq!(word(&memory, STATE + 752), 2);
        records.push(word(&memory, STATE + 88));
    }
    assert_eq!(word(&memory, GROUP + 8), 1);
    assert_eq!(word(&memory, STATE + 768), 1);
    assert_eq!(h.calls.iter().filter(|(kind, _)| *kind == 0).count(), 4);
    memory[MESSAGE as usize..MESSAGE as usize + 4].fill(0);
    memory[PATH as usize..PATH as usize + 7].fill(0);
    for record in records {
        let message = word(&memory, record) as usize;
        let path = word(&memory, record + 16) as usize;
        assert_eq!(&memory[message..message + 4], b"boom");
        assert_eq!(&memory[path..path + 7], b"test.gg");
        assert_eq!(
            (word(&memory, record + 32), word(&memory, record + 40)),
            (3, 5)
        );
    }
}

#[test]
fn panic_during_active_unwind_remains_fatal() {
    let (mut h, mut memory) = setup();
    dispatch(&mut h, &mut memory);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(&mut h, &mut memory)
    }))
    .expect_err("展开尚未结束时的第二次 panic 必须 fatal");
    assert_eq!(
        failure
            .downcast_ref::<RuntimeExit>()
            .expect("必须退出进程")
            .0,
        2
    );
    assert_eq!(word(&memory, STATE + 104), 1);
    assert!(!h.calls.is_empty(), "fatal 必须输出报告");
}
