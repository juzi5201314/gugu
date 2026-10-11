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
        artifact.as_ref().map_err(Clone::clone)
    }

    /// 返回目标 artifact 的 manifest 摘要，用于 action identity。
    pub fn manifest_digest(target: crate::TargetName) -> Result<[u8; 32], RuntimeArtifactError> {
        let artifact = Self::for_target(target)?;
        let bytes =
            serde_json::to_vec(&artifact.manifest).expect("runtime artifact manifest 可序列化");
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
        assert_eq!(linux.manifest.sources.len(), 32);
        assert_eq!(windows.manifest.sources.len(), 32);
        assert_ne!(linux.manifest.target, windows.manifest.target);
        assert_ne!(
            RuntimeArtifacts::manifest_digest(crate::TargetName::X86_64Linux)
                .expect("linux digest"),
            RuntimeArtifacts::manifest_digest(crate::TargetName::X86_64Windows)
                .expect("windows digest")
        );
    }
}
