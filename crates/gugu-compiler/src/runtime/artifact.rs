//! compiler 侧静态 runtime artifact 视图。
//!
//! 这里不保存 runtime source table；source/artifact 只在 build.rs 由 builder 生成，运行时
//! 首次访问时解码并验证。解码错误是 compiler invariant，不会回退到源码重编。

use std::sync::LazyLock;

pub use gugu_compiler_core::{
    RuntimeArtifactError, RuntimeArtifactManifestV1, RuntimeArtifactPayloadV1, RuntimeArtifactV1,
    RuntimeContractRecordV1, RuntimeFunctionArtifactV1, RuntimeGenericTemplateV1,
    RuntimeModuleInterfaceV1, RuntimeSourceRecordV1,
};

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

/// 静态嵌入的两个 target 专属 runtime artifact。
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeArtifacts;

impl RuntimeArtifacts {
    /// 返回指定目标的已验证 artifact。
    pub fn for_target(
        target: crate::TargetName,
    ) -> Result<&'static RuntimeArtifactV1, RuntimeArtifactError> {
        let artifact = match target {
            crate::TargetName::X86_64Linux => &*LINUX,
            crate::TargetName::X86_64Windows => &*WINDOWS,
        };
        let artifact = artifact.as_ref().map_err(Clone::clone)?;
        let target = match target {
            crate::TargetName::X86_64Linux => gugu_compiler_core::TargetName::X86_64Linux,
            crate::TargetName::X86_64Windows => gugu_compiler_core::TargetName::X86_64Windows,
        };
        artifact.for_target(target)?;
        // 当前 builder 提供 source manifest；执行体只能来自本次源码闭包，不能丢弃预编译片段。
        if !artifact.payload.functions.is_empty() {
            return Err(RuntimeArtifactError::Manifest(
                "源码 runtime artifact 不得携带未物化的 executable function".to_owned(),
            ));
        }
        Ok(artifact)
    }

    /// 返回目标 artifact 的 manifest 摘要，用于 action identity。
    pub fn manifest_digest(target: crate::TargetName) -> Result<[u8; 32], RuntimeArtifactError> {
        let artifact = Self::for_target(target)?;
        let bytes = gugu_compiler_core::canonical_manifest_bytes(&artifact.manifest);
        let mut hasher = blake3::Hasher::new_derive_key("gugu-runtime-manifest-v1");
        hasher.update(&bytes);
        Ok(*hasher.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_targets_decode_and_have_distinct_manifests() {
        let linux =
            RuntimeArtifacts::for_target(crate::TargetName::X86_64Linux).expect("linux artifact");
        let windows = RuntimeArtifacts::for_target(crate::TargetName::X86_64Windows)
            .expect("windows artifact");
        assert_ne!(linux.manifest.target, windows.manifest.target);
        assert_ne!(
            RuntimeArtifacts::manifest_digest(crate::TargetName::X86_64Linux)
                .expect("linux digest"),
            RuntimeArtifacts::manifest_digest(crate::TargetName::X86_64Windows)
                .expect("windows digest")
        );
    }
}
