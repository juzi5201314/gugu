//! 语义层之上的派生编排：GIR 构建 → 单态化闭包 → late → 可安装集 → 分析 → 放置 → 具体化 →
//! GIR pass。
//!
//! 语义层只产出 [`SemaWorld`]；本模块把它与 GIR 层内建前缀接起来，冻结出完整的前端 world。

use super::analysis;
use super::semantics::{CheckedSemantics, Identities, SemaWorld, comptime};
use crate::Diagnostic;

/// 一次前端派生冻结出的全部 world 产物。
///
/// 字段顺序即管线顺序：早期常量 → 类型检查 → 布局 → HIR → 身份 → 分析 → 单态化 → GIR。
/// 构建期物化从这里取与用户无关的层，用户编译路径消费同一组字段。
pub struct DerivedWorld {
    pub checked: CheckedSemantics,
    /// 早期常量表：构建期物化读取，用户编译路径只消费冻结进 HIR 的结果。
    pub early: comptime::EarlyConstTable,
    pub layouts: Vec<super::types::Layout>,
    pub registry_identity: (u32, [u8; 32]),
    pub hir: super::hir::Validated,
    /// 身份表：构建期物化读取，用户编译路径只经查询键间接消费。
    pub identities: Identities,
    pub analysis: analysis::AnalysisWorldV1,
    /// 本 world 从内建函数表安装的实例；强制现算或函数表为空时为空集。
    pub installed: crate::functions::InstalledFunctions,
    pub mono: super::mono::MonoWorldV1,
    pub gir: super::gir::GirWorldV1,
    pub gir_stats: super::gir::pass::GirPassStats,
}

/// 在语义层产物上完成 GIR 层派生。
pub fn derive(
    sema: SemaWorld<'_>,
    sources: &crate::SourceMap,
    cfg: &super::cfg::CfgContext,
    queries: &crate::query::QueryEngine,
    prefix: Option<&super::prefix::BuiltinPrefix<'_>>,
) -> Result<DerivedWorld, Vec<Diagnostic>> {
    let SemaWorld {
        model,
        checked,
        early,
        layouts,
        registry_identity,
        hir,
        identities,
        dependency,
        lower_dependency,
    } = sema;
    let gir = super::gir::build_world(&hir, queries, sources, prefix)?;
    let context = super::mono::keys::MonoContext::new(
        &model,
        &checked,
        &identities,
        hir.module(),
        sources,
        cfg.target(),
        cfg.harness(),
    );
    let mut mono_world = super::mono::close(&context, queries, &gir)?;
    // 内建 link 未被裁剪时，契约记录类型目录直取物化表：七个 layout verifier 仍随后逐字段
    // 校验它，加载即校验的规矩不变。
    if let Some(prefix) = prefix {
        mono_world.contracts = prefix.record_types().to_vec();
    }
    super::late::run(hir.module(), &mut mono_world, queries, sources)?;
    let gir = super::gir::attach_fragments(gir, &mono_world);
    let entries = prefix.map_or(&[][..], super::prefix::BuiltinPrefix::functions);
    let installed =
        crate::functions::InstalledFunctions::resolve(hir.module(), &mono_world, entries)
            .map_err(|error| vec![error])?;
    let (analysis_world, analysis_dependency) = analysis::run_world(
        hir.module(),
        hir.fingerprint(),
        &gir,
        &mono_world,
        cfg,
        &dependency,
        &lower_dependency,
        analysis::AnalysisPolicyV1::default(),
        queries,
        sources,
        (&installed, entries),
    )?;
    let gir = super::gir::place_world(&hir, gir, &analysis_world, queries, sources)?;
    let summaries = super::mono::summary::project(
        cfg.target(),
        &mono_world,
        &analysis_world,
        &analysis_dependency,
        queries,
    )?;
    mono_world.public_summaries = summaries;
    let mut gir = gir;
    gir.concrete = super::gir::concrete::build(&context, &gir, &mono_world)?;
    let gir_stats = super::gir::pass::run(&mut gir, hir.module(), &analysis_world)?;
    gir.fingerprint = super::gir::world_fingerprint(
        &gir.bodies,
        &gir.fragments,
        gir.hir_fingerprint,
        &gir.placement,
        &gir.concrete,
    );
    Ok(DerivedWorld {
        checked,
        early,
        layouts,
        registry_identity,
        hir,
        identities,
        analysis: analysis_world,
        installed,
        mono: mono_world,
        gir,
        gir_stats,
    })
}
