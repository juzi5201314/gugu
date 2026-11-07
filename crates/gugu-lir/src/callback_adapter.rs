//! C 导出的 managed 入口复用普通 Invoke/展开构造；唯一参数是等待线程持有的 raw 包。
use crate::body::*;
use crate::exports::{Export, Lane};
use crate::{Diagnostic, invalid};

pub(crate) fn build(target: &Body, export: &Export, lanes: &[Lane]) -> Result<Body, Diagnostic> {
    let mut body = crate::function_adapter::build(target, true);
    body.instance = export.adapter;
    body.name = format!("{}$callback", target.name);
    let old_parameters: Vec<_> = body
        .params(body.entry)
        .iter()
        .skip(1)
        .map(|p| p.value)
        .collect();
    if old_parameters.len() != lanes.len() {
        return Err(invalid("C 导出内部参数数量不匹配"));
    }
    let packet = value(
        &mut body,
        ValueType::pointer(Provenance::Foreign),
        Definition::Parameter {
            block: BlockId(0),
            index: 1,
        },
        Origin::Parameter(0),
    );
    let end = body.blocks[0].parameters.end as usize;
    body.parameters.splice(
        1..end,
        [Parameter {
            value: packet,
            source: Some((0, 0)),
        }],
    );
    let delta = 2 - end as i64;
    body.blocks[0].parameters = 0..2;
    for block in body.blocks.iter_mut().skip(1) {
        block.parameters = (block.parameters.start as i64 + delta) as u32
            ..(block.parameters.end as i64 + delta) as u32;
    }
    body.signature = Signature {
        parameters: vec![ValueType::pointer(Provenance::Foreign)],
        results: Vec::new(),
        sret: None,
        by_value: Vec::new(),
    };
    let mut memory = body.instructions[0].memory.as_ref().unwrap().output;
    for (&parameter, lane) in old_parameters.iter().zip(lanes) {
        let address = address(&mut body, &mut memory, packet, lane.offset);
        let op = if lane.indirect {
            Op::Convert(Conversion::PointerCast)
        } else {
            Op::Load(access())
        };
        emit(&mut body, &mut memory, op, &[address], Some(parameter));
        body.values[parameter.index()].origin = Origin::Derived(address);
    }
    let entry_end = id(body.instructions.len());
    body.blocks[0].instructions.end = entry_end;
    if let Terminator::Invoke { memory: invoke, .. } = &mut body.blocks[0].terminator {
        invoke.input = memory;
    }
    let Terminator::Return {
        values,
        memory: returned,
    } = body.blocks[1].terminator.clone()
    else {
        return Err(invalid("C 导出适配器缺少正常返回"));
    };
    memory = returned;
    for (index, result) in body.args(&values).to_vec().into_iter().enumerate() {
        let address = address(
            &mut body,
            &mut memory,
            packet,
            export.result_offset + index as u64 * 8,
        );
        emit(
            &mut body,
            &mut memory,
            Op::Store(access()),
            &[address, result],
            None,
        );
    }
    let normal_end = id(body.instructions.len());
    body.blocks[1].instructions = entry_end..normal_end;
    body.blocks[1].terminator = Terminator::Return {
        values: 0..0,
        memory,
    };
    body.blocks[2].instructions = normal_end..normal_end;
    let Terminator::ResumePanic { memory } = body.blocks[2].terminator else {
        return Err(invalid("C 导出适配器缺少 panic 出口"));
    };
    // 未捕获 panic 在受管清理完成后终止，不能展开到等待线程的 C 帧。
    body.blocks[2].terminator = Terminator::Trap { memory };
    crate::uses::rebuild(&mut body);
    Ok(body)
}

fn access() -> Access {
    Access {
        alias: AliasClass::Foreign,
        align: 1,
        volatile: false,
    }
}

fn value(body: &mut Body, kind: ValueType, definition: Definition, origin: Origin) -> ValueId {
    let id = ValueId(id(body.values.len()));
    body.values.push(Value {
        kind,
        definition,
        origin,
        uses: 0..0,
    });
    id
}

fn address(body: &mut Body, memory: &mut ValueId, packet: ValueId, offset: u64) -> ValueId {
    let constant = value(
        body,
        ValueType::scalar(Type::I64),
        Definition::Instruction {
            instruction: InstId(id(body.instructions.len())),
            result: 0,
        },
        Origin::None,
    );
    emit(body, memory, Op::IConst(offset), &[], Some(constant));
    let address = value(
        body,
        ValueType::pointer(Provenance::Foreign),
        Definition::Instruction {
            instruction: InstId(id(body.instructions.len())),
            result: 0,
        },
        Origin::Derived(packet),
    );
    emit(
        body,
        memory,
        Op::PtrOffset,
        &[packet, constant],
        Some(address),
    );
    address
}

fn emit(body: &mut Body, memory: &mut ValueId, op: Op, args: &[ValueId], result: Option<ValueId>) {
    let instruction = InstId(id(body.instructions.len()));
    if let Some(value) = result {
        body.values[value.index()].definition = Definition::Instruction {
            instruction,
            result: 0,
        };
    }
    let effect = op.has_memory().then(|| {
        let output = value(
            body,
            ValueType::scalar(Type::Mem),
            Definition::Instruction {
                instruction,
                result: u32::from(result.is_some()),
            },
            Origin::None,
        );
        let input = std::mem::replace(memory, output);
        Memory { input, output }
    });
    let start = id(body.operands.len());
    body.operands.extend_from_slice(args);
    body.instructions.push(Instruction {
        op,
        arguments: start..id(body.operands.len()),
        results: result.map_or(0..0, |value| value.0..value.0 + 1),
        memory: effect,
        safepoint: None,
        source: body.blocks[0].source.clone(),
    });
}
