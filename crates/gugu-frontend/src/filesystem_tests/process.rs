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
}

impl Protocol {
    fn harness(&self) -> Harness<'_> {
        let mut harness = Harness::from_bodies(self.frontend.hir.module(), self.lir.bodies());
        harness.set_sizes(&self.frontend.gir);
        harness.metadata = Some(
            crate::frontend::gc::derive(
                &self.frontend.mono.universe,
                &self.frontend.gir,
                self.frontend.hir.module(),
            )
            .unwrap(),
        );
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
    Protocol { frontend, lir }
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
    let output = Compiler::shared().compile(CompileRequest::single_file(
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
