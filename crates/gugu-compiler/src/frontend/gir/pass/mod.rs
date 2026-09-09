//! GIR 固定优化管线：pass 顺序、共享可变视图与唯一驱动入口。
//!
//! 顺序在编译期固定；每跑一个 pass 立即运行 `ConcreteBody::verify` 与
//! `gir::verify`，失败包成 `GirInvariant`。
pub(crate) mod bounds;
pub(crate) mod cfg;
pub(crate) mod constants;
pub(crate) mod cow;
pub(crate) mod gvn;
pub(crate) mod inline;

use super::body::{BlockId, GirBlock, GirBody, LocalId, Statement, StatementKind, Terminator};
use super::gir_error;
use crate::Diagnostic;
use crate::frontend::analysis::AnalysisWorldV1;
use crate::frontend::hir;
use std::collections::BTreeSet;

/// 固定 GIR pass；枚举顺序即执行顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GirPass {
    Inline,
    SimplifyCfg,
    SparseConditionalConstants,
    CopyPropagationAndGvn,
    BoundsCheckElimination,
    CowAndResourceElision,
}

/// 固定管线顺序；禁止运行时重排。
pub(crate) const GIR_PASS_ORDER: &[GirPass] = &[
    GirPass::Inline,
    GirPass::SimplifyCfg,
    GirPass::SparseConditionalConstants,
    GirPass::CopyPropagationAndGvn,
    GirPass::BoundsCheckElimination,
    GirPass::CowAndResourceElision,
];

/// 管线 revision，进入 action key。
pub(crate) const GIR_PIPELINE_REVISION: u32 = 1;

impl GirPass {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Inline => "Inline",
            Self::SimplifyCfg => "SimplifyCfg",
            Self::SparseConditionalConstants => "SparseConditionalConstants",
            Self::CopyPropagationAndGvn => "CopyPropagationAndGvn",
            Self::BoundsCheckElimination => "BoundsCheckElimination",
            Self::CowAndResourceElision => "CowAndResourceElision",
        }
    }
}

/// 管线统计，供 dump 与 ImagePlan 消费。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GirPassStats {
    pub(crate) passes: u32,
    pub(crate) inlined: u32,
    pub(crate) checks_elided: u32,
}

/// 进入 action key 的 GIR 优化策略规范字节。
pub(crate) fn policy_bytes() -> Vec<u8> {
    let names: Vec<&str> = GIR_PASS_ORDER.iter().map(|pass| pass.name()).collect();
    serde_json::to_vec(&(GIR_PIPELINE_REVISION, names)).expect("GIR 管线策略可序列化")
}

/// 对全部具体 GIR 函数体运行固定管线。
pub(crate) fn run(
    world: &mut super::GirWorldV1,
    module: &hir::Module,
    analysis: &AnalysisWorldV1,
) -> Result<GirPassStats, Vec<Diagnostic>> {
    let mut stats = GirPassStats::default();
    for pass in GIR_PASS_ORDER {
        let callees = (*pass == GirPass::Inline).then(|| {
            world
                .concrete
                .iter()
                .map(|concrete| (concrete.instance, concrete.body.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        for concrete in world.concrete.iter_mut() {
            let mut editor = Editor::new(concrete.body.clone());
            let outcome = match pass {
                GirPass::Inline => inline::run(
                    &mut editor,
                    module,
                    callees.as_ref().expect("Inline 需要具体体表"),
                    &concrete.calls,
                ),
                GirPass::SimplifyCfg => cfg::simplify(&mut editor),
                GirPass::SparseConditionalConstants => constants::propagate(&mut editor),
                GirPass::CopyPropagationAndGvn => gvn::run(&mut editor),
                GirPass::BoundsCheckElimination => bounds::run(
                    &mut editor,
                    module,
                    analysis,
                    &world.bodies,
                    concrete.generic_body,
                ),
                GirPass::CowAndResourceElision => cow::run(&mut editor),
            };
            let outcome = outcome.map_err(|error| vec![pack(pass.name(), error)])?;
            stats.inlined += outcome.inlined;
            stats.checks_elided += outcome.checks_elided;
            concrete.body = editor
                .finish()
                .map_err(|error| vec![pack(pass.name(), error)])?;
            concrete.fingerprint = concrete.fingerprint();
            super::verify(module, &concrete.body)
                .map_err(|error| vec![pack(pass.name(), error)])?;
            concrete
                .verify()
                .map_err(|error| vec![pack(pass.name(), error)])?;
        }
        stats.passes += 1;
    }
    world.fingerprint = super::world_fingerprint(
        &world.bodies,
        &world.fragments,
        world.hir_fingerprint,
        &world.placement,
        &world.concrete,
    );
    Ok(stats)
}

/// 单个 pass 的结果。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) changed: bool,
    pub(crate) inlined: u32,
    pub(crate) checks_elided: u32,
}

fn pack(pass: &str, error: Diagnostic) -> Diagnostic {
    gir_error(
        &format!("GIR pass {pass} 之后不变量被破坏：{}", error.message()),
        None,
    )
}

/// 编辑器内的 block：显式语句列表，便于合并与删除。
pub(crate) struct GBlock {
    pub(crate) statements: Vec<usize>,
    pub(crate) terminator: Terminator,
    pub(crate) source: super::body::SourceInfo,
    pub(crate) cleanup: bool,
}

/// 可变 GIR 函数体视图；所有 pass 共享。
pub(crate) struct Editor {
    body: GirBody,
    blocks: Vec<Option<GBlock>>,
}

impl Editor {
    pub(crate) fn new(body: GirBody) -> Self {
        let blocks = body
            .blocks
            .iter()
            .map(|block| {
                Some(GBlock {
                    statements: block
                        .statements
                        .clone()
                        .map(|index| index as usize)
                        .collect(),
                    terminator: block.terminator.clone(),
                    source: block.source.clone(),
                    cleanup: block.cleanup,
                })
            })
            .collect();
        Self { body, blocks }
    }

    pub(crate) fn body(&self) -> &GirBody {
        &self.body
    }

    pub(crate) fn block(&self, block: BlockId) -> Option<&GBlock> {
        self.blocks.get(block.index()).and_then(Option::as_ref)
    }

    pub(crate) fn block_mut(&mut self, block: BlockId) -> Option<&mut GBlock> {
        self.blocks.get_mut(block.index()).and_then(Option::as_mut)
    }

    pub(crate) fn terminator(&self, block: BlockId) -> &Terminator {
        &self.block(block).expect("活跃 block").terminator
    }

    pub(crate) fn set_terminator(&mut self, block: BlockId, terminator: Terminator) {
        self.block_mut(block).expect("活跃 block").terminator = terminator;
    }

    /// 该 block 的语句 `(全局下标, 语句)`。
    pub(crate) fn statements(&self, block: BlockId) -> Vec<(usize, &Statement)> {
        self.block(block)
            .expect("活跃 block")
            .statements
            .iter()
            .map(|index| (*index, &self.body.statements[*index]))
            .collect()
    }

    pub(crate) fn statement(&self, index: usize) -> &Statement {
        &self.body.statements[index]
    }

    pub(crate) fn set_statement(&mut self, index: usize, kind: StatementKind) {
        self.body.statements[index].kind = kind;
    }

    /// 追加一个局部并返回其编号。
    pub(crate) fn add_local(&mut self, local: super::body::GirLocal) -> LocalId {
        let id = LocalId(crate::frontend::gir::concrete::id(self.body.locals.len()));
        self.body.locals.push(local);
        id
    }

    /// 追加一个常量并返回其编号。
    pub(crate) fn add_constant(&mut self, constant: super::body::Constant) -> super::body::ConstId {
        let id = super::body::ConstId(crate::frontend::gir::concrete::id(
            self.body.constants.len(),
        ));
        self.body.constants.push(constant);
        id
    }

    /// 追加一条语句到 `block` 末尾并返回其全局下标。
    pub(crate) fn push_statement(&mut self, block: BlockId, statement: Statement) -> usize {
        let index = self.body.statements.len();
        self.body.statements.push(statement);
        if let Some(target) = self.block_mut(block) {
            target.statements.push(index);
        }
        index
    }

    /// 从 `block` 删除一条语句。
    pub(crate) fn remove_statement(&mut self, block: BlockId, index: usize) {
        if let Some(block) = self.block_mut(block) {
            block.statements.retain(|candidate| *candidate != index);
        }
    }

    /// 把 `indices` 追加到 `block` 末尾。
    pub(crate) fn append_statements(&mut self, block: BlockId, indices: &[usize]) {
        if let Some(target) = self.block_mut(block) {
            target.statements.extend_from_slice(indices);
        }
    }

    pub(crate) fn live_blocks(&self) -> Vec<BlockId> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.is_some())
            .map(|(index, _)| BlockId(crate::frontend::gir::concrete::id(index)))
            .collect()
    }

    pub(crate) fn remove_block(&mut self, block: BlockId) {
        self.blocks[block.index()] = None;
    }

    fn reachable_from(&self, seeds: &[BlockId]) -> Vec<bool> {
        let mut seen = vec![false; self.blocks.len()];
        let mut stack: Vec<BlockId> = seeds.to_vec();
        while let Some(block) = stack.pop() {
            if std::mem::replace(&mut seen[block.index()], true) {
                continue;
            }
            if let Some(data) = self.block(block) {
                stack.extend(data.terminator.successors());
            }
        }
        seen
    }

    /// cleanup 区域与出口记录引用的 block 不能被删除或合并。
    pub(crate) fn protected_blocks(&self) -> BTreeSet<BlockId> {
        let mut protected = BTreeSet::new();
        for region in &self.body.cleanup_regions {
            protected.insert(region.entry);
            protected.insert(region.exit);
        }
        for record in &self.body.exit_records {
            protected.insert(record.entry);
            if let Some(destination) = record.destination {
                protected.insert(destination);
            }
        }
        for (block, _) in &self.body.match_leaves {
            protected.insert(*block);
        }
        protected
    }

    pub(crate) fn remove_unreachable(&mut self) -> bool {
        let protected = self.protected_blocks();
        let mut seeds = vec![self.body.entry];
        seeds.extend(protected.iter().copied());
        let reachable = self.reachable_from(&seeds);
        let mut changed = false;
        for (index, reachable) in reachable.iter().enumerate() {
            let block = BlockId(crate::frontend::gir::concrete::id(index));
            if !reachable && !protected.contains(&block) && self.blocks[index].is_some() {
                self.blocks[index] = None;
                changed = true;
            }
        }
        changed
    }

    /// 重建稠密 arena 并返回新的函数体。
    pub(crate) fn finish(self) -> Result<GirBody, Diagnostic> {
        let Editor { body, blocks } = self;
        let mut block_map = vec![None; blocks.len()];
        let mut kept = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.is_some() {
                block_map[index] = Some(BlockId(crate::frontend::gir::concrete::id(kept.len())));
                kept.push(index);
            }
        }
        let entry = block_map[body.entry.index()]
            .ok_or_else(|| gir_error("GIR 入口 block 被删除", None))?;
        let mut statements = Vec::new();
        let mut out_blocks = Vec::with_capacity(kept.len());
        for old in &kept {
            let block = blocks[*old].as_ref().expect("活跃 block");
            let start = crate::frontend::gir::concrete::id(statements.len());
            for index in &block.statements {
                statements.push(body.statements[*index].clone());
            }
            let end = crate::frontend::gir::concrete::id(statements.len());
            let terminator = remap_terminator(&block.terminator, &block_map).map_err(|error| {
                gir_error(
                    &format!("block {old} 的终结符引用非法：{}", error.message()),
                    None,
                )
            })?;
            out_blocks.push(GirBlock {
                statements: start..end,
                terminator,
                source: block.source.clone(),
                predecessors: 0..0,
                cleanup: block.cleanup,
            });
        }
        let mut incoming: Vec<Vec<BlockId>> = vec![Vec::new(); out_blocks.len()];
        for (index, block) in out_blocks.iter().enumerate() {
            let from = BlockId(crate::frontend::gir::concrete::id(index));
            for successor in block.terminator.successors() {
                incoming[successor.index()].push(from);
            }
        }
        let mut predecessors = Vec::new();
        for (index, block) in out_blocks.iter_mut().enumerate() {
            let start = crate::frontend::gir::concrete::id(predecessors.len());
            predecessors.extend(incoming[index].iter().copied());
            block.predecessors = start..crate::frontend::gir::concrete::id(predecessors.len());
        }
        let mut body = body;
        body.statements = statements;
        body.blocks = out_blocks;
        body.predecessors = predecessors;
        body.entry = entry;
        for region in &mut body.cleanup_regions {
            region.entry = remap_block(&block_map, region.entry)?;
            region.exit = remap_block(&block_map, region.exit)?;
        }
        for record in &mut body.exit_records {
            record.entry = remap_block(&block_map, record.entry)?;
            if let Some(destination) = record.destination {
                record.destination = Some(remap_block(&block_map, destination)?);
            }
        }
        for (block, _) in &mut body.match_leaves {
            *block = remap_block(&block_map, *block)?;
        }
        Ok(body)
    }
}

fn remap_block(block_map: &[Option<BlockId>], block: BlockId) -> Result<BlockId, Diagnostic> {
    block_map
        .get(block.index())
        .copied()
        .flatten()
        .ok_or_else(|| gir_error(&format!("GIR 引用了已删除的 block {}", block.0), None))
}

fn remap_terminator(
    terminator: &Terminator,
    block_map: &[Option<BlockId>],
) -> Result<Terminator, Diagnostic> {
    Ok(match terminator {
        Terminator::Goto { target } => Terminator::Goto {
            target: remap_block(block_map, *target)?,
        },
        Terminator::SwitchInt {
            value,
            targets,
            otherwise,
        } => Terminator::SwitchInt {
            value: value.clone(),
            targets: targets
                .iter()
                .map(|(case, block)| Ok((*case, remap_block(block_map, *block)?)))
                .collect::<Result<_, Diagnostic>>()?,
            otherwise: remap_block(block_map, *otherwise)?,
        },
        Terminator::Call {
            callee,
            args,
            destination,
            normal,
            unwind,
            call_kind,
            site,
        } => Terminator::Call {
            callee: callee.clone(),
            args: args.clone(),
            destination: *destination,
            normal: remap_block(block_map, *normal)?,
            unwind: unwind
                .map(|block| remap_block(block_map, block))
                .transpose()?,
            call_kind: *call_kind,
            site: site.clone(),
        },
        Terminator::Return => Terminator::Return,
        Terminator::Panic { payload, unwind } => Terminator::Panic {
            payload: payload.clone(),
            unwind: remap_block(block_map, *unwind)?,
        },
        Terminator::ResumePanic => Terminator::ResumePanic,
        Terminator::Abort => Terminator::Abort,
        Terminator::Unreachable => Terminator::Unreachable,
        Terminator::Suspend {
            reason,
            destination,
            resume,
            cancelled,
            safepoint,
        } => Terminator::Suspend {
            reason: reason.clone(),
            destination: *destination,
            resume: remap_block(block_map, *resume)?,
            cancelled: cancelled
                .map(|block| remap_block(block_map, block))
                .transpose()?,
            safepoint: *safepoint,
        },
        Terminator::SelectCommit {
            cases,
            index,
            ready,
            suspend,
            cancelled,
            safepoint,
        } => Terminator::SelectCommit {
            cases: cases.clone(),
            index: *index,
            ready: remap_block(block_map, *ready)?,
            suspend: suspend
                .map(|block| remap_block(block_map, block))
                .transpose()?,
            cancelled: cancelled
                .map(|block| remap_block(block_map, block))
                .transpose()?,
            safepoint: *safepoint,
        },
    })
}

/// 语句定义的局部。
pub(crate) fn statement_defined(statement: &Statement) -> Option<LocalId> {
    match &statement.kind {
        StatementKind::Assign(place, _) if place.is_local() => Some(place.local),
        StatementKind::SetDiscriminant { place, .. } if place.is_local() => Some(place.local),
        _ => None,
    }
}

/// 语句读取的局部。
pub(crate) fn statement_reads(statement: &Statement, out: &mut BTreeSet<LocalId>) {
    match &statement.kind {
        StatementKind::Assign(_, rvalue) => rvalue_reads(rvalue, out),
        StatementKind::SetDiscriminant { .. } => {}
        StatementKind::ValueAction { place, .. } | StatementKind::ResourceAction { place, .. } => {
            out.insert(place.local);
        }
        StatementKind::GcWrite {
            owner,
            destination,
            value,
        } => {
            out.insert(owner.local);
            out.insert(destination.local);
            operand_reads(value, out);
        }
        StatementKind::Pin { place, .. } => {
            out.insert(place.local);
        }
        StatementKind::ScopedViewBegin { source, .. } => {
            out.insert(source.local);
        }
        StatementKind::Atomic {
            pointer,
            operands,
            destination,
            ..
        } => {
            if let Some(pointer) = pointer {
                operand_reads(pointer, out);
            }
            for operand in operands {
                operand_reads(operand, out);
            }
            if let Some(destination) = destination {
                out.insert(destination.local);
            }
        }
        StatementKind::Volatile {
            pointer,
            value,
            destination,
            ..
        } => {
            operand_reads(pointer, out);
            if let Some(value) = value {
                operand_reads(value, out);
            }
            if let Some(destination) = destination {
                out.insert(destination.local);
            }
        }
        _ => {}
    }
}

fn operand_reads(operand: &super::body::Operand, out: &mut BTreeSet<LocalId>) {
    if let super::body::Operand::Copy(place) | super::body::Operand::MoveInternal(place) = operand {
        out.insert(place.local);
    }
}

fn rvalue_reads(rvalue: &super::body::Rvalue, out: &mut BTreeSet<LocalId>) {
    use super::body::Rvalue;
    let place = |place: &super::body::Place, out: &mut BTreeSet<LocalId>| {
        out.insert(place.local);
    };
    match rvalue {
        Rvalue::Use(operand) => operand_reads(operand, out),
        Rvalue::UnaryOp { operand, .. } => operand_reads(operand, out),
        Rvalue::BinaryOp { left, right, .. } | Rvalue::Compare { left, right, .. } => {
            operand_reads(left, out);
            operand_reads(right, out);
        }
        Rvalue::CheckedOp { operands, .. } => {
            for operand in operands {
                operand_reads(operand, out);
            }
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::AllocObject { operands, .. } => {
            for operand in operands {
                operand_reads(operand, out);
            }
        }
        Rvalue::Repeat { operand, .. } => operand_reads(operand, out),
        Rvalue::Discriminant(target)
        | Rvalue::Len(target)
        | Rvalue::Ref(target)
        | Rvalue::RawAddress(target)
        | Rvalue::ValueCopy(target)
        | Rvalue::CowSnapshot(target) => place(target, out),
        Rvalue::Cast { operand, .. } | Rvalue::DynErase { operand, .. } => {
            operand_reads(operand, out)
        }
        Rvalue::AllocArray { length, .. } => operand_reads(length, out),
        Rvalue::StackSlotAddress(_) | Rvalue::FunctionValue(_) => {}
        Rvalue::Intrinsic { operands, .. } => {
            for operand in operands {
                operand_reads(operand, out);
            }
        }
    }
}
