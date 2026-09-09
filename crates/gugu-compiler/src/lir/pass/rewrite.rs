//! 所有 CFG/SSA 改写的唯一入口。
//!
//! pass 只通过 `Editor` 修改函数体；`finish` 重建稠密 arena，维护
//! Mem 链连续、block 参数规范排序与 use 链不变量，随后由驱动运行结构 verifier。
use super::super::body::{
    Block, BlockId, Body, Call, Definition, Edge, EdgeId, InstId, Instruction, Lifetime, Memory,
    Op, Origin, Parameter, PermitId, PollSummary, Safepoint, SafepointId, SafepointKind,
    Terminator, Value, ValueId, ValueType, id, range,
};
use super::super::invalid;
use crate::Diagnostic;
use crate::frontend::gir::body::{NoSafepointReason, SourceInfo, SourceScope};
use crate::frontend::hir::Assembly;
use std::ops::Range;

/// 一条可寻址指令；`index` 在 `finish` 前稳定，删除只打墓碑。
pub(crate) type InstRef = (BlockId, usize);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MValue {
    pub(crate) kind: ValueType,
    pub(crate) origin: Origin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MParam {
    pub(crate) value: ValueId,
    pub(crate) source: Option<(u32, u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MInst {
    pub(crate) op: Op,
    pub(crate) arguments: Vec<ValueId>,
    pub(crate) results: Vec<ValueId>,
    pub(crate) memory: Option<Memory>,
    pub(crate) safepoint: Option<SafepointKind>,
    pub(crate) source: SourceInfo,
    /// 输入 body 的原始 `InstId`；屏障的 `store` 元数据依赖该映射。
    pub(crate) origin: Option<InstId>,
    pub(crate) removed: bool,
}

/// 编辑器内的终结符；边与 switch case 已展开为显式列表。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Term {
    Jump(EdgeId),
    Branch {
        condition: ValueId,
        yes: EdgeId,
        no: EdgeId,
    },
    Switch {
        value: ValueId,
        cases: Vec<(u64, EdgeId)>,
        otherwise: EdgeId,
    },
    Invoke {
        call: Call,
        arguments: Vec<ValueId>,
        results: Vec<ValueId>,
        memory: Memory,
        normal: EdgeId,
        unwind: EdgeId,
        safepoint: Option<SafepointKind>,
    },
    Return {
        values: Vec<ValueId>,
        memory: ValueId,
    },
    ResumePanic {
        memory: ValueId,
    },
    TailCall {
        call: Call,
        arguments: Vec<ValueId>,
        memory: ValueId,
    },
    Trap {
        memory: ValueId,
    },
    Unreachable {
        memory: ValueId,
    },
}

impl Term {
    /// 该终结符引用的边。
    pub(crate) fn edges(&self) -> Vec<EdgeId> {
        match self {
            Self::Jump(edge) => vec![*edge],
            Self::Branch { yes, no, .. } => vec![*yes, *no],
            Self::Switch {
                cases, otherwise, ..
            } => cases
                .iter()
                .map(|(_, edge)| *edge)
                .chain([*otherwise])
                .collect(),
            Self::Invoke { normal, unwind, .. } => vec![*normal, *unwind],
            _ => Vec::new(),
        }
    }
    /// 该终结符读取的值（不含由 Invoke 定义的内存输出）。
    pub(crate) fn uses(&self) -> Vec<ValueId> {
        match self {
            Self::Branch { condition, .. } => vec![*condition],
            Self::Switch { value, .. } => vec![*value],
            Self::Invoke {
                arguments, memory, ..
            } => arguments.iter().copied().chain([memory.input]).collect(),
            Self::Return { values, memory } => values.iter().copied().chain([*memory]).collect(),
            Self::ResumePanic { memory } | Self::Trap { memory } | Self::Unreachable { memory } => {
                vec![*memory]
            }
            Self::TailCall {
                arguments, memory, ..
            } => arguments.iter().copied().chain([*memory]).collect(),
            Self::Jump(_) => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MBlock {
    pub(crate) params: Vec<MParam>,
    pub(crate) insts: Vec<MInst>,
    pub(crate) term: Term,
    pub(crate) source: SourceInfo,
    pub(crate) cleanup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MEdge {
    pub(crate) from: BlockId,
    pub(crate) to: BlockId,
    pub(crate) arguments: Vec<ValueId>,
    pub(crate) unwind: bool,
}

/// 可变函数体视图；所有 pass 共享。
pub(crate) struct Editor {
    pub(crate) values: Vec<MValue>,
    pub(crate) blocks: Vec<Option<MBlock>>,
    pub(crate) edges: Vec<Option<MEdge>>,
    replaced: Vec<Option<ValueId>>,
    use_counts: Vec<u32>,
    definitions: Vec<Option<InstRef>>,
    /// 每个 block 的指令原始 `InstId`；被合并的 block 保留自己的记录，
    /// 供 `finish` 重映射栈槽寿命点。
    block_origins: Vec<Vec<Option<InstId>>>,
    instruction_count: usize,
    entry: BlockId,
    pub(crate) revision: u32,
    pub(crate) instance: [u8; 32],
    pub(crate) owner: [u8; 32],
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) signature: super::super::body::Signature,
    pub(crate) stack_slots: Vec<super::super::body::StackSlot>,
    pub(crate) lifetimes: Vec<super::super::body::Lifetime>,
    pub(crate) barrier_permits: Vec<super::super::body::BarrierPermit>,
    pub(crate) no_safepoint_regions: Vec<NoSafepointReason>,
    pub(crate) source_scopes: Vec<SourceScope>,
    pub(crate) data: Vec<super::super::body::Data>,
    pub(crate) assembly: Vec<Assembly>,
    pub(crate) environments: Vec<super::super::body::Environment>,
    pub(crate) input_fingerprint: [u8; 32],
    pub(crate) poll_summary: PollSummary,
}

impl Editor {
    /// 把已验证的函数体拆成可变视图。
    pub(crate) fn new(body: Body) -> Self {
        let instruction_count = body.instructions.len();
        let values = body
            .values
            .iter()
            .map(|value| MValue {
                kind: value.kind,
                origin: value.origin.clone(),
            })
            .collect();
        let mut blocks = Vec::with_capacity(body.blocks.len());
        let mut block_origins = Vec::with_capacity(body.blocks.len());
        for index in 0..body.blocks.len() {
            let block = &body.blocks[index];
            block_origins.push(
                range(&block.instructions)
                    .map(|at| Some(InstId(id(at))))
                    .collect(),
            );
            let params = body
                .params(BlockId(id(index)))
                .iter()
                .map(|parameter| MParam {
                    value: parameter.value,
                    source: parameter.source,
                })
                .collect();
            let insts = range(&block.instructions)
                .map(|at| {
                    let instruction = &body.instructions[at];
                    MInst {
                        op: instruction.op.clone(),
                        arguments: body.args(&instruction.arguments).to_vec(),
                        results: range(&instruction.results)
                            .map(|value| ValueId(id(value)))
                            .collect(),
                        memory: instruction.memory,
                        safepoint: instruction
                            .safepoint
                            .map(|point| body.safepoints[point.index()].kind),
                        source: instruction.source.clone(),
                        origin: Some(InstId(id(at))),
                        removed: false,
                    }
                })
                .collect();
            blocks.push(Some(MBlock {
                params,
                insts,
                term: term_of(&body, &block.terminator),
                source: block.source.clone(),
                cleanup: block.cleanup,
            }));
        }
        let edges = body
            .edges
            .iter()
            .map(|edge| {
                Some(MEdge {
                    from: edge.from,
                    to: edge.to,
                    arguments: body.args(&edge.arguments).to_vec(),
                    unwind: edge.unwind,
                })
            })
            .collect();
        let mut editor = Self {
            values,
            blocks,
            edges,
            replaced: vec![None; body.values.len()],
            use_counts: vec![0; body.values.len()],
            definitions: vec![None; body.values.len()],
            block_origins,
            instruction_count,
            entry: body.entry,
            revision: body.revision,
            instance: body.instance,
            owner: body.owner,
            name: body.name,
            target: body.target,
            signature: body.signature,
            stack_slots: body.stack_slots,
            lifetimes: body.lifetimes,
            barrier_permits: body.barrier_permits,
            no_safepoint_regions: body.no_safepoint_regions,
            source_scopes: body.source_scopes,
            data: body.data,
            assembly: body.assembly,
            environments: body.environments,
            input_fingerprint: body.input_fingerprint,
            poll_summary: body.poll_summary,
        };
        editor.recount_uses();
        editor.recount_definitions();
        editor
    }

    fn recount_definitions(&mut self) {
        self.definitions.iter_mut().for_each(|entry| *entry = None);
        for (bindex, block) in self.blocks.iter().enumerate() {
            let Some(block) = block else { continue };
            for (index, instruction) in block.insts.iter().enumerate() {
                for result in &instruction.results {
                    self.definitions[result.index()] = Some((BlockId(id(bindex)), index));
                }
            }
        }
    }

    /// 返回定义该值的指令；block 参数与 Invoke 结果返回 `None`。
    pub(crate) fn defining_instruction(&self, value: ValueId) -> Option<InstRef> {
        self.definitions[self.resolve(value).index()]
    }

    pub(crate) fn entry(&self) -> BlockId {
        self.entry
    }

    pub(crate) fn live_blocks(&self) -> Vec<BlockId> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.is_some())
            .map(|(index, _)| BlockId(id(index)))
            .collect()
    }

    pub(crate) fn block(&self, block: BlockId) -> Option<&MBlock> {
        self.blocks.get(block.index()).and_then(Option::as_ref)
    }

    pub(crate) fn terminator(&self, block: BlockId) -> &Term {
        &self.block(block).expect("活跃 block").term
    }

    pub(crate) fn instruction(&self, at: InstRef) -> &MInst {
        &self.block(at.0).expect("活跃 block").insts[at.1]
    }

    pub(crate) fn instruction_count(&self, block: BlockId) -> usize {
        self.block(block).map_or(0, |block| block.insts.len())
    }

    pub(crate) fn kind(&self, value: ValueId) -> ValueType {
        self.values[value.index()].kind
    }

    pub(crate) fn origin(&self, value: ValueId) -> &Origin {
        &self.values[value.index()].origin
    }

    pub(crate) fn edge(&self, edge: EdgeId) -> Option<&MEdge> {
        self.edges.get(edge.index()).and_then(Option::as_ref)
    }

    pub(crate) fn edges(&self) -> impl Iterator<Item = (EdgeId, &MEdge)> {
        self.edges
            .iter()
            .enumerate()
            .filter_map(|(index, edge)| edge.as_ref().map(|edge| (EdgeId(id(index)), edge)))
    }

    pub(crate) fn predecessors(&self, block: BlockId) -> Vec<EdgeId> {
        self.edges()
            .filter(|(_, edge)| edge.to == block)
            .map(|(id, _)| id)
            .collect()
    }

    pub(crate) fn successors(&self, block: BlockId) -> Vec<BlockId> {
        self.terminator(block)
            .edges()
            .into_iter()
            .filter_map(|edge| self.edge(edge).map(|edge| edge.to))
            .collect()
    }

    /// 迭代式支配树；存在不可达 block 时返回 `None`。
    pub(crate) fn dominators(&self) -> Option<Vec<BlockId>> {
        super::graph::dominators(
            self.blocks.len(),
            self.entry,
            |block| self.successors(block),
            |block| {
                self.predecessors(block)
                    .into_iter()
                    .filter_map(|edge| self.edge(edge).map(|edge| edge.from))
                    .collect()
            },
        )
    }

    /// 逆后序。
    pub(crate) fn reverse_postorder(&self) -> Vec<BlockId> {
        super::graph::reverse_postorder_from(self.blocks.len(), self.entry, |block| {
            self.successors(block)
        })
    }

    /// 返回该值（含替换链）被使用的次数。
    pub(crate) fn use_count(&self, value: ValueId) -> u32 {
        self.use_counts[self.resolve(value).index()]
    }

    pub(crate) fn is_used(&self, value: ValueId) -> bool {
        self.use_count(value) != 0
    }

    fn resolve(&self, mut value: ValueId) -> ValueId {
        for _ in 0..self.replaced.len() {
            match self.replaced[value.index()] {
                Some(next) if next != value => value = next,
                _ => return value,
            }
        }
        value
    }

    /// 把所有对 `old` 的使用改到 `new`；`new` 必须支配 `old` 的全部使用点。
    pub(crate) fn replace_value(&mut self, old: ValueId, new: ValueId) {
        let old = self.resolve(old);
        let new = self.resolve(new);
        if old == new {
            return;
        }
        debug_assert!(
            super::super::verify::compatible(
                self.values[new.index()].kind,
                self.values[old.index()].kind
            ),
            "替换值必须类型兼容"
        );
        debug_assert!(!self.chain_contains(new, old), "替换不得引入环");
        self.use_counts[new.index()] =
            self.use_counts[new.index()].saturating_add(self.use_counts[old.index()]);
        self.use_counts[old.index()] = 0;
        self.replaced[old.index()] = Some(new);
    }

    fn chain_contains(&self, mut value: ValueId, needle: ValueId) -> bool {
        for _ in 0..self.replaced.len() {
            if value == needle {
                return true;
            }
            match self.replaced[value.index()] {
                Some(next) if next != value => value = next,
                _ => return false,
            }
        }
        false
    }

    fn add_uses(&mut self, values: &[ValueId]) {
        for value in values {
            let value = self.resolve(*value);
            self.use_counts[value.index()] += 1;
        }
    }

    fn remove_uses(&mut self, values: &[ValueId]) {
        for value in values {
            let value = self.resolve(*value);
            self.use_counts[value.index()] = self.use_counts[value.index()].saturating_sub(1);
        }
    }

    fn recount_uses(&mut self) {
        self.use_counts.iter_mut().for_each(|count| *count = 0);
        let mut values = Vec::new();
        for block in self.blocks.iter().flatten() {
            for instruction in &block.insts {
                values.extend(instruction.arguments.iter().copied());
                if let Some(memory) = instruction.memory {
                    values.push(memory.input);
                }
            }
            values.extend(block.term.uses());
        }
        for edge in self.edges.iter().flatten() {
            values.extend(edge.arguments.iter().copied());
        }
        self.add_uses(&values);
    }

    /// 在 `block` 末尾追加一条无内存的纯指令。
    pub(crate) fn emit(
        &mut self,
        block: BlockId,
        op: Op,
        arguments: &[ValueId],
        results: &[(ValueType, Origin)],
    ) -> Vec<ValueId> {
        debug_assert!(!op.has_memory(), "纯指令不得携带 Mem");
        self.emit_inner(block, op, arguments, results)
    }

    fn emit_inner(
        &mut self,
        block: BlockId,
        op: Op,
        arguments: &[ValueId],
        results: &[(ValueType, Origin)],
    ) -> Vec<ValueId> {
        let safepoint = op.safepoint_kind();
        let result_values: Vec<ValueId> = results
            .iter()
            .map(|(kind, origin)| self.push_value(*kind, origin.clone()))
            .collect();
        let arguments = arguments.to_vec();
        self.add_uses(&arguments);
        let block_ref = self.blocks[block.index()].as_mut().expect("活跃 block");
        let index = block_ref.insts.len();
        block_ref.insts.push(MInst {
            op,
            arguments,
            results: result_values.clone(),
            memory: None,
            safepoint,
            source: block_ref.source.clone(),
            origin: None,
            removed: false,
        });
        for value in &result_values {
            self.definitions[value.index()] = Some((block, index));
        }
        self.block_origins[block.index()].push(None);
        result_values
    }

    fn push_value(&mut self, kind: ValueType, origin: Origin) -> ValueId {
        let value = ValueId(id(self.values.len()));
        self.values.push(MValue { kind, origin });
        self.use_counts.push(0);
        self.replaced.push(None);
        self.definitions.push(None);
        value
    }

    /// 在入口 block 末尾物化一个常量；常量与 block 状态无关，支配本 block 全部使用。
    pub(crate) fn constant(&mut self, value: u64, ty: super::super::body::Type) -> ValueId {
        self.emit(
            self.entry,
            Op::IConst(value),
            &[],
            &[(ValueType::scalar(ty), Origin::None)],
        )
        .into_iter()
        .next()
        .expect("常量产生一个结果")
    }

    /// 删除一条指令；结果必须无使用，内存指令先把 Mem 输出接到输入。
    pub(crate) fn remove_instruction(&mut self, at: InstRef) -> Result<(), Diagnostic> {
        let instruction = self
            .blocks
            .get(at.0.index())
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid("删除指令引用了已删除 block"))?
            .insts
            .get(at.1)
            .ok_or_else(|| invalid("删除指令编号越界"))?;
        if instruction.removed {
            return Ok(());
        }
        let memory = instruction.memory;
        let results = instruction.results.clone();
        let arguments = instruction.arguments.clone();
        if let Some(memory) = memory {
            self.replace_value(memory.output, memory.input);
        }
        if results
            .iter()
            .any(|value| self.use_counts[value.index()] != 0)
        {
            return Err(invalid("删除指令仍有活跃结果"));
        }
        self.remove_uses(&arguments);
        if let Some(memory) = memory {
            self.remove_uses(&[memory.input]);
        }
        self.blocks[at.0.index()]
            .as_mut()
            .expect("活跃 block")
            .insts[at.1]
            .removed = true;
        Ok(())
    }

    /// 替换一条指令的操作数并维护 use 计数。
    pub(crate) fn set_operand(&mut self, at: InstRef, position: usize, value: ValueId) {
        let previous = self.blocks[at.0.index()]
            .as_ref()
            .expect("活跃 block")
            .insts[at.1]
            .arguments[position];
        self.remove_uses(&[previous]);
        self.add_uses(&[value]);
        self.blocks[at.0.index()]
            .as_mut()
            .expect("活跃 block")
            .insts[at.1]
            .arguments[position] = value;
    }

    /// 替换一条指令的 opcode；不改变操作数或结果。
    pub(crate) fn set_op(&mut self, at: InstRef, op: Op) {
        self.blocks[at.0.index()]
            .as_mut()
            .expect("活跃 block")
            .insts[at.1]
            .op = op;
    }

    pub(crate) fn set_terminator(&mut self, block: BlockId, term: Term) {
        let previous = std::mem::replace(
            &mut self.blocks[block.index()]
                .as_mut()
                .expect("活跃 block")
                .term,
            term,
        );
        let uses = previous.uses();
        self.remove_uses(&uses);
        let uses = self.blocks[block.index()]
            .as_ref()
            .expect("活跃 block")
            .term
            .uses();
        self.add_uses(&uses);
    }

    /// 新建一个 block；参数 0 必须是 `Mem`。
    pub(crate) fn add_block(
        &mut self,
        params: Vec<(ValueType, Option<(u32, u64)>)>,
        source: SourceInfo,
        cleanup: bool,
    ) -> BlockId {
        debug_assert!(
            params
                .first()
                .is_some_and(|(kind, _)| kind.ty == super::super::body::Type::Mem),
            "block 参数 0 必须是 Mem"
        );
        let mut values = Vec::with_capacity(params.len());
        for (kind, _) in &params {
            values.push(self.push_value(*kind, Origin::None));
        }
        let memory = values[0];
        let block = BlockId(id(self.blocks.len()));
        self.blocks.push(Some(MBlock {
            params: params
                .into_iter()
                .zip(values)
                .map(|((_, source), value)| MParam { value, source })
                .collect(),
            insts: Vec::new(),
            term: Term::Unreachable { memory },
            source,
            cleanup,
        }));
        self.block_origins.push(Vec::new());
        self.add_uses(&[memory]);
        block
    }

    /// 删除一个 block；引用它的边必须已经改向或删除。
    pub(crate) fn remove_block(&mut self, block: BlockId) -> Result<(), Diagnostic> {
        if block == self.entry {
            return Err(invalid("不能删除入口 block"));
        }
        if self
            .edges()
            .any(|(_, edge)| edge.from == block || edge.to == block)
        {
            return Err(invalid("删除 block 时仍有活跃边引用"));
        }
        if self.blocks[block.index()].take().is_none() {
            return Ok(());
        }
        Ok(())
    }

    /// 把一条边的目标与实参改向到另一个 block。
    pub(crate) fn redirect_edge(&mut self, edge: EdgeId, to: BlockId, arguments: Vec<ValueId>) {
        self.add_uses(&arguments);
        let edge_ref = self.edges[edge.index()].as_mut().expect("活跃边");
        let previous = std::mem::replace(&mut edge_ref.arguments, arguments);
        edge_ref.to = to;
        self.remove_uses(&previous);
    }

    /// 删除一条边；调用方必须同时从其终结符里移除该引用。
    pub(crate) fn remove_edge(&mut self, edge: EdgeId) {
        if let Some(removed) = self.edges[edge.index()].take() {
            self.remove_uses(&removed.arguments);
        }
    }

    /// 在 `at` 之前插入一条内存指令；返回普通结果与新的 Mem 输出。
    pub(crate) fn insert_memory(
        &mut self,
        at: InstRef,
        op: Op,
        arguments: &[ValueId],
        results: &[(ValueType, Origin)],
        input: ValueId,
    ) -> (Vec<ValueId>, ValueId) {
        debug_assert!(op.has_memory(), "插入的内存指令必须携带 Mem");
        let safepoint = op.safepoint_kind();
        let result_values: Vec<ValueId> = results
            .iter()
            .map(|(kind, origin)| self.push_value(*kind, origin.clone()))
            .collect();
        let output = self.push_value(
            ValueType {
                ty: super::super::body::Type::Mem,
                provenance: None,
            },
            Origin::None,
        );
        let arguments = arguments.to_vec();
        self.add_uses(&arguments);
        self.add_uses(&[input]);
        let block_ref = self.blocks[at.0.index()].as_mut().expect("活跃 block");
        block_ref.insts.insert(
            at.1,
            MInst {
                op,
                arguments,
                results: result_values.clone(),
                memory: Some(Memory { input, output }),
                safepoint,
                source: block_ref.source.clone(),
                origin: None,
                removed: false,
            },
        );
        self.recount_definitions();
        self.block_origins[at.0.index()].insert(at.1, None);
        (result_values, output)
    }

    /// 改写一条内存指令的 Mem 输入并维护 use 计数。
    pub(crate) fn set_memory_input(&mut self, at: InstRef, input: ValueId) {
        let previous = self.blocks[at.0.index()]
            .as_ref()
            .expect("活跃 block")
            .insts[at.1]
            .memory
            .expect("内存指令")
            .input;
        self.remove_uses(&[previous]);
        self.add_uses(&[input]);
        self.blocks[at.0.index()]
            .as_mut()
            .expect("活跃 block")
            .insts[at.1]
            .memory
            .as_mut()
            .expect("内存指令")
            .input = input;
    }

    /// 读取终结符消费的 Mem；没有 Mem 的终结符返回 `None`。
    pub(crate) fn terminator_memory(&self, block: BlockId) -> Option<ValueId> {
        match self.terminator(block) {
            Term::Invoke { memory, .. } => Some(memory.input),
            Term::Return { memory, .. }
            | Term::ResumePanic { memory }
            | Term::TailCall { memory, .. }
            | Term::Trap { memory }
            | Term::Unreachable { memory } => Some(*memory),
            _ => None,
        }
    }

    /// 改写终结符消费的 Mem。
    pub(crate) fn set_terminator_memory(&mut self, block: BlockId, output: ValueId) {
        let previous = self.terminator_memory(block).expect("终结符消费 Mem");
        self.remove_uses(&[previous]);
        self.add_uses(&[output]);
        match &mut self.blocks[block.index()]
            .as_mut()
            .expect("活跃 block")
            .term
        {
            Term::Invoke { memory, .. } => memory.input = output,
            Term::Return { memory, .. }
            | Term::ResumePanic { memory }
            | Term::TailCall { memory, .. }
            | Term::Trap { memory }
            | Term::Unreachable { memory } => *memory = output,
            _ => unreachable!("终结符消费 Mem"),
        }
    }

    fn push_edge(
        &mut self,
        from: BlockId,
        to: BlockId,
        arguments: Vec<ValueId>,
        unwind: bool,
    ) -> EdgeId {
        self.add_uses(&arguments);
        let edge = EdgeId(id(self.edges.len()));
        self.edges.push(Some(MEdge {
            from,
            to,
            arguments,
            unwind,
        }));
        edge
    }

    /// 新建一条普通边；调用方负责在某个终结符里引用它。
    pub(crate) fn add_edge(
        &mut self,
        from: BlockId,
        to: BlockId,
        arguments: Vec<ValueId>,
    ) -> EdgeId {
        self.push_edge(from, to, arguments, false)
    }

    /// 新建一条可标记 unwind 的边。
    pub(crate) fn add_edge_unwind(
        &mut self,
        from: BlockId,
        to: BlockId,
        arguments: Vec<ValueId>,
        unwind: bool,
    ) -> EdgeId {
        self.push_edge(from, to, arguments, unwind)
    }

    /// 分配一个尚未定义的新 value；调用方必须通过 [`Editor::emit_raw`] 定义它。
    pub(crate) fn fresh_value(&mut self, kind: ValueType, origin: Origin) -> ValueId {
        self.push_value(kind, origin)
    }

    /// 以预先分配的 result 值追加一条指令（用于子图克隆）。
    pub(crate) fn emit_raw(
        &mut self,
        block: BlockId,
        op: Op,
        arguments: Vec<ValueId>,
        results: Vec<ValueId>,
        memory: Option<Memory>,
        safepoint: Option<SafepointKind>,
    ) {
        debug_assert_eq!(op.has_memory(), memory.is_some(), "内存属性与 Mem 必须一致");
        self.add_uses(&arguments);
        if let Some(memory) = memory {
            self.add_uses(&[memory.input]);
        }
        let block_ref = self.blocks[block.index()].as_mut().expect("活跃 block");
        let index = block_ref.insts.len();
        block_ref.insts.push(MInst {
            op,
            arguments,
            results: results.clone(),
            memory,
            safepoint,
            source: block_ref.source.clone(),
            origin: None,
            removed: false,
        });
        for value in &results {
            self.definitions[value.index()] = Some((block, index));
        }
        self.block_origins[block.index()].push(None);
    }

    /// 合并 `a` 与它唯一后继 `b`。
    pub(crate) fn merge_blocks(&mut self, a: BlockId, b: BlockId) -> Result<(), Diagnostic> {
        if self.successors(a) != [b] {
            return Err(invalid("合并要求 a 的唯一后继是 b"));
        }
        let predecessors = self.predecessors(b);
        if predecessors.len() != 1 {
            return Err(invalid("合并要求 b 只有一个前驱"));
        }
        let edge = predecessors[0];
        let arguments = self.edges[edge.index()]
            .as_ref()
            .expect("活跃边")
            .arguments
            .clone();
        let params: Vec<_> = self.blocks[b.index()]
            .as_ref()
            .expect("活跃 block")
            .params
            .iter()
            .map(|param| param.value)
            .collect();
        for (parameter, argument) in params.iter().zip(&arguments) {
            self.replace_value(*parameter, *argument);
        }
        let (insts, term) = {
            let block = self.blocks[b.index()].take().expect("活跃 block");
            (block.insts, block.term)
        };
        let origins = self.block_origins[b.index()].clone();
        self.block_origins[a.index()].extend(origins);
        self.blocks[a.index()]
            .as_mut()
            .expect("活跃 block")
            .insts
            .extend(insts);
        self.set_terminator(a, term);
        self.edges[edge.index()] = None;
        // b 的后继边现在从 a 出发。
        for edge in self.edges.iter_mut().flatten() {
            if edge.from == b {
                edge.from = a;
            }
        }
        Ok(())
    }

    /// 追加一条 barrier permit，返回其编号。
    pub(crate) fn add_barrier_permit(&mut self, region: u32, max_shades: u32) -> PermitId {
        let permit = PermitId(id(self.barrier_permits.len()));
        self.barrier_permits
            .push(super::super::body::BarrierPermit { region, max_shades });
        permit
    }

    /// 重建稠密 arena 并返回新的函数体；调用方随后运行结构 verifier。
    pub(crate) fn finish(self) -> Result<Body, Diagnostic> {
        let Editor {
            values,
            mut blocks,
            edges,
            replaced,
            use_counts: _,
            definitions: _,
            block_origins,
            instruction_count,
            entry,
            revision,
            instance,
            owner,
            name,
            target,
            signature,
            stack_slots,
            lifetimes,
            barrier_permits,
            no_safepoint_regions,
            source_scopes,
            data,
            assembly,
            environments,
            input_fingerprint,
            poll_summary,
        } = self;

        let mut block_map = vec![None; blocks.len()];
        let mut live_blocks = Vec::with_capacity(blocks.len());
        for (index, block) in blocks.iter().enumerate() {
            if block.is_some() {
                block_map[index] = Some(BlockId(id(live_blocks.len())));
                live_blocks.push(BlockId(id(index)));
            }
        }
        let entry = block_map[entry.index()].ok_or_else(|| invalid("入口 block 被删除"))?;

        let mut edge_map = vec![None; edges.len()];
        let mut live_edges = Vec::with_capacity(edges.len());
        for (index, edge) in edges.iter().enumerate() {
            if let Some(edge) = edge {
                let from = block_map[edge.from.index()]
                    .ok_or_else(|| invalid("活跃边引用了已删除 block"))?;
                let to = block_map[edge.to.index()]
                    .ok_or_else(|| invalid("活跃边引用了已删除 block"))?;
                edge_map[index] = Some(EdgeId(id(live_edges.len())));
                live_edges.push((from, to, edge.unwind, edge.arguments.clone()));
            }
        }

        let mut value_map: Vec<Option<ValueId>> = vec![None; values.len()];
        let mut out_values: Vec<Value> = Vec::with_capacity(values.len());
        // 阶段 1：分配全部 value 与定义；记录每条指令的结果范围与内存输出。
        let mut inst_map: Vec<Option<InstId>> = vec![None; instruction_count];
        let mut inst_results: Vec<(Range<u32>, Option<ValueId>)> = Vec::new();
        let mut invoke_results: Vec<Option<(Range<u32>, ValueId)>> = vec![None; live_blocks.len()];
        for old_block in &live_blocks {
            let new_block = block_map[old_block.index()].expect("活跃 block");
            let block = blocks[old_block.index()].as_ref().expect("活跃 block");
            for (index, param) in block.params.iter().enumerate() {
                define_value(
                    &values,
                    &replaced,
                    &mut value_map,
                    &mut out_values,
                    param.value,
                    Definition::Parameter {
                        block: new_block,
                        index: id(index),
                    },
                );
            }
            for instruction in &block.insts {
                if instruction.removed {
                    continue;
                }
                let new_inst = InstId(id(inst_results.len()));
                if let Some(origin) = instruction.origin {
                    inst_map[origin.index()] = Some(new_inst);
                }
                let start = id(out_values.len());
                for (index, result) in instruction.results.iter().enumerate() {
                    define_value(
                        &values,
                        &replaced,
                        &mut value_map,
                        &mut out_values,
                        *result,
                        Definition::Instruction {
                            instruction: new_inst,
                            result: id(index),
                        },
                    );
                }
                let end = id(out_values.len());
                let memory = instruction.memory.map(|memory| {
                    define_value(
                        &values,
                        &replaced,
                        &mut value_map,
                        &mut out_values,
                        memory.output,
                        Definition::Instruction {
                            instruction: new_inst,
                            result: id(instruction.results.len()),
                        },
                    )
                });
                inst_results.push((start..end, memory));
            }
            if let Term::Invoke {
                results, memory, ..
            } = &block.term
            {
                let start = id(out_values.len());
                for (index, result) in results.iter().enumerate() {
                    define_value(
                        &values,
                        &replaced,
                        &mut value_map,
                        &mut out_values,
                        *result,
                        Definition::Invoke {
                            block: new_block,
                            result: id(index),
                        },
                    );
                }
                let end = id(out_values.len());
                let output = define_value(
                    &values,
                    &replaced,
                    &mut value_map,
                    &mut out_values,
                    memory.output,
                    Definition::Invoke {
                        block: new_block,
                        result: id(results.len()),
                    },
                );
                invoke_results[new_block.index()] = Some((start..end, output));
            }
        }
        // 重映射 Derived origin。
        for value in &mut out_values {
            if let Origin::Derived(base) = value.origin {
                let resolved = resolve_value(&replaced, base);
                value.origin = Origin::Derived(
                    value_map[resolved.index()]
                        .ok_or_else(|| invalid("origin 引用了被删除的 value"))?,
                );
            }
        }

        // 阶段 2：构建边。
        let mut out_operands: Vec<ValueId> = Vec::new();
        let mut out_edges: Vec<Edge> = Vec::with_capacity(live_edges.len());
        for (from, to, unwind, arguments) in &live_edges {
            let start = id(out_operands.len());
            for value in arguments {
                out_operands.push(use_value(&value_map, &replaced, *value)?);
            }
            out_edges.push(Edge {
                from: *from,
                to: *to,
                arguments: start..id(out_operands.len()),
                unwind: *unwind,
            });
        }

        // 阶段 3：构建 block、指令与终结符。
        let mut out_inst_blocks: Vec<BlockId> = Vec::with_capacity(inst_results.len());
        let mut out_blocks: Vec<Block> = Vec::with_capacity(live_blocks.len());
        let mut out_instructions: Vec<Instruction> = Vec::with_capacity(inst_results.len());
        let mut out_parameters: Vec<Parameter> = Vec::new();
        let mut out_switch_cases: Vec<(u64, EdgeId)> = Vec::new();
        let mut out_safepoints: Vec<Safepoint> = Vec::new();
        for old_block in &live_blocks {
            let block = blocks[old_block.index()].take().expect("活跃 block");
            let new_block = block_map[old_block.index()].expect("活跃 block");
            let params_start = id(out_parameters.len());
            for param in &block.params {
                out_parameters.push(Parameter {
                    value: use_value(&value_map, &replaced, param.value)?,
                    source: param.source,
                });
            }
            let insts_start = id(out_instructions.len());
            for instruction in &block.insts {
                if instruction.removed {
                    continue;
                }
                let new_inst = InstId(id(out_instructions.len()));
                let (results_range, memory_output) = inst_results[new_inst.index()].clone();
                let args_start = id(out_operands.len());
                for value in &instruction.arguments {
                    out_operands.push(use_value(&value_map, &replaced, *value)?);
                }
                let args_end = id(out_operands.len());
                let memory = memory_output.map(|output| {
                    Ok::<_, Diagnostic>(Memory {
                        input: use_value(&value_map, &replaced, instruction.memory.unwrap().input)?,
                        output,
                    })
                });
                let memory = match memory {
                    Some(memory) => Some(memory?),
                    None => None,
                };
                let safepoint = instruction.safepoint.map(|kind| {
                    let point = SafepointId(id(out_safepoints.len()));
                    out_safepoints.push(Safepoint {
                        kind,
                        block: new_block,
                        instruction: Some(new_inst),
                    });
                    point
                });
                let op = remap_op(instruction.op.clone(), &inst_map)?;
                out_inst_blocks.push(new_block);
                out_instructions.push(Instruction {
                    op,
                    arguments: args_start..args_end,
                    results: results_range,
                    memory,
                    safepoint,
                    source: instruction.source.clone(),
                });
            }
            let insts_end = id(out_instructions.len());
            let term = build_terminator(
                block.term,
                &value_map,
                &replaced,
                &edge_map,
                &mut out_operands,
                &mut out_switch_cases,
                &invoke_results[new_block.index()],
                &mut out_safepoints,
                new_block,
            )?;
            out_blocks.push(Block {
                parameters: params_start..id(out_parameters.len()),
                instructions: insts_start..insts_end,
                predecessors: 0..0,
                terminator: term,
                source: block.source,
                cleanup: block.cleanup,
            });
        }
        let mut predecessors: Vec<Vec<EdgeId>> = vec![Vec::new(); out_blocks.len()];
        for (index, edge) in out_edges.iter().enumerate() {
            predecessors[edge.to.index()].push(EdgeId(id(index)));
        }
        let mut out_predecessors: Vec<EdgeId> = Vec::new();
        for (index, block) in out_blocks.iter_mut().enumerate() {
            let start = id(out_predecessors.len());
            out_predecessors.extend(predecessors[index].iter().copied());
            block.predecessors = start..id(out_predecessors.len());
        }
        let mut out_lifetimes: Vec<Lifetime> = Vec::new();
        for lifetime in lifetimes {
            let Some(origins) = block_origins.get(lifetime.block.index()) else {
                continue;
            };
            let position = usize::try_from(lifetime.position).unwrap_or(usize::MAX);
            let mut mapped = None;
            for index in position..origins.len() {
                if let Some(origin) = origins[index]
                    && let Some(new_inst) = inst_map[origin.index()]
                {
                    mapped = Some(new_inst);
                    break;
                }
            }
            let (new_block, new_position) = match mapped {
                Some(new_inst) => {
                    let block = out_inst_blocks[new_inst.index()];
                    (
                        block,
                        new_inst.0 - out_blocks[block.index()].instructions.start,
                    )
                }
                None => {
                    // 该 block 可能已被合并：锚定到最后一个仍存活的原指令之后。
                    let mut previous = None;
                    for index in (0..position.min(origins.len())).rev() {
                        if let Some(origin) = origins[index]
                            && let Some(new_inst) = inst_map[origin.index()]
                        {
                            previous = Some(new_inst);
                            break;
                        }
                    }
                    match previous {
                        Some(new_inst) => {
                            let block = out_inst_blocks[new_inst.index()];
                            (
                                block,
                                new_inst.0 - out_blocks[block.index()].instructions.start + 1,
                            )
                        }
                        None => {
                            let Some(new_block) = block_map[lifetime.block.index()] else {
                                continue;
                            };
                            let instructions = out_blocks[new_block.index()].instructions.clone();
                            (new_block, instructions.end - instructions.start)
                        }
                    }
                }
            };
            out_lifetimes.push(Lifetime {
                slot: lifetime.slot,
                block: new_block,
                position: new_position,
                live: lifetime.live,
            });
        }
        let mut body = Body {
            revision,
            instance,
            owner,
            name,
            target,
            signature,
            values: out_values,
            blocks: out_blocks,
            instructions: out_instructions,
            operands: out_operands,
            parameters: out_parameters,
            edges: out_edges,
            predecessors: out_predecessors,
            switch_cases: out_switch_cases,
            stack_slots,
            lifetimes: out_lifetimes,
            safepoints: out_safepoints,
            barrier_permits,
            no_safepoint_regions,
            source_scopes,
            uses: Vec::new(),
            data,
            assembly,
            environments,
            entry,
            input_fingerprint,
            poll_summary,
        };
        super::super::uses::rebuild(&mut body);
        Ok(body)
    }
}

fn resolve_value(replaced: &[Option<ValueId>], mut value: ValueId) -> ValueId {
    for _ in 0..replaced.len() {
        match replaced[value.index()] {
            Some(next) if next != value => value = next,
            _ => return value,
        }
    }
    value
}

fn define_value(
    values: &[MValue],
    replaced: &[Option<ValueId>],
    value_map: &mut [Option<ValueId>],
    out: &mut Vec<Value>,
    old: ValueId,
    definition: Definition,
) -> ValueId {
    let resolved = resolve_value(replaced, old);
    if let Some(new) = value_map[resolved.index()] {
        return new;
    }
    let new = ValueId(id(out.len()));
    value_map[resolved.index()] = Some(new);
    out.push(Value {
        definition,
        kind: values[resolved.index()].kind,
        origin: values[resolved.index()].origin.clone(),
        uses: 0..0,
    });
    new
}

fn use_value(
    value_map: &[Option<ValueId>],
    replaced: &[Option<ValueId>],
    old: ValueId,
) -> Result<ValueId, Diagnostic> {
    let resolved = resolve_value(replaced, old);
    value_map[resolved.index()].ok_or_else(|| invalid("pass 删除了仍被使用的 LIR value"))
}

fn remap_op(op: Op, inst_map: &[Option<InstId>]) -> Result<Op, Diagnostic> {
    Ok(match op {
        Op::GcWriteBarrier { store } => Op::GcWriteBarrier {
            store: inst_map
                .get(store.index())
                .copied()
                .flatten()
                .ok_or_else(|| invalid("写屏障引用了被删除的存储指令"))?,
        },
        Op::GcWriteBarrierReserved { store, permit } => Op::GcWriteBarrierReserved {
            store: inst_map
                .get(store.index())
                .copied()
                .flatten()
                .ok_or_else(|| invalid("写屏障引用了被删除的存储指令"))?,
            permit,
        },
        other => other,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_terminator(
    term: Term,
    value_map: &[Option<ValueId>],
    replaced: &[Option<ValueId>],
    edge_map: &[Option<EdgeId>],
    operands: &mut Vec<ValueId>,
    switch_cases: &mut Vec<(u64, EdgeId)>,
    invoke: &Option<(Range<u32>, ValueId)>,
    safepoints: &mut Vec<Safepoint>,
    block: BlockId,
) -> Result<Terminator, Diagnostic> {
    let edge = |edge: EdgeId| -> Result<EdgeId, Diagnostic> {
        edge_map
            .get(edge.index())
            .copied()
            .flatten()
            .ok_or_else(|| invalid("终结符引用了已删除的边"))
    };
    let value =
        |value: ValueId| -> Result<ValueId, Diagnostic> { use_value(value_map, replaced, value) };
    let mut write = |values: &[ValueId]| -> Result<Range<u32>, Diagnostic> {
        let start = id(operands.len());
        for operand in values {
            operands.push(value(*operand)?);
        }
        Ok(start..id(operands.len()))
    };
    Ok(match term {
        Term::Jump(target) => Terminator::Jump(edge(target)?),
        Term::Branch { condition, yes, no } => Terminator::Branch {
            condition: value(condition)?,
            yes: edge(yes)?,
            no: edge(no)?,
        },
        Term::Switch {
            value: condition,
            cases,
            otherwise,
        } => {
            let start = id(switch_cases.len());
            for (case, target) in cases {
                switch_cases.push((case, edge(target)?));
            }
            Terminator::Switch {
                value: value(condition)?,
                cases: start..id(switch_cases.len()),
                otherwise: edge(otherwise)?,
            }
        }
        Term::Invoke {
            call,
            arguments,
            results,
            memory,
            normal,
            unwind,
            safepoint,
        } => {
            let arguments = write(&arguments)?;
            let (results_range, output) = invoke
                .clone()
                .ok_or_else(|| invalid("Invoke 结果缺少定义"))?;
            if usize::try_from(results_range.end - results_range.start).expect("范围适配宿主")
                != results.len()
            {
                return Err(invalid("Invoke 结果范围与结果数量不一致"));
            }
            let safepoint = safepoint.map(|kind| {
                let point = SafepointId(id(safepoints.len()));
                safepoints.push(Safepoint {
                    kind,
                    block,
                    instruction: None,
                });
                point
            });
            Terminator::Invoke {
                call,
                arguments,
                results: results_range,
                memory: Memory {
                    input: value(memory.input)?,
                    output,
                },
                normal: edge(normal)?,
                unwind: edge(unwind)?,
                safepoint,
            }
        }
        Term::Return { values, memory } => Terminator::Return {
            values: write(&values)?,
            memory: value(memory)?,
        },
        Term::ResumePanic { memory } => Terminator::ResumePanic {
            memory: value(memory)?,
        },
        Term::TailCall {
            call,
            arguments,
            memory,
        } => Terminator::TailCall {
            call,
            arguments: write(&arguments)?,
            memory: value(memory)?,
        },
        Term::Trap { memory } => Terminator::Trap {
            memory: value(memory)?,
        },
        Term::Unreachable { memory } => Terminator::Unreachable {
            memory: value(memory)?,
        },
    })
}

fn term_of(body: &Body, terminator: &Terminator) -> Term {
    match terminator {
        Terminator::Jump(edge) => Term::Jump(*edge),
        Terminator::Branch { condition, yes, no } => Term::Branch {
            condition: *condition,
            yes: *yes,
            no: *no,
        },
        Terminator::Switch {
            value,
            cases,
            otherwise,
        } => Term::Switch {
            value: *value,
            cases: body.switch_cases[range(cases)].to_vec(),
            otherwise: *otherwise,
        },
        Terminator::Invoke {
            call,
            arguments,
            results,
            memory,
            normal,
            unwind,
            safepoint,
        } => Term::Invoke {
            call: call.clone(),
            arguments: body.args(arguments).to_vec(),
            results: range(results).map(|value| ValueId(id(value))).collect(),
            memory: *memory,
            normal: *normal,
            unwind: *unwind,
            safepoint: safepoint.map(|point| body.safepoints[point.index()].kind),
        },
        Terminator::Return { values, memory } => Term::Return {
            values: body.args(values).to_vec(),
            memory: *memory,
        },
        Terminator::ResumePanic { memory } => Term::ResumePanic { memory: *memory },
        Terminator::TailCall {
            call,
            arguments,
            memory,
        } => Term::TailCall {
            call: call.clone(),
            arguments: body.args(arguments).to_vec(),
            memory: *memory,
        },
        Terminator::Trap { memory } => Term::Trap { memory: *memory },
        Terminator::Unreachable { memory } => Term::Unreachable { memory: *memory },
    }
}
