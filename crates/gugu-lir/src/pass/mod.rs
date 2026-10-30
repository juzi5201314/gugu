//! LIR 固定优化管线：pass 顺序、共享图算法与唯一重写入口。
//!
//! 顺序在编译期固定；驱动每跑一个 pass 立即重建 arena 并运行结构 verifier。
pub mod abi;
pub mod barriers;
pub mod cfg;
pub mod constants;
pub mod dce;
pub mod graph;
pub mod gvn;
pub mod gvn_key;
pub mod licm;
pub mod loops;
pub mod policy;
pub mod poll;
pub mod rewrite;
pub mod strength;
pub mod vectorize;
pub mod versioning;

use super::body::Body;
use super::invalid;
use crate::hir;
use crate::target::TargetOracle;
use crate::{BackendCostProfile, Diagnostic, DiagnosticCode};
use rewrite::Editor;

/// 固定 LIR pass；枚举顺序即执行顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LirPass {
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
pub const LIR_PASS_ORDER: &[LirPass] = &[
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
    pub fn name(self) -> &'static str {
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
    target: &dyn TargetOracle,
) -> Result<bool, Diagnostic> {
    match pass {
        LirPass::VerifySsaAndMemory | LirPass::PrepareRegisterAllocation => {
            Err(invalid("结构验证阶段不进入改写编辑器"))
        }
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
        LirPass::LegalizeX86_64 => abi::legalize_x86_64(editor, target),
        LirPass::ClassifyPollFreeLeafAndPlaceBudgetedPolls | LirPass::LowerPollFastPaths => {
            Err(invalid("poll pass 必须在 world 级运行"))
        }
    }
}

/// 对全部函数体运行固定管线；每个 pass 后立即运行结构 verifier。
pub fn optimize_world(
    bodies: &mut Vec<Body>,
    module: &hir::Module,
    profile: &BackendCostProfile,
    target: &dyn TargetOracle,
) -> Result<(), Vec<Diagnostic>> {
    let mut optimized = Vec::with_capacity(bodies.len());
    for body in bodies.drain(..) {
        super::verify::verify_structure(&body, module)
            .map_err(|error| vec![pack(LirPass::VerifySsaAndMemory.name(), error)])?;
        let mut editor = Editor::new(body);
        for pass in LIR_PASS_ORDER {
            if matches!(
                pass,
                LirPass::VerifySsaAndMemory | LirPass::PrepareRegisterAllocation
            ) {
                continue;
            }
            if matches!(
                pass,
                LirPass::ClassifyPollFreeLeafAndPlaceBudgetedPolls | LirPass::LowerPollFastPaths
            ) {
                continue;
            }
            // no-op pass 沿用工作图；只有变化时才物化并重新验证。
            let changed = run_pass(*pass, &mut editor, profile, target)
                .map_err(|error| vec![pack(pass.name(), error)])?;
            if changed {
                let current = editor
                    .finish()
                    .map_err(|error| vec![pack(pass.name(), error)])?;
                super::verify::verify_structure(&current, module)
                    .map_err(|error| vec![pack(pass.name(), error)])?;
                editor = Editor::new(current);
            }
        }
        let current = editor.unchanged();
        super::verify::verify_structure(&current, module)
            .map_err(|error| vec![pack(LirPass::PrepareRegisterAllocation.name(), error)])?;
        optimized.push(current);
    }
    *bodies = optimized;
    poll::run_world(bodies, module, target).map_err(|error| vec![error])?;
    Ok(())
}

fn pack(pass: &str, error: Diagnostic) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::LirInvariant,
        format!("LIR pass {pass} 之后不变量被破坏：{}", error.message()),
        None,
    )
}
