//! runtime 层：契约与 verifier 在 `gugu-runtime-abi`，这里只保留 world 参照实现与需要整条
//! 管线的 runtime 回归。
//!
//! world 参照实现（owner/路由/协程世界、rt0 启动与终止、集合/哈希/I/O 参照模型）只服务
//! 确定性测试与 bench，不进入镜像执行路径。

pub use gugu_runtime_abi::*;

#[cfg(all(test, feature = "slow-tests"))]
mod blocking_io_tests;
#[allow(dead_code, reason = "GC schema 由 metadata 编码与镜像探针契约消费")]
pub(crate) mod candidate_schema;
#[allow(
    dead_code,
    reason = "Map/Set 参照模型由确定性测试消费，尚未降成 Gugu 可调用的集合实现"
)]
pub(crate) mod collections;
#[cfg(all(test, feature = "slow-tests"))]
mod collections_tests;
#[allow(
    dead_code,
    reason = "typed combining 参照实现由确定性测试与 bench 消费"
)]
mod combining;
#[cfg(test)]
mod combining_tests;
#[allow(
    dead_code,
    reason = "动态根登记契约由镜像 runtime、探针与后续收集器共同消费"
)]
pub(crate) mod dynamic_roots_schema;
#[allow(dead_code, reason = "外调交接的确定性参照实现由契约与测试消费")]
mod foreign;
#[cfg(all(test, feature = "slow-tests"))]
mod foreign_tests;
#[allow(
    dead_code,
    reason = "哈希族与稳定键参照模型由集合模型与确定性测试消费，尚未降成 Gugu 可调用的 intrinsic"
)]
pub(crate) mod hash;
#[cfg(test)]
mod hash_tests;
#[allow(
    dead_code,
    reason = "I/O 组合函数与阻塞等待由确定性测试消费，尚未降成 Gugu 可调用实现"
)]
pub(crate) mod io;
#[cfg(test)]
mod io_tests;
#[allow(
    dead_code,
    reason = "无损 Path 参照模型由确定性测试消费，尚未降成 Gugu 可调用实现"
)]
pub(crate) mod path;
#[cfg(test)]
mod path_tests;
#[allow(
    dead_code,
    reason = "raw link provenance 与 release 安全 profile 的确定性参照实现由 world 与确定性测试消费"
)]
pub(crate) mod provenance;
#[allow(
    dead_code,
    reason = "temporal radix fan-out 的确定性参照实现由 world 与确定性测试消费"
)]
pub(crate) mod routing;
#[allow(
    dead_code,
    reason = "stable handle 与 forwarding grace 的确定性参照实现"
)]
mod shared_heap;
#[allow(dead_code, reason = "栈arena与cache的确定性参照实现")]
mod stack_arena;
#[cfg(all(test, feature = "slow-tests"))]
mod text_tests;
// rt0 启动、生命周期、报告与终止的参照实现：lib 构建只消费契约段，确定性验证
// 套件直接消费这些状态机。
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod lifecycle;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod owner;
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod report;
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod startup;
#[allow(dead_code, reason = "rt0 参照实现由确定性测试消费")]
mod termination;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
mod world;

#[cfg(all(test, feature = "slow-tests"))]
mod platform_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod termination_tests;
