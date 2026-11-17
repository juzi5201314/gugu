//! 进程内执行生产 LIR；仅在分配、调度和系统边界注入确定性替身。
use super::{Boundary, Compilation, interpret_bounded};
use crate::lir::body::{Body, CallTarget, Op, RuntimeCall, Symbol};
use std::collections::HashMap;
pub(super) fn word(memory: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(memory[at as usize..at as usize + 8].try_into().unwrap())
}
pub(super) fn put(memory: &mut [u8], at: u64, value: u64) {
    memory[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn runtime_bodies() -> (crate::frontend::hir::Module, Vec<Body>) {
    runtime_bodies_for(super::TargetName::X86_64Linux)
}

pub(super) fn runtime_bodies_for(
    target: super::TargetName,
) -> (crate::frontend::hir::Module, Vec<Body>) {
    let artifact = crate::testing::provider()
        .artifact(target)
        .expect("目标构建期 artifact");
    let resources = crate::runtime::RuntimeResources::new(artifact);
    let prebuilt = resources.prebuilt().expect("构建期生产函数表");
    let hir = crate::canonical::from_slice(&prebuilt.hir).expect("生产 HIR 可解码");
    let bodies = prebuilt
        .functions
        .iter()
        .map(|function| crate::canonical::from_slice(&function.lir).expect("生产 LIR 可解码"))
        .collect();
    (hir, bodies)
}

pub(super) struct Harness<'a> {
    pub(super) bodies: HashMap<[u8; 32], &'a Body>,
    pub(super) heap: usize,
    sizes: HashMap<[u8; 32], u64>,
    pub(super) names: HashMap<[u8; 32], (&'a str, &'a str)>,
    pub(super) globals: HashMap<[u8; 32], u64>,
    pub(super) script: std::collections::VecDeque<(u64, u64, u64)>,
    pub(super) calls: Vec<(u64, Vec<u64>)>,
    pub(super) woken: Vec<u64>,
    pub(super) close_requests: Vec<Vec<u64>>,
    pub(super) park_hook: Option<fn(&mut Harness<'_>, &mut Vec<u8>)>,
    pub(super) current: u64,
    pub(super) now: u64,
    pub(super) native_hook: Option<fn(&mut Harness<'_>, u64, u64, &mut Vec<u8>)>,
    pub(super) bridge_hook: Option<fn(&mut Harness<'_>, &[u64], &mut Vec<u8>) -> Vec<u64>>,
    pub(super) allocations: HashMap<u64, u64>,
    pub(super) freed: Vec<(u64, u64)>,
    pub(super) machine_hook: Option<
        fn(&mut Harness<'_>, crate::runtime::RuntimeMachineOp, &[u64], &mut Vec<u8>) -> Vec<u64>,
    >,
    functions: Vec<([u8; 32], bool)>,
    pub(super) metadata: Option<crate::frontend::gc::GcMetadataBundle>,
    type_section: u64,
    resources: Vec<(u64, [u8; 32])>,
    scheduled: Vec<Option<([u8; 32], Vec<u64>)>>,
}
impl<'a> Harness<'a> {
    pub(super) fn new(output: &'a Compilation) -> Self {
        let mut harness = Self::from_bodies(
            output.hir.as_ref().unwrap().module(),
            output.lir.as_ref().unwrap().bodies(),
        );
        harness.set_sizes(output.gir.as_ref().unwrap());
        harness
    }
    pub(super) fn set_sizes(&mut self, gir: &crate::frontend::gir::GirWorldV1) {
        self.sizes = gir
            .concrete
            .iter()
            .flat_map(|body| &body.types)
            .filter_map(|ty| ty.layout.map(|layout| (ty.key, layout.size)))
            .collect();
    }
    fn types(&mut self, memory: &mut [u8]) -> u64 {
        if self.type_section == 0 {
            let bytes = self
                .metadata
                .as_ref()
                .expect("真实类型表")
                .type_section
                .len();
            self.type_section = self.allocate(memory, bytes);
            let at = usize::try_from(self.type_section).unwrap();
            memory[at..at + bytes].copy_from_slice(&self.metadata.as_ref().unwrap().type_section);
        }
        self.type_section
    }
    /// 调用方已撤去全部根；回收事件只选择对象，释放动作仍解释生产 value program。
    pub(super) fn collect_unreachable_resources(&mut self, memory: &mut Vec<u8>) {
        for (address, key) in std::mem::take(&mut self.resources) {
            let base = self.types(memory);
            let index = self
                .metadata
                .as_ref()
                .unwrap()
                .world
                .types
                .iter()
                .position(|entry| entry.type_key == key)
                .unwrap();
            let descriptor = base
                + crate::runtime::gc_metadata_section::descriptor_offset(
                    u32::try_from(index).unwrap(),
                );
            self.invoke(
                "std/runtime/value.gg",
                "resource_finalize",
                &[address, descriptor],
                memory,
            );
        }
    }
    pub(super) fn from_bodies(hir: &'a crate::frontend::hir::Module, bodies: &'a [Body]) -> Self {
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
            names: bodies
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
            park_hook: None,
            current: 128,
            now: 1000,
            native_hook: None,
            bridge_hook: None,
            scheduled: Vec::new(),
            allocations: HashMap::new(),
            freed: Vec::new(),
            machine_hook: None,
            metadata: None,
            type_section: 0,
            resources: Vec::new(),
            functions: Vec::new(),
            bodies: bodies.iter().map(|b| (b.instance, b)).collect(),
            heap: 4096,
            sizes: HashMap::new(),
        }
    }
    pub(super) fn invoke(
        &mut self,
        module: &str,
        name: &str,
        args: &[u64],
        memory: &mut Vec<u8>,
    ) -> Vec<u64> {
        let body = self
            .names
            .iter()
            .find_map(|(key, &(path, candidate))| {
                let body = self.bodies[key];
                let extra = usize::from(body.signature.sret.is_some())
                    + usize::from(body.name.ends_with("$function"));
                (path == module
                    && candidate == name
                    && body.params(body.entry).len() == args.len() + extra + 1)
                    .then_some(body)
            })
            .unwrap_or_else(|| panic!("缺少 {module}::{name} 的 {} 参数入口", args.len()));
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
    pub(super) fn allocate(&mut self, memory: &mut [u8], bytes: usize) -> u64 {
        let at = self.heap;
        self.heap = (at + bytes.max(1)).next_multiple_of(16);
        assert!(self.heap < memory.len(), "固定测试堆容量");
        memory[at..self.heap].fill(0);
        at as u64
    }
    pub(super) fn run_scheduled(&mut self, index: usize, memory: &mut Vec<u8>) {
        let (key, args) = self.scheduled[index].take().expect("任务只执行一次");
        assert!(
            self.run(self.bodies[&key], &args, memory).is_empty(),
            "此调度脚本只运行单位返回任务"
        );
    }
    pub(super) fn run(&mut self, body: &Body, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
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
                        Some(("std/runtime/roots.gg", "gc_types_section"))
                            if self.metadata.is_some() =>
                        {
                            return vec![self.types(memory)];
                        }
                        Some((
                            "std/runtime/fs_platform.gg",
                            "raw_allocate" | "control_allocate",
                        ))
                        | Some(("std/runtime/native_memory.gg", "allocate")) => {
                            let at = self.allocate(memory, usize::try_from(args[0]).unwrap());
                            assert!(self.allocations.insert(at, args[0]).is_none());
                            return vec![at];
                        }
                        Some(("std/runtime/fs_platform.gg", "raw_free" | "control_free"))
                        | Some(("std/runtime/native_memory.gg", "release")) => {
                            assert_eq!(self.allocations.remove(&args[0]), Some(args[1]));
                            self.freed.push((args[0], args[1]));
                            return vec![];
                        }
                        Some(("std/runtime/fs_platform.gg", "ensure_pool")) => return vec![],
                        Some((
                            "std/runtime/network_service.gg",
                            "install" | "install_resolver" | "install_process",
                        )) => return vec![],
                        Some((
                            "std/runtime/net_dispatch.gg",
                            "install_transport" | "install_timer",
                        )) => return vec![],
                        Some(("std/runtime/fs_platform.gg", "await_close")) => {
                            if word(memory, args[0] + 8) & 8 == 0 {
                                self.park_hook.expect("关闭调度事件")(self, memory);
                            }
                            assert_ne!(word(memory, args[0] + 8) & 8, 0);
                            return vec![word(memory, args[0] + 88)];
                        }
                        Some(("std/runtime/scheduler.gg", "park_current")) => {
                            self.park_hook.expect("固定调度事件")(self, memory);
                            return vec![];
                        }
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
                Boundary::Machine(crate::runtime::RuntimeMachineOp::CurrentSlot) => {
                    vec![self.current]
                }
                Boundary::Machine(crate::runtime::RuntimeMachineOp::MonotonicNanos) => {
                    vec![self.now]
                }
                Boundary::Machine(crate::runtime::RuntimeMachineOp::ResolverSymbol) => {
                    vec![args[0] + 100]
                }
                Boundary::Machine(
                    crate::runtime::RuntimeMachineOp::NetCall
                    | crate::runtime::RuntimeMachineOp::ProcessCall,
                ) => {
                    let (op, result, error) = self.script.pop_front().expect("固定平台脚本外调用");
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
                    let heap = &mut self.heap;
                    let at = *self.globals.entry(*key).or_insert_with(|| {
                        let at = *heap;
                        *heap = (at + *bytes as usize + 8).next_multiple_of(16);
                        assert!(*heap < memory.len());
                        memory[at..*heap].fill(0);
                        at as u64
                    });
                    vec![at]
                }
                Boundary::Storage(Op::SymbolAddr(
                    symbol @ (Symbol::Instance(_) | Symbol::FunctionAdapter { .. }),
                )) => {
                    let entry = match symbol {
                        Symbol::Instance(key) => (*key, true),
                        Symbol::FunctionAdapter {
                            target,
                            has_environment,
                        } => (*target, *has_environment),
                        _ => unreachable!(),
                    };
                    let index = self
                        .functions
                        .iter()
                        .position(|value| *value == entry)
                        .unwrap_or_else(|| {
                            self.functions.push(entry);
                            self.functions.len() - 1
                        });
                    vec![u64::try_from(index + 1).unwrap()]
                }
                Boundary::Call(CallTarget::Indirect) => {
                    let (key, environment) = self.functions[usize::try_from(args[0] - 1).unwrap()];
                    self.run(
                        self.bodies[&key],
                        &args[if environment { 1 } else { 2 }..],
                        memory,
                    )
                }
                Boundary::Call(CallTarget::Runtime(RuntimeCall::Spawn)) => {
                    let (key, _) = self.functions[usize::try_from(args[0] - 1).unwrap()];
                    let body = self.bodies[&key];
                    assert!(body.signature.sret.is_none());
                    let count = body.params(body.entry).len() - 1;
                    let lanes = (0..count)
                        .map(|index| word(memory, args[1] + u64::try_from(index).unwrap() * 8))
                        .collect();
                    self.scheduled.push(Some((key, lanes)));
                    vec![u64::try_from(self.scheduled.len()).unwrap()]
                }
                Boundary::Call(CallTarget::Runtime(RuntimeCall::JoinWait)) => {
                    assert_eq!(args[1], 0, "单位返回没有载荷槽");
                    let index = usize::try_from(args[0] - 1).unwrap();
                    if self.scheduled[index].is_some() {
                        self.run_scheduled(index, memory);
                    }
                    vec![1]
                }
                Boundary::Call(CallTarget::Runtime(RuntimeCall::StringCompare)) => {
                    self.invoke("std/text.gg", "string_compare", args, memory)
                }
                Boundary::Call(CallTarget::Runtime(call))
                    if self.metadata.is_some()
                        && matches!(
                            call,
                            RuntimeCall::ResourceAcquire
                                | RuntimeCall::ResourceRelease
                                | RuntimeCall::ResourceTransfer
                                | RuntimeCall::ResourceFinalize
                                | RuntimeCall::ValueCopy
                                | RuntimeCall::ValuePublish
                                | RuntimeCall::ValueDrop
                                | RuntimeCall::ValueForget
                                | RuntimeCall::ValueTransfer
                                | RuntimeCall::CowSnapshot
                        ) =>
                {
                    let name = match call {
                        RuntimeCall::ResourceAcquire => "resource_acquire",
                        RuntimeCall::ResourceRelease => "resource_release",
                        RuntimeCall::ResourceTransfer => "resource_transfer",
                        RuntimeCall::ResourceFinalize => "resource_finalize",
                        RuntimeCall::ValueCopy => "value_copy",
                        RuntimeCall::ValuePublish => "value_publish",
                        RuntimeCall::ValueDrop => "value_drop",
                        RuntimeCall::ValueForget => "value_forget",
                        RuntimeCall::ValueTransfer => "value_transfer",
                        RuntimeCall::CowSnapshot => "cow_snapshot",
                        _ => unreachable!(),
                    };
                    self.invoke("std/runtime/value.gg", name, args, memory)
                }
                Boundary::Call(CallTarget::Runtime(
                    RuntimeCall::ValueTransfer | RuntimeCall::CowSnapshot,
                )) => {
                    memory.copy_within(
                        args[1] as usize..(args[1] + args[2]) as usize,
                        args[0] as usize,
                    );
                    vec![]
                }
                Boundary::Call(CallTarget::Runtime(RuntimeCall::ValueRepeat))
                    if self.metadata.is_some() =>
                {
                    self.invoke("std/runtime/heap.gg", "value_repeat", args, memory)
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
                    if self.metadata.is_some() {
                        let base = self.types(memory);
                        let index = self
                            .metadata
                            .as_ref()
                            .unwrap()
                            .world
                            .types
                            .iter()
                            .position(|entry| entry.type_key == *key)
                            .expect("类型已冻结");
                        vec![
                            base + crate::runtime::gc_metadata_section::descriptor_offset(
                                u32::try_from(index).unwrap(),
                            ),
                        ]
                    } else {
                        vec![self.sizes[key]]
                    }
                }
                Boundary::Storage(Op::GcAlloc {
                    descriptor,
                    placement,
                    ..
                }) => {
                    let at = self.allocate(memory, usize::try_from(args[0]).unwrap());
                    if *placement == crate::frontend::gir::placement::PlacementKind::Resource {
                        self.resources.push((at, *descriptor));
                    }
                    vec![at]
                }
                Boundary::Storage(Op::GcWriteBarrier { .. }) => vec![],
                Boundary::Storage(Op::SymbolAddr(Symbol::Data(index))) => {
                    let data = &body.data[*index as usize].bytes;
                    let at = self.allocate(memory, data.len()) as usize;
                    memory[at..at + data.len()].copy_from_slice(data);
                    vec![at as u64]
                }
                Boundary::Machine(op) => {
                    self.machine_hook.expect("固定机器边界脚本")(self, op, args, memory)
                }
                other => panic!("地址函数不应跨越未登记边界 {other:?}"),
            },
            8192,
        )
    }
    pub(super) fn text(
        &mut self,
        body: &Body,
        text: &str,
        extra: &[u64],
        memory: &mut Vec<u8>,
    ) -> Vec<u64> {
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
