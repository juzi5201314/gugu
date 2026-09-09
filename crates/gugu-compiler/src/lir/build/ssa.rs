use super::{Builder, Diagnostic, Variable, invalid};
use crate::lir::body::{
    BlockId, Definition, Origin, Parameter, Provenance, Type, ValueId, ValueType, id,
};

impl Builder<'_> {
    pub(super) fn read(&mut self, variable: u32) -> Result<ValueId, Diagnostic> {
        self.read_at(variable, self.current)
    }

    fn read_at(&mut self, variable: u32, block: BlockId) -> Result<ValueId, Diagnostic> {
        let index = usize::try_from(variable).expect("变量编号适配宿主");
        if let Some(value) = self.blocks[block.index()].definitions[index] {
            return Ok(value);
        }
        if block == self.body.entry {
            return Err(invalid(&format!(
                "LIR 读取未初始化 local {}",
                self.variables[index].source.0
            )));
        }
        let Variable { source, kind } = self.variables[index];
        let parameter_index = id(self.blocks[block.index()].parameters.len());
        let value = self.value(
            kind,
            Definition::Parameter {
                block,
                index: parameter_index,
            },
            Origin::Merge,
        );
        self.blocks[block.index()].parameters.push((
            Some(variable),
            Parameter {
                value,
                source: Some(source),
            },
        ));
        self.blocks[block.index()].definitions[index] = Some(value);
        Ok(value)
    }

    pub(super) fn define(&mut self, variable: u32, value: ValueId) {
        self.blocks[self.current.index()].definitions
            [usize::try_from(variable).expect("变量编号适配宿主")] = Some(value);
    }

    pub(super) fn entry_parameter(&mut self, kind: ValueType, source: (u32, u64)) -> ValueId {
        let block = self.body.entry;
        let index = id(self.blocks[block.index()].parameters.len());
        let value = self.value(
            kind,
            Definition::Parameter { block, index },
            Origin::Parameter(index - 1),
        );
        self.blocks[block.index()].parameters.push((
            None,
            Parameter {
                value,
                source: Some(source),
            },
        ));
        self.body.signature.parameters.push(kind);
        value
    }

    pub(super) fn result_parameter(
        &mut self,
        block: BlockId,
        kind: ValueType,
        index: u32,
    ) -> ValueId {
        let position = id(self.blocks[block.index()].parameters.len());
        let value = self.value(
            kind,
            Definition::Parameter {
                block,
                index: position,
            },
            Origin::Merge,
        );
        self.blocks[block.index()].parameters.push((
            None,
            Parameter {
                value,
                source: Some((u32::MAX, u64::from(index))),
            },
        ));
        value
    }

    pub(super) fn seal_ssa(&mut self) -> Result<(), Diagnostic> {
        // 每次迭代至少新增一个 (block, variable) 参数；上界为 blocks × variables。
        loop {
            let before: usize = self.blocks.iter().map(|block| block.parameters.len()).sum();
            for edge_index in 0..self.body.edges.len() {
                let edge = &self.body.edges[edge_index];
                let (from, to) = (edge.from, edge.to);
                let variables: Vec<_> = self.blocks[to.index()]
                    .parameters
                    .iter()
                    .filter_map(|(variable, _)| *variable)
                    .collect();
                for variable in variables {
                    self.read_at(variable, from)?;
                }
            }
            if before
                == self
                    .blocks
                    .iter()
                    .map(|block| block.parameters.len())
                    .sum::<usize>()
            {
                break;
            }
        }
        for (block_index, block) in self.blocks.iter_mut().enumerate() {
            if BlockId(id(block_index)) != self.body.entry {
                block
                    .parameters
                    .sort_by_key(|(_, parameter)| parameter.source);
            }
            for (index, (_, parameter)) in block.parameters.iter().enumerate() {
                self.body.values[parameter.value.index()].definition = Definition::Parameter {
                    block: BlockId(id(block_index)),
                    index: id(index),
                };
            }
        }
        for edge_index in 0..self.body.edges.len() {
            let edge = &self.body.edges[edge_index];
            let (from, to) = (edge.from, edge.to);
            let mut arguments = Vec::with_capacity(self.blocks[to.index()].parameters.len());
            for (variable, parameter) in self.blocks[to.index()].parameters.clone() {
                let value = if self.body.values[parameter.value.index()].kind.ty == Type::Mem {
                    self.blocks[from.index()].memory
                } else if let Some(variable) = variable {
                    self.read_at(variable, from)?
                } else {
                    self.fixed_edges
                        .iter()
                        .find(|(edge, destination, _)| {
                            edge.index() == edge_index && *destination == parameter.value
                        })
                        .map(|(_, _, value)| *value)
                        .ok_or_else(|| invalid("Invoke 正常边缺少结果实参"))?
                };
                arguments.push(value);
            }
            let arguments = self.arguments(&arguments);
            self.body.edges[edge_index].arguments = arguments;
        }
        self.merge_provenance()
    }

    fn merge_provenance(&mut self) -> Result<(), Diagnostic> {
        for _ in 0..=self.body.values.len() {
            let mut changed = false;
            for block in 0..self.blocks.len() {
                for (_, parameter) in &self.blocks[block].parameters {
                    let value = parameter.value;
                    if self.body.values[value.index()].kind.ty != Type::Ptr {
                        continue;
                    }
                    let mut joined = None;
                    for edge in self
                        .body
                        .edges
                        .iter()
                        .filter(|edge| edge.to.index() == block)
                    {
                        let Definition::Parameter { index, .. } =
                            self.body.values[value.index()].definition
                        else {
                            unreachable!()
                        };
                        let argument = self.body.args(&edge.arguments)
                            [usize::try_from(index).expect("参数编号")];
                        if argument == value {
                            continue;
                        }
                        let incoming = self.body.values[argument.index()].kind;
                        joined = Some(match joined {
                            None => incoming,
                            Some(previous) => merge(previous, incoming)?,
                        });
                    }
                    if let Some(joined) = joined
                        && joined != self.body.values[value.index()].kind
                    {
                        self.body.values[value.index()].kind = joined;
                        changed = true;
                    }
                }
            }
            for instruction in &self.body.instructions {
                if matches!(instruction.op, crate::lir::body::Op::PtrOffset) {
                    let source = self.body.args(&instruction.arguments)[0];
                    let result =
                        usize::try_from(instruction.results.start).expect("pointer 结果编号");
                    let mut kind = self.body.values[source.index()].kind;
                    if kind.provenance == Some(Provenance::GcHeap) {
                        kind.provenance = Some(Provenance::GcInterior);
                    }
                    if self.body.values[result].kind != kind {
                        self.body.values[result].kind = kind;
                        changed = true;
                    }
                }
            }
            if !changed {
                return Ok(());
            }
        }
        Err(invalid("pointer provenance 合流没有收敛"))
    }
}

fn merge(left: ValueType, right: ValueType) -> Result<ValueType, Diagnostic> {
    if left.ty != right.ty {
        return Err(invalid("SSA 合流机器类型不一致"));
    }
    if left.provenance == right.provenance {
        return Ok(left);
    }
    match (left.provenance, right.provenance) {
        (Some(a), Some(b))
            if (a.managed() || a == Provenance::Stack)
                && (b.managed() || b == Provenance::Stack) =>
        {
            Ok(ValueType::pointer(Provenance::GcInterior))
        }
        (
            Some(Provenance::Raw | Provenance::Foreign),
            Some(Provenance::Raw | Provenance::Foreign),
        ) => Ok(ValueType::pointer(Provenance::Foreign)),
        _ => Err(invalid(
            "SSA 合流不能把 raw/code/metadata 指针提升为 managed 根",
        )),
    }
}
