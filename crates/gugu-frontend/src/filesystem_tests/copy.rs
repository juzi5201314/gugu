//! 执行 std.fs.copy、std.io.copy 与 write_all 的真实控制流。
//! 单文件操作和受管存储使用显式替身；资源租约状态机由 resource 模块测试覆盖。
//! 本模块验证短写尾片、失败后停止、部分内容和 close 调用顺序，不模拟 GC 或原生句柄。
use super::{Boundary, Compilation, CompileRequest, Compiler, TargetName, interpret};
use crate::lir::body::{Body, CallTarget, Op, RuntimeCall, Symbol};

// std.io.Error 的无载荷判别值；与返回的真实 InvalidInput/WriteZero 一起校验。
const PERMISSION_DENIED: u8 = 1;
const INVALID_INPUT: u8 = 3;
const WRITE_ZERO: u8 = 6;
const INTERRUPTED: u8 = 7;
const STORAGE_FULL: u8 = 26;
const READ_ONLY: u8 = 27;

#[derive(Clone, Copy, Debug)]
enum Failure {
    None,
    SourceOpen,
    DestinationOpen,
    SameFile,
    Truncate,
    Transfer,
    WriteZero,
    ReadAfterWrite,
    DestinationClose,
}
struct CopyHarness<'a> {
    output: &'a Compilation,
    cwd_symbol: [u8; 32],
    failure: Failure,
    events: Vec<String>,
    next: u64,
    opened: u64,
    target: Vec<u8>,
    reads: usize,
    writes: usize,
    depth: usize,
}
fn word(memory: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(memory[at as usize..at as usize + 8].try_into().unwrap())
}
fn put(memory: &mut [u8], at: u64, value: u64) {
    memory[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}
fn result(memory: &mut [u8], out: u64, value: u64, error: Option<u8>) {
    memory[out as usize..out as usize + 40].fill(0);
    if let Some(error) = error {
        memory[out as usize] = 1;
        memory[out as usize + 8] = error;
    } else {
        put(memory, out + 8, value);
    }
}
impl CopyHarness<'_> {
    // 固定用例只分配一个 8192B 读缓冲、四字节快照和少量描述符；
    // 受管替身位于预置区，解释器的临时栈在其后，返回时不会截断这些值。
    fn allocate(&mut self, bytes: u64) -> u64 {
        let address = self.next;
        self.next += bytes.div_ceil(8) * 8;
        assert!(self.next < 16384);
        address
    }
    fn evaluate(&mut self, body: &Body, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        self.depth += 1;
        assert!(self.depth <= 32);
        let returned = interpret(body, args, memory, &mut |boundary, args, memory| {
            self.call(boundary, args, memory)
        });
        self.depth -= 1;
        returned
    }
    fn call(&mut self, boundary: Boundary<'_>, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        match boundary {
            Boundary::Storage(Op::GcAlloc { .. }) => vec![self.allocate(args[0])],
            Boundary::Storage(Op::SymbolAddr(Symbol::Global { key, .. }))
                if key == &self.cwd_symbol =>
            {
                vec![512]
            }
            Boundary::Machine(crate::runtime::RuntimeMachineOp::ThreadId) => vec![1],
            Boundary::Storage(Op::SymbolAddr(Symbol::TypeDescriptor(key))) => {
                let bytes = self
                    .output
                    .lir
                    .as_ref()
                    .unwrap()
                    .bodies()
                    .iter()
                    .flat_map(|body| &body.stack_slots)
                    .find(|slot| slot.descriptor == *key)
                    .expect("实际栈槽布局")
                    .bytes;
                let address = self.allocate(8);
                put(memory, address, bytes);
                vec![address]
            }
            Boundary::Storage(Op::GcWriteBarrier { .. } | Op::GcWriteBarrierReserved { .. }) => {
                Vec::new()
            }
            Boundary::Call(CallTarget::Runtime(
                RuntimeCall::CowSnapshot | RuntimeCall::ValueTransfer,
            )) => {
                let bytes = word(memory, args[2]) as usize;
                memory.copy_within(args[1] as usize..args[1] as usize + bytes, args[0] as usize);
                Vec::new()
            }
            Boundary::Call(CallTarget::Runtime(
                RuntimeCall::ResourceAcquire
                | RuntimeCall::ResourceRelease
                | RuntimeCall::ValueForget,
            )) => Vec::new(),
            Boundary::Call(CallTarget::Instance(instance)) => self.instance(instance, args, memory),
            _ => panic!("未登记的 copy 测试边界：{boundary:?}"),
        }
    }
    fn instance(&mut self, instance: &[u8; 32], args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        let body = self
            .output
            .lir
            .as_ref()
            .unwrap()
            .bodies()
            .iter()
            .find(|body| body.instance == *instance)
            .unwrap();
        let hir = self.output.hir.as_ref().unwrap().module();
        let definition = hir
            .definitions
            .iter()
            .find(|definition| definition.key == body.owner)
            .unwrap();
        let path = &hir.sources[definition.location.as_ref().unwrap().source as usize].path;
        match (path.as_str(), definition.name.as_str()) {
            ("std/fs.gg", "open") => self.open(args, memory),
            ("std/fs.gg", "metadata") => self.metadata(args, memory),
            ("std/fs.gg", "set_len") => self.truncate(args, memory),
            ("std/fs.gg", "read") => self.read(args, memory),
            ("std/fs.gg", "write") => self.write(args, memory),
            ("std/fs.gg", "close") => self.close(args, memory),
            ("std/io.gg", "copy") => {
                assert_eq!(word(memory, args[1]), 1);
                assert_eq!(word(memory, args[2]), 2);
                self.events.push("transfer".into());
                return self.evaluate(body, args, memory);
            }
            ("std/text.gg", "alloc_bytes") => return vec![self.allocate(args[0]), args[0]],
            _ => return self.evaluate(body, args, memory),
        }
        Vec::new()
    }
    fn open(&mut self, args: &[u64], memory: &mut [u8]) {
        self.opened += 1;
        let handle = self.opened;
        let options = args[2].to_le_bytes();
        assert_eq!(
            &options[..7],
            if handle == 1 {
                &[1, 0, 0, 0, 0, 0, 1]
            } else {
                &[0, 1, 0, 0, 1, 0, 1]
            }
        );
        self.events.push(format!("open{handle}"));
        let at = usize::try_from(word(memory, args[1])).unwrap();
        let length = usize::try_from(word(memory, args[1] + 8)).unwrap();
        let windows = self.output.lir.as_ref().unwrap().bodies()[0].target == "x86_64-windows";
        let expected: &[u8] = match (windows, handle) {
            (false, 1) => b"/one/source",
            (false, _) => b"/one/target",
            (true, 1) => b"/one\\source",
            (true, _) => b"/one\\target",
        };
        assert_eq!(&memory[at..at + length], expected);
        // 第一次打开后改变全局 cwd，第二个路径仍须使用入口捕获的同一 base。
        let bytes = if self.output.lir.as_ref().unwrap().bodies()[0].target == "x86_64-windows" {
            &b"/\0t\0w\0o\0"[..]
        } else {
            &b"/two"[..]
        };
        memory[768..768 + bytes.len()].copy_from_slice(bytes);
        put(memory, 640 + 32, u64::try_from(bytes.len()).unwrap());
        let failed = (handle == 1 && matches!(self.failure, Failure::SourceOpen))
            || (handle == 2 && matches!(self.failure, Failure::DestinationOpen));
        result(memory, args[0], handle, failed.then_some(PERMISSION_DENIED));
    }
    fn metadata(&mut self, args: &[u64], memory: &mut [u8]) {
        let handle = word(memory, args[1]);
        self.events.push(format!("metadata{handle}"));
        memory[args[0] as usize..args[0] as usize + 72].fill(0);
        // Metadata 的 device/inode 在 payload 48/56。
        put(memory, args[0] + 8 + 48, 1);
        put(
            memory,
            args[0] + 8 + 56,
            if matches!(self.failure, Failure::SameFile) {
                1
            } else {
                handle
            },
        );
    }
    fn truncate(&mut self, args: &[u64], memory: &mut [u8]) {
        assert_eq!(word(memory, args[1]), 2);
        assert_eq!(args[2], 0);
        self.events.push("truncate".into());
        let failed = matches!(self.failure, Failure::Truncate);
        if !failed {
            self.target.clear();
        }
        result(memory, args[0], 0, failed.then_some(READ_ONLY));
    }
    fn read(&mut self, args: &[u64], memory: &mut [u8]) {
        assert_eq!(word(memory, args[1]), 1);
        assert_eq!(args[3], 8192);
        self.reads += 1;
        if self.reads == 1 {
            memory[args[2] as usize..args[2] as usize + 4].copy_from_slice(b"abcd");
            result(memory, args[0], 4, None);
        } else {
            assert_eq!(self.reads, 2);
            result(
                memory,
                args[0],
                0,
                matches!(self.failure, Failure::ReadAfterWrite).then_some(PERMISSION_DENIED),
            );
        }
    }
    fn write(&mut self, args: &[u64], memory: &mut [u8]) {
        assert_eq!(word(memory, args[1]), 2);
        let start = word(memory, args[2]) as usize;
        let length = word(memory, args[2] + 8) as usize;
        self.writes += 1;
        let remaining = if self.writes == 1 {
            &b"abcd"[..]
        } else {
            &b"cd"[..]
        };
        assert!(self.writes <= 2);
        assert_eq!(&memory[start..start + length], remaining);
        let failed = self.writes == 2 && matches!(self.failure, Failure::Transfer);
        let zero = self.writes == 2 && matches!(self.failure, Failure::WriteZero);
        if !failed && !zero {
            self.target.extend_from_slice(&memory[start..start + 2]);
        }
        result(
            memory,
            args[0],
            if zero { 0 } else { 2 },
            failed.then_some(STORAGE_FULL),
        );
    }
    fn close(&mut self, args: &[u64], memory: &mut [u8]) {
        let handle = word(memory, args[1]);
        self.events.push(format!("close{handle}"));
        // 同时注入 close 错误，验证清理不覆盖已有主错误。
        let failed = matches!(
            self.failure,
            Failure::Transfer
                | Failure::WriteZero
                | Failure::ReadAfterWrite
                | Failure::Truncate
                | Failure::DestinationClose
        );
        result(memory, args[0], 0, failed.then_some(INTERRUPTED));
    }
}

#[test]
fn linux_copy_cleanup_preserves_primary_failure_and_partial_destination() {
    verify_target(TargetName::X86_64Linux);
}

#[test]
fn windows_copy_cleanup_preserves_primary_failure_and_partial_destination() {
    verify_target(TargetName::X86_64Windows);
}

fn verify_target(target: TargetName) {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/fs_api.gg"),
        target,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    verify_copy_cases(&output);
}
fn verify_copy_cases(output: &Compilation) {
    let cwd_symbol = output
        .lir
        .as_ref()
        .unwrap()
        .bodies()
        .iter()
        .find(|body| body.name == "cwd_locked")
        .expect("真实 cwd 快照实现")
        .instructions
        .iter()
        .find_map(|instruction| match instruction.op {
            Op::SymbolAddr(Symbol::Global { key, .. }) => Some(key),
            _ => None,
        })
        .expect("启动 cwd 控制记录");
    let hir = output.hir.as_ref().unwrap().module();
    let key = hir
        .definitions
        .iter()
        .find(|definition| {
            definition.name == "copy"
                && definition.location.as_ref().is_some_and(|location| {
                    hir.sources[location.source as usize].path == "std/fs.gg"
                })
        })
        .unwrap()
        .key;
    let body = output
        .lir
        .as_ref()
        .unwrap()
        .bodies()
        .iter()
        .find(|body| body.owner == key)
        .unwrap();
    // 调用序列是独立期望值；显式 close 后 defer 再调用一次幂等 close。
    for (failure, error, content, events, io) in [
        (
            Failure::SourceOpen,
            Some(PERMISSION_DENIED),
            "original",
            "open1",
            (0, 0),
        ),
        (
            Failure::DestinationOpen,
            Some(PERMISSION_DENIED),
            "original",
            "open1 open2 close1",
            (0, 0),
        ),
        (
            Failure::SameFile,
            Some(INVALID_INPUT),
            "original",
            "open1 open2 metadata1 metadata2 close2 close1",
            (0, 0),
        ),
        (
            Failure::Truncate,
            Some(READ_ONLY),
            "original",
            "open1 open2 metadata1 metadata2 truncate close2 close1",
            (0, 0),
        ),
        (
            Failure::Transfer,
            Some(STORAGE_FULL),
            "ab",
            "open1 open2 metadata1 metadata2 truncate transfer close2 close1",
            (1, 2),
        ),
        (
            Failure::WriteZero,
            Some(WRITE_ZERO),
            "ab",
            "open1 open2 metadata1 metadata2 truncate transfer close2 close1",
            (1, 2),
        ),
        (
            Failure::ReadAfterWrite,
            Some(PERMISSION_DENIED),
            "abcd",
            "open1 open2 metadata1 metadata2 truncate transfer close2 close1",
            (2, 2),
        ),
        (
            Failure::DestinationClose,
            Some(INTERRUPTED),
            "abcd",
            "open1 open2 metadata1 metadata2 truncate transfer close2 close2 close1",
            (2, 2),
        ),
        (
            Failure::None,
            None,
            "abcd",
            "open1 open2 metadata1 metadata2 truncate transfer close2 close2 close1",
            (2, 2),
        ),
    ] {
        let mut harness = CopyHarness {
            output,
            cwd_symbol,
            failure,
            events: Vec::new(),
            next: 4096,
            opened: 0,
            target: b"original".to_vec(),
            reads: 0,
            writes: 0,
            depth: 0,
        };
        let mut memory = vec![0; 16384];
        // 显式提供启动快照和两个相对 Path；继续执行真实锁与双路径定位。
        put(&mut memory, 512, 1);
        put(&mut memory, 520, 640);
        put(&mut memory, 640 + 24, 768);
        let cwd = if body.target == "x86_64-windows" {
            &b"/\0o\0n\0e\0"[..]
        } else {
            &b"/one"[..]
        };
        memory[768..768 + cwd.len()].copy_from_slice(cwd);
        put(&mut memory, 640 + 32, u64::try_from(cwd.len()).unwrap());
        put(&mut memory, 128, 1024);
        put(&mut memory, 136, 6);
        put(&mut memory, 256, 1040);
        put(&mut memory, 264, 6);
        memory[1024..1030].copy_from_slice(b"source");
        memory[1040..1046].copy_from_slice(b"target");
        harness.evaluate(body, &[0, 128, 256], &mut memory);
        assert_eq!(memory[0], u8::from(error.is_some()), "{failure:?}");
        if let Some(error) = error {
            assert_eq!(memory[8], error, "{failure:?}");
        } else {
            assert_eq!(word(&memory, 8), 4);
        }
        assert_eq!(
            harness.events,
            events.split_whitespace().collect::<Vec<_>>(),
            "{failure:?}"
        );
        assert_eq!(harness.target, content.as_bytes(), "{failure:?}");
        assert_eq!((harness.reads, harness.writes), io, "{failure:?}");
    }
}
