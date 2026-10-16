//! build-time runtime source table。
//!
//! 只有本 crate 持有 `include_str!`；compiler normal dependency 不携带这张源码表。

use gugu_compiler_core::{
    RuntimeArtifactBuilder, RuntimeArtifactError, RuntimeArtifactV1, RuntimeSourceInput,
    RuntimeSourceRole, TargetName,
};

const SOURCES: &[RuntimeSourceInput<'static>] = &[
    source(
        "std/prelude.gg",
        include_str!("../../gugu-frontend/resources/std/prelude.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/option.gg",
        include_str!("../../gugu-frontend/resources/std/option.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/result.gg",
        include_str!("../../gugu-frontend/resources/std/result.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/error.gg",
        include_str!("../../gugu-frontend/resources/std/error.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/cmp.gg",
        include_str!("../../gugu-frontend/resources/std/cmp.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/ops.gg",
        include_str!("../../gugu-frontend/resources/std/ops.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/iter.gg",
        include_str!("../../gugu-frontend/resources/std/iter.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/text.gg",
        include_str!("../../gugu-frontend/resources/std/text.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/io.gg",
        include_str!("../../gugu-frontend/resources/std/io.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/path.gg",
        include_str!("../../gugu-frontend/resources/std/path.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/fmt.gg",
        include_str!("../../gugu-frontend/resources/std/fmt.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/hash.gg",
        include_str!("../../gugu-frontend/resources/std/hash.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/collections.gg",
        include_str!("../../gugu-frontend/resources/std/collections.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/sync.gg",
        include_str!("../../gugu-frontend/resources/std/sync.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/runtime/core.gg",
        include_str!("../../gugu-frontend/resources/runtime/core.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/platform.gg",
        include_str!("../../gugu-frontend/resources/runtime/platform.gg"),
        RuntimeSourceRole::StandardLibrary,
    ),
    source(
        "std/runtime/coroutine.gg",
        include_str!("../../gugu-frontend/resources/runtime/coroutine.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/channel.gg",
        include_str!("../../gugu-frontend/resources/runtime/channel.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/scheduler.gg",
        include_str!("../../gugu-frontend/resources/runtime/scheduler.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/sync.gg",
        include_str!("../../gugu-frontend/resources/runtime/sync.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/barrier.gg",
        include_str!("../../gugu-frontend/resources/runtime/barrier.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/trace.gg",
        include_str!("../../gugu-frontend/resources/runtime/trace.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/heap.gg",
        include_str!("../../gugu-frontend/resources/runtime/heap.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/mark.gg",
        include_str!("../../gugu-frontend/resources/runtime/mark.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/gc.gg",
        include_str!("../../gugu-frontend/resources/runtime/gc.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/roots.gg",
        include_str!("../../gugu-frontend/resources/runtime/roots.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/collect.gg",
        include_str!("../../gugu-frontend/resources/runtime/collect.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/region.gg",
        include_str!("../../gugu-frontend/resources/runtime/region.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/edge.gg",
        include_str!("../../gugu-frontend/resources/runtime/edge.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/candidate.gg",
        include_str!("../../gugu-frontend/resources/runtime/candidate.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/pacing.gg",
        include_str!("../../gugu-frontend/resources/runtime/pacing.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/block_return.gg",
        include_str!("../../gugu-frontend/resources/runtime/block_return.gg"),
        RuntimeSourceRole::Runtime,
    ),
];

#[cfg(feature = "runtime-gc-probes")]
const PROBE_SOURCES: &[RuntimeSourceInput<'static>] = &[
    source(
        "std/runtime/gc_probes.gg",
        include_str!("../../gugu-frontend/resources/runtime/gc_probes.gg"),
        RuntimeSourceRole::Runtime,
    ),
    source(
        "std/runtime/fixtures/gc_image.gg",
        include_str!("../../gugu-frontend/resources/runtime/fixtures/gc_image.gg"),
        RuntimeSourceRole::Runtime,
    ),
];

const fn source(
    logical_path: &'static str,
    source: &'static str,
    role: RuntimeSourceRole,
) -> RuntimeSourceInput<'static> {
    RuntimeSourceInput {
        logical_path,
        source,
        role,
    }
}

/// 返回 build-time 唯一权威 source table。
pub fn sources() -> Vec<RuntimeSourceInput<'static>> {
    let extra: &[RuntimeSourceInput<'static>] = {
        #[cfg(feature = "runtime-gc-probes")]
        {
            PROBE_SOURCES
        }
        #[cfg(not(feature = "runtime-gc-probes"))]
        {
            &[]
        }
    };
    SOURCES.iter().chain(extra.iter()).copied().collect()
}

/// 为一个目标构建并验证 runtime artifact。
pub fn build(target: TargetName) -> Result<RuntimeArtifactV1, RuntimeArtifactError> {
    RuntimeArtifactBuilder::build(target, &sources())
}

/// 为 build.rs 提供两个目标的确定性构建入口。
pub fn build_all() -> Result<[(TargetName, RuntimeArtifactV1); 2], RuntimeArtifactError> {
    Ok([
        (TargetName::X86_64Linux, build(TargetName::X86_64Linux)?),
        (TargetName::X86_64Windows, build(TargetName::X86_64Windows)?),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_table_is_complete_and_decodable() {
        let sources = sources();
        assert_eq!(
            sources.len(),
            32 + usize::from(cfg!(feature = "runtime-gc-probes")) * 2
        );
        let artifact = build(TargetName::X86_64Linux).expect("build runtime artifact");
        let bytes = artifact.encode().expect("encode runtime artifact");
        let decoded = RuntimeArtifactV1::decode(&bytes).expect("decode runtime artifact");
        assert_eq!(decoded.manifest.sources.len(), sources.len());
        assert_eq!(decoded.manifest.target, "x86_64-linux");
    }
}
