//! 擦除函数句柄的 checked 入口：只适配隐藏环境，不改变目标函数和 runtime 的直接调用 ABI。
use super::body::{
    Block, BlockId, Body, Call, CallTarget, Definition, Edge, EdgeId, InstId, Instruction, Memory,
    Op, Origin, Parameter, PollInterface, PollSummary, Provenance, REVISION, Safepoint,
    SafepointId, SafepointKind, Symbol, Terminator, Type, Value, ValueId, ValueType, id,
};
use super::{invalid, uses, verify};
use crate::Diagnostic;
use crate::{gir::body::CallKind, hir};

pub fn key(target: &[u8; 32], has_environment: bool) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-function-adapter-v1");
    hash.update(target);
    hash.update(&[u8::from(has_environment)]);
    *hash.finalize().as_bytes()
}

/// 每个实际取址的目标只生成一个入口；排序后去重，保持 world 和缓存身份确定性。
pub(super) fn materialize(bodies: &mut Vec<Body>, module: &hir::Module) -> Result<(), Diagnostic> {
    let mut requests = Vec::new();
    for body in bodies.iter() {
        for instruction in &body.instructions {
            if let Op::SymbolAddr(Symbol::FunctionAdapter {
                target,
                has_environment,
            }) = instruction.op
            {
                requests.push((target, has_environment));
            }
        }
    }
    requests.sort_unstable();
    requests.dedup();
    // 输入是已按实例身份排序的 concrete bodies；追加入口期间只查原始区间。
    let original_count = bodies.len();
    for (target, has_environment) in requests {
        let index = bodies[..original_count]
            .binary_search_by_key(&target, |body| body.instance)
            .map_err(|_| invalid("函数句柄目标没有具体 LIR body"))?;
        let adapter = build(&bodies[index], has_environment);
        verify::verify_structure(&adapter, module)?;
        bodies.push(adapter);
    }
    bodies.sort_unstable_by_key(|body| body.instance);
    Ok(())
}

pub(crate) fn build(target: &Body, has_environment: bool) -> Body {
    let mut signature = target.signature.clone();
    let environment = usize::from(signature.sret.is_some());
    if !has_environment {
        signature
            .parameters
            .insert(environment, ValueType::pointer(Provenance::GcHeap));
        for (parameter, _, _) in &mut signature.by_value {
            if *parameter >= id(environment) {
                *parameter += 1;
            }
        }
    }
    let source = target.blocks[target.entry.index()].source.clone();
    let mut body = Body {
        revision: REVISION,
        instance: key(&target.instance, has_environment),
        owner: target.owner,
        name: format!("{}$function", target.name),
        target: target.target.clone(),
        signature,
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
        root_inits: Vec::new(),
        safepoints: Vec::new(),
        barrier_permits: Vec::new(),
        no_safepoint_regions: Vec::new(),
        source_scopes: target.source_scopes.clone(),
        uses: Vec::new(),
        data: Vec::new(),
        assembly: Vec::new(),
        environments: Vec::new(),
        entry: BlockId(0),
        poll_summary: PollSummary::default(),
        poll_interface: PollInterface::default(),
    };
    let memory = parameter(&mut body, BlockId(0), 0, ValueType::scalar(Type::Mem));
    let mut arguments = Vec::with_capacity(target.signature.parameters.len());
    for index in 0..body.signature.parameters.len() {
        let kind = body.signature.parameters[index];
        let value = parameter(&mut body, BlockId(0), id(index + 1), kind);
        if has_environment || index != environment {
            arguments.push(value);
        }
    }
    let parameter_end = id(body.parameters.len());
    let checked = value(
        &mut body,
        ValueType::scalar(Type::Mem),
        Definition::Instruction {
            instruction: InstId(0),
            result: 0,
        },
    );
    body.instructions.push(Instruction {
        op: Op::StackCheck,
        arguments: 0..0,
        results: checked.0..checked.0,
        memory: Some(Memory {
            input: memory,
            output: checked,
        }),
        safepoint: Some(SafepointId(0)),
        source: source.clone(),
    });
    body.safepoints.push(Safepoint {
        kind: SafepointKind::StackCheck,
        block: BlockId(0),
        instruction: Some(InstId(0)),
    });
    let result_start = id(body.values.len());
    for (index, kind) in target.signature.results.iter().enumerate() {
        value(
            &mut body,
            *kind,
            Definition::Invoke {
                block: BlockId(0),
                result: id(index),
            },
        );
    }
    let result_end = id(body.values.len());
    let returned = value(
        &mut body,
        ValueType::scalar(Type::Mem),
        Definition::Invoke {
            block: BlockId(0),
            result: id(target.signature.results.len()),
        },
    );
    body.operands.extend(arguments);
    let argument_end = id(body.operands.len());
    body.blocks.push(Block {
        parameters: 0..parameter_end,
        instructions: 0..1,
        predecessors: 0..0,
        terminator: Terminator::Invoke {
            call: Call {
                target: CallTarget::Instance(target.instance),
                kind: CallKind::Managed,
                parameters: target.signature.parameters.clone(),
                results: target.signature.results.clone(),
                may_unwind: true,
                may_suspend: true,
                may_allocate: true,
                captures_arguments: true,
                by_value: target.signature.by_value.clone(),
                sret: target
                    .signature
                    .sret
                    .map(|(bytes, _, descriptor)| (0, bytes, descriptor)),
                foreign_signature: None,
                foreign_arguments: Vec::new(),
                poll_free_leaf: false,
                scoped_arguments: Vec::new(),
            },
            arguments: 0..argument_end,
            results: result_start..result_end,
            memory: Memory {
                input: checked,
                output: returned,
            },
            normal: EdgeId(0),
            unwind: EdgeId(1),
            safepoint: Some(SafepointId(1)),
        },
        source: source.clone(),
        cleanup: false,
    });
    body.safepoints.push(Safepoint {
        kind: SafepointKind::Suspend,
        block: BlockId(0),
        instruction: None,
    });
    continuations(&mut body, returned, result_start..result_end, source);
    uses::rebuild(&mut body);
    body
}

fn continuations(
    body: &mut Body,
    returned: ValueId,
    results: std::ops::Range<u32>,
    source: crate::gir::body::SourceInfo,
) {
    let parameter_start = id(body.parameters.len());
    let memory = parameter(body, BlockId(1), 0, ValueType::scalar(Type::Mem));
    let return_start = id(body.operands.len());
    for (index, result) in results.clone().enumerate() {
        let kind = body.values[result as usize].kind;
        let value = parameter(body, BlockId(1), id(index + 1), kind);
        body.operands.push(value);
    }
    let return_end = id(body.operands.len());
    body.blocks.push(Block {
        parameters: parameter_start..id(body.parameters.len()),
        instructions: 1..1,
        predecessors: 0..1,
        terminator: Terminator::Return {
            values: return_start..return_end,
            memory,
        },
        source: source.clone(),
        cleanup: false,
    });
    let parameter_start = id(body.parameters.len());
    let memory = parameter(body, BlockId(2), 0, ValueType::scalar(Type::Mem));
    body.blocks.push(Block {
        parameters: parameter_start..id(body.parameters.len()),
        instructions: 1..1,
        predecessors: 1..2,
        terminator: Terminator::ResumePanic { memory },
        source,
        cleanup: true,
    });
    let edge_start = id(body.operands.len());
    body.operands.push(returned);
    body.operands.extend(results.map(ValueId));
    let edge_end = id(body.operands.len());
    body.edges.push(Edge {
        from: BlockId(0),
        to: BlockId(1),
        arguments: edge_start..edge_end,
        unwind: false,
    });
    body.operands.push(returned);
    body.edges.push(Edge {
        from: BlockId(0),
        to: BlockId(2),
        arguments: edge_end..edge_end + 1,
        unwind: true,
    });
    body.predecessors.extend([EdgeId(0), EdgeId(1)]);
}

fn parameter(body: &mut Body, block: BlockId, index: u32, kind: ValueType) -> ValueId {
    let value = value(body, kind, Definition::Parameter { block, index });
    body.parameters.push(Parameter {
        value,
        source: (index != 0).then_some((index, 0)),
    });
    body.values[value.index()].origin = if block == body.entry && index != 0 {
        Origin::Parameter(index - 1)
    } else {
        Origin::None
    };
    value
}

fn value(body: &mut Body, kind: ValueType, definition: Definition) -> ValueId {
    let value = ValueId(id(body.values.len()));
    body.values.push(Value {
        definition,
        kind,
        origin: Origin::None,
        uses: 0..0,
    });
    value
}
