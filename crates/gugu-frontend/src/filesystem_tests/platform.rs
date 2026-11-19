//! 执行真实平台适配器，仅替换原生请求与 raw 缓冲分配；固定脚本逐次消费。
use super::{
    Boundary, Compilation, CompileRequest, Compiler, TargetName, interpret_bounded, runtime_body,
};
use crate::lir::body::{Body, CallTarget};
use std::collections::{BTreeMap, VecDeque};

mod directory;
mod reparse;

const INPUT: u64 = 128;
const OUTPUT: u64 = 1024;

struct Reply {
    op: u64,
    value: u64,
    error: u64,
    expected: Vec<(usize, u64)>,
    writes: Vec<(usize, Vec<u8>)>,
}
impl Reply {
    fn new(op: u64, value: u64, error: u64) -> Self {
        Self {
            op,
            value,
            error,
            expected: Vec::new(),
            writes: Vec::new(),
        }
    }
    fn argument(mut self, index: usize, value: u64) -> Self {
        self.expected.push((index, value));
        self
    }
    fn write(mut self, argument: usize, bytes: Vec<u8>) -> Self {
        self.writes.push((argument, bytes));
        self
    }
}

struct Adapter<'a> {
    output: &'a Compilation,
    replies: VecDeque<Reply>,
    live: BTreeMap<u64, u64>,
    next: u64,
    depth: usize,
    block_budget: usize,
}
impl<'a> Adapter<'a> {
    fn new(output: &'a Compilation, replies: Vec<Reply>) -> Self {
        Self {
            output,
            replies: replies.into(),
            live: BTreeMap::new(),
            next: 8192,
            depth: 0,
            block_budget: 256,
        }
    }
    fn evaluate(&mut self, body: &Body, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        self.depth += 1;
        assert!(self.depth <= 32);
        let block_budget = self.block_budget;
        let result = interpret_bounded(
            body,
            args,
            memory,
            &mut |target, args, memory| self.call(target, args, memory),
            block_budget,
        );
        self.depth -= 1;
        result
    }
    fn call(&mut self, target: Boundary<'_>, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        let Boundary::Call(CallTarget::Instance(instance)) = target else {
            panic!("未登记的适配器边界：{target:?}")
        };
        let body = self
            .output
            .lir
            .as_ref()
            .unwrap()
            .bodies()
            .iter()
            .find(|body| body.instance == *instance)
            .unwrap();
        let matches =
            |name| body.instance == runtime_body(self.output, "fs_platform", name).instance;
        if matches("raw_allocate") || matches("control_allocate") {
            let base = self.next;
            self.next += 16384;
            assert!(
                args[0] <= 16384 && self.next <= 65536,
                "固定输入最多三个小缓冲"
            );
            memory[base as usize..self.next as usize].fill(0);
            assert!(self.live.insert(base, args[0]).is_none());
            vec![base]
        } else if matches("raw_free") || matches("control_free") {
            assert_eq!(
                self.live.remove(&args[0]),
                Some(args[1]),
                "每个缓冲按原大小归还一次"
            );
            Vec::new()
        } else if matches("native_call") {
            self.reply(body.signature.sret.is_some(), args, memory)
        } else {
            self.evaluate(body, args, memory)
        }
    }
    fn reply(&mut self, sret: bool, args: &[u64], memory: &mut [u8]) -> Vec<u64> {
        let input = &args[usize::from(sret)..];
        let reply = self
            .replies
            .pop_front()
            .expect("不得执行脚本外的额外原生操作");
        assert_eq!(input[0], reply.op);
        for (index, expected) in reply.expected {
            assert_eq!(input[index], expected, "op={} arg={index}", reply.op);
        }
        for (argument, bytes) in reply.writes {
            let buffer = input[argument] as usize;
            memory[buffer..buffer + bytes.len()].copy_from_slice(&bytes);
        }
        if sret {
            let out = args[0] as usize;
            memory[out..out + 8].copy_from_slice(&reply.value.to_le_bytes());
            memory[out + 8..out + 16].copy_from_slice(&reply.error.to_le_bytes());
            Vec::new()
        } else {
            vec![reply.value, reply.error]
        }
    }

    fn run(&mut self, module: &str, name: &str, args: &[u64]) -> (Vec<u64>, Vec<u8>) {
        let body = runtime_body(self.output, module, name);
        let mut memory = vec![0; 65536];
        memory[OUTPUT as usize..OUTPUT as usize + 64].fill(0xa5);
        let mut arguments = args.to_vec();
        if body.signature.sret.is_some() {
            arguments.insert(0, 0);
        }
        let mut result = self.evaluate(body, &arguments, &mut memory);
        if body.signature.sret.is_some() {
            result = memory[..16]
                .as_chunks::<8>()
                .0
                .iter()
                .map(|word| u64::from_le_bytes(*word))
                .collect();
        }
        assert!(self.replies.is_empty(), "必须完成脚本中的清理调用");
        assert!(self.live.is_empty(), "失败后不得遗留原生缓冲");
        (result, memory)
    }
}

fn compile(target: TargetName) -> Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/fs_api.gg"),
        target,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

#[test]
fn linux_metadata_reads_x86_kernel_stat_fields() {
    let output = compile(TargetName::X86_64Linux);
    let mut stat = vec![0; 144];
    for (offset, value) in [
        (0, 11_u64),
        (8, 12),
        (24, 0o100640),
        (48, 123),
        (56, 4096),
        (64, 8),
        (88, 999),
        (96, 888),
    ] {
        stat[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    let reply = Reply::new(262, 0, 0)
        .argument(1, !99)
        .argument(2, INPUT)
        .argument(4, 0)
        .write(3, stat);
    let mut adapter = Adapter::new(&output, vec![reply]);
    let (result, memory) = adapter.run("fs_platform", "linux_stat", &[5, INPUT, 1, OUTPUT]);
    assert_eq!(result, vec![0, 0]);
    let actual: Vec<_> = memory[OUTPUT as usize..OUTPUT as usize + 64]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_le_bytes(*word))
        .collect();
    assert_eq!(actual, [123, 0o100640, 1, 999, 888, 0, 11, 12]);
}

#[test]
fn linux_metadata_failure_leaves_output_and_frees_scratch() {
    let output = compile(TargetName::X86_64Linux);
    let mut adapter = Adapter::new(&output, vec![Reply::new(262, 0, 13).argument(2, INPUT)]);
    let (result, memory) = adapter.run("fs_platform", "linux_stat", &[5, INPUT, 1, OUTPUT]);
    assert_eq!(result, vec![0, 13]);
    assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 64], &[0xa5; 64]);
}

#[test]
fn linux_cross_device_rename_has_no_copy_fallback() {
    let output = compile(TargetName::X86_64Linux);
    let mut adapter = Adapter::new(
        &output,
        vec![Reply::new(82, 0, 18).argument(1, INPUT).argument(2, OUTPUT)],
    );
    assert_eq!(
        adapter
            .run("fs_platform", "call", &[17, INPUT, OUTPUT, 0, 0, 0, 0])
            .0,
        vec![0, 18]
    );
}

#[test]
fn darwin_canonicalize_reports_required_capacity_without_truncating() {
    let output = compile(TargetName::X86_64Macos);
    let mut path = vec![b'a'; 300];
    path[0] = b'/';
    path[151] = b'/';
    path.push(0);
    for capacity in [256, 512] {
        let mut adapter = Adapter::new(
            &output,
            vec![
                Reply::new(21, 1, 0)
                    .argument(1, INPUT)
                    .write(2, path.clone()),
            ],
        );
        adapter.block_budget = 4096;
        let (result, memory) = adapter.run(
            "fs_platform",
            "call",
            &[21, INPUT, OUTPUT, capacity, 0, 0, 0],
        );
        assert_eq!(result, vec![300, 0]);
        if capacity < 300 {
            assert_eq!(&memory[OUTPUT as usize..][..64], &[0xa5; 64]);
        } else {
            assert_eq!(&memory[OUTPUT as usize..][..300], &path[..300]);
        }
    }
}

#[test]
fn linux_canonical_failure_closes_handle_and_preserves_primary_error() {
    let output = compile(TargetName::X86_64Linux);
    let mut adapter = Adapter::new(
        &output,
        vec![
            Reply::new(2, 42, 0).argument(1, INPUT),
            Reply::new(89, 0, 13).argument(2, OUTPUT),
            Reply::new(3, 0, 5).argument(1, 42),
        ],
    );
    assert_eq!(
        adapter
            .run("fs_platform", "linux_canonical", &[INPUT, OUTPUT, 64])
            .0,
        vec![0, 13]
    );
}

#[test]
fn windows_cancelled_read_preserves_completed_prefix_and_frees_count_buffer() {
    let output = compile(TargetName::X86_64Windows);
    let mut adapter = Adapter::new(
        &output,
        vec![
            Reply::new(2, 0, u64::MAX)
                .argument(1, 42)
                .argument(3, 64)
                .argument(9, 777)
                .write(2, b"abc".to_vec())
                .write(4, 3u32.to_le_bytes().to_vec()),
        ],
    );
    let (result, memory) = adapter.run(
        "fs_windows",
        "call_cancel",
        &[2, 42, OUTPUT, 64, 0, 0, 0, 777],
    );
    assert_eq!(result, vec![3, u64::MAX]);
    assert_eq!(&memory[OUTPUT as usize..OUTPUT as usize + 3], b"abc");
    assert_eq!(memory[OUTPUT as usize + 3], 0xa5, "未完成的尾部保持原样");
}

#[test]
fn windows_nofollow_validates_before_truncation_and_closes_on_error() {
    let output = compile(TargetName::X86_64Windows);
    for (attributes, stat_error, truncate_error, expected) in [
        (1024, 0, 0, 1921),
        (0, 5, 0, 5),
        (0, 0, 112, 112),
        (0, 0, 0, 0),
    ] {
        let mut replies = vec![
            Reply::new(0, 42, 0).argument(5, 4), // OPEN_ALWAYS，不能提前截断。
            Reply::new(5, 0, stat_error)
                .argument(1, 42)
                .write(2, u32::to_le_bytes(attributes).to_vec()),
        ];
        if attributes == 0 && stat_error == 0 {
            replies.push(
                Reply::new(20, 0, truncate_error)
                    .argument(1, 42)
                    .argument(2, 6),
            );
        }
        if expected != 0 {
            replies.push(Reply::new(1, 0, 0).argument(1, 42));
        }
        let mut adapter = Adapter::new(&output, replies);
        let result = adapter.run("fs_windows", "open", &[INPUT, 2 | 8 | 16]).0;
        assert_eq!(
            result,
            if expected == 0 {
                vec![42, 0]
            } else {
                vec![0, expected]
            }
        );
    }
}
