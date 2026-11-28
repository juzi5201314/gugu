//! 分阶段物化保真：source → 基础表与基础表 → 函数表都必须逐字节复现。
//! 两段使用生产 materialize 组合的相同入口；基础表的完整比较保证组合输入相同，
//! 函数阶段比较完整 artifact，原有的表、顺序、编码和全部目标覆盖均保留。
use gugu_compiler::{
    RuntimeArtifactV1, TargetName, embedded_artifact, materialize_functions, materialize_tables,
};

#[test]
fn linux_base_tables_reproduce_materialization() {
    assert_tables(TargetName::X86_64Linux);
}
#[test]
fn windows_base_tables_reproduce_materialization() {
    assert_tables(TargetName::X86_64Windows);
}
#[test]
fn linux_function_table_reproduces_materialization() {
    assert_functions(TargetName::X86_64Linux);
}
#[test]
fn windows_function_table_reproduces_materialization() {
    assert_functions(TargetName::X86_64Windows);
}

fn tables_only(artifact: &RuntimeArtifactV1) -> RuntimeArtifactV1 {
    let mut artifact = artifact.clone();
    let prebuilt = artifact.payload.prebuilt.as_mut().expect("嵌入基础表");
    assert!(!prebuilt.functions.is_empty(), "嵌入函数表不能为空");
    prebuilt.functions.clear();
    prebuilt.function_revisions.clear();
    artifact
}

fn assert_tables(target: TargetName) {
    let embedded = embedded_artifact(target).expect("嵌入 artifact 可解码");
    let expected = tables_only(embedded);
    let mut source = embedded.clone();
    source.payload.prebuilt = None;
    let derived = materialize_tables(target, source).expect("基础表重新物化");
    assert_eq!(
        derived.encode().unwrap(),
        expected.encode().unwrap(),
        "{target:?} 基础表必须复现全部字节"
    );
}

fn assert_functions(target: TargetName) {
    let embedded = embedded_artifact(target).expect("嵌入 artifact 可解码");
    let derived = materialize_functions(target, tables_only(embedded)).expect("函数表重新物化");
    assert_eq!(
        derived.encode().unwrap(),
        embedded.encode().unwrap(),
        "{target:?} 函数表阶段必须复现完整 artifact"
    );
}
