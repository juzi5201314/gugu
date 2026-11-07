//! 执行实际地址实现的 LIR；分配和字符串原语使用进程内内存，不访问网络。
use super::{
    Boundary, Compilation, CompileRequest, Compiler, TargetName, interpret_bounded, named,
};
use crate::lir::body::{Body, CallTarget, Op, RuntimeCall, Symbol};
use std::collections::HashMap;

mod protocol;

fn word(memory: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(memory[at as usize..at as usize + 8].try_into().unwrap())
}
fn put(memory: &mut [u8], at: u64, value: u64) {
    memory[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}

struct Harness<'a> {
    bodies: HashMap<[u8; 32], &'a Body>,
    heap: usize,
    sizes: HashMap<[u8; 32], u64>,
    names: HashMap<[u8; 32], (&'a str, &'a str)>,
    globals: HashMap<[u8; 32], u64>,
    script: std::collections::VecDeque<(u64, u64, u64)>,
    calls: Vec<(u64, Vec<u64>)>,
    woken: Vec<u64>,
    close_requests: Vec<Vec<u64>>,
    native_hook: Option<fn(&mut Harness<'_>, u64, u64, &mut Vec<u8>)>,
    bridge_hook: Option<fn(&mut Harness<'_>, &[u64], &mut Vec<u8>) -> Vec<u64>>,
}
impl<'a> Harness<'a> {
    fn new(output: &'a Compilation) -> Self {
        let hir = output.hir.as_ref().unwrap().module();
        let definitions: HashMap<_, _> = hir
            .definitions
            .iter()
            .filter_map(|definition| {
                let location = definition.location.as_ref()?;
                Some((
                    definition.key,
                    (
                        hir.sources[location.source as usize].path.as_str(),
                        definition.name.as_str(),
                    ),
                ))
            })
            .collect();
        Self {
            names: output
                .lir
                .as_ref()
                .unwrap()
                .bodies()
                .iter()
                .filter_map(|body| {
                    definitions
                        .get(&body.owner)
                        .map(|name| (body.instance, *name))
                })
                .collect(),
            globals: HashMap::new(),
            script: Default::default(),
            calls: vec![],
            woken: vec![],
            close_requests: vec![],
            native_hook: None,
            bridge_hook: None,
            bodies: output
                .lir
                .as_ref()
                .unwrap()
                .bodies()
                .iter()
                .map(|b| (b.instance, b))
                .collect(),
            heap: 4096,
            sizes: output
                .gir
                .as_ref()
                .unwrap()
                .concrete
                .iter()
                .flat_map(|body| &body.types)
                .filter_map(|ty| ty.layout.map(|layout| (ty.key, layout.size)))
                .collect(),
        }
    }
    fn body(&self, module: &str, name: &str) -> &'a Body {
        let key = self
            .names
            .iter()
            .filter(|(_, value)| **value == (module, name))
            .min_by_key(|(key, _)| self.bodies[*key].name.ends_with("$function"))
            .expect("真实网络函数")
            .0;
        self.bodies[key]
    }
    fn invoke(&mut self, module: &str, name: &str, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        let body = self.body(module, name);
        let args: Vec<_> = body
            .name
            .ends_with("$function")
            .then_some(0)
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        if let Some((bytes, _, _)) = body.signature.sret {
            let at = self.allocate(memory, bytes as usize);
            let arguments: Vec<_> = std::iter::once(at).chain(args.iter().copied()).collect();
            assert!(self.run(body, &arguments, memory).is_empty());
            (0..bytes)
                .step_by(8)
                .map(|offset| word(memory, at + u64::from(offset)))
                .collect()
        } else {
            self.run(body, &args, memory)
        }
    }
    fn allocate(&mut self, memory: &mut [u8], bytes: usize) -> u64 {
        let at = self.heap;
        self.heap = (at + bytes.max(1)).next_multiple_of(16);
        assert!(self.heap < memory.len(), "固定测试堆容量");
        memory[at..self.heap].fill(0);
        at as u64
    }
    fn run(&mut self, body: &Body, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        assert_eq!(
            body.params(body.entry).len() - 1,
            args.len(),
            "函数 {} 参数 {:?}，签名 {:?}",
            body.name,
            args,
            body.signature
        );
        interpret_bounded(
            body,
            args,
            memory,
            &mut |target, args, memory| match target {
                Boundary::Call(CallTarget::Instance(instance)) => {
                    match self.names.get(instance).copied() {
                        Some((
                            "std/runtime/fs_platform.gg",
                            "raw_allocate" | "control_allocate",
                        )) => return vec![self.allocate(memory, args[0] as usize)],
                        Some((
                            "std/runtime/fs_platform.gg",
                            "raw_free" | "control_free" | "ensure_pool",
                        )) => return vec![],
                        Some((
                            "std/runtime/network_service.gg",
                            "install" | "install_resolver",
                        )) => return vec![],
                        Some(("std/runtime/scheduler.gg", "ready_hot")) => {
                            self.woken.push(args[0]);
                            return vec![];
                        }
                        Some(("std/runtime/fs_platform.gg", "enqueue_close")) => {
                            self.close_requests.push(args.to_vec());
                            return vec![];
                        }
                        Some(("std/runtime/fs_platform.gg", "native_call")) => {
                            return self.bridge_hook.expect("原生 worker 脚本")(
                                self, args, memory,
                            );
                        }
                        _ => {}
                    }
                    let next = self.bodies[instance];
                    self.run(next, args, memory)
                }
                Boundary::Machine(crate::runtime::RuntimeMachineOp::ResolverSymbol) => {
                    vec![args[0] + 100]
                }
                Boundary::Machine(crate::runtime::RuntimeMachineOp::NetCall) => {
                    let (op, result, error) = self.script.pop_front().expect("网络平台脚本外调用");
                    assert_eq!(args[0], op);
                    let request = args[1];
                    self.calls.push((
                        op,
                        (0..9)
                            .map(|index| word(memory, request + index * 8))
                            .collect(),
                    ));
                    put(memory, request + 72, result);
                    put(memory, request + 80, error);
                    if let Some(hook) = self.native_hook {
                        hook(self, op, request, memory);
                    }
                    vec![result]
                }
                Boundary::Storage(Op::SymbolAddr(Symbol::Global { key, bytes, .. })) => {
                    let at = match self.globals.get(key) {
                        Some(&at) => at,
                        None => {
                            let at = self.allocate(memory, *bytes as usize + 8);
                            self.globals.insert(*key, at);
                            at
                        }
                    };
                    vec![at]
                }
                Boundary::Storage(Op::SymbolAddr(
                    Symbol::Instance(_) | Symbol::FunctionAdapter { .. },
                )) => vec![1],
                Boundary::Call(CallTarget::Runtime(
                    RuntimeCall::ValueTransfer | RuntimeCall::CowSnapshot,
                )) => {
                    memory.copy_within(
                        args[1] as usize..(args[1] + args[2]) as usize,
                        args[0] as usize,
                    );
                    vec![]
                }
                Boundary::Call(CallTarget::Runtime(RuntimeCall::ValueRepeat)) => {
                    let stride = args[3] / args[2];
                    for index in 0..args[2] {
                        memory.copy_within(
                            args[1] as usize..(args[1] + stride) as usize,
                            (args[0] + index * stride) as usize,
                        );
                    }
                    vec![]
                }
                Boundary::Call(CallTarget::Runtime(
                    RuntimeCall::ValueCopy
                    | RuntimeCall::ValuePublish
                    | RuntimeCall::ValueDrop
                    | RuntimeCall::ValueForget,
                )) => vec![],
                Boundary::Storage(Op::SymbolAddr(Symbol::TypeDescriptor(key))) => {
                    vec![self.sizes[key]]
                }
                Boundary::Storage(Op::GcAlloc { .. }) => {
                    vec![self.allocate(memory, args[0] as usize)]
                }
                Boundary::Storage(Op::GcWriteBarrier { .. }) => vec![],
                Boundary::Storage(Op::SymbolAddr(Symbol::Data(index))) => {
                    let data = &body.data[*index as usize].bytes;
                    let at = self.allocate(memory, data.len()) as usize;
                    memory[at..at + data.len()].copy_from_slice(data);
                    vec![at as u64]
                }
                other => panic!("地址函数不应跨越未登记边界 {other:?}"),
            },
            8192,
        )
    }
    fn text(&mut self, body: &Body, text: &str, extra: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
        self.heap = 4096;
        memory[128..128 + text.len()].copy_from_slice(text.as_bytes());
        memory[64..72].copy_from_slice(&128u64.to_le_bytes());
        memory[72..80].copy_from_slice(&(text.len() as u64).to_le_bytes());
        let mut args = if body.signature.sret.is_some() {
            vec![32, 64]
        } else {
            vec![64]
        };
        args.extend_from_slice(extra);
        let result = self.run(body, &args, memory);
        if let Some((bytes, _, _)) = body.signature.sret {
            assert!(result.is_empty());
            (0..bytes)
                .step_by(8)
                .map(|offset| {
                    u64::from_le_bytes(
                        memory[32 + offset as usize..40 + offset as usize]
                            .try_into()
                            .unwrap(),
                    )
                })
                .collect()
        } else {
            result
        }
    }
}

fn compilation(target: TargetName) -> Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/net_address.gg"),
        target,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

#[test]
fn numeric_addresses_validate_components_ports_and_unix_paths() {
    let output = compilation(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 65536];
    assert_eq!(
        h.run(named(&output, "vector_snapshot"), &[], &mut memory),
        [1]
    );
    let body = named(&output, "ipv4");
    for (text, value) in [
        ("0.0.0.0", 0),
        ("127.0.0.1", 0x7f000001),
        ("255.255.255.255", 0xffffffff),
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [value], "{text}");
    }
    for text in [
        "",
        "127.1",
        "1.2.3.4.5",
        "01.2.3.4",
        "256.1.2.3",
        "1..2.3",
        "1.2.3.",
        "1.2.3.4\0",
        " 1.2.3.4",
        "１.2.3.4",
        "+1.2.3.4",
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [u64::MAX], "{text:?}");
    }
    let body = named(&output, "ipv6");
    for (text, expected) in [
        ("::", [0; 8]),
        ("::1", [0, 0, 0, 0, 0, 0, 0, 1]),
        ("2001:DB8:0:1::2", [0x2001, 0xdb8, 0, 1, 0, 0, 0, 2]),
        ("::ffff:192.0.2.1", [0, 0, 0, 0, 0, 0xffff, 0xc000, 0x201]),
        ("1:2:3:4:5:6:7:8", [1, 2, 3, 4, 5, 6, 7, 8]),
    ] {
        for (index, expected) in expected.into_iter().enumerate() {
            assert_eq!(
                h.text(body, text, &[index as u64], &mut memory),
                [expected],
                "{text} segment {index}"
            );
        }
    }
    for text in [
        "",
        ":",
        ":::1",
        "1::2::3",
        "1:2:3:4:5:6:7",
        "1:2:3:4:5:6:7:8:9",
        "1:2:3:4:5:6:7:8::",
        "12345::",
        "gg::",
        "::1:",
        "::192.01.2.1",
        "::1%eth0",
    ] {
        assert_eq!(h.text(body, text, &[0], &mut memory), [u64::MAX], "{text}");
    }
    let body = named(&output, "socket");
    for (text, expected) in [
        ("127.0.0.1:0", 0),
        ("[::1]:65535", 65535),
        ("[fe80::1%12]:80", (12 << 16) | 80),
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [expected], "{text}");
    }
    for text in [
        "127.0.0.1:",
        "127.0.0.1:-1",
        "[::1]:65536",
        "[::1]:+80",
        "[::1]:1a",
    ] {
        assert_eq!(
            h.text(body, text, &[], &mut memory),
            [u64::MAX - 1],
            "{text}"
        );
    }
    for text in [
        "::1:80",
        "[]:80",
        "[::1]80",
        "[::1%eth0]:80",
        "[::1%4294967296]:80",
        "localhost:80",
        ":80",
    ] {
        assert_eq!(h.text(body, text, &[], &mut memory), [u64::MAX], "{text}");
    }
    let body = named(&output, "host_valid");
    for (text, expected) in [
        ("localhost".into(), 1),
        ("example.COM.".into(), 1),
        ("a-b.example".into(), 1),
        ("::1".into(), 1),
        ("fe80::1%12".into(), 1),
        ("".into(), 0),
        ("a\0b".into(), 0),
        ("中文.example".into(), 0),
        ("a..b".into(), 0),
        ("-host".into(), 0),
        ("host-".into(), 0),
        ("a_b".into(), 0),
        ("::1%eth0".into(), 0),
        ("x".repeat(64), 0),
        ("x".repeat(255), 0),
    ] {
        assert_eq!(
            h.text(body, &text, &[], &mut memory),
            [expected],
            "{text:?}"
        );
    }
    let body = named(&output, "unix_path");
    for (text, expected) in [
        ("/tmp/test.sock".into(), 1),
        ("x".repeat(107), 1),
        ("x".repeat(108), 0),
        ("".into(), 0),
        ("a\0b".into(), 0),
    ] {
        assert_eq!(h.text(body, &text, &[], &mut memory), [expected]);
    }
}

#[test]
fn numeric_addresses_format_canonically_on_windows() {
    let output = compilation(TargetName::X86_64Windows);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 65536];
    for (function, text, expected) in [
        (
            "format_socket",
            "[fe80::1%4294967295]:65535",
            "[fe80::1%4294967295]:65535",
        ),
        ("format_socket", "127.0.0.1:0", "127.0.0.1:0"),
        ("format4", "192.0.2.1", "192.0.2.1"),
        ("format6", "0:0:0:0:0:0:0:0", "::"),
        ("format6", "0:0:0:0:0:0:0:1", "::1"),
        ("format6", "1:0:0:2:0:0:3:4", "1::2:0:0:3:4"),
        ("format6", "2001:DB8:0:1:0:0:0:0", "2001:db8:0:1::"),
        ("format6", "::ffff:192.0.2.1", "::ffff:c000:201"),
    ] {
        let result = h.text(named(&output, function), text, &[], &mut memory);
        assert_eq!(result.len(), 2);
        let value = &memory[result[0] as usize..(result[0] + result[1]) as usize];
        assert_eq!(value, expected.as_bytes(), "{text}");
    }
}
