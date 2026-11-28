//! 发布 workload 性能与延迟门禁：确定性 in-process 测量的发布 gate。
//!
//! 三个维度：双目标端到端编译（耗时/镜像字节）、raw owner return 消息
//! 在 direct 与 temporal radix 两种 routing profile 下的吞吐与尾延迟、
//! 以及分配-归还循环的 pacing 开销。runtime 侧使用 `gugu-runtime-world`
//! 确定性参照实现——同一实现即规范模型，不针对单一 benchmark 走特殊路径。
//!
//! 不进默认测试套件；`cargo bench --profile test --bench workload_gate` 手工验收。

use std::time::Instant;

use gugu_compiler::{
    CompileRequest, Compiler, EMBEDDED_ARTIFACTS, NativeImport, NativeLink, TargetName,
};
use gugu_runtime_abi::inbox::{ServiceBudget, ShardIndex};
use gugu_runtime_abi::message::{BatchLimits, ReturnKind};
use gugu_runtime_abi::routing_schema::{
    RouteMode, RoutingDemand, RoutingPolicyV1, RoutingRuntimeContract,
};
use gugu_runtime_abi::size_class::RuntimeSizeClassId;
use gugu_runtime_world::world::RawWorld;

/// 触达 channel/select、I/O、FFI 与 collection 的代表性用户源。
const WORKLOAD: &str = "\
use std.fs
use std.path.{Path}
#[repr(C)] struct Pair { a: uint, b: uint }
unsafe extern \"C\" fn native_probe(value: Pair) Pair
fn worker(values: chan[uint]) { unsafe {
    let received = match values.recv() { Ok(value) => value, Err(_) => panic(\"recv\") }
    _ = native_probe(Pair { a: received, b: 2 }).a
} }
fn main() {
    let ch = chan[uint](4)
    let handle = async worker(ch)
    ch.send(7)
    _ = handle.wait()
    _ = fs.exists(Path::from_string(\"gate\"))
}
";

fn compile_metrics(target: TargetName) {
    let started = Instant::now();
    let library = if target == TargetName::X86_64Linux {
        "libprobe.so"
    } else {
        "probe.dll"
    };
    let mut imports = vec![NativeImport::function(library, "native_probe")];
    if target == TargetName::X86_64Linux {
        imports.extend(
            NativeLink::THREAD_SYMBOLS
                .into_iter()
                .map(|name| NativeImport::function("libc.so.6", name)),
        );
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "workload bench 测量完整编译与镜像统计"
    )]
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(
        CompileRequest::single_file("workload.gg", WORKLOAD, target).with_native_link(NativeLink {
            imports,
            interpreter: (target == TargetName::X86_64Linux).then(|| "/toolchain/ld.so".into()),
        }),
    );
    let elapsed = started.elapsed();
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let plan = output.image_plan().unwrap();
    let bytes = if target == TargetName::X86_64Linux {
        plan.linux_image().len()
    } else {
        plan.windows_image().len()
    };
    println!(
        "compile {}: {}us image={}B action={}",
        target.name(),
        elapsed.as_micros(),
        bytes,
        &output.action_key().expect("action key").hex()[..16]
    );
}

fn stride(world: &RawWorld, class: RuntimeSizeClassId) -> u32 {
    world
        .classes()
        .get(class)
        .expect("class 已登记")
        .slot_stride
}

/// owner return 工作负载：`owners-1` 个 producer 各自分配并向自身归还 raw slot，
/// 报告发布与消费的 item/µs、转发跳数与单次发布尾 µs。
fn routing_gate(mode: RouteMode, owners: u32, items: u32) {
    let mut world = RawWorld::new(11, owners, items * owners + 64, BatchLimits::default())
        .expect("world 可创建");
    let contract = RoutingRuntimeContract::build(
        RoutingDemand {
            owners,
            return_publish_sites: items * (owners - 1),
        },
        RoutingPolicyV1 { mode },
    )
    .expect("routing 契约可构建");
    world
        .configure_routing(&contract)
        .expect("routing 平面可配置");

    let class = RuntimeSizeClassId::from_raw(0);
    let shard = ShardIndex::from_raw(0).expect("shard 合法");
    let mut publish_max = 0_u64;
    let started = Instant::now();
    for producer in 1..owners {
        // pending_return_bytes 记在分配方账本：归还消息送回 owner 自身。
        let target = world.token(producer);
        for _ in 0..items {
            let allocation = world.allocate(producer, class).expect("分配成功");
            let bytes = stride(&world, class);
            world
                .queue_return(producer, allocation.slot, u64::from(bytes))
                .expect("归还成功");
            let message = world
                .message(target, ReturnKind::RawSlot, allocation.slot, bytes)
                .expect("消息可构造");
            let mark = Instant::now();
            world
                .publish_message(producer, &message, shard, None)
                .expect("发布成功");
            publish_max = publish_max.max(mark.elapsed().as_micros() as u64);
        }
    }
    // 发布耗时必须覆盖交付到 inbox 的全程：radix 的逐跳路由与 direct 的
    // inbox 直达使用同一口径，drain_routing 计入发布窗口。
    if mode == RouteMode::Radix {
        world.drain_routing().expect("radix 排空");
    }
    let publish_us = started.elapsed().as_micros() as u64;
    let hops = world.routing_stats().remote_return_hops;
    let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
    let mark = Instant::now();
    let mut consumed = 0_u32;
    for producer in 1..owners {
        let (_forwarded, items) = world.drain_all(producer, &budget).expect("service 可运行");
        consumed += items;
    }
    let drain_us = mark.elapsed().as_micros() as u64;
    let total = items * (owners - 1);
    assert_eq!(consumed, total, "exactly-once 消费");
    // 每个 producer 的待归还账本必须清零。
    for producer in 1..owners {
        assert_eq!(
            world.accounting(producer).pending_return_bytes(),
            0,
            "producer {producer} 的待归还账本必须清零"
        );
    }
    println!(
        "return-{mode:?}: published={total} in={publish_us}us ({:.3} item/us, max={publish_max}us) drained={consumed} hops={hops} in={drain_us}us",
        total as f64 / publish_us.max(1) as f64,
    );
}

/// 分配-归还循环的 pacing 开销：每轮 allocate + local_return 构成完整的
/// 分配、归还与复用周期。
fn alloc_pressure_gate(class: RuntimeSizeClassId, items: u32) {
    let mut world = RawWorld::new(7, 2, items + 16, BatchLimits::default()).expect("world 可创建");
    let stride = stride(&world, class);
    let started = Instant::now();
    for _ in 0..items {
        let allocation = world.allocate(1, class).expect("分配成功");
        world
            .local_return(1, allocation.slot, u64::from(stride))
            .expect("本地归还成功");
    }
    assert_eq!(world.accounting(1).pending_return_bytes(), 0);
    let elapsed = started.elapsed().as_micros() as u64;
    println!(
        "alloc-return: items={items} elapsed={elapsed}us ({:.3} op/us)",
        (items * 2) as f64 / elapsed.max(1) as f64,
    );
}

fn main() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        compile_metrics(target);
    }
    routing_gate(RouteMode::Direct, 4, 4096);
    routing_gate(RouteMode::Radix, 4, 4096);
    alloc_pressure_gate(RuntimeSizeClassId::from_raw(0), 8192);
}
