//! LIR 固定优化管线：pass 顺序、共享图算法与唯一重写入口。
//!
//! 顺序在编译期固定；驱动每跑一个 pass 立即重建 arena 并运行结构 verifier。
pub(crate) mod abi;
pub(crate) mod barriers;
pub(crate) mod cfg;
pub(crate) mod constants;
pub(crate) mod dce;
pub(crate) mod graph;
pub(crate) mod gvn;
pub(crate) mod licm;
pub(crate) mod loops;
pub(crate) mod policy;
pub(crate) mod poll;
pub(crate) mod rewrite;
pub(crate) mod strength;
pub(crate) mod vectorize;
pub(crate) mod versioning;

use super::body::Body;
use super::invalid;
use crate::frontend::hir;
use crate::{BackendCostProfile, Diagnostic, DiagnosticCode};
use rewrite::Editor;

/// 固定 LIR pass；枚举顺序即执行顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LirPass {
    VerifySsaAndMemory,
    CanonicalizeCfg,
    SparseConditionalConstants,
    AlgebraicSimplification,
    GlobalValueNumbering,
    DeadStoreAndDeadValueElimination,
    CanonicalizeLoops,
    LoopInvariantCodeMotion,
    StrengthReduction,
    LoopVersioningAndUnswitching,
    LoopVectorizationAndUnrolling,
    LowerAllocationAndBarrierFastPaths,
    LowerTargetAbi,
    LegalizeX86_64,
    ClassifyPollFreeLeafAndPlaceBudgetedPolls,
    LowerPollFastPaths,
    PrepareRegisterAllocation,
}

/// 固定管线顺序；禁止运行时重排。
pub(crate) const LIR_PASS_ORDER: &[LirPass] = &[
    LirPass::VerifySsaAndMemory,
    LirPass::CanonicalizeCfg,
    LirPass::SparseConditionalConstants,
    LirPass::AlgebraicSimplification,
    LirPass::GlobalValueNumbering,
    LirPass::DeadStoreAndDeadValueElimination,
    LirPass::CanonicalizeLoops,
    LirPass::LoopInvariantCodeMotion,
    LirPass::StrengthReduction,
    LirPass::LoopVersioningAndUnswitching,
    LirPass::LoopVectorizationAndUnrolling,
    LirPass::LowerAllocationAndBarrierFastPaths,
    LirPass::LowerTargetAbi,
    LirPass::LegalizeX86_64,
    LirPass::ClassifyPollFreeLeafAndPlaceBudgetedPolls,
    LirPass::LowerPollFastPaths,
    LirPass::PrepareRegisterAllocation,
];

impl LirPass {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::VerifySsaAndMemory => "VerifySsaAndMemory",
            Self::CanonicalizeCfg => "CanonicalizeCfg",
            Self::SparseConditionalConstants => "SparseConditionalConstants",
            Self::AlgebraicSimplification => "AlgebraicSimplification",
            Self::GlobalValueNumbering => "GlobalValueNumbering",
            Self::DeadStoreAndDeadValueElimination => "DeadStoreAndDeadValueElimination",
            Self::CanonicalizeLoops => "CanonicalizeLoops",
            Self::LoopInvariantCodeMotion => "LoopInvariantCodeMotion",
            Self::StrengthReduction => "StrengthReduction",
            Self::LoopVersioningAndUnswitching => "LoopVersioningAndUnswitching",
            Self::LoopVectorizationAndUnrolling => "LoopVectorizationAndUnrolling",
            Self::LowerAllocationAndBarrierFastPaths => "LowerAllocationAndBarrierFastPaths",
            Self::LowerTargetAbi => "LowerTargetAbi",
            Self::LegalizeX86_64 => "LegalizeX86_64",
            Self::ClassifyPollFreeLeafAndPlaceBudgetedPolls => {
                "ClassifyPollFreeLeafAndPlaceBudgetedPolls"
            }
            Self::LowerPollFastPaths => "LowerPollFastPaths",
            Self::PrepareRegisterAllocation => "PrepareRegisterAllocation",
        }
    }
}

/// 单函数 pass 的派发；poll 两个 pass 在 world 级运行，不在此处出现。
fn run_pass(
    pass: LirPass,
    editor: &mut Editor,
    profile: &BackendCostProfile,
) -> Result<bool, Diagnostic> {
    match pass {
        LirPass::VerifySsaAndMemory => Ok(false),
        LirPass::CanonicalizeCfg => cfg::canonicalize(editor),
        LirPass::SparseConditionalConstants => constants::sparse_conditional(editor),
        LirPass::AlgebraicSimplification => constants::algebraic_simplify(editor),
        LirPass::GlobalValueNumbering => gvn::run(editor),
        LirPass::DeadStoreAndDeadValueElimination => dce::run(editor),
        LirPass::CanonicalizeLoops => loops::canonicalize(editor),
        LirPass::LoopInvariantCodeMotion => licm::run(editor),
        LirPass::StrengthReduction => strength::run(editor),
        LirPass::LoopVersioningAndUnswitching => versioning::run(editor),
        LirPass::LoopVectorizationAndUnrolling => vectorize::run(editor, profile),
        LirPass::LowerAllocationAndBarrierFastPaths => barriers::run(editor),
        LirPass::LowerTargetAbi => abi::lower_target_abi(editor),
        LirPass::LegalizeX86_64 => abi::legalize_x86_64(editor),
        LirPass::ClassifyPollFreeLeafAndPlaceBudgetedPolls | LirPass::LowerPollFastPaths => {
            Err(invalid("poll pass 必须在 world 级运行"))
        }
        LirPass::PrepareRegisterAllocation => Ok(false),
    }
}

/// 对全部函数体运行固定管线；每个 pass 后立即运行结构 verifier。
pub(crate) fn optimize_world(
    bodies: &mut Vec<Body>,
    module: &hir::Module,
    profile: &BackendCostProfile,
) -> Result<(), Vec<Diagnostic>> {
    let mut optimized = Vec::with_capacity(bodies.len());
    for body in bodies.drain(..) {
        let mut current = body;
        for pass in LIR_PASS_ORDER {
            if matches!(
                pass,
                LirPass::ClassifyPollFreeLeafAndPlaceBudgetedPolls | LirPass::LowerPollFastPaths
            ) {
                continue;
            }
            let mut editor = Editor::new(current);
            let _changed = run_pass(*pass, &mut editor, profile)
                .map_err(|error| vec![pack(pass.name(), error)])?;
            current = editor
                .finish()
                .map_err(|error| vec![pack(pass.name(), error)])?;
            super::verify::verify_structure(&current, module)
                .map_err(|error| vec![pack(pass.name(), error)])?;
        }
        optimized.push(current);
    }
    *bodies = optimized;
    poll::run_world(bodies, module, profile).map_err(|error| vec![error])?;
    Ok(())
}

fn pack(pass: &str, error: Diagnostic) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::LirInvariant,
        format!("LIR pass {pass} 之后不变量被破坏：{}", error.message()),
        None,
    )
}
