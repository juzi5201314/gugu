//! `dyn Trait` vtable shim：动态调用收到的是 dyn 对象/对象引用，入口解包出具体载荷后
//! 调用已单态化的方法实例。`self: &Self` 直接解包；无 COW/resource 且直接 ABI 的
//! 按值 `self` 从载荷重新读取接收者车道。
use super::body::{
    Access, AliasClass, Block, BlockId, Body, Call, CallTarget, Definition, Edge, EdgeId, InstId,
    Instruction, Memory, Op, Origin, Parameter, PollInterface, PollSummary, Provenance, REVISION,
    Safepoint, SafepointId, SafepointKind, Terminator, Type, Value, ValueId, ValueType, id,
};
use super::{invalid, uses, verify};
use crate::Diagnostic;
use crate::{gir::body::CallKind, hir, mono::MonoWorldV1};

pub fn key(interface: &[u8; 32], concrete: &[u8; 32], target: &[u8; 32]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-vtable-shim-v1");
    hash.update(interface);
    hash.update(concrete);
    hash.update(target);
    *hash.finalize().as_bytes()
}

/// 为 frozen universe 中每个可调用 vtable 槽生成一个 shim。
pub(super) fn materialize(
    bodies: &mut Vec<Body>,
    module: &hir::Module,
    mono: &MonoWorldV1,
) -> Result<(), Diagnostic> {
    let mut requests = Vec::new();
    for vtable in &mono.universe.vtables {
        let concrete = mono
            .universe
            .records
            .get(vtable.concrete as usize)
            .ok_or_else(|| invalid("vtable 具体类型编号越界"))?
            .key;
        let record = &mono.universe.records[vtable.concrete as usize];
        for method in &vtable.methods {
            requests.push((
                vtable.interface,
                concrete,
                method.instance,
                method.receiver_by_ref,
                record.layout,
                record.passing,
            ));
        }
    }
    requests.sort_unstable();
    requests.dedup();
    let original_count = bodies.len();
    for (interface, concrete, target, receiver_by_ref, layout, passing) in requests {
        let index = bodies[..original_count]
            .binary_search_by_key(&target, |body| body.instance)
            .map_err(|_| invalid("vtable 方法没有具体 LIR body"))?;
        let shim = build(
            &bodies[index],
            interface,
            concrete,
            receiver_by_ref,
            layout,
            passing,
        )?;
        verify::verify_structure(&shim, module)?;
        bodies.push(shim);
    }
    bodies.sort_unstable_by_key(|body| body.instance);
    Ok(())
}

fn receiver_lanes(
    receiver_by_ref: bool,
    layout: Option<(u64, u64)>,
    passing: u8,
) -> Result<usize, Diagnostic> {
    if receiver_by_ref {
        return Ok(1);
    }
    // passing 位：bits=1、identity=2、cow=4、resource=8。按值 dyn 调用必须先产生
    // 接收者语义副本；当前 shim 只支持无 COW/resource 动作的位与身份值。
    if passing & (4 | 8) != 0 {
        return Err(invalid("按值 dyn receiver 暂不支持 COW 或 resource 载荷"));
    }
    let (size, _) = layout.ok_or_else(|| invalid("dyn 按值 receiver 缺少布局"))?;
    if size > 16 {
        return Err(invalid("按值 dyn receiver 暂不支持间接 ABI 载荷"));
    }
    Ok(usize::try_from(size.div_ceil(8)).map_err(|_| invalid("dyn receiver 车道数溢出"))?)
}

fn signature(
    target: &Body,
    receiver_by_ref: bool,
    layout: Option<(u64, u64)>,
    passing: u8,
) -> Result<super::body::Signature, Diagnostic> {
    let mut signature = target.signature.clone();
    if receiver_by_ref {
        return Ok(signature);
    }
    let receiver = usize::from(target.signature.sret.is_some());
    let lanes = receiver_lanes(receiver_by_ref, layout, passing)?;
    if receiver + lanes > signature.parameters.len() {
        return Err(invalid("vtable 目标 receiver 车道越界"));
    }
    let mut parameters = Vec::with_capacity(signature.parameters.len() + 2 - lanes);
    parameters.extend_from_slice(&signature.parameters[..receiver]);
    parameters.push(ValueType::pointer(Provenance::GcHeap));
    parameters.push(ValueType::pointer(Provenance::Metadata));
    parameters.extend_from_slice(&signature.parameters[receiver + lanes..]);
    let delta = 2_i32 - i32::try_from(lanes).map_err(|_| invalid("receiver 车道数溢出"))?;
    let mut by_value = Vec::new();
    for (index, descriptor, bytes) in &signature.by_value {
        let index = *index as i32;
        let receiver_start = receiver as i32;
        let receiver_end = receiver_start + lanes as i32;
        if (receiver_start..receiver_end).contains(&index) {
            return Err(invalid("按值 dyn receiver 不能使用间接目标 ABI"));
        }
        let shifted = if index >= receiver_end {
            index + delta
        } else {
            index
        };
        by_value.push((
            u32::try_from(shifted).map_err(|_| invalid("by_value 参数编号溢出"))?,
            *descriptor,
            *bytes,
        ));
    }
    signature.parameters = parameters;
    signature.by_value = by_value;
    Ok(signature)
}

fn build(
    target: &Body,
    interface: [u8; 32],
    concrete: [u8; 32],
    receiver_by_ref: bool,
    layout: Option<(u64, u64)>,
    passing: u8,
) -> Result<Body, Diagnostic> {
    let source = target.blocks[target.entry.index()].source.clone();
    let mut body = Body {
        revision: REVISION,
        instance: key(&interface, &concrete, &target.instance),
        owner: target.owner,
        name: format!("{}$vtable", target.name),
        target: target.target.clone(),
        signature: signature(target, receiver_by_ref, layout, passing)?,
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
        arguments.push(parameter(&mut body, BlockId(0), id(index + 1), kind));
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
    let receiver_index = usize::from(target.signature.sret.is_some());
    let lanes = receiver_lanes(receiver_by_ref, layout, passing)?;
    let (_, receiver_align) = layout.unwrap_or((0, 8));
    let mut memory_input = checked;
    let mut receiver_values = Vec::with_capacity(lanes);
    if receiver_by_ref {
        let (data, memory) = emit_load(
            &mut body,
            arguments[receiver_index],
            ValueType::pointer(Provenance::GcHeap),
            memory_input,
            8,
            &source,
        );
        receiver_values.push(data);
        memory_input = memory;
    } else {
        let data = arguments[receiver_index];
        for lane in 0..lanes {
            let offset = lane as u64 * 8;
            let address = if offset == 0 {
                data
            } else {
                let offset_value = emit_pure(
                    &mut body,
                    Op::IConst(offset),
                    &[],
                    ValueType::scalar(Type::I64),
                    &source,
                );
                let derived = emit_pure(
                    &mut body,
                    Op::PtrOffset,
                    &[data, offset_value],
                    ValueType::pointer(Provenance::GcInterior),
                    &source,
                );
                body.values[derived.index()].origin = Origin::Derived(data);
                derived
            };
            let kind = target.signature.parameters[receiver_index + lane];
            let align = u32::try_from(receiver_align.min(8).max(1))
                .map_err(|_| invalid("receiver 对齐溢出"))?;
            let (value, memory) = emit_load(&mut body, address, kind, memory_input, align, &source);
            receiver_values.push(value);
            memory_input = memory;
        }
    }
    let consumed = if receiver_by_ref { 1 } else { 2 };
    let mut call_arguments = Vec::with_capacity(target.signature.parameters.len());
    call_arguments.extend_from_slice(&arguments[..receiver_index]);
    call_arguments.extend(receiver_values);
    call_arguments.extend_from_slice(&arguments[receiver_index + consumed..]);
    if call_arguments.len() != target.signature.parameters.len() {
        return Err(invalid("vtable shim 参数车道数不一致"));
    }
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
    let invoke_start = id(body.operands.len());
    body.operands.extend(call_arguments);
    let argument_end = id(body.operands.len());
    body.blocks.push(Block {
        parameters: 0..parameter_end,
        instructions: 0..id(body.instructions.len()),
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
            },
            arguments: invoke_start..argument_end,
            results: result_start..result_end,
            memory: Memory {
                input: memory_input,
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
    let instruction_end = id(body.instructions.len());
    continuations(
        &mut body,
        returned,
        result_start..result_end,
        source,
        instruction_end,
    );
    uses::rebuild(&mut body);
    Ok(body)
}

fn emit_pure(
    body: &mut Body,
    op: Op,
    arguments: &[ValueId],
    kind: ValueType,
    source: &crate::gir::body::SourceInfo,
) -> ValueId {
    let instruction = InstId(id(body.instructions.len()));
    let start = id(body.operands.len());
    body.operands.extend_from_slice(arguments);
    let end = id(body.operands.len());
    let result = value(
        body,
        kind,
        Definition::Instruction {
            instruction,
            result: 0,
        },
    );
    body.instructions.push(Instruction {
        op,
        arguments: start..end,
        results: result.0..result.0 + 1,
        memory: None,
        safepoint: None,
        source: source.clone(),
    });
    result
}

fn emit_load(
    body: &mut Body,
    address: ValueId,
    kind: ValueType,
    memory_input: ValueId,
    align: u32,
    source: &crate::gir::body::SourceInfo,
) -> (ValueId, ValueId) {
    let instruction = InstId(id(body.instructions.len()));
    let start = id(body.operands.len());
    body.operands.push(address);
    let end = id(body.operands.len());
    let result = value(
        body,
        kind,
        Definition::Instruction {
            instruction,
            result: 0,
        },
    );
    let memory = value(
        body,
        ValueType::scalar(Type::Mem),
        Definition::Instruction {
            instruction,
            result: 1,
        },
    );
    body.instructions.push(Instruction {
        op: Op::Load(Access {
            alias: AliasClass::Heap,
            align,
            volatile: false,
        }),
        arguments: start..end,
        results: result.0..result.0 + 1,
        memory: Some(Memory {
            input: memory_input,
            output: memory,
        }),
        safepoint: None,
        source: source.clone(),
    });
    (result, memory)
}

fn continuations(
    body: &mut Body,
    returned: ValueId,
    results: std::ops::Range<u32>,
    source: crate::gir::body::SourceInfo,
    instruction_end: u32,
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
        instructions: instruction_end..instruction_end,
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
        instructions: instruction_end..instruction_end,
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
