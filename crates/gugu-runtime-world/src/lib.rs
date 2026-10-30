//! runtime world 参照实现：owner/路由/协程世界、rt0 启动与终止、集合/哈希/I/O/Path 参照模型。
//!
//! 只服务确定性测试与 bench，不进入镜像执行路径，也不进入 compiler 的 lib 构建；契约与
//! verifier 在 `gugu-runtime-abi`。子模块经 `super::` 访问的契约项由根部显式导入，
//! 本 crate 的公开面只有这里声明的 world 模块。
#![forbid(unsafe_code)]

use gugu_runtime_abi as runtime;
use gugu_runtime_abi::{
    OWNER_INBOX_SHARDS, PlatformProfile, RAW_SLAB_PAGE_BYTES, RESOURCE_DEDICATED_ALIGN_LIMIT,
    barrier_schema, combining_schema, coroutine, edge_schema, extent, foreign_schema, inbox,
    local_heap, message, model, platform, provenance_schema, provider, resource, routing_schema,
    scheduler_schema, shared_heap_schema, size_class, slab, stack, startup_schema, text,
};

pub mod candidate_schema;
pub mod collections;
pub mod combining;
pub mod dynamic_roots_schema;
pub mod foreign;
pub mod hash;
pub mod io;
pub mod lifecycle;
pub mod owner;
pub mod path;
pub mod provenance;
pub mod report;
pub mod routing;
pub mod shared_heap;
pub mod stack_arena;
pub mod startup;
pub mod termination;
#[allow(dead_code, reason = "raw plane 参照实现由确定性测试与 bench 消费")]
pub mod world;

#[cfg(test)]
mod combining_tests;
#[cfg(test)]
mod hash_tests;
#[cfg(test)]
mod io_tests;
#[cfg(test)]
mod path_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod termination_tests;
