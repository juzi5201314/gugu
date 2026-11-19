//! Hand-built leaf fixtures; no host assembler, native code or processes.
mod atomic;
mod atomic_vm;
mod calls;
mod control;
mod foreign_worker;
mod fp;
mod fp_vm;
mod integer;
mod layout;
mod leaf_reserve;
mod locals;
mod memory;
mod roots;
mod runtime;
mod runtime_vm;
mod vm;

use super::*;
use crate::lir::body::*;

struct Fixture {
    body: Body,
    block: usize,
    memory: ValueId,
}

fn source() -> crate::gir::body::SourceInfo {
    crate::gir::body::SourceInfo {
        location: crate::hir::Location {
            source: 0,
            start: 0,
            end: 0,
            expansion: u32::MAX,
        },
        scope: crate::gir::body::ScopeId(0),
    }
}

impl Fixture {
    fn new(params: &[Type], results: &[Type]) -> Self {
        let mut fixture = Self {
            body: Body {
                revision: REVISION,
                instance: [0; 32],
                owner: [0; 32],
                name: "a64_leaf_fixture".into(),
                target: "internal-leaf".into(),
                signature: Signature {
                    parameters: params.iter().map(|ty| value_type(*ty)).collect(),
                    results: results.iter().map(|ty| value_type(*ty)).collect(),
                    sret: None,
                    by_value: vec![],
                },
                values: vec![],
                blocks: vec![],
                instructions: vec![],
                operands: vec![],
                parameters: vec![],
                edges: vec![],
                predecessors: vec![],
                switch_cases: vec![],
                stack_slots: vec![],
                lifetimes: vec![],
                root_inits: vec![],
                safepoints: vec![],
                barrier_permits: vec![],
                no_safepoint_regions: vec![],
                source_scopes: vec![],
                uses: vec![],
                data: vec![],
                assembly: vec![],
                environments: vec![],
                entry: BlockId(0),
                poll_summary: Default::default(),
                poll_interface: PollInterface {
                    leaf: true,
                    summary: Default::default(),
                },
            },
            block: 0,
            memory: ValueId(0),
        };
        fixture.block(params);
        fixture
    }
    fn value(&mut self, ty: Type, definition: Definition) -> ValueId {
        let id = ValueId(self.body.values.len() as u32);
        self.body.values.push(Value {
            kind: value_type(ty),
            definition,
            origin: Origin::None,
            uses: 0..0,
        });
        id
    }
    fn block(&mut self, params: &[Type]) -> usize {
        let index = self.body.blocks.len();
        let start = self.body.parameters.len() as u32;
        for (position, ty) in std::iter::once(&Type::Mem).chain(params).enumerate() {
            let value = self.value(
                *ty,
                Definition::Parameter {
                    block: BlockId(index as u32),
                    index: position as u32,
                },
            );
            self.body.parameters.push(Parameter {
                value,
                source: None,
            });
        }
        let memory = self.body.parameters[start as usize].value;
        self.body.blocks.push(crate::lir::body::Block {
            parameters: start..self.body.parameters.len() as u32,
            instructions: self.body.instructions.len() as u32..self.body.instructions.len() as u32,
            predecessors: 0..0,
            terminator: Terminator::Return {
                values: 0..0,
                memory,
            },
            source: source(),
            cleanup: false,
        });
        index
    }
    fn switch_to(&mut self, block: usize) {
        self.block = block;
        self.memory = self.body.params(BlockId(block as u32))[0].value;
        let at = self.body.instructions.len() as u32;
        self.body.blocks[block].instructions = at..at;
    }
    fn param(&self, block: usize, index: usize) -> ValueId {
        self.body.params(BlockId(block as u32))[index + 1].value
    }
    fn args(&mut self, values: &[ValueId]) -> std::ops::Range<u32> {
        let start = self.body.operands.len() as u32;
        self.body.operands.extend_from_slice(values);
        start..self.body.operands.len() as u32
    }
    fn op(&mut self, op: Op, args: &[ValueId], types: &[Type]) -> Vec<ValueId> {
        let site = self.body.instructions.len() as u32;
        let output = types
            .iter()
            .enumerate()
            .map(|(result, ty)| {
                self.value(
                    *ty,
                    Definition::Instruction {
                        instruction: InstId(site),
                        result: result as u32,
                    },
                )
            })
            .collect::<Vec<_>>();
        let arguments = self.args(args);
        let results = self.args(&output);
        let memory = if matches!(
            op,
            Op::Load(_)
                | Op::Store(_)
                | Op::Atomic { .. }
                | Op::Call(_)
                | Op::StackCheck
                | Op::SafepointPoll { .. }
        ) {
            let next = self.value(
                Type::Mem,
                Definition::Instruction {
                    instruction: InstId(site),
                    result: output.len() as u32,
                },
            );
            let memory = Some(Memory {
                input: self.memory,
                output: next,
            });
            self.memory = next;
            memory
        } else {
            None
        };
        self.body.instructions.push(Instruction {
            op,
            arguments,
            results,
            memory,
            safepoint: None,
            source: source(),
        });
        self.body.blocks[self.block].instructions.end = site + 1;
        output
    }
    fn constant(&mut self, ty: Type, bits: u64) -> ValueId {
        self.op(Op::IConst(bits), &[], &[ty])[0]
    }
    fn ret(&mut self, values: &[ValueId]) {
        let values = self.args(values);
        self.body.blocks[self.block].terminator = Terminator::Return {
            values,
            memory: self.memory,
        };
    }
    fn edge(&mut self, to: usize, args: &[ValueId]) -> EdgeId {
        let mut arguments = vec![self.memory];
        arguments.extend_from_slice(args);
        let arguments = self.args(&arguments);
        let id = EdgeId(self.body.edges.len() as u32);
        self.body.edges.push(Edge {
            from: BlockId(self.block as u32),
            to: BlockId(to as u32),
            arguments,
            unwind: false,
        });
        id
    }
    fn compile(&self) -> codegen::LeafFragment {
        codegen::compile_leaf(&self.body).unwrap()
    }
}

fn value_type(ty: Type) -> ValueType {
    if ty == Type::Ptr {
        ValueType::pointer(Provenance::Raw)
    } else {
        ValueType::scalar(ty)
    }
}

fn decoded(fragment: &codegen::LeafFragment) -> Vec<inst::Inst> {
    fragment
        .bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| {
            let word = u32::from_le_bytes(*chunk);
            let inst = decode::decode(word).unwrap();
            assert_eq!(encode::encode(&inst).unwrap().word, word);
            inst
        })
        .collect()
}

struct Tiny;
impl crate::regalloc::RegisterFile for Tiny {
    fn class(&self, _: crate::regalloc::PhysReg) -> crate::regalloc::Class {
        abi::GPR
    }
    fn pool(&self, _: crate::regalloc::Class, _: bool) -> &[crate::regalloc::PhysReg] {
        &[crate::regalloc::PhysReg(0), crate::regalloc::PhysReg(1)]
    }
    fn spill_layout(&self, _: crate::regalloc::Class) -> (u32, u32) {
        (8, 8)
    }
    fn copy_scratch(&self, _: crate::regalloc::Class) -> Option<crate::regalloc::PhysReg> {
        Some(crate::regalloc::PhysReg(16))
    }
}
