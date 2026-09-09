//! 具体 GIR 到 LIR；变量读取由 block 参数封口，不引入 phi 或平行语义 IR。
mod calls;
mod intrinsics;
mod memory;
mod numeric;
mod rvalue;
mod ssa;
mod storage;
mod values;

use super::body::{
    self, Block, BlockId, Body, Definition, Edge, EdgeId, InstId, Instruction, Memory, Op, Origin,
    Parameter, Safepoint, SafepointId, Signature, Terminator, Type, Value, ValueId, ValueType, id,
};
use super::invalid;
use crate::Diagnostic;
use crate::frontend::{gir, hir, mono};
use gir::body::{GirBody, SourceInfo};
use gir::concrete::{ConcreteBody, TypeKind, TypeLayout};
use mono::collect::InstanceSummaryV1;
use std::ops::Range;

#[derive(Clone, Copy)]
struct Variable {
    source: (u32, u64),
    kind: ValueType,
}

#[derive(Clone)]
enum Storage {
    Values(Range<u32>),
    Stack {
        slot: body::SlotId,
        address: ValueId,
    },
    Heap {
        variable: u32,
        placement: gir::placement::PlacementKind,
    },
    Capture {
        address: ValueId,
    },
}

struct PendingBlock {
    parameters: Vec<(Option<u32>, Parameter)>,
    instructions: Vec<InstId>,
    definitions: Vec<Option<ValueId>>,
    memory: ValueId,
    terminator: Option<Terminator>,
    source: SourceInfo,
    cleanup: bool,
}

struct Builder<'a> {
    concrete: &'a ConcreteBody,
    gir: &'a GirBody,
    module: &'a hir::Module,
    world: &'a gir::GirWorldV1,
    mono: &'a mono::MonoWorldV1,
    instance: &'a InstanceSummaryV1,
    owner: &'a hir::Owner,
    body: Body,
    blocks: Vec<PendingBlock>,
    block_map: Vec<Option<BlockId>>,
    variables: Vec<Variable>,
    storage: Vec<Storage>,
    entry_arguments: Vec<Option<Vec<ValueId>>>,
    current: BlockId,
    source: SourceInfo,
    sret: Option<ValueId>,
    environment: Option<ValueId>,
    next_allocation: u32,
    fixed_edges: Vec<(EdgeId, ValueId, ValueId)>,
}

pub(crate) fn lower(
    concrete: &ConcreteBody,
    module: &hir::Module,
    world: &gir::GirWorldV1,
    mono: &mono::MonoWorldV1,
    target: &str,
    input_fingerprint: [u8; 32],
) -> Result<Body, Diagnostic> {
    concrete.verify()?;
    let instance = mono
        .instances
        .iter()
        .find(|instance| mono::digest_of(&instance.mono_key) == concrete.instance)
        .ok_or_else(|| invalid("LIR 输入不在闭合实例图中"))?;
    let owner = module
        .owners
        .iter()
        .find(|owner| owner.definition == concrete.body.owner)
        .ok_or_else(|| invalid("具体 GIR 缺少冻结 owner"))?;
    let source = concrete.body.blocks[concrete.body.entry.index()]
        .source
        .clone();
    let mut builder = Builder {
        concrete,
        gir: &concrete.body,
        module,
        world,
        mono,
        instance,
        owner,
        body: Body {
            revision: body::REVISION,
            instance: concrete.instance,
            owner: concrete.body.owner_key,
            name: instance.symbol.clone(),
            target: target.to_owned(),
            signature: Signature {
                parameters: Vec::new(),
                results: Vec::new(),
                sret: None,
                by_value: Vec::new(),
            },
            values: Vec::new(),
            blocks: Vec::new(),
            instructions: Vec::new(),
            operands: Vec::new(),
            parameters: Vec::new(),
            edges: Vec::new(),
            predecessors: Vec::new(),
            switch_cases: Vec::new(),
            stack_slots: Vec::new(),
            lifetimes: Vec::new(),
            safepoints: Vec::new(),
            barrier_permits: Vec::new(),
            no_safepoint_regions: concrete.body.no_safepoint_regions.clone(),
            source_scopes: concrete.body.source_scopes.clone(),
            uses: Vec::new(),
            data: Vec::new(),
            assembly: owner.assembly.clone(),
            environments: Vec::new(),
            entry: BlockId(0),
            input_fingerprint,
        },
        blocks: Vec::new(),
        block_map: vec![None; concrete.body.blocks.len()],
        variables: Vec::new(),
        storage: Vec::new(),
        entry_arguments: vec![None; concrete.body.locals.len()],
        current: BlockId(0),
        source,
        sret: None,
        environment: None,
        next_allocation: 0,
        fixed_edges: Vec::new(),
    };
    builder.prepare_storage()?;
    builder.prepare_blocks();
    builder.prepare_entry()?;
    for original in 0..builder.gir.blocks.len() {
        let Some(block) = builder.block_map[original] else {
            continue;
        };
        builder.current = block;
        let input = &builder.gir.blocks[original];
        builder.source = input.source.clone();
        for statement in builder
            .gir
            .block_statements(gir::body::BlockId(id(original)))
        {
            builder.source = statement.source.clone();
            builder.statement(&statement.kind)?;
        }
        builder.source = input.source.clone();
        builder.terminator(&input.terminator)?;
    }
    builder.finish()
}

impl Builder<'_> {
    fn layout(&self, ty: u32) -> &TypeLayout {
        &self.concrete.types[usize::try_from(ty).expect("类型编号适配宿主")]
    }
    fn local_ty(&self, local: gir::body::LocalId) -> u32 {
        self.gir.locals[local.index()].ty.0
    }
    fn kind(&self, ty: u32) -> &TypeKind {
        &self.layout(ty).kind
    }
    fn machine_type(&self, value: ValueId) -> ValueType {
        self.body.values[value.index()].kind
    }

    fn variable(&mut self, local: u32, offset: u64, kind: ValueType) -> u32 {
        let index = id(self.variables.len());
        self.variables.push(Variable {
            source: (local, offset),
            kind,
        });
        index
    }

    fn value(&mut self, kind: ValueType, definition: Definition, origin: Origin) -> ValueId {
        let value = ValueId(id(self.body.values.len()));
        self.body.values.push(Value {
            kind,
            definition,
            origin,
            uses: 0..0,
        });
        value
    }

    fn arguments(&mut self, arguments: &[ValueId]) -> Range<u32> {
        let start = id(self.body.operands.len());
        self.body.operands.extend_from_slice(arguments);
        start..id(self.body.operands.len())
    }

    fn emit(
        &mut self,
        op: Op,
        arguments: &[ValueId],
        results: &[(ValueType, Origin)],
    ) -> Vec<ValueId> {
        let instruction = InstId(id(self.body.instructions.len()));
        let first = id(self.body.values.len());
        for (index, (kind, origin)) in results.iter().enumerate() {
            self.value(
                *kind,
                Definition::Instruction {
                    instruction,
                    result: id(index),
                },
                origin.clone(),
            );
        }
        let result_range = first..id(self.body.values.len());
        let memory = op.has_memory().then(|| {
            let input = self.blocks[self.current.index()].memory;
            let output = self.value(
                ValueType::scalar(Type::Mem),
                Definition::Instruction {
                    instruction,
                    result: id(results.len()),
                },
                Origin::None,
            );
            self.blocks[self.current.index()].memory = output;
            Memory { input, output }
        });
        let safepoint = op.safepoint_kind().map(|kind| {
            let safepoint = SafepointId(id(self.body.safepoints.len()));
            self.body.safepoints.push(Safepoint {
                kind,
                block: self.current,
                instruction: Some(instruction),
            });
            safepoint
        });
        let arguments = self.arguments(arguments);
        self.body.instructions.push(Instruction {
            op,
            arguments,
            results: result_range.clone(),
            memory,
            safepoint,
            source: self.source.clone(),
        });
        self.blocks[self.current.index()]
            .instructions
            .push(instruction);
        result_range.map(ValueId).collect()
    }

    fn emit_one(
        &mut self,
        op: Op,
        arguments: &[ValueId],
        kind: ValueType,
        origin: Origin,
    ) -> ValueId {
        self.emit(op, arguments, &[(kind, origin)])[0]
    }

    fn constant(&mut self, value: u64, ty: Type) -> ValueId {
        self.emit_one(Op::IConst(value), &[], ValueType::scalar(ty), Origin::None)
    }

    fn fresh(&mut self, source: SourceInfo, cleanup: bool) -> BlockId {
        let block = BlockId(id(self.blocks.len()));
        let memory = self.value(
            ValueType::scalar(Type::Mem),
            Definition::Parameter { block, index: 0 },
            Origin::None,
        );
        self.blocks.push(PendingBlock {
            parameters: vec![(
                None,
                Parameter {
                    value: memory,
                    source: None,
                },
            )],
            instructions: Vec::new(),
            definitions: vec![None; self.variables.len()],
            memory,
            terminator: None,
            source,
            cleanup,
        });
        block
    }

    fn prepare_blocks(&mut self) {
        let mut reachable = vec![false; self.gir.blocks.len()];
        let mut work = vec![self.gir.entry];
        while let Some(block) = work.pop() {
            if std::mem::replace(&mut reachable[block.index()], true) {
                continue;
            }
            work.extend(self.gir.blocks[block.index()].terminator.successors());
        }
        for (index, reachable) in reachable.into_iter().enumerate() {
            if reachable {
                let block = &self.gir.blocks[index];
                self.block_map[index] = Some(self.fresh(block.source.clone(), block.cleanup));
            }
        }
        self.body.entry = self.block_map[self.gir.entry.index()].expect("入口可达");
        self.current = self.body.entry;
    }

    fn target(&self, target: gir::body::BlockId) -> BlockId {
        self.block_map[target.index()].expect("可达 block 的后继也可达")
    }
    fn edge(&mut self, target: BlockId, unwind: bool) -> EdgeId {
        let edge = EdgeId(id(self.body.edges.len()));
        self.body.edges.push(Edge {
            from: self.current,
            to: target,
            arguments: 0..0,
            unwind,
        });
        edge
    }
    fn jump(&mut self, target: BlockId) {
        let edge = self.edge(target, false);
        self.blocks[self.current.index()].terminator = Some(Terminator::Jump(edge));
    }

    fn finish(mut self) -> Result<Body, Diagnostic> {
        self.seal_ssa()?;
        self.materialize_blocks()?;
        super::uses::rebuild(&mut self.body);
        Ok(self.body)
    }

    fn materialize_blocks(&mut self) -> Result<(), Diagnostic> {
        let mut instructions = Vec::with_capacity(self.body.instructions.len());
        let mut remap = vec![InstId(0); self.body.instructions.len()];
        for (index, pending) in self.blocks.iter_mut().enumerate() {
            let instruction_start = id(instructions.len());
            for old in &pending.instructions {
                remap[old.index()] = InstId(id(instructions.len()));
                instructions.push(self.body.instructions[old.index()].clone());
            }
            let parameter_start = id(self.body.parameters.len());
            self.body.parameters.extend(
                pending
                    .parameters
                    .iter()
                    .map(|(_, parameter)| parameter.clone()),
            );
            let predecessor_start = id(self.body.predecessors.len());
            self.body.predecessors.extend(
                self.body
                    .edges
                    .iter()
                    .enumerate()
                    .filter(|(_, edge)| edge.to.index() == index)
                    .map(|(edge, _)| EdgeId(id(edge))),
            );
            self.body.blocks.push(Block {
                parameters: parameter_start..id(self.body.parameters.len()),
                instructions: instruction_start..id(instructions.len()),
                predecessors: predecessor_start..id(self.body.predecessors.len()),
                terminator: pending
                    .terminator
                    .take()
                    .ok_or_else(|| invalid("LIR block 没有终结符"))?,
                source: pending.source.clone(),
                cleanup: pending.cleanup,
            });
        }
        for value in &mut self.body.values {
            if let Definition::Instruction { instruction, .. } = &mut value.definition {
                *instruction = remap[instruction.index()];
            }
        }
        for instruction in &mut instructions {
            if let Op::GcWriteBarrier { store } | Op::GcWriteBarrierReserved { store, .. } =
                &mut instruction.op
            {
                *store = remap[store.index()];
            }
        }
        for safepoint in &mut self.body.safepoints {
            if let Some(instruction) = &mut safepoint.instruction {
                *instruction = remap[instruction.index()];
            }
        }
        self.body.instructions = instructions;
        Ok(())
    }
}
