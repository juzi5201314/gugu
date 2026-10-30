//! GIR 固定优化管线：pass 顺序、共享可变视图与唯一驱动入口。
//!
//! 顺序在编译期固定；每跑一个 pass 立即运行 `ConcreteBody::verify` 与
//! `gir::verify`，失败包成 `GirInvariant`。
pub mod bounds;
pub mod cfg;
pub mod constants;
pub mod cow;
pub mod gvn;
pub mod inline;

use super::body::{BlockId, GirBlock, GirBody, LocalId, Statement, StatementKind, Terminator};
use super::gir_error;
use crate::Diagnostic;
use crate::analysis::AnalysisWorldV1;
use crate::hir;
use std::collections::BTreeSet;

/// 固定 GIR pass；枚举顺序即执行顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GirPass {
    Inline,
    SimplifyCfg,
    SparseConditionalConstants,
    CopyPropagationAndGvn,
    BoundsCheckElimination,
    CowAndResourceElision,
}

/// 固定管线顺序；禁止运行时重排。
pub const GIR_PASS_ORDER: &[GirPass] = &[
    GirPass::Inline,
    GirPass::SimplifyCfg,
    GirPass::SparseConditionalConstants,
    GirPass::CopyPropagationAndGvn,
    GirPass::BoundsCheckElimination,
    GirPass::CowAndResourceElision,
];

/// 管线 revision，进入 action key。
pub const GIR_PIPELINE_REVISION: u32 = 2;

impl GirPass {
    pub fn name(self) -> &'static str {
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
pub struct GirPassStats {
    pub passes: u32,
    pub inlined: u32,
    pub checks_elided: u32,
}

pub fn policy_bytes() -> Vec<u8> {
    crate::canonical::to_vec(&(
        GIR_PIPELINE_REVISION,
        GIR_PASS_ORDER
            .iter()
            .map(|pass| pass.name())
            .collect::<Vec<_>>(),
    ))
    .expect("GIR 管线策略编码")
}

/// 对全部具体 GIR 函数体运行固定管线。
pub fn run(
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
                .map(|concrete| (concrete.instance, &concrete.body))
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        let mut updates = Vec::new();
        for (index, concrete) in world.concrete.iter().enumerate() {
            let mut editor = Editor::borrowed(&concrete.body);
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
            }
            .map_err(|error| vec![pack(pass.name(), error)])?;
            stats.inlined += outcome.inlined;
            stats.checks_elided += outcome.checks_elided;
            if outcome.changed {
                let body = editor
                    .finish()
                    .map_err(|error| vec![pack(pass.name(), error)])?;
                super::verify(module, &body).map_err(|error| vec![pack(pass.name(), error)])?;
                updates.push((index, body));
            }
        }
        for (index, body) in updates {
            let concrete = &mut world.concrete[index];
            concrete.body = body;
            concrete
                .refresh_fingerprint()
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
pub struct Outcome {
    pub changed: bool,
    pub inlined: u32,
    pub checks_elided: u32,
}

fn pack(pass: &str, error: Diagnostic) -> Diagnostic {
    gir_error(
        &format!("GIR pass {pass} 之后不变量被破坏：{}", error.message()),
        None,
    )
}

/// 编辑器内的 block：显式语句列表，便于合并与删除。
pub struct GBlock {
    pub statements: Vec<usize>,
    pub terminator: Terminator,
    pub source: super::body::SourceInfo,
    pub cleanup: bool,
}

/// 可变 GIR 函数体视图；所有 pass 共享。
pub struct Editor<'body> {
    body: std::borrow::Cow<'body, GirBody>,
    blocks: Vec<Option<GBlock>>,
}

impl<'body> Editor<'body> {
    fn borrowed(body: &'body GirBody) -> Self {
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
        Self {
            body: std::borrow::Cow::Borrowed(body),
            blocks,
        }
    }

    pub fn body(&self) -> &GirBody {
        &self.body
    }

    pub fn block(&self, block: BlockId) -> Option<&GBlock> {
        self.blocks.get(block.index()).and_then(Option::as_ref)
    }

    pub fn block_mut(&mut self, block: BlockId) -> Option<&mut GBlock> {
        self.blocks.get_mut(block.index()).and_then(Option::as_mut)
    }

    pub fn terminator(&self, block: BlockId) -> &Terminator {
        &self.block(block).expect("活跃 block").terminator
    }

    pub fn set_terminator(&mut self, block: BlockId, terminator: Terminator) {
        self.block_mut(block).expect("活跃 block").terminator = terminator;
    }

    /// 该 block 的语句 `(全局下标, 语句)`。
    pub fn statements(&self, block: BlockId) -> Vec<(usize, &Statement)> {
        self.block(block)
            .expect("活跃 block")
            .statements
            .iter()
            .map(|index| (*index, &self.body.statements[*index]))
            .collect()
    }

    pub fn statement(&self, index: usize) -> &Statement {
        &self.body.statements[index]
    }

    pub fn set_statement(&mut self, index: usize, kind: StatementKind) {
        self.body.to_mut().statements[index].kind = kind;
    }

    /// 追加一个局部并返回其编号。
    pub fn add_local(&mut self, local: super::body::GirLocal) -> LocalId {
        let id = LocalId(crate::gir::concrete::id(self.body.locals.len()));
        self.body.to_mut().locals.push(local);
        id
    }

    /// 追加一个常量并返回其编号。
    pub fn add_constant(&mut self, constant: super::body::Constant) -> super::body::ConstId {
        let id = super::body::ConstId(crate::gir::concrete::id(self.body.constants.len()));
        self.body.to_mut().constants.push(constant);
        id
    }

    /// 追加一条语句到 `block` 末尾并返回其全局下标。
    pub fn push_statement(&mut self, block: BlockId, statement: Statement) -> usize {
        let index = self.body.statements.len();
        self.body.to_mut().statements.push(statement);
        if let Some(target) = self.block_mut(block) {
            target.statements.push(index);
        }
        index
    }

    /// 从 `block` 删除一条语句。
    pub fn remove_statement(&mut self, block: BlockId, index: usize) {
        if let Some(block) = self.block_mut(block) {
            block.statements.retain(|candidate| *candidate != index);
        }
    }

    /// 把 `indices` 追加到 `block` 末尾。
    pub fn append_statements(&mut self, block: BlockId, indices: &[usize]) {
        if let Some(target) = self.block_mut(block) {
            target.statements.extend_from_slice(indices);
        }
    }

    pub fn live_blocks(&self) -> Vec<BlockId> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.is_some())
            .map(|(index, _)| BlockId(crate::gir::concrete::id(index)))
            .collect()
    }

    pub fn remove_block(&mut self, block: BlockId) {
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
    pub fn protected_blocks(&self) -> BTreeSet<BlockId> {
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

    pub fn remove_unreachable(&mut self) -> bool {
        let protected = self.protected_blocks();
        let mut seeds = vec![self.body.entry];
        seeds.extend(protected.iter().copied());
        let reachable = self.reachable_from(&seeds);
        let mut changed = false;
        for (index, reachable) in reachable.iter().enumerate() {
            let block = BlockId(crate::gir::concrete::id(index));
            if !reachable && !protected.contains(&block) && self.blocks[index].is_some() {
                self.blocks[index] = None;
                changed = true;
            }
        }
        changed
    }

    /// 重建稠密 arena 并返回新的函数体。
    pub fn finish(self) -> Result<GirBody, Diagnostic> {
        let Editor { body, blocks } = self;
        let body = body.into_owned();
        let mut block_map = vec![None; blocks.len()];
        let mut kept = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.is_some() {
                block_map[index] = Some(BlockId(crate::gir::concrete::id(kept.len())));
                kept.push(index);
            }
        }
        let entry = block_map[body.entry.index()]
            .ok_or_else(|| gir_error("GIR 入口 block 被删除", None))?;
        let mut statements = Vec::new();
        let mut out_blocks = Vec::with_capacity(kept.len());
        for old in &kept {
            let block = blocks[*old].as_ref().expect("活跃 block");
            let start = crate::gir::concrete::id(statements.len());
            for index in &block.statements {
                statements.push(body.statements[*index].clone());
            }
            let end = crate::gir::concrete::id(statements.len());
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
            let from = BlockId(crate::gir::concrete::id(index));
            for successor in block.terminator.successors() {
                incoming[successor.index()].push(from);
            }
        }
        let mut predecessors = Vec::new();
        for (index, block) in out_blocks.iter_mut().enumerate() {
            let start = crate::gir::concrete::id(predecessors.len());
            predecessors.extend(incoming[index].iter().copied());
            block.predecessors = start..crate::gir::concrete::id(predecessors.len());
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
            site: *site,
        },
        Terminator::Return => Terminator::Return,
        Terminator::Panic {
            payload,
            location,
            unwind,
        } => Terminator::Panic {
            payload: payload.clone(),
            location: location.clone(),
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
pub fn statement_defined(statement: &Statement) -> Option<LocalId> {
    match &statement.kind {
        StatementKind::Assign(place, _) if place.is_local() => Some(place.local),
        StatementKind::SetDiscriminant { place, .. } if place.is_local() => Some(place.local),
        StatementKind::PlatformCall {
            destination: Some(place),
            ..
        } if place.is_local() => Some(place.local),
        StatementKind::RuntimeMachineCall {
            destination: Some(place),
            ..
        } if place.is_local() => Some(place.local),
        _ => None,
    }
}

/// 语句读取的局部。
/// 稠密 `LocalId` 的位图集合。
///
/// `LocalId` 从 0 开始稠密分配、单个 body 的 local 数有上界，因此用按机器字组织的位图代替
/// `BTreeSet`：插入/查询是常数次位运算，并集/差集/比较按 `u64` 字批量处理，且不再逐元素分配
/// 树节点。位图按需增长，容量按最高出现的 local 下标对齐到字。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalSet {
    words: Vec<u64>,
}

impl LocalSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn grow(&mut self, index: usize) {
        if self.words.len() * 64 <= index {
            self.words.resize(index / 64 + 1, 0);
        }
    }

    pub fn insert(&mut self, local: super::body::LocalId) {
        let index = local.index();
        self.grow(index);
        self.words[index / 64] |= 1 << (index % 64);
    }

    pub fn contains(&self, local: super::body::LocalId) -> bool {
        let index = local.index();
        self.words
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    pub fn remove(&mut self, local: super::body::LocalId) {
        let index = local.index();
        if let Some(word) = self.words.get_mut(index / 64) {
            *word &= !(1 << (index % 64));
        }
    }

    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// 并入 `other`，返回是否有变化。
    pub fn union_with(&mut self, other: &Self) -> bool {
        if self.words.len() < other.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        let mut changed = false;
        for (word, &other) in self.words.iter_mut().zip(&other.words) {
            let merged = *word | other;
            changed |= merged != *word;
            *word = merged;
        }
        changed
    }

    /// 从自身删掉 `other` 里的位。
    pub fn subtract(&mut self, other: &Self) {
        for (word, &other) in self.words.iter_mut().zip(&other.words) {
            *word &= !other;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = super::body::LocalId> + '_ {
        self.words.iter().enumerate().flat_map(|(index, &word)| {
            let base = index * 64;
            (0..64).filter_map(move |bit| {
                (word >> bit & 1 == 1).then(|| {
                    super::body::LocalId(
                        u32::try_from(base + bit).expect("local 下标已由 HIR 验证"),
                    )
                })
            })
        })
    }
}

pub fn statement_reads(statement: &Statement, out: &mut LocalSet) {
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
        StatementKind::PlatformCall {
            operands,
            destination,
            ..
        } => {
            for operand in operands {
                operand_reads(operand, out);
            }
            if let Some(destination) = destination {
                out.insert(destination.local);
            }
        }
        StatementKind::RuntimeMachineCall {
            operands,
            destination,
            ..
        } => {
            for operand in operands {
                operand_reads(operand, out);
            }
            if let Some(destination) = destination {
                out.insert(destination.local);
            }
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

fn operand_reads(operand: &super::body::Operand, out: &mut LocalSet) {
    if let super::body::Operand::Copy(place) | super::body::Operand::MoveInternal(place) = operand {
        out.insert(place.local);
    }
}

fn rvalue_reads(rvalue: &super::body::Rvalue, out: &mut LocalSet) {
    use super::body::Rvalue;
    let place = |place: &super::body::Place, out: &mut LocalSet| {
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
