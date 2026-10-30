//! world 级回归共用的整体契约与世界构造。
//!
//! 契约走 `RuntimeRawContractV1::build` 的真实装配点：空 GC metadata、各段默认需求，只让调用方
//! 指定 policy 与压缩需求；跨段等式因此与编译器产出的空程序契约同源。

use super::RawWorld;
use crate::runtime::message::BatchLimits;
use crate::runtime::model::RawPlanePolicyV1;
use crate::runtime::{
    BarrierDemand, CompressionDemand, GcMetadataDemand, GcPacingDemand, LocalHeapDemand,
    MarkDemand, PlatformProfile, RawPlaneDemand, RawResourceDemand, Rt0Demand,
    RuntimeRawContractV1, SchedulerDemand, StackMapDemand, SyncDemand, WaitDemand,
};
use gugu_base::target::TargetName;

/// 默认 policy 的整体契约。
pub(super) fn raw_contract() -> RuntimeRawContractV1 {
    raw_contract_with(RawPlanePolicyV1::default(), CompressionDemand::default())
}

/// 指定 policy 与压缩需求的整体契约。
pub(super) fn raw_contract_with(
    policy: RawPlanePolicyV1,
    compression_demand: CompressionDemand,
) -> RuntimeRawContractV1 {
    RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        policy,
        RawPlaneDemand::default(),
        RawResourceDemand::default(),
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        StackMapDemand::default(),
        GcMetadataDemand::empty(),
        BarrierDemand::default(),
        GcPacingDemand::default(),
        MarkDemand::default(),
        LocalHeapDemand::default(),
        compression_demand,
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建")
}

/// 按契约创建并配置 routing 与 provenance 平面的世界。
pub(super) fn configured_world(
    contract: &RuntimeRawContractV1,
    seed: u64,
    owners: u32,
    nodes: u32,
) -> RawWorld {
    let mut world =
        RawWorld::new(seed, owners, nodes, BatchLimits::default()).expect("world 可创建");
    world
        .configure_routing(contract.routing())
        .expect("routing 平面可配置");
    world.configure_provenance(contract.provenance());
    world
}
