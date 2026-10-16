//! 物化保真：对同一份 source 表重新物化，必须逐字节复现嵌入 artifact。
//!
//! source 表直接取自嵌入 artifact（清掉 `prebuilt` 段），因此比对不受 builder 特性配置影响：
//! 这里验证的是「物化是 source 表的确定函数」。物化路径与用户编译路径共用同一段内建块派生，
//! 所以不一致只可能来自表集、顺序或编码漂移。

use gugu_compiler::{RuntimeArtifactV1, TargetName, embedded_artifact, materialize};

#[test]
fn embedded_artifacts_reproduce_materialization() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let embedded = embedded_artifact(target).expect("嵌入 artifact 可解码");
        assert!(
            embedded.payload.prebuilt.is_some(),
            "{target:?} 的 artifact 必须带物化表"
        );
        let rematerialized = materialize(target, source_only(embedded)).expect("重新物化");
        assert_eq!(
            rematerialized.encode().expect("encode"),
            embedded.encode().expect("encode embedded"),
            "{target:?} 的物化表必须复现嵌入字节"
        );
    }
}

/// 清掉物化段后的 source-only artifact。
fn source_only(artifact: &RuntimeArtifactV1) -> RuntimeArtifactV1 {
    let mut source = artifact.clone();
    source.payload.prebuilt = None;
    source
}
