//! 前端测试的 artifact 与 session helper。
//!
//! 前端不嵌入 artifact：`build.rs` 产出的字节由 façade 固定成静态并注入，同一个 build script
//! 也把字节写到 workspace `target/prebuilt/`，供这里解码后作为测试 provider。字节来源因此
//! 与生产路径完全一致，只是装载位置不同；`cargo build -p gugu-compiler` 之前该文件不存在，
//! 缺失时以可操作信息失败而不是回退到源码派生。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use crate::{
    Compilation, TargetName,
    runtime::{RuntimeArtifactError, RuntimeArtifactProvider, RuntimeArtifactV1, RuntimeResources},
};

/// build-time 物化产物的落盘位置。
fn prebuilt_path(target: TargetName) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/prebuilt")
        .join(format!("runtime-{}.gbc", target.name()))
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
    RuntimeArtifactV1::decode(&bytes).expect("物化产物必须可解码")
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
pub(crate) fn provider() -> &'static dyn RuntimeArtifactProvider {
    static PROVIDER: PrebuiltArtifacts = PrebuiltArtifacts;
    &PROVIDER
}

/// 目标专属的 artifact source manifest 视图。
///
/// 物化表按 target 派生（内建源的 `os`/`arch` 谓词参与裁剪），因此编译目标与 artifact 必须
/// 一致：非默认目标的用例要用这里取同目标的 artifact，不能沿用宿主默认目标那一份。
pub(crate) fn resources_for(target: TargetName) -> RuntimeResources<'static> {
    RuntimeResources::new(artifact(target)).expect("物化 artifact 必须通过校验")
}

/// 宿主默认目标的 artifact source manifest 视图。
pub(crate) fn resources() -> RuntimeResources<'static> {
    resources_for(TargetName::X86_64Linux)
}

/// 断言会话源码表恰由给定用户源码与内建库源组成。
///
/// 探针源只在入口本身是探针程序时安装，因此期望集合按安装规则从 artifact 源表派生，
/// 而不是把内建源个数写死。
pub(crate) fn assert_session_sources(compilation: &Compilation, user: &[&str], entry: &str) {
    let mut installed: Vec<&str> = compilation
        .source_map()
        .snapshots()
        .iter()
        .map(|snapshot| snapshot.logical_path())
        .filter(|path| !user.contains(path))
        .collect();
    installed.sort_unstable();
    let builtin = resources();
    let mut expected: Vec<&str> = builtin
        .sources()
        .iter()
        .map(|source| source.logical_path.as_str())
        .filter(|path| crate::frontend::installs_builtin_source(path, entry))
        .collect();
    expected.sort_unstable();
    assert_eq!(installed, expected, "会话源码表必须恰为内建库源与用户源码");
}
