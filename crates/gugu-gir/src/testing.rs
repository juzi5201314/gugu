//! 测试用 build-time artifact 装载：provider 与 source manifest 视图。
//!
//! 前端不嵌入 artifact：`build.rs` 产出的字节由 façade 固定成静态并注入，同一个 build script
//! 也把字节写到 workspace `target/prebuilt/`，供这里解码后作为测试 provider。字节来源因此
//! 与生产路径完全一致，只是装载位置不同；`cargo build -p gugu-compiler` 之前该文件不存在，
//! 缺失时以可操作信息失败而不是回退到源码派生。上层 crate 的测试经 `test-support` 复用这里。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use crate::{
    TargetName,
    runtime::{RuntimeArtifactError, RuntimeArtifactProvider, RuntimeArtifactV1, RuntimeResources},
};

/// build-time 物化产物的落盘位置。
///
/// 探针源只随 `runtime-gc-probes` 进入 artifact，两种风味各占一个文件：无探针构建不能覆盖
/// 探针构建写出的物化产物，否则探针用例会拿到缺探针源的表。
fn prebuilt_path(target: TargetName) -> PathBuf {
    let flavor = if cfg!(feature = "runtime-gc-probes") {
        "-probes"
    } else {
        ""
    };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/prebuilt")
        .join(format!("runtime-{}{flavor}.gbc", target.name()))
}

/// 读取并解码目标专属的物化产物。
fn load(target: TargetName) -> RuntimeArtifactV1 {
    let path = prebuilt_path(target);
    let bytes = fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "读不到 build-time 物化产物 {}：{error}；请先构建 gugu-compiler",
            path.display()
        )
    });
    RuntimeArtifactV1::decode(bytes.into()).expect("物化产物必须可解码")
}

/// x86_64-linux 的物化产物。
fn linux_artifact() -> &'static RuntimeArtifactV1 {
    static LINUX: LazyLock<RuntimeArtifactV1> = LazyLock::new(|| load(TargetName::X86_64Linux));
    &LINUX
}

/// x86_64-windows 的物化产物。
fn windows_artifact() -> &'static RuntimeArtifactV1 {
    static WINDOWS: LazyLock<RuntimeArtifactV1> = LazyLock::new(|| load(TargetName::X86_64Windows));
    &WINDOWS
}

/// 目标专属的物化产物。
fn artifact(target: TargetName) -> &'static RuntimeArtifactV1 {
    match target {
        TargetName::X86_64Linux => linux_artifact(),
        TargetName::X86_64Windows => windows_artifact(),
        TargetName::X86_64Macos => {
            static MACOS: LazyLock<RuntimeArtifactV1> =
                LazyLock::new(|| load(TargetName::X86_64Macos));
            &MACOS
        }
        TargetName::Aarch64Linux => {
            static AARCH64_LINUX: LazyLock<RuntimeArtifactV1> =
                LazyLock::new(|| load(TargetName::Aarch64Linux));
            &AARCH64_LINUX
        }
        TargetName::Aarch64Macos => {
            static AARCH64_MACOS: LazyLock<RuntimeArtifactV1> =
                LazyLock::new(|| load(TargetName::Aarch64Macos));
            &AARCH64_MACOS
        }
    }
}

/// 测试用 artifact provider：按目标返回同一份物化产物。
#[derive(Debug)]
struct PrebuiltArtifacts;

impl RuntimeArtifactProvider for PrebuiltArtifacts {
    fn artifact(
        &self,
        target: TargetName,
    ) -> Result<&'static RuntimeArtifactV1, RuntimeArtifactError> {
        Ok(artifact(target))
    }
}

/// 测试用 provider。
pub fn provider() -> &'static dyn RuntimeArtifactProvider {
    static PROVIDER: PrebuiltArtifacts = PrebuiltArtifacts;
    &PROVIDER
}

/// 目标专属的 artifact source manifest 视图。
///
/// 物化表按 target 派生（内建源的 `os`/`arch` 谓词参与裁剪），因此编译目标与 artifact 必须
/// 一致：非默认目标的用例要用这里取同目标的 artifact，不能沿用宿主默认目标那一份。
pub fn resources_for(target: TargetName) -> RuntimeResources<'static> {
    RuntimeResources::new(artifact(target))
}

/// 宿主默认目标的 artifact source manifest 视图。
pub fn resources() -> RuntimeResources<'static> {
    resources_for(TargetName::X86_64Linux)
}
