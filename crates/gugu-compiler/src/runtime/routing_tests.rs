//! routing 契约段与 temporal radix fan-out 平面的确定性测试。
//!
//! 覆盖默认 direct、模式参与契约身份、契约漂移拒绝、direct 不分配 bucket、bucket
//! 路由确定性、hop 上限与错误 forward 的稳定 invariant、maintenance 相位序列、
//! 旧 topology 转发收敛。全部进程内运行。

use super::message::{ReturnNodeId, StagedChain};
use super::routing::{MaintenancePhase, RouteAction, RouteResolution, RoutingPlane};
use super::routing_schema::{
    MAX_RADIX_LEVELS, RADIX_BUCKET_LOG2, RADIX_BUCKETS, RADIX_HOP_LIMIT, ROUTE_MODES, RouteMode,
    RoutingDemand, RoutingPolicyV1, RoutingRuntimeContract,
};
use super::slab::{
    Epoch, MemoryDomainId, OwnerGeneration, OwnerId, OwnerToken, RawInvariant, RouteKey,
};

/// 构造一个 owner token；route key 由调用方指定以便 bucket 放置的确定性断言。
fn token(route_key: u64) -> OwnerToken {
    OwnerToken {
        domain: MemoryDomainId::RUNTIME_RAW,
        owner_id: OwnerId::from_raw(1),
        generation: OwnerGeneration::from_raw(1),
        route_key: RouteKey::from_raw(route_key),
    }
}

/// 构造一个挂在给定 token 上的单节点 chain；node id 只有平面账本使用。
fn chain(route_key: u64, bytes: u64) -> StagedChain {
    StagedChain {
        first: ReturnNodeId::from_raw(1),
        last: ReturnNodeId::from_raw(1),
        count: 1,
        bytes,
        target: Some(token(route_key)),
        shard: None,
    }
}

/// 构建一个 radix 平面；参数与契约登记值一致。
fn radix_plane() -> RoutingPlane {
    RoutingPlane::new(
        RouteMode::Radix,
        MAX_RADIX_LEVELS,
        RADIX_BUCKETS,
        RADIX_BUCKET_LOG2,
        RADIX_HOP_LIMIT,
    )
}

/// 构建一个 direct 平面。
fn direct_plane() -> RoutingPlane {
    RoutingPlane::new(
        RouteMode::Direct,
        MAX_RADIX_LEVELS,
        RADIX_BUCKETS,
        RADIX_BUCKET_LOG2,
        RADIX_HOP_LIMIT,
    )
}

/// 默认解析：所有 owner 都匹配。
fn deliver_all() -> impl FnMut(OwnerToken) -> Result<RouteResolution, RawInvariant> {
    |_| Ok(RouteResolution::Deliver)
}

/// 真实编译默认走 direct：不分配 bucket、指纹可复现、dump 固定口径。
#[test]
fn real_compilation_defaults_to_direct_routing() {
    let source = "fn main() { let value = 1 _ = value }";
    let compile = || {
        crate::Compiler::new().compile(crate::CompileRequest::single_file(
            "main.gg",
            source,
            crate::TargetName::X86_64Linux,
        ))
    };
    let compilation = compile();
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let raw = compilation.raw_contract().expect("真实契约必须存在");
    let routing = raw.routing();
    assert_eq!(routing.mode(), RouteMode::Direct);
    assert_eq!(routing.profile(), "mosaic-routing");
    assert_eq!(routing.profile_revision(), 1);
    assert_eq!(routing.bucket_count(), RADIX_BUCKETS);
    assert_eq!(routing.max_levels(), MAX_RADIX_LEVELS);
    assert_eq!(routing.hop_limit(), RADIX_HOP_LIMIT);
    assert_eq!(routing.demand().owners, 0);
    let again = compile();
    assert_eq!(
        again
            .raw_contract()
            .expect("真实契约必须存在")
            .routing()
            .fingerprint(),
        routing.fingerprint(),
        "同一输入的内容身份必须稳定"
    );
    assert!(
        routing.dump().starts_with(
            "routing schema=1 profile=mosaic-routing revision=1 mode=direct buckets=64"
        ),
        "dump 头必须固定 profile 与模式：{}",
        routing.dump()
    );
    assert!(
        raw.dump()
            .contains("routing-families return\nrouting-phases idle,freeze-target-cache"),
        "整体 dump 必须包含路由目录：{}",
        raw.dump()
    );
}

/// 路由模式参与契约身份：显式 radix 改变指纹与 action key。
#[test]
fn routing_mode_participates_in_contract_identity() {
    let source = "fn main() { let value = 1 _ = value }";
    let compile = |policy: RoutingPolicyV1| {
        crate::Compiler::new().compile(
            crate::CompileRequest::single_file("main.gg", source, crate::TargetName::X86_64Linux)
                .with_routing_policy(policy),
        )
    };
    let direct = compile(RoutingPolicyV1::default());
    assert!(direct.is_success(), "{:?}", direct.diagnostics().items());
    let radix = compile(RoutingPolicyV1 {
        mode: RouteMode::Radix,
    });
    assert!(radix.is_success(), "{:?}", radix.diagnostics().items());
    let direct_raw = direct.raw_contract().expect("真实契约必须存在");
    let radix_raw = radix.raw_contract().expect("真实契约必须存在");
    assert_eq!(radix_raw.routing().mode(), RouteMode::Radix);
    assert_ne!(
        direct_raw.fingerprint(),
        radix_raw.fingerprint(),
        "路由模式必须改变整体契约指纹"
    );
    assert_ne!(
        direct.action_key().expect("action key 存在"),
        radix.action_key().expect("action key 存在"),
        "路由模式必须改变 action key"
    );
    // 契约段自身：不同模式给出不同指纹，同参数重建给出相同指纹。
    let demand = RoutingDemand {
        owners: 2,
        return_publish_sites: 4,
    };
    let direct_contract =
        RoutingRuntimeContract::build(demand, RoutingPolicyV1::default()).expect("契约可构建");
    let radix_policy = RoutingPolicyV1 {
        mode: RouteMode::Radix,
    };
    let radix_contract = RoutingRuntimeContract::build(demand, radix_policy).expect("契约可构建");
    assert_ne!(direct_contract.fingerprint(), radix_contract.fingerprint());
    assert_eq!(
        radix_contract.fingerprint(),
        RoutingRuntimeContract::build(radix_contract.demand(), radix_policy)
            .expect("同参数契约可构建")
            .fingerprint(),
        "同参数构建必须给出同指纹"
    );
}

/// 子段自洽的篡改必须在 `verify` 里逐条暴露。
#[test]
fn tampered_routing_contract_fields_are_rejected() {
    let contract = RoutingRuntimeContract::build(
        RoutingDemand {
            owners: 2,
            return_publish_sites: 4,
        },
        RoutingPolicyV1 {
            mode: RouteMode::Radix,
        },
    )
    .expect("radix 契约可构建");
    let tamper = |mutate: &dyn Fn(&mut RoutingRuntimeContract)| {
        let mut tampered = contract.clone();
        mutate(&mut tampered);
        tampered.fingerprint = tampered.compute_fingerprint();
        tampered
            .verify()
            .expect_err("篡改必须拒绝")
            .message()
            .to_owned()
    };
    assert_eq!(
        tamper(&|c| c.bucket_count = 32),
        "radix bucket 参数与登记值不一致；bucket 数必须是固定 2^k"
    );
    assert_eq!(
        tamper(&|c| c.bucket_log2 = 5),
        "radix bucket 参数与登记值不一致；bucket 数必须是固定 2^k"
    );
    assert_eq!(tamper(&|c| c.max_levels = 3), "路由目录层数与登记值不一致");
    assert_eq!(tamper(&|c| c.max_levels = 0), "路由目录层数与登记值不一致");
    assert_eq!(
        tamper(&|c| c.hop_limit = 2),
        "转发 hop 上限与登记值不一致，且不得小于目录层数"
    );
    assert_eq!(
        tamper(&|c| c.routed_families = vec!["card-mark".to_owned()]),
        "radix 拦截消息族目录与登记表不一致"
    );
    assert_eq!(
        tamper(&|c| c.maintenance_phases = vec!["idle".to_owned()]),
        "维护相位目录与登记表不一致"
    );
    assert_eq!(
        tamper(&|c| {
            c.direct_to_radix_phases = vec![
                "flush-staging".to_owned(),
                "freeze-target-cache".to_owned(),
                "publish-mode".to_owned(),
            ]
        }),
        "direct → radix 切换序列目录与登记表不一致"
    );
    assert_eq!(
        tamper(&|c| {
            c.radix_to_direct_phases = vec![
                "drain-buckets".to_owned(),
                "publish-mode".to_owned(),
                "restore-target-cache".to_owned(),
            ]
        }),
        "radix → direct 切换序列目录与登记表不一致"
    );
    assert_eq!(
        tamper(&|c| c.statistics = vec!["hops".to_owned()]),
        "路由统计目录与登记表不一致"
    );
    let mut broken_fingerprint = contract.clone();
    broken_fingerprint.fingerprint[0] ^= 1;
    assert_eq!(
        broken_fingerprint
            .verify()
            .expect_err("指纹篡改必须拒绝")
            .message(),
        "routing 契约指纹与内容不一致"
    );
    // 模式目录参与内容身份：不同 mode 的契约给出不同指纹。
    let direct = RoutingRuntimeContract::build(RoutingDemand::default(), RoutingPolicyV1::direct())
        .expect("direct 契约可构建");
    assert_ne!(direct.fingerprint(), contract.fingerprint());
    assert_eq!(ROUTE_MODES, ["direct", "radix"]);
}

/// direct 平面不分配任何 bucket：容量为 0，发布与路由步都被拒绝。
#[test]
fn direct_mode_allocates_no_bucket_matrix() {
    let mut plane = direct_plane();
    assert_eq!(plane.mode(), RouteMode::Direct);
    assert_eq!(plane.bucket_capacity(), 0);
    assert_eq!(plane.pending_bytes(), 0);
    assert_eq!(plane.pending_batches(), 0);
    assert_eq!(
        plane
            .enqueue(chain(0xdead_beef, 64))
            .expect_err("direct 不进 radix"),
        RawInvariant::new("direct 模式不进入 radix staging")
    );
    assert!(plane.route_step(&mut deliver_all()).is_err());
}

/// radix bucket 路由按原始 target 的 route key bits 确定，且每步恰好一跳。
#[test]
fn radix_routing_is_deterministic_and_counts_hops() {
    let mut plane = radix_plane();
    plane
        .enqueue(chain(0x0000_0000_0000_00ff, 128))
        .expect("入队成功");
    plane
        .enqueue(chain(0xffff_ffff_ffff_ffff, 64))
        .expect("入队成功");
    assert_eq!(plane.pending_batches(), 2);
    assert_eq!(plane.pending_bytes(), 192);
    let level0 = |key: u64| (key & (RADIX_BUCKETS as u64 - 1)) as usize;
    let level1 = |key: u64| ((key >> RADIX_BUCKET_LOG2) & (RADIX_BUCKETS as u64 - 1)) as usize;
    assert_ne!(
        level0(0x0000_0000_0000_00ff),
        level1(0x0000_0000_0000_00ff),
        "两个层的 bits 段不同，batch 必须逐层移动"
    );
    // 第一跳：两个 batch 从 level 0 下降到 level 1，无交付。
    let actions = plane.route_step(&mut deliver_all()).expect("第一跳无交付");
    assert!(actions.is_empty());
    assert_eq!(plane.stats().remote_return_hops, 2);
    assert_eq!(plane.pending_batches(), 2);
    // 第二跳：终层解析后交付。
    let actions = plane.route_step(&mut deliver_all()).expect("第二跳交付");
    assert_eq!(actions.len(), 2);
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, RouteAction::Deliver(_)))
    );
    assert_eq!(plane.stats().remote_return_hops, 4);
    assert_eq!(plane.pending_batches(), 0);
    assert_eq!(plane.pending_bytes(), 0);
    assert_eq!(plane.stats().radix_batches, 2);
    assert_eq!(
        plane
            .route_step(&mut deliver_all())
            .expect("空平面推进合法")
            .len(),
        0,
        "空平面的 route step 不产生动作"
    );
    // 相同 route key 的 batch 落进同一 bucket，FIFO 顺序交付。
    plane.enqueue(chain(0x1234, 8)).expect("入队成功");
    plane.enqueue(chain(0x1234, 16)).expect("入队成功");
    let actions = plane.route_step(&mut deliver_all()).expect("第一跳无交付");
    assert!(actions.is_empty());
    let actions = plane.route_step(&mut deliver_all()).expect("第二跳交付");
    assert_eq!(actions.len(), 2, "同 bucket 的两个 batch 同步交付");
    assert_eq!(plane.stats().remote_return_hops, 8);
}

/// hop 上限与错误 forward 都是稳定 invariant。
#[test]
fn hop_limit_and_wrong_forward_are_stable_invariants() {
    // hop_limit = 1：第一跳把 batch 带进终层（hops = 1），第二跳交付必然越限。
    let mut tight = RoutingPlane::new(RouteMode::Radix, 2, RADIX_BUCKETS, RADIX_BUCKET_LOG2, 1);
    tight.enqueue(chain(0x00ff, 64)).expect("入队成功");
    tight
        .route_step(&mut deliver_all())
        .expect("第一跳仍在预算内");
    assert_eq!(
        tight
            .route_step(&mut deliver_all())
            .expect_err("第二跳交付必须越限"),
        RawInvariant::new("radix 路由 hop 超过固定上限")
    );
    // 错误 forward：终点解析失败按原错误稳定传播。
    let mut plane = radix_plane();
    plane.enqueue(chain(0x00ff, 64)).expect("入队成功");
    plane.route_step(&mut deliver_all()).expect("第一跳");
    let mut unknown = |_: OwnerToken| -> Result<RouteResolution, RawInvariant> {
        Err(RawInvariant::new(
            "radix 终点解析遇到未知 owner 或伪造 route key",
        ))
    };
    assert_eq!(
        plane
            .route_step(&mut unknown)
            .expect_err("错误 forward 必须拒绝"),
        RawInvariant::new("radix 终点解析遇到未知 owner 或伪造 route key")
    );
}

/// 旧 epoch 的转发与注入沿记录排空后进入新 token/domain，计数逐项可见。
#[test]
fn old_topology_drains_into_new_token_or_domain() {
    let mut plane = radix_plane();
    plane.enqueue(chain(0x00ff, 64)).expect("入队成功");
    plane.enqueue(chain(0xff00, 32)).expect("入队成功");
    plane.route_step(&mut deliver_all()).expect("第一跳");
    let new_token = token(0xabcd);
    let domain = token(0x0001);
    let mut first_resolution = true;
    let mut resolve = move |_: OwnerToken| {
        if first_resolution {
            first_resolution = false;
            Ok(RouteResolution::Forward(new_token))
        } else {
            Ok(RouteResolution::Inject(domain))
        }
    };
    let actions = plane.route_step(&mut resolve).expect("终层解析");
    assert!(actions.is_empty(), "转发与注入都先进入在飞记录");
    assert_eq!(plane.forwarding_count(), 2);
    assert_eq!(plane.stats().injection_fallbacks, 1);
    let actions = plane.route_step(&mut deliver_all()).expect("排空转发记录");
    assert_eq!(actions.len(), 2);
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, RouteAction::Retarget(_)))
    );
    assert_eq!(plane.stats().old_topology_drained_batches, 2);
    assert_eq!(plane.forwarding_count(), 0);
    assert_eq!(plane.pending_batches(), 0);
    assert!(plane.observe_topology(Epoch::from_raw(3)).is_ok());
    assert_eq!(
        plane
            .observe_topology(Epoch::from_raw(2))
            .expect_err("epoch 回退必须拒绝"),
        RawInvariant::new("routing 观测的 topology epoch 回退")
    );
}

/// radix → direct 的相位序列不可跳过、不可逆转，且切换期间发布被冻结。
#[test]
fn radix_to_direct_sequence_is_enforced() {
    let mut plane = radix_plane();
    plane.enqueue(chain(0x00ff, 64)).expect("入队成功");
    assert_eq!(
        plane
            .begin_maintenance(RouteMode::Radix, 1)
            .expect_err("目标模式与当前相同必须拒绝"),
        RawInvariant::new("routing 模式切换的目标与当前模式相同")
    );
    plane
        .begin_maintenance(RouteMode::Direct, 7)
        .expect("r2d 切换可进入");
    assert_eq!(plane.maintenance(), MaintenancePhase::DrainBuckets);
    assert_eq!(plane.maintenance_epoch(), 7);
    assert!(!plane.publish_allowed(), "maintenance 期间发布必须被冻结");
    assert_eq!(
        plane
            .enqueue(chain(0x00ff, 64))
            .expect_err("冻结期入队必须拒绝"),
        RawInvariant::new("routing maintenance 期间不接受新的 radix batch")
    );
    assert_eq!(
        plane
            .advance_maintenance()
            .expect_err("bucket 未排空必须拒绝"),
        RawInvariant::new("drain-buckets 相位要求 bucket 已排空")
    );
    plane.route_step(&mut deliver_all()).expect("第一跳");
    plane.route_step(&mut deliver_all()).expect("第二跳交付");
    plane.advance_maintenance().expect("bucket 排空后可推进");
    assert_eq!(plane.maintenance(), MaintenancePhase::DrainForwarding);
    plane.advance_maintenance().expect("无转发记录时可直接推进");
    assert_eq!(plane.maintenance(), MaintenancePhase::PublishMode);
    plane.advance_maintenance().expect("publish-mode 可推进");
    assert_eq!(plane.mode(), RouteMode::Direct);
    assert_eq!(plane.bucket_capacity(), 0, "radix → direct 释放 bucket 表");
    assert_eq!(plane.maintenance(), MaintenancePhase::RestoreTargetCache);
    assert_eq!(plane.stats().maintenance_switches, 1);
    assert_eq!(plane.maintenance_epoch(), 8);
    plane
        .advance_maintenance()
        .expect("恢复 target cache 后结束");
    assert_eq!(plane.maintenance(), MaintenancePhase::Idle);
    assert!(plane.publish_allowed());
    assert_eq!(
        plane
            .advance_maintenance()
            .expect_err("idle 相位没有下一步"),
        RawInvariant::new("routing 相位 Idle 与模式 Direct 的组合没有登记的下一步")
    );
}

/// direct → radix 的序列：冻结 target cache、冲刷 staging、publish-mode 分配 bucket。
#[test]
fn direct_to_radix_sequence_flips_mode_at_publish() {
    let mut plane = direct_plane();
    plane
        .begin_maintenance(RouteMode::Radix, 3)
        .expect("d2r 切换可进入");
    assert_eq!(plane.maintenance(), MaintenancePhase::FreezeTargetCache);
    assert!(!plane.publish_allowed());
    plane.advance_maintenance().expect("冻结后推进");
    assert_eq!(plane.maintenance(), MaintenancePhase::FlushStaging);
    plane.advance_maintenance().expect("冲刷后推进");
    assert_eq!(plane.mode(), RouteMode::Radix);
    assert_eq!(
        plane.bucket_capacity(),
        (RADIX_BUCKETS * MAX_RADIX_LEVELS) as usize
    );
    assert_eq!(plane.maintenance(), MaintenancePhase::Idle);
    assert_eq!(plane.stats().maintenance_switches, 1);
    assert_eq!(plane.maintenance_epoch(), 4);
    plane.enqueue(chain(0x00ff, 64)).expect("新模式可入队");
    assert_eq!(plane.pending_batches(), 1);
}
