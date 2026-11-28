//! 资源终结、计数、关闭与账本回收执行真实 LIR；平台操作和调度由固定钩子控制。
use super::harness::{runtime_bodies, runtime_bodies_for};
use super::{Compilation, CompileRequest, Compiler, TargetName};
use crate::lir::body::{Body, CallTarget, Op, Symbol};

mod admission;
mod callback;
mod channel;
mod collection;
mod directory;
mod foreign;
mod join;
mod snapshot;

fn runtime_harness() -> ResourceHarness<'static> {
    let (hir, bodies) = runtime_bodies();
    ResourceHarness::from_bodies(hir, bodies)
}

fn runtime_harness_for(target: TargetName) -> ResourceHarness<'static> {
    let (hir, bodies) = runtime_bodies_for(target);
    ResourceHarness::from_bodies(hir, bodies)
}

const CELL: u64 = 128;
const CONTROL: u64 = 256;

fn word(memory: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(memory[at as usize..at as usize + 8].try_into().unwrap())
}
fn put(memory: &mut [u8], at: u64, value: u64) {
    memory[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}

#[derive(Debug)]
struct RuntimeExit(u64);

type CallbackHook = fn(&[u64], &mut Vec<u8>) -> u64;
type ResourceHook = fn(&mut ResourceHarness<'_>, &mut Vec<u8>);

pub(super) struct ResourceHarness<'a> {
    instances: std::collections::HashMap<[u8; 32], &'a Body>,
    runtime_bodies: std::collections::HashMap<(&'a str, &'a str), &'a Body>,
    globals: std::collections::HashMap<[u8; 32], u64>,
    next_global: u64,
    submitted: Vec<Vec<u64>>,
    reclaimed: Vec<u64>,
    completion_error: u64,
    poll_complete: bool,
    start_error: u64,
    reserve_result: u64,
    reserve_script: std::collections::VecDeque<u64>,
    decommit_error: u64,
    decommit_alias: Option<(u64, u64)>,
    decommitted: Vec<(u64, u64)>,
    committed: Vec<(u64, u64)>,
    commit_error: u64,
    reserved: Vec<(u64, u64)>,
    released: Vec<(u64, u64)>,
    pressure_entries: usize,
    started: Vec<u64>,
    woken: Vec<u64>,
    gc_config: u64,
    clock: std::collections::VecDeque<u64>,
    block_budget: usize,
    on_callback: Option<CallbackHook>,
    on_park: Option<ResourceHook>,
    on_close_wait: Option<ResourceHook>,
    depth: usize,
}

impl<'a> ResourceHarness<'a> {
    fn new(output: &'a Compilation) -> Self {
        Self::from_bodies(
            output.hir.as_ref().unwrap().module(),
            output.lir.as_ref().unwrap().bodies(),
        )
    }

    pub(super) fn from_bodies(hir: &'a crate::frontend::hir::Module, bodies: &'a [Body]) -> Self {
        let instances = bodies.iter().map(|body| (body.instance, body)).collect();
        let owners: std::collections::HashMap<_, _> =
            bodies.iter().map(|body| (body.owner, body)).collect();
        let runtime_bodies = hir
            .definitions
            .iter()
            .filter_map(|definition| {
                let location = definition.location.as_ref()?;
                let path = &hir.sources[location.source as usize].path;
                let module = path.strip_prefix("std/runtime/")?.strip_suffix(".gg")?;
                let body = *owners.get(&definition.key)?;
                Some(((module, definition.name.as_str()), body))
            })
            .collect();
        Self {
            instances,
            runtime_bodies,
            globals: std::collections::HashMap::new(),
            next_global: 12288,
            submitted: Vec::new(),
            reclaimed: Vec::new(),
            completion_error: 0,
            poll_complete: false,
            start_error: 0,
            reserve_result: 12288,
            reserve_script: std::collections::VecDeque::new(),
            decommit_error: 0,
            decommit_alias: None,
            decommitted: Vec::new(),
            committed: Vec::new(),
            commit_error: 0,
            reserved: Vec::new(),
            released: Vec::new(),
            pressure_entries: 0,
            started: Vec::new(),
            woken: Vec::new(),
            gc_config: 0,
            clock: std::collections::VecDeque::new(),
            block_budget: 256,
            on_callback: None,
            on_park: None,
            on_close_wait: None,
            depth: 0,
        }
    }

    fn body(&self, module: &str, name: &str) -> &'a Body {
        self.runtime_bodies
            .get(&(module, name))
            .unwrap_or_else(|| panic!("未找到 runtime 入口 {module}.{name}"))
    }

    fn is_body(&self, body: &Body, module: &str, name: &str) -> bool {
        self.runtime_bodies
            .get(&(module, name))
            .is_some_and(|candidate| candidate.instance == body.instance)
    }

    pub(super) fn run(
        &mut self,
        module: &str,
        name: &str,
        args: &[u64],
        memory: &mut Vec<u8>,
    ) -> Vec<u64> {
        let body = self.body(module, name);
        if let Some((bytes, _, _)) = body.signature.sret
            && body.params(body.entry).len() == args.len() + 2
        {
            let base = memory.len();
            memory.resize(base + bytes as usize, 0);
            let mut actual = Vec::with_capacity(args.len() + 1);
            actual.push(base as u64);
            actual.extend_from_slice(args);
            assert!(self.evaluate(body, &actual, memory).is_empty());
            let result = (0..bytes)
                .step_by(8)
                .map(|offset| word(memory, base as u64 + offset))
                .collect();
            memory.truncate(base);
            result
        } else {
            self.evaluate(body, args, memory)
        }
    }

    fn evaluate(&mut self, body: &Body, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        self.depth += 1;
        assert!(self.depth <= 32, "固定资源调用链不得无限递归");
        let budget = self.block_budget;
        let returned = super::interpret_bounded(
            body,
            args,
            memory,
            &mut |target, args, memory| self.call(target, args, memory),
            budget,
        );
        self.depth -= 1;
        returned
    }

    fn call(
        &mut self,
        target: super::Boundary<'_>,
        args: &[u64],
        memory: &mut Vec<u8>,
    ) -> Vec<u64> {
        let target = match target {
            super::Boundary::Storage(Op::SymbolAddr(Symbol::Global {
                key,
                bytes,
                thread_local: false,
            })) => {
                let address = match self.globals.entry(*key) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let address = self.next_global;
                        self.next_global += bytes.div_ceil(8) * 8;
                        entry.insert(address);
                        address
                    }
                };
                return vec![address];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::ThreadId) => return vec![1],
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::CallbackControl) => {
                return vec![64];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::CallbackQueue) => {
                return vec![self.on_callback.expect("须登记回调机器边界")(
                    args, memory,
                )];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::CageControl) => {
                return vec![768];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::StackReadWord) => {
                return vec![word(memory, word(memory, HOT + 80) - args[0])];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::StackCopyRaw) => {
                let stack = word(memory, HOT + 80) - args[0];
                let (source, destination) = if args[3] == 0 {
                    (stack, args[1])
                } else {
                    (args[1], stack)
                };
                memory.copy_within(
                    source as usize..(source + args[2]) as usize,
                    destination as usize,
                );
                return vec![];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::ProcessExit) => {
                std::panic::panic_any(RuntimeExit(args[0]));
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::MonotonicNanos) => {
                return vec![self.clock.pop_front().expect("须登记单调时钟输入")];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::GcConfig) => {
                assert_ne!(self.gc_config, 0, "用例须显式提供 GC 配置");
                return vec![self.gc_config];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::GcEnter) => {
                assert_eq!(args[0], PROCESSOR + 3616);
                assert_eq!(word(memory, args[0]) & 0xffffffff, 4);
                assert_eq!(word(memory, PROCESSOR + 3912), 2);
                self.pressure_entries += 1;
                self.run("native_memory", "pressure_poll", &[PROCESSOR], memory);
                return vec![0];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::CurrentSlot) => {
                return vec![HOT];
            }
            super::Boundary::Platform(
                crate::runtime::platform_schema::PlatformOp::ReserveAligned,
            ) => {
                let base = self
                    .reserve_script
                    .pop_front()
                    .unwrap_or(self.reserve_result);
                self.reserved.push((base, args[0]));
                return vec![base];
            }
            super::Boundary::Platform(crate::runtime::platform_schema::PlatformOp::Decommit) => {
                let base = word(memory, args[0]);
                let bytes = word(memory, args[0] + 8);
                self.decommitted.push((base, bytes));
                if self.decommit_error == 0 {
                    let backing = self.decommit_alias.map_or(base, |(virtual_base, backing)| {
                        assert_eq!(base, virtual_base);
                        backing
                    });
                    memory[backing as usize..(backing + bytes) as usize].fill(0);
                }
                return vec![self.decommit_error];
            }
            super::Boundary::Platform(crate::runtime::platform_schema::PlatformOp::Commit) => {
                self.committed
                    .push((word(memory, args[0]), word(memory, args[0] + 8)));
                return vec![self.commit_error];
            }
            super::Boundary::Platform(crate::runtime::platform_schema::PlatformOp::Release) => {
                let base = word(memory, args[0]);
                self.released.push((base, word(memory, args[0] + 8)));
                let mut registered = word(memory, PROCESSOR + 4016);
                for _ in 0..4 {
                    if registered == 0 {
                        break;
                    }
                    assert_ne!(registered, base, "释放前必须摘除资源登记");
                    registered = word(memory, registered + 184);
                }
                assert_eq!(registered, 0, "固定登记链必须终止");
                assert!(!self.reclaimed.contains(&base), "控制页仅归还平台一次");
                self.reclaimed.push(base);
                return Vec::new();
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::FsPoll) => {
                return vec![u64::from(self.poll_complete)];
            }
            super::Boundary::Machine(crate::runtime::RuntimeMachineOp::FsStart) => {
                self.started.push(args[0]);
                return vec![self.start_error];
            }
            super::Boundary::Call(target) => target,
            other => panic!("未登记的机器边界：{other:?}"),
        };
        let CallTarget::Instance(instance) = target else {
            panic!("未登记的资源测试边界：{target:?}")
        };
        let body = *self.instances.get(instance).expect("调用目标");
        if self.is_body(body, "scheduler", "park_current") {
            self.on_park.take().expect("未登记 park 事件")(self, memory);
            Vec::new()
        } else if self.is_body(body, "roots", "gc_types_section") {
            vec![TYPE_SECTION]
        } else if self.is_body(body, "scheduler", "ready_hot") {
            self.woken.push(args[0]);
            Vec::new()
        } else if self.is_body(body, "fs_platform", "enqueue_close") {
            assert_eq!(word(memory, args[0] + 80), 0, "存在操作租约时不得关闭");
            self.submitted.push(args.to_vec());
            Vec::new()
        } else if self.is_body(body, "resource_state", "reclaim") {
            assert_eq!(word(memory, args[0]), 0, "用户租约未归还不得回收");
            assert_ne!(word(memory, args[0] + 8) & 8, 0, "关闭未完成不得回收");
            self.evaluate(body, args, memory)
        } else if self.is_body(body, "fs_platform", "await_close") {
            if let Some(event) = self.on_close_wait.take() {
                event(self, memory);
            }
            if word(memory, args[0] + 8) & 8 == 0 {
                assert_eq!(self.submitted.len(), 1, "等待前必须提交一次关闭");
                self.run(
                    "resource_state",
                    "complete_close",
                    &[args[0], self.completion_error],
                    memory,
                );
            }
            vec![word(memory, args[0] + 88)]
        } else {
            self.evaluate(body, args, memory)
        }
    }
}

fn memory() -> Vec<u8> {
    let mut memory = vec![0; 16384];
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, CONTROL + 192, PROCESSOR);
    put(&mut memory, PROCESSOR + 4016, CONTROL);
    put(&mut memory, PROCESSOR + 3968, 4096);
    put(&mut memory, PROCESSOR + 3976, 4096);
    put(&mut memory, PROCESSOR + 3984, 1);
    put(&mut memory, PROCESSOR + 3920, 4096);
    put(&mut memory, PROCESSOR + 3928, 4096);
    put(&mut memory, PROCESSOR + 3936, 1);
    put(&mut memory, CELL, CONTROL);
    put(&mut memory, CONTROL, 1);
    put(&mut memory, CONTROL + 64, 42);
    put(&mut memory, CONTROL + 72, 1);
    memory
}

#[test]
fn resource_lifecycle_admission_and_directory_contracts() {
    let output = compile_fs_api();
    last_lease_waits_for_operation_then_closes_and_reclaims_once(&output);
    explicit_close_is_shared_idempotent_and_preserves_error(&output);
    gc_finalization_releases_resource_leaf_once_and_skips_forwarded_copy(&output);
    gc_finalization_nested_program_resumes_without_managed_allocation(&output);
    running_cancellation_retains_credit_until_worker_exits(&output);
    completed_request_wins_over_unobserved_cancellation(&output);
    queued_cancellation_and_start_failure_return_credit_once(&output);
    full_worker_pool_keeps_waiter_queued_until_credit_is_returned(&output);
    reclaim_unlinks_middle_tail_and_head_and_drains_ledger(&output);
    runtime_mapping_ledger_charges_only_successful_platform_operations(&output);
    pressure_hysteresis_defers_gc_and_allows_one_cycle_per_episode(&output);
    pressure_threshold_does_not_overflow_at_largest_memory_limit(&output);
    startup_environment_reads_gc_target_and_memory_limit_keys(&output);
    startup_environment_reads_diagnostics_and_backtrace_keys(&output);
    file_pool_initialization_does_not_allocate_worker_stacks(&output);
    worker_stack_is_reused_then_trimmed_only_after_thread_exit(&output);
    pressure_reuses_completed_stack_for_waiter_before_trimming(&output);
    admission::run_all(&output);
    directory::run_all(&output);
    snapshot::blocking_snapshot_tracks_waiting_inflight_completion_and_stack_cache(&output);
}

fn compile_fs_api() -> Compilation {
    let output = Compiler::shared().compile_to_lir(fs_api_request());
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

fn last_lease_waits_for_operation_then_closes_and_reclaims_once(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = memory();
    harness.run("resource", "acquire_control", &[CONTROL], &mut memory);
    assert_eq!(
        harness.run("resource", "begin", &[CELL], &mut memory),
        vec![42]
    );
    harness.run("resource", "release_control", &[CONTROL], &mut memory);
    assert_eq!(word(&memory, CONTROL), 1);
    assert!(harness.submitted.is_empty());
    harness.run("resource", "release_control", &[CONTROL], &mut memory);
    assert_eq!(word(&memory, CONTROL), 0);
    assert_eq!(
        harness.run("counters", "pending_returns", &[PROCESSOR], &mut memory),
        [4096]
    );
    assert!(harness.submitted.is_empty(), "等待操作租约归还");
    assert_eq!(
        harness.run("resource", "begin", &[CELL], &mut memory),
        vec![u64::MAX]
    );
    harness.run("resource", "end", &[CELL], &mut memory);
    harness.run("resource", "request_close", &[CONTROL], &mut memory);
    assert_eq!(harness.submitted, vec![vec![CONTROL, 42, 1]]);
    assert!(harness.reclaimed.is_empty(), "提交不等于完成");
    harness.run(
        "resource_state",
        "complete_close",
        &[CONTROL, 0],
        &mut memory,
    );
    assert_eq!(harness.reclaimed, vec![CONTROL]);
    assert_eq!(harness.released, vec![(CONTROL, 4096)]);
    harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
}

fn explicit_close_is_shared_idempotent_and_preserves_error(output: &Compilation) {
    for error in [0, 5] {
        let mut harness = ResourceHarness::new(output);
        harness.completion_error = error;
        let mut memory = memory();
        harness.run("resource", "acquire_control", &[CONTROL], &mut memory);
        for _ in 0..2 {
            assert_eq!(
                harness.run("resource", "close", &[CELL], &mut memory),
                vec![error]
            );
            assert_eq!(
                harness.run("resource", "begin", &[CELL], &mut memory),
                vec![u64::MAX]
            );
        }
        assert_eq!(harness.submitted, vec![vec![CONTROL, 42, 1]]);
        harness.run("resource", "release_control", &[CONTROL], &mut memory);
        assert!(harness.reclaimed.is_empty());
        harness.run("resource", "release_control", &[CONTROL], &mut memory);
        assert_eq!(harness.reclaimed, vec![CONTROL]);
        assert_eq!(harness.released, vec![(CONTROL, 4096)]);
        harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
    }
}

const TYPE_SECTION: u64 = 2048;
const OBJECT: u64 = 512;
const RESOURCE_FLAG: u64 = 0x40000000000;
const FORWARDED_FLAG: u64 = 0x4000000000;

fn resource_object_memory() -> Vec<u8> {
    let mut memory = memory();
    put(&mut memory, OBJECT, RESOURCE_FLAG);
    put(&mut memory, OBJECT + 8, 8);
    put(&mut memory, OBJECT + 16, CONTROL);
    // 固定合法 ResourceCell 叶描述符：8 字节 payload、资源 flag、单字节 End 程序。
    put(&mut memory, TYPE_SECTION + 24, 128);
    put(&mut memory, TYPE_SECTION + 48, 256);
    let descriptor = TYPE_SECTION + 128;
    put(&mut memory, descriptor, 8);
    memory[descriptor as usize + 12..descriptor as usize + 16].copy_from_slice(&8u32.to_le_bytes());
    memory[descriptor as usize + 36..descriptor as usize + 40].copy_from_slice(&1u32.to_le_bytes());
    memory
}

fn gc_finalization_releases_resource_leaf_once_and_skips_forwarded_copy(output: &Compilation) {
    for flags in [RESOURCE_FLAG, RESOURCE_FLAG | FORWARDED_FLAG, 0] {
        let mut harness = ResourceHarness::new(output);
        let mut memory = resource_object_memory();
        put(&mut memory, OBJECT, flags);
        for _ in 0..2 {
            harness.run("collect", "finalize_resource", &[OBJECT], &mut memory);
        }
        if flags == RESOURCE_FLAG {
            assert_eq!(word(&memory, OBJECT) & RESOURCE_FLAG, 0);
            assert_eq!(word(&memory, OBJECT + 16), 0, "释放后清空资源载荷");
            assert_eq!(word(&memory, CONTROL), 0);
            assert_eq!(harness.submitted, vec![vec![CONTROL, 42, 1]]);
            assert!(harness.reclaimed.is_empty(), "终结只入队，不伪造关闭完成");
            harness.run(
                "resource_state",
                "complete_close",
                &[CONTROL, 0],
                &mut memory,
            );
            assert_eq!(harness.reclaimed, vec![CONTROL]);
            assert_eq!(harness.released, vec![(CONTROL, 4096)]);
            harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
        } else {
            assert_eq!(word(&memory, CONTROL), 1);
            assert_eq!(word(&memory, OBJECT + 16), CONTROL);
            assert!(harness.submitted.is_empty());
        }
    }
}

fn gc_finalization_nested_program_resumes_without_managed_allocation(output: &Compilation) {
    // Switch 活跃分支中的 Repeat 释放两个槽，然后父程序继续释放第三个槽。
    let mut repeated = vec![21, 1, 2, 1];
    repeated.extend_from_slice(&4u32.to_le_bytes());
    repeated.extend_from_slice(&[20, 0, 1, 0]);
    repeated.push(0);
    let mut program = vec![22, 0, 1, 1];
    program.extend_from_slice(&0u64.to_le_bytes());
    program.extend_from_slice(&u32::try_from(repeated.len()).unwrap().to_le_bytes());
    program.extend_from_slice(&repeated);
    program.extend_from_slice(&1u32.to_le_bytes());
    program.extend_from_slice(&[0, 20, 24, 1, 0]);
    for tag in [0, 1] {
        let mut harness = ResourceHarness::new(output);
        // 此 harness 不接受托管分配边界；资源遍历取地址产生的盒会立即失败。
        let mut memory = resource_object_memory();
        let descriptor = TYPE_SECTION + 128;
        let leaf = descriptor + 80;
        put(&mut memory, TYPE_SECTION + 48, 320);
        put(&mut memory, descriptor, 32);
        let descriptor_at = usize::try_from(descriptor).unwrap();
        memory[descriptor_at + 36..descriptor_at + 40]
            .copy_from_slice(&u32::try_from(program.len()).unwrap().to_le_bytes());
        put(&mut memory, leaf, 8);
        let leaf_at = usize::try_from(leaf).unwrap();
        memory[leaf_at + 12..leaf_at + 16].copy_from_slice(&8u32.to_le_bytes());
        memory[leaf_at + 32..leaf_at + 36]
            .copy_from_slice(&u32::try_from(program.len()).unwrap().to_le_bytes());
        memory[leaf_at + 36..leaf_at + 40].copy_from_slice(&1u32.to_le_bytes());
        let program_at = usize::try_from(TYPE_SECTION + 320).unwrap();
        memory[program_at..program_at + program.len()].copy_from_slice(&program);
        put(&mut memory, OBJECT + 8, 32);
        put(&mut memory, OBJECT + 16, tag);
        for offset in [24, 32, 40] {
            put(&mut memory, OBJECT + offset, CONTROL);
        }
        put(&mut memory, CONTROL, if tag == 0 { 3 } else { 1 });
        for _ in 0..2 {
            harness.run("collect", "finalize_resource", &[OBJECT], &mut memory);
        }
        let inactive = if tag == 0 { 0 } else { CONTROL };
        assert_eq!(word(&memory, OBJECT + 24), inactive);
        assert_eq!(word(&memory, OBJECT + 32), inactive);
        assert_eq!(
            word(&memory, OBJECT + 40),
            0,
            "父程序必须从 Switch 之后继续"
        );
        assert_eq!(word(&memory, CONTROL), 0);
        assert_eq!(harness.submitted, vec![vec![CONTROL, 42, 1]]);
    }
}

const PROCESSOR: u64 = 4096;
const POOL: u64 = 8192;
const HOT: u64 = 128;
const COLD: u64 = 1024;
const CANCEL: u64 = 2048;
const REQUEST: u64 = POOL + 64;
const WAIT: u64 = COLD + 272;

fn request_memory() -> Vec<u8> {
    let mut memory = vec![0; 16384];
    put(&mut memory, PROCESSOR + 4024, POOL);
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, HOT + 48, COLD);
    put(&mut memory, REQUEST + 104, HOT);
    put(&mut memory, REQUEST + 112, CANCEL);
    put(&mut memory, CANCEL, 1);
    memory
}

fn running_cancellation_retains_credit_until_worker_exits(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = request_memory();
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, REQUEST + 120), 1);
    assert_eq!(
        word(&memory, REQUEST + 104),
        HOT,
        "运行中的请求继续占用额度"
    );
    assert!(harness.woken.is_empty());
    put(&mut memory, REQUEST + 64, 7);
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, WAIT + 80), 7, "取消仍保留已读前缀长度");
    assert_eq!(word(&memory, WAIT + 88), u64::MAX);
    assert_eq!(word(&memory, REQUEST + 104), 0);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.woken, vec![HOT]);
}

fn completed_request_wins_over_unobserved_cancellation(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = request_memory();
    harness.poll_complete = true;
    put(&mut memory, REQUEST + 64, 7);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, WAIT + 80), 7);
    assert_eq!(word(&memory, WAIT + 88), 0);
    assert_eq!(word(&memory, REQUEST + 120), 0);
    assert_eq!(harness.woken, vec![HOT]);
}

fn queued_cancellation_and_start_failure_return_credit_once(output: &Compilation) {
    for (cancelled, error) in [(true, 0), (false, 11)] {
        let mut harness = ResourceHarness::new(output);
        harness.start_error = error;
        let mut memory = request_memory();
        put(&mut memory, REQUEST + 104, 0);
        put(&mut memory, CANCEL, u64::from(cancelled));
        put(&mut memory, COLD + 464, CANCEL);
        put(&mut memory, POOL, HOT);
        put(&mut memory, POOL + 8, HOT);
        put(&mut memory, POOL + 32, 1);
        harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
        assert_eq!(
            word(&memory, WAIT + 88),
            if cancelled { u64::MAX } else { error }
        );
        assert_eq!(word(&memory, POOL + 32), 0);
        assert_eq!(word(&memory, REQUEST + 104), 0);
        assert_eq!(harness.started.len(), usize::from(!cancelled));
        assert_eq!(
            harness.reserved.len(),
            usize::from(!cancelled),
            "预取消不分配线程栈"
        );
        put(&mut memory, PROCESSOR + 3888, 2);
        harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
        assert_eq!(
            harness.released,
            if cancelled {
                vec![]
            } else {
                vec![(STACK_BASE, STACK_BYTES)]
            }
        );
        assert_eq!(word(&memory, STACK_SLOT), 0);
        harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
        assert_eq!(harness.woken, vec![HOT]);
    }
}

fn full_worker_pool_keeps_waiter_queued_until_credit_is_returned(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = request_memory();
    put(&mut memory, CANCEL, 0);
    for slot in 0..8 {
        put(
            &mut memory,
            REQUEST + slot * 128 + 104,
            HOT + (slot + 1) * 64,
        );
    }
    put(&mut memory, POOL, HOT);
    put(&mut memory, POOL + 8, HOT);
    put(&mut memory, POOL + 32, 1);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert!(
        harness.started.is_empty(),
        "全部额度占用时不得创建第九个 worker"
    );
    assert!(harness.woken.is_empty());
    assert_eq!(word(&memory, POOL), HOT);
    assert_eq!(word(&memory, POOL + 32), 1);
    put(&mut memory, REQUEST + 104, 0);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.started, vec![REQUEST]);
    assert_eq!(word(&memory, REQUEST + 104), HOT);
    assert_eq!(word(&memory, POOL + 32), 0);
    assert!(harness.woken.is_empty(), "提交成功仍要等待完成");
}

fn reclaim_unlinks_middle_tail_and_head_and_drains_ledger(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = memory();
    let nodes = [CONTROL, 1024, 2048];
    put(&mut memory, PROCESSOR + 3968, 3 * 4096);
    put(&mut memory, PROCESSOR + 3976, 3 * 4096);
    put(&mut memory, PROCESSOR + 3984, 3);
    put(&mut memory, PROCESSOR + 3920, 3 * 4096);
    put(&mut memory, PROCESSOR + 3928, 3 * 4096);
    put(&mut memory, PROCESSOR + 3936, 3);
    for (index, &node) in nodes.iter().enumerate() {
        put(&mut memory, node, 0);
        put(&mut memory, node + 8, 14);
        put(
            &mut memory,
            node + 176,
            if index == 0 { 0 } else { nodes[index - 1] },
        );
        put(
            &mut memory,
            node + 184,
            nodes.get(index + 1).copied().unwrap_or(0),
        );
        put(&mut memory, node + 192, PROCESSOR);
    }
    harness.run("resource_state", "reclaim", &[nodes[1]], &mut memory);
    assert_eq!(word(&memory, nodes[0] + 184), nodes[2]);
    assert_eq!(word(&memory, nodes[2] + 176), nodes[0]);
    assert_eq!(word(&memory, PROCESSOR + 3984), 2);
    harness.run("resource_state", "reclaim", &[nodes[2]], &mut memory);
    assert_eq!(word(&memory, nodes[0] + 184), 0);
    harness.run("resource_state", "reclaim", &[nodes[0]], &mut memory);
    assert_eq!(word(&memory, PROCESSOR + 4016), 0);
    assert_eq!(harness.reclaimed, vec![nodes[1], nodes[2], nodes[0]]);
    harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
}

fn runtime_mapping_ledger_charges_only_successful_platform_operations(output: &Compilation) {
    for commit_error in [0, 12] {
        let mut harness = ResourceHarness::new(output);
        let mut memory = vec![0; 16384];
        put(&mut memory, HOT + 16, PROCESSOR);
        harness.run(
            "native_memory",
            "adopt_boot",
            &[PROCESSOR, 1 << 30, 65536, 1 << 24],
            &mut memory,
        );
        let reserved = word(&memory, PROCESSOR + 3920);
        let committed = word(&memory, PROCESSOR + 3928);
        assert_eq!(reserved, (1 << 30) + 8192);
        assert_eq!(committed, 65536 + 8192);
        assert_eq!(word(&memory, PROCESSOR + 3952), 1 << 24);
        harness.reserve_result = 0;
        assert_eq!(
            harness.run("native_memory", "reserve_aligned", &[1, 4096], &mut memory),
            vec![0]
        );
        assert_eq!(word(&memory, PROCESSOR + 3920), reserved);
        assert_eq!(word(&memory, PROCESSOR + 3936), 3);
        harness.reserve_result = 12288;
        assert_eq!(
            harness.run("native_memory", "reserve_aligned", &[1, 4096], &mut memory),
            vec![12288]
        );
        assert_eq!(word(&memory, PROCESSOR + 3920), reserved + 4096);
        put(&mut memory, 2048, 12288);
        put(&mut memory, 2056, 1);
        harness.commit_error = commit_error;
        assert_eq!(
            harness.run("native_memory", "commit", &[2048], &mut memory),
            vec![commit_error]
        );
        let charged = if commit_error == 0 { 4096 } else { 0 };
        assert_eq!(word(&memory, PROCESSOR + 3928), committed + charged);
        harness.run(
            "native_memory",
            "release_mapping",
            &[12288, 1, charged],
            &mut memory,
        );
        assert_eq!(word(&memory, PROCESSOR + 3920), reserved);
        assert_eq!(word(&memory, PROCESSOR + 3928), committed);
        assert_eq!(word(&memory, PROCESSOR + 3936), 3);
        assert_eq!(word(&memory, PROCESSOR + 3944), committed + charged);
        assert_eq!(harness.reclaimed, vec![12288]);
    }
}

fn pressure_hysteresis_defers_gc_and_allows_one_cycle_per_episode(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = vec![0; 16384];
    put(&mut memory, PROCESSOR + 3952, 100);
    for used in [84, 85] {
        put(&mut memory, PROCESSOR + 3928, used);
        harness.run(
            "native_memory",
            "pressure_update",
            &[PROCESSOR],
            &mut memory,
        );
        assert_eq!(word(&memory, PROCESSOR + 3888), u64::from(used == 85));
    }
    harness.run("native_memory", "pressure_poll", &[PROCESSOR], &mut memory);
    assert_eq!(harness.pressure_entries, 0, "启动未就绪不得收集");
    harness.run(
        "native_memory",
        "pressure_enable",
        &[PROCESSOR],
        &mut memory,
    );
    harness.run("native_memory", "pressure_poll", &[PROCESSOR], &mut memory);
    assert_eq!(
        harness.pressure_entries, 0,
        "85% 仅发布压力，不提前执行强制周期"
    );
    assert_eq!(
        word(&memory, PROCESSOR),
        4,
        "水位压力通知 owner drain，不请求强制 GC"
    );
    put(&mut memory, PROCESSOR, 2);
    put(&mut memory, PROCESSOR + 3928, 101);
    harness.run(
        "native_memory",
        "pressure_update",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(
        word(&memory, PROCESSOR),
        7,
        "发布 PREEMPT 必须保留 GC_STOP 和 owner drain"
    );
    for _ in 0..3 {
        harness.run("native_memory", "pressure_poll", &[PROCESSOR], &mut memory);
    }
    assert_eq!(harness.pressure_entries, 1);
    assert_eq!(word(&memory, PROCESSOR + 3904), 1);
    put(&mut memory, PROCESSOR + 3928, 71);
    harness.run(
        "native_memory",
        "pressure_update",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(word(&memory, PROCESSOR + 3888), 2);
    put(&mut memory, PROCESSOR + 3928, 70);
    harness.run(
        "native_memory",
        "pressure_update",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(word(&memory, PROCESSOR + 3888), 2, "低水位本身不解除压力");
    for (category, expected) in [(1, 2), (2, 2), (4, 0)] {
        harness.run(
            "native_memory",
            "pressure_drained",
            &[PROCESSOR, category],
            &mut memory,
        );
        assert_eq!(word(&memory, PROCESSOR + 3888), expected);
    }
    put(&mut memory, PROCESSOR + 3928, 101);
    harness.run(
        "native_memory",
        "pressure_update",
        &[PROCESSOR],
        &mut memory,
    );
    harness.run("native_memory", "pressure_poll", &[PROCESSOR], &mut memory);
    assert_eq!(harness.pressure_entries, 2);
    assert_eq!(word(&memory, PROCESSOR + 3896), 2);
    put(&mut memory, PROCESSOR + 3952, 0);
    harness.run(
        "native_memory",
        "pressure_update",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(word(&memory, PROCESSOR + 3888), 0);
}

fn pressure_threshold_does_not_overflow_at_largest_memory_limit(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = vec![0; 16384];
    let threshold = ((u128::from(u64::MAX) * 85).div_ceil(100)) as u64;
    put(&mut memory, PROCESSOR + 3952, u64::MAX);
    for used in [threshold - 1, threshold] {
        put(&mut memory, PROCESSOR + 3928, used);
        harness.run(
            "native_memory",
            "pressure_update",
            &[PROCESSOR],
            &mut memory,
        );
        assert_eq!(
            word(&memory, PROCESSOR + 3888),
            u64::from(used == threshold)
        );
    }
}

fn startup_environment_reads_gc_target_and_memory_limit_keys(output: &Compilation) {
    for (limit, expected) in [("1MiB", 1048576), ("off", 0)] {
        let mut harness = ResourceHarness::new(output);
        let mut memory = vec![0; 16384];
        let entries = [
            "A=1".to_owned(),
            "GUGU_RUNTIME_GC_TARGET=125".to_owned(),
            format!("GUGU_RUNTIME_MEMORY_LIMIT={limit}"),
        ];
        for (index, entry) in entries.iter().enumerate() {
            let address = 2048 + index * 128;
            memory[address..address + entry.len()].copy_from_slice(entry.as_bytes());
            put(&mut memory, 1024 + index as u64 * 8, address as u64);
        }
        harness.run(
            "core",
            "parse_startup_environment",
            &[1024, 512],
            &mut memory,
        );
        assert_eq!(word(&memory, 512), 125);
        assert_eq!(word(&memory, 520), expected);
    }
    // 整个可访问区域只有一个 NUL，键比较不得进行跨终止符的 word 读取。
    for name in ["is_gc_target_key", "is_memory_limit_key"] {
        let mut harness = ResourceHarness::new(output);
        let mut memory = vec![0];
        assert_eq!(harness.run("core", name, &[0], &mut memory), vec![0]);
    }
}

fn startup_environment_reads_diagnostics_and_backtrace_keys(output: &Compilation) {
    for (diagnostics, backtrace, expected) in [
        ("json", "full", (1, 2)),
        ("both", "0", (2, 0)),
        ("text", "1", (0, 1)),
    ] {
        let mut harness = ResourceHarness::new(output);
        let mut memory = vec![0; 16384];
        let entries = [
            "A=1".to_owned(),
            format!("GUGU_RUNTIME_DIAGNOSTICS={diagnostics}"),
            format!("GUGU_BACKTRACE={backtrace}"),
        ];
        for (index, entry) in entries.iter().enumerate() {
            let address = 2048 + index * 128;
            memory[address..address + entry.len()].copy_from_slice(entry.as_bytes());
            put(&mut memory, 1024 + index as u64 * 8, address as u64);
        }
        harness.run(
            "core",
            "parse_startup_environment",
            &[1024, 512],
            &mut memory,
        );
        assert_eq!(word(&memory, 512 + 56), expected.0);
        assert_eq!(word(&memory, 512 + 64), expected.1);
    }
    // 变量缺失时使用规范默认值 text=0 与 triggering=1。
    let mut harness = ResourceHarness::new(output);
    let mut memory = vec![0; 16384];
    harness.run(
        "core",
        "parse_startup_environment",
        &[1024, 512],
        &mut memory,
    );
    assert_eq!(word(&memory, 512 + 56), 0);
    assert_eq!(word(&memory, 512 + 64), 1);
}

const STACK_SLOT: u64 = POOL + 64 + 8 * 128;
const STACK_BASE: u64 = 12288;
const STACK_BYTES: u64 = 65536;

fn file_pool_initialization_does_not_allocate_worker_stacks(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = request_memory();
    put(&mut memory, PROCESSOR + 4024, 0);
    assert_eq!(
        harness.run("fs_platform", "state", &[PROCESSOR], &mut memory),
        vec![STACK_BASE]
    );
    assert_eq!(harness.reserved, vec![(STACK_BASE, 4096)]);
    assert_eq!(word(&memory, PROCESSOR + 3928), 4096);
    for index in 0..8 {
        assert_eq!(word(&memory, STACK_BASE + 1088 + index * 8), 0);
    }
}

fn worker_stack_is_reused_then_trimmed_only_after_thread_exit(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = request_memory();
    put(&mut memory, CANCEL, 0);
    harness.run("fs_platform", "stack_for", &[POOL, 0], &mut memory);
    harness.run("fs_platform", "stack_for", &[POOL, 0], &mut memory);
    assert_eq!(harness.reserved, vec![(STACK_BASE, STACK_BYTES)]);
    assert_eq!(word(&memory, STACK_SLOT), STACK_BASE + STACK_BYTES);
    // 压力时仍有在途 syscall：额度和栈都保持，不能因为取消就归还。
    put(&mut memory, PROCESSOR + 3888, 2);
    put(&mut memory, CANCEL, 1);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert!(harness.released.is_empty());
    assert_eq!(word(&memory, STACK_SLOT), STACK_BASE + STACK_BYTES);
    // 无压力的完成保留栈以便复用。
    put(&mut memory, PROCESSOR + 3888, 0);
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert!(harness.released.is_empty());
    assert_eq!(word(&memory, REQUEST + 104), 0);
    // 已退出的空闲槽在下一次压力 service 归还一次。
    put(&mut memory, PROCESSOR + 3888, 2);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.released, vec![(STACK_BASE, STACK_BYTES)]);
    assert_eq!(word(&memory, STACK_SLOT), 0);
    assert_eq!(word(&memory, PROCESSOR + 3928), 0);
    assert_eq!(word(&memory, PROCESSOR + 3936), 0);
    harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
}

fn pressure_reuses_completed_stack_for_waiter_before_trimming(output: &Compilation) {
    let mut harness = ResourceHarness::new(output);
    let mut memory = request_memory();
    put(&mut memory, CANCEL, 0);
    harness.run("fs_platform", "stack_for", &[POOL, 0], &mut memory);
    put(&mut memory, PROCESSOR + 3888, 2);
    // 等待者与刚完成的调用者是不同协程。
    let waiting = HOT + 64;
    put(&mut memory, waiting + 48, COLD + 512);
    put(&mut memory, POOL, waiting);
    put(&mut memory, POOL + 8, waiting);
    put(&mut memory, POOL + 32, 1);
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(harness.started, vec![REQUEST]);
    assert_eq!(harness.woken, vec![HOT]);
    assert_eq!(word(&memory, REQUEST + 104), waiting);
    assert_eq!(harness.reserved, vec![(STACK_BASE, STACK_BYTES)]);
    assert!(harness.released.is_empty());
}

fn fs_api_request() -> CompileRequest {
    CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/fs_api.gg"),
        TargetName::X86_64Linux,
    )
}
