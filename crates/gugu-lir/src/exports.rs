//! C 导出参数采用对象字节布局；原生入口和 managed 适配器消费同一份清单。
use crate::body::*;
use crate::{Diagnostic, gir, hir, invalid};
use serde::{Deserialize, Serialize};

/// 节点、协程参数包与隐藏返回指针后保留一条 128 字节 native 请求。
pub const DATA_OFFSET: u64 = 416;
pub const SRET_OFFSET: u64 = 240;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    pub offset: u64,
    pub bytes: u64,
    /// Win64 大聚合的参数车道是指向调用者副本的指针。
    pub indirect: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Export {
    pub name: String,
    pub adapter: [u8; 32],
    pub call: Call,
    pub arguments: Vec<Lane>,
    pub results: Vec<Lane>,
    pub result_offset: u64,
    pub result_bytes: u64,
    pub packet_bytes: u64,
}

pub fn key(target: &[u8; 32]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-callback-adapter-v1");
    hash.update(target);
    *hash.finalize().as_bytes()
}

pub(crate) fn materialize(
    bodies: &mut Vec<Body>,
    module: &hir::Module,
    concrete: &[gir::concrete::ConcreteBody],
) -> Result<Vec<Export>, Diagnostic> {
    let mut exports = Vec::new();
    for linkage in &module.linkage {
        let Some(name) = &linkage.export_name else {
            continue;
        };
        if linkage.foreign.is_none() || linkage.naked {
            continue;
        }
        let definition = &module.definitions[linkage.definition.index()];
        let Some(input) = concrete
            .iter()
            .find(|body| body.body.owner_key == definition.key)
        else {
            return Err(invalid("C 导出缺少闭合实例"));
        };
        let target = bodies
            .iter()
            .find(|body| body.instance == input.instance)
            .ok_or_else(|| invalid("C 导出缺少 managed body"))?;
        let (export, offsets) = layout(name, target, input)?;
        let adapter = super::callback_adapter::build(target, &export, &offsets)?;
        crate::verify::verify_structure(&adapter, module)?;
        bodies.push(adapter);
        exports.push(export);
    }
    exports.sort_by(|a, b| a.name.cmp(&b.name));
    bodies.sort_unstable_by_key(|body| body.instance);
    Ok(exports)
}

fn layout(
    name: &str,
    target: &Body,
    input: &gir::concrete::ConcreteBody,
) -> Result<(Export, Vec<Lane>), Diagnostic> {
    let windows = target.target == "x86_64-windows";
    let result_ty = input.body.signature.result.0;
    let result = &input.types[result_ty as usize];
    let result_size = result.layout.expect("具体返回布局").size;
    let c_result = crate::build::foreign::classify(&input.types, result_ty, windows);
    let mut call = Call {
        target: CallTarget::Instance(target.instance),
        kind: gir::body::CallKind::ForeignBridge,
        parameters: Vec::new(),
        results: Vec::new(),
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: Vec::new(),
        foreign_signature: Some(crate::foreign::Signature {
            arguments: Vec::new(),
            result: crate::build::foreign::describe(&input.types, result_ty),
            variadic: None,
        }),
        foreign_arguments: Vec::new(),
        sret: None,
        poll_free_leaf: false,
    };
    let mut arguments = Vec::new();
    if c_result.memory {
        call.parameters.push(ValueType::pointer(Provenance::Raw));
        arguments.push(Lane {
            offset: SRET_OFFSET,
            bytes: 8,
            indirect: false,
        });
        call.sret = Some((0, result_size, result.key));
    }
    let mut locals = vec![0; input.body.locals.len()];
    let mut cursor = DATA_OFFSET;
    for (index, local) in input.body.locals.iter().enumerate() {
        if local.kind != gir::body::LocalKind::Argument {
            continue;
        }
        let ty = local.ty.0;
        call.foreign_signature
            .as_mut()
            .unwrap()
            .arguments
            .push(crate::build::foreign::describe(&input.types, ty));
        let value = &input.types[ty as usize];
        let physical = value.layout.expect("具体参数布局");
        cursor = cursor.next_multiple_of(physical.align.max(16));
        locals[index] = cursor;
        let c = crate::build::foreign::classify(&input.types, ty, windows);
        let first = id(arguments.len());
        if windows && c.memory {
            call.parameters.push(ValueType::pointer(Provenance::Raw));
            call.by_value.push((first, value.key, physical.size));
            arguments.push(Lane {
                offset: cursor,
                bytes: physical.size,
                indirect: true,
            });
        } else {
            for (offset, bytes, kind) in &c.lanes {
                call.parameters.push(*kind);
                arguments.push(Lane {
                    offset: cursor + offset,
                    bytes: *bytes,
                    indirect: false,
                });
            }
        }
        let lanes = id(arguments.len()) - first;
        if lanes != 0 {
            call.foreign_arguments.push(ForeignArgument {
                first,
                lanes,
                bytes: u32::try_from(physical.size).map_err(|_| invalid("C 参数过大"))?,
                align: c.align,
                memory: c.memory,
            });
        }
        cursor = cursor
            .checked_add(physical.size.next_multiple_of(16))
            .ok_or_else(|| invalid("C 导出参数包过大"))?;
    }
    let result_offset = cursor;
    let results = if c_result.memory {
        Vec::new()
    } else {
        c_result
            .lanes
            .iter()
            .map(|&(offset, bytes, kind)| {
                call.results.push(kind);
                Lane {
                    offset: result_offset + offset,
                    bytes,
                    indirect: false,
                }
            })
            .collect()
    };
    let packet_bytes = (cursor + result_size.next_multiple_of(16)).max(DATA_OFFSET);
    if packet_bytes > i32::MAX as u64 {
        return Err(invalid("C 导出参数包超过机器栈偏移"));
    }
    let mut internal = Vec::new();
    for (index, parameter) in target.params(target.entry).iter().skip(1).enumerate() {
        let lane = if index == 0 && target.signature.sret.is_some() {
            Lane {
                offset: result_offset,
                bytes: result_size,
                indirect: true,
            }
        } else {
            let (local, offset) = parameter
                .source
                .ok_or_else(|| invalid("C 导出参数缺少来源"))?;
            let offset = locals
                .get(local as usize)
                .ok_or_else(|| invalid("C 导出参数来源越界"))?
                + offset;
            Lane {
                offset,
                bytes: target.signature.parameters[index].ty.bytes().unwrap(),
                indirect: target
                    .signature
                    .by_value
                    .iter()
                    .any(|(at, _, _)| *at == index as u32),
            }
        };
        internal.push(lane);
    }
    Ok((
        Export {
            name: name.into(),
            adapter: key(&target.instance),
            call,
            arguments,
            results,
            result_offset,
            result_bytes: result_size,
            packet_bytes,
        },
        internal,
    ))
}
