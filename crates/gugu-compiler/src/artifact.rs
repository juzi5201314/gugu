//! compiler 侧静态 runtime artifact 视图。
//!
//! 这里不保存 runtime source table；source/artifact 只在 `build.rs` 由 builder 生成，进程内
//! 首次访问时解码并验证。解码错误是 compiler invariant，不会回退到源码重编。

use std::sync::LazyLock;

use gugu_frontend::{RuntimeArtifactError, RuntimeArtifactProvider, RuntimeArtifactV1, TargetName};

static LINUX: LazyLock<Result<RuntimeArtifactV1, RuntimeArtifactError>> = LazyLock::new(|| {
    RuntimeArtifactV1::decode(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/runtime-x86_64-linux.gbc"
    )))
});

static WINDOWS: LazyLock<Result<RuntimeArtifactV1, RuntimeArtifactError>> = LazyLock::new(|| {
    RuntimeArtifactV1::decode(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/runtime-x86_64-windows.gbc"
    )))
});

/// 返回目标的 core 目标名。
const fn core_target(target: TargetName) -> gugu_compiler_core::TargetName {
    match target {
        TargetName::X86_64Linux => gugu_compiler_core::TargetName::X86_64Linux,
        TargetName::X86_64Windows => gugu_compiler_core::TargetName::X86_64Windows,
    }
}

/// 解码并校验目标专属 artifact。
fn decode(
    embedded: &'static LazyLock<Result<RuntimeArtifactV1, RuntimeArtifactError>>,
    target: TargetName,
) -> Result<&'static RuntimeArtifactV1, RuntimeArtifactError> {
    let artifact = embedded.as_ref().map_err(Clone::clone)?;
    artifact.for_target(core_target(target))?;
    Ok(artifact)
}

/// 返回目标专属的嵌入 runtime artifact。
pub fn embedded_artifact(
    target: TargetName,
) -> Result<&'static RuntimeArtifactV1, RuntimeArtifactError> {
    match target {
        TargetName::X86_64Linux => decode(&LINUX, target),
        TargetName::X86_64Windows => decode(&WINDOWS, target),
    }
}

/// 嵌入 artifact 的提供者：session 按目标从这里取字节。
#[derive(Clone, Copy, Debug, Default)]
pub struct EmbeddedRuntimeArtifacts;

impl RuntimeArtifactProvider for EmbeddedRuntimeArtifacts {
    fn artifact(
        &self,
        target: TargetName,
    ) -> Result<&'static RuntimeArtifactV1, RuntimeArtifactError> {
        embedded_artifact(target)
    }
}

/// 进程内固定的嵌入 artifact 提供者。
pub static EMBEDDED_ARTIFACTS: EmbeddedRuntimeArtifacts = EmbeddedRuntimeArtifacts;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_targets_decode_and_have_distinct_manifests() {
        let linux = embedded_artifact(TargetName::X86_64Linux).expect("linux artifact");
        let windows = embedded_artifact(TargetName::X86_64Windows).expect("windows artifact");
        assert_ne!(linux.manifest.target, windows.manifest.target);
    }
}
