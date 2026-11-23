//! 构建期物化的 driver 段：GIR 层物化表之后，录制内建实例的函数表。
//!
//! 录制在刚物化的 artifact 上按安装模式编译 canonical 程序 `fn main() {}`：此时函数表为空，
//! 内建实例全部现算，产物与用户编译现算的结果同源。分析分片取自 GIR 层，LIR 层取 poll
//! 之后的 body，x64 层取 absorb 之前的片段；入选规则由 [`frontend::functions::record`] 统一判定。

use std::collections::BTreeSet;

use crate::backend::x64::{codegen, glue::RuntimeGlueBindings, installed, oracle::X64Oracle};
use crate::frontend::{self, PrefixMode, SourceInput, cfg::CfgContext, prebuild};
use crate::{MaterializeError, QueryEngine, RuntimeArtifactV1, RuntimeResources, SourceMap};
use crate::{SourceSnapshot, TargetName, lir, raw_model, runtime::RawPlanePolicyV1};

/// 把 source-only artifact 物化成带内建块派生表与函数表的 artifact。
pub fn materialize(
    target: TargetName,
    source: RuntimeArtifactV1,
) -> Result<RuntimeArtifactV1, MaterializeError> {
    materialize_functions(target, prebuild::materialize_tables(target, source)?)
}

/// 在已物化的基础表上录制函数表；输入的函数表必须为空。
///
/// aarch64 还没有 a64 片段录制路径，函数表留空：加载端照常现算全部内建实例，
/// 产物与用户编译现算的结果同源这一不变式不受影响。
pub fn materialize_functions(
    target: TargetName,
    mut artifact: RuntimeArtifactV1,
) -> Result<RuntimeArtifactV1, MaterializeError> {
    let functions = if matches!(
        target.descriptor().arch,
        crate::target::Architecture::Aarch64
    ) {
        Vec::new()
    } else {
        record_functions(&artifact, target).map_err(MaterializeError::Derive)?
    };
    let prebuilt = artifact
        .payload
        .prebuilt
        .as_mut()
        .expect("物化表刚写入 artifact");
    prebuilt.function_revisions = frontend::functions::revisions();
    prebuilt
        .function_revisions
        .extend(lir::installed::revisions());
    prebuilt.function_revisions.extend(installed::revisions());
    prebuilt.functions = functions;
    artifact.validate()?;
    Ok(artifact)
}

/// 编译 canonical 程序并录制内建实例的函数表。
fn record_functions(
    artifact: &RuntimeArtifactV1,
    target: TargetName,
) -> Result<Vec<crate::RuntimeFunctionArtifactV1>, Vec<crate::Diagnostic>> {
    let resources = RuntimeResources::new(artifact);
    let prebuilt = resources.prebuilt().expect("物化表刚写入 artifact");
    let cfg = CfgContext::target_only(target);
    let queries = QueryEngine::new();
    let mut source_map = SourceMap::new(vec![
        SourceSnapshot::from_str("main.gg", "fn main() {}\n").expect("canonical 程序是合法快照"),
    ])
    .expect("canonical 程序的源码表合法");
    let output = frontend::bootstrap_with(
        SourceInput::Sources {
            source_map: &mut source_map,
            entry: "main.gg",
            source_root: "",
            package_identity: "single-file",
            require_main: true,
            cfg: &cfg,
            external_packages: &BTreeSet::new(),
            embed: None,
        },
        &queries,
        resources,
        PrefixMode::Install,
    )?;
    let lir = lir::build(
        &output.hir,
        &output.gir,
        &output.mono,
        target,
        lir::installed::RECORDED_COMPRESSION,
        &X64Oracle::new(target.descriptor().cpu_baseline),
        &queries,
        &source_map,
        (&output.installed, Some(prebuilt)),
    )?;
    let glues =
        RuntimeGlueBindings::resolve(&resources, &output.hir, &lir).map_err(|error| vec![error])?;
    let gc_metadata = frontend::gc::derive(&output.mono.universe, &output.gir, output.hir.module())
        .map_err(|error| vec![error])?;
    let policy = RawPlanePolicyV1 {
        compression: lir::installed::RECORDED_COMPRESSION,
        ..RawPlanePolicyV1::default()
    };
    let raw = raw_model::derive(
        target,
        policy,
        &output,
        &lir,
        &gc_metadata,
        &resources,
        &queries,
    )?;
    let contract =
        crate::backend::x64::contract::EncoderContract::build(target.descriptor().cpu_baseline);
    contract.verify().expect("目标 encoder 契约");
    let bodies = lir.bodies();
    frontend::functions::record(
        output.hir.module(),
        &output.mono,
        &output.gir,
        &output.analysis,
        prebuild::builtin_definitions(prebuilt),
        |digest| {
            let index = bodies
                .binary_search_by_key(digest, |body| body.instance)
                .expect("录制集实例都有 LIR body");
            let x64 = if installed::recordable(&bodies[index]) {
                let fragment = codegen::record_fragment(
                    &bodies[index],
                    &output.mono.universe,
                    &raw,
                    target,
                    &contract,
                    &glues,
                )
                .map_err(|errors| errors.into_iter().next().expect("编码失败包含诊断"))?;
                installed::encode(&fragment)
            } else {
                Vec::new()
            };
            Ok((lir::installed::encode(&bodies[index]), x64))
        },
    )
    .map_err(|error| vec![error])
}
