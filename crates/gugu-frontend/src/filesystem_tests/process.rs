//! 解释生产进程、环境和管道 LIR；原生调用与调度事件使用固定脚本。
use super::harness::{Harness, put, word};
use super::{Compilation, CompileRequest, Compiler, TargetName};

mod child;
mod context;
mod environment;
mod output;
mod pipe;
mod snapshot;

const PIPE: &str = "std/runtime/process_pipe.gg";
const CONTEXT: &str = "std/runtime/process_context.gg";

struct Protocol {
    frontend: crate::frontend::FrontendOutput,
    lir: crate::lir::Validated,
    metadata: crate::frontend::gc::GcMetadataBundle,
}

impl Protocol {
    fn harness(&self) -> Harness<'_> {
        let mut harness = Harness::from_bodies(self.frontend.hir.module(), self.lir.bodies());
        harness.set_sizes(&self.frontend.gir);
        harness.metadata = Some(self.metadata.clone());
        harness
    }
}

fn protocol(target: TargetName) -> Protocol {
    protocol_source(
        target,
        include_str!("../../../gugu-compiler/tests/fixtures/process_protocol.gg"),
    )
}

fn protocol_source(target: TargetName, source: &str) -> Protocol {
    use crate::frontend::{self, PrefixMode, SourceInput, cfg::CfgContext};
    let queries = crate::QueryEngine::new();
    let mut sources = crate::SourceMap::new(vec![
        crate::SourceSnapshot::from_str("main.gg", source).unwrap(),
    ])
    .unwrap();
    let resources =
        crate::runtime::RuntimeResources::new(crate::testing::provider().artifact(target).unwrap());
    let frontend = frontend::bootstrap_with(
        SourceInput::Sources {
            packages: &[],
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "single-file",
            require_main: true,
            cfg: &CfgContext::target_only(target),
            external_packages: &Default::default(),
            embed: None,
        },
        &queries,
        resources,
        PrefixMode::Install,
    )
    .unwrap();
    let lir = crate::lir::build(
        &frontend.hir,
        &frontend.gir,
        &frontend.mono,
        target,
        crate::CompressionPolicyV1::disabled(),
        &crate::backend::x64::oracle::X64Oracle::new(target.descriptor().cpu_baseline),
        &queries,
        &sources,
        (&frontend.installed, resources.prebuilt()),
    )
    .unwrap();
    let metadata = crate::frontend::gc::derive(
        &frontend.mono.universe,
        &frontend.gir,
        frontend.hir.module(),
    )
    .unwrap();
    Protocol {
        frontend,
        lir,
        metadata,
    }
}

fn store(memory: &mut [u8], address: u64, values: &[u64]) {
    for (index, &value) in values.iter().enumerate() {
        put(memory, address + u64::try_from(index).unwrap() * 8, value);
    }
}

fn child(harness: &mut Harness<'_>, memory: &mut Vec<u8>) -> u64 {
    store(memory, 512, &[71, 71, u64::MAX, u64::MAX, u64::MAX]);
    let value = harness.invoke(STATE, "adopt", &[512], memory);
    store(memory, 512, &value);
    word(memory, 512)
}

#[expect(clippy::ptr_arg, reason = "签名需与 native_hook 一致")]
fn exit_code(_: &mut Harness<'_>, op: u64, request: u64, memory: &mut Vec<u8>) {
    if op == 0 {
        put(memory, request + 16, 7);
    }
}

const STATE: &str = "std/runtime/process_state.gg";
const STATUS: u64 = 1024;

fn compilation() -> Compilation {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/process_status.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

fn status(memory: &mut [u8], kind: u64, value: u64) {
    put(memory, STATUS, kind);
    put(memory, STATUS + 8, value);
}

#[test]
fn linux_process_protocol_contracts() {
    let output = protocol(TargetName::X86_64Linux);
    child::child_exit_is_shared_and_cancellation_removes_only_its_waiter(&output);
    child::explicit_close_kills_once_and_detach_never_kills(&output);
    child::child_wait_precancel_has_no_effect_and_supervisor_does_not_keep_runtime_alive(&output);
    child::nine_children_progress_without_blocking_worker_credits(&output);
    child::wait_closes_stdin_but_try_wait_does_not(&output);
    environment::linux_environment_empty_value_remove_and_case_sensitive_names(&output);
    environment::stdio_pipe_and_null_failures_release_every_acquired_handle(&output);
    pipe::linux_pipe_consumes_short_completion_before_cancellation(&output);
    pipe::pipe_round_robin_reaches_cancelled_tail_beyond_service_budget(&output);
    pipe::pipe_eof_short_write_and_broken_pipe_have_distinct_terminal_results(&output);
    context::environment_names_reject_invalid_bytes_before_any_native_call(&output);
}

#[test]
fn windows_process_protocol_contracts() {
    let output = protocol(TargetName::X86_64Windows);
    environment::windows_invalid_environment_block_is_freed_before_error_returns(&output);
    pipe::windows_cancel_keeps_overlapped_request_until_terminal_completion(&output);
    context::windows_command_line_round_trips_empty_quotes_slashes_and_surrogates();
}

#[test]
fn exit_status_decodes_code_signal_and_success() {
    let output = compilation();
    let mut harness = Harness::new(&output);
    let mut memory = vec![0u8; 65536];
    status(&mut memory, 1, 7);
    assert_eq!(
        harness.invoke(STATE, "code", &[STATUS], &mut memory),
        vec![0, 7],
        "退出码 7 编码为 Some(7)"
    );
    assert_eq!(
        harness.invoke(STATE, "signal", &[STATUS], &mut memory),
        vec![1, 0],
        "正常退出没有信号"
    );
    assert_eq!(
        harness.invoke(STATE, "success", &[STATUS], &mut memory),
        vec![0]
    );
    status(&mut memory, 1, 0);
    assert_eq!(
        harness.invoke(STATE, "success", &[STATUS], &mut memory),
        vec![1]
    );
    status(&mut memory, 2, 9);
    assert_eq!(
        harness.invoke(STATE, "code", &[STATUS], &mut memory),
        vec![1, 0],
        "信号致死没有退出码"
    );
    assert_eq!(
        harness.invoke(STATE, "signal", &[STATUS], &mut memory),
        vec![0, 9],
        "信号编号 9 编码为 Some(9)"
    );
}
