//! 执行真实文件/资源函数的 LIR；地址是 Vec 下标，外部操作通过显式钩子注入。
mod a64_binding;
mod collections;
mod copy;
mod debug_builders;
mod docs_examples;
mod float_formatting;
mod formatting;
mod harness;
mod hashing;
mod network;
mod normalization;
mod platform;
mod printing;
mod process;
mod resource;
mod signal;
mod strings;
mod trace;

use crate::lir::body::{Body, Condition, IntOp, Op, Terminator, Type, range};
use crate::{Compilation, CompileRequest, Compiler, TargetName};
use std::cmp::Ordering;

fn named<'a>(output: &'a Compilation, name: &str) -> &'a Body {
    let module = output.hir.as_ref().unwrap().module();
    output
        .lir
        .as_ref()
        .unwrap()
        .bodies()
        .iter()
        .find(|body| {
            module
                .definitions
                .iter()
                .any(|definition| definition.key == body.owner && definition.name == name)
        })
        .expect("真实标准库函数")
}

fn runtime_body<'a>(output: &'a Compilation, module: &str, name: &str) -> &'a Body {
    let hir = output.hir.as_ref().unwrap().module();
    let path = format!("std/runtime/{module}.gg");
    let key = hir
        .definitions
        .iter()
        .find(|definition| {
            definition.name == name
                && definition
                    .location
                    .as_ref()
                    .is_some_and(|location| hir.sources[location.source as usize].path == path)
        })
        .expect("运行时定义")
        .key;
    output
        .lir
        .as_ref()
        .unwrap()
        .bodies()
        .iter()
        .find(|body| body.owner == key)
        .expect("运行时 LIR")
}

#[test]
fn native_file_errors_preserve_categories_and_unknown_codes() {
    for (target, cases) in [
        (
            TargetName::X86_64Linux,
            &[
                (1, 1),
                (13, 1),
                (18, 21),
                (17, 2),
                (30, 27),
                (28, 26),
                (9, 10),
                (20, 18),
                (21, 19),
                (39, 20),
            ][..],
        ),
        (
            TargetName::X86_64Windows,
            &[
                (5, 1),
                (32, 1),
                (33, 1),
                (1314, 1),
                (17, 21),
                (80, 2),
                (183, 2),
                (19, 27),
                (112, 26),
                (6, 10),
                (267, 18),
                (145, 20),
            ][..],
        ),
    ] {
        let output = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            "use std.io as io\nfn main() { _ = io.from_native_error(13) }",
            target,
        ));
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        let body = named(&output, "from_native_error");
        for &(code, tag) in cases.iter().chain(&[(u64::MAX, 9), (123456, 30)]) {
            let memory = execute(body, &[0, code]);
            assert_eq!(memory[0], tag, "{target:?} code={code}");
            if tag == 30 {
                assert_eq!(u64::from_le_bytes(memory[8..16].try_into().unwrap()), code);
                assert_eq!(memory[16], 1, "未知错误没有伪造来源链");
            }
        }
    }
}

#[test]
fn file_options_validate_every_combination_before_native_open() {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg", "use std.fs as fs\nuse std.path.{Path}\nfn main() { _ = fs.open(Path::from_string(\"unused\"), fs.OpenOptions::new()) }", TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let body = named(&output, "flags");
    for bits in 0..128u64 {
        let packed = (0..7).fold(0, |packed, field| {
            packed | ((bits >> field & 1) << (field * 8))
        });
        let memory = execute(body, &[0, packed]);
        let read = bits & 1 != 0;
        let write = bits & 2 != 0;
        let append = bits & 4 != 0;
        let truncate = bits & 8 != 0;
        let valid = (read || write || append)
            && (bits & 56 == 0 || write || append)
            && !(truncate && append);
        assert_eq!(memory[0] == 0, valid, "options={bits:07b}");
        if valid {
            let expected = if bits & 32 != 0 { bits & !8 } else { bits };
            assert_eq!(
                u64::from_le_bytes(memory[8..16].try_into().unwrap()),
                expected
            );
        } else {
            assert_eq!(memory[8], 3, "非法组合必须返回 InvalidInput");
        }
    }
}

#[test]
fn windows_nofollow_defers_truncation_until_handle_validation() {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg", "use std.fs as fs\nuse std.path.{Path}\nfn main() { _ = fs.open(Path::from_string(\"unused\"), fs.OpenOptions::new()) }", TargetName::X86_64Windows,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let body = named(&output, "open_creation");
    for bits in 0..128u64 {
        let memory = execute(body, &[bits]);
        let disposition = u64::from_le_bytes(memory[..8].try_into().unwrap());
        if bits & 32 != 0 {
            assert_eq!(disposition, 1, "create_new 必须排他创建");
        } else if bits & 64 == 0 {
            assert_eq!(
                disposition,
                if bits & 16 != 0 { 4 } else { 3 },
                "nofollow 不能使用 CREATE_ALWAYS/TRUNCATE_EXISTING"
            );
        } else {
            let expected = match (bits & 16 != 0, bits & 8 != 0) {
                (true, true) => 2,
                (true, false) => 4,
                (false, true) => 5,
                (false, false) => 3,
            };
            assert_eq!(disposition, expected);
        }
    }
}

/// 仅支持这些固定纯函数的操作集；额外调用或未登记操作直接失败。
fn execute(body: &Body, arguments: &[u64]) -> Vec<u8> {
    let mut memory = vec![0; 128];
    let returned = interpret(body, arguments, &mut memory, &mut |target, _, _| {
        panic!("纯函数不应调用外部目标：{target:?}")
    });
    for (index, value) in returned.into_iter().enumerate() {
        memory[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    memory
}

#[derive(Debug)]
enum Boundary<'a> {
    Call(&'a crate::lir::body::CallTarget),
    Machine(crate::runtime::RuntimeMachineOp),
    Platform(crate::runtime::platform_schema::PlatformOp),
    Storage(&'a Op),
}

type CallHook<'a> = dyn FnMut(Boundary<'_>, &[u64], &mut Vec<u8>) -> Vec<u64> + 'a;

fn interpret(
    body: &Body,
    arguments: &[u64],
    memory: &mut Vec<u8>,
    call: &mut CallHook<'_>,
) -> Vec<u64> {
    interpret_bounded(body, arguments, memory, call, 256)
}

fn interpret_bounded(
    body: &Body,
    arguments: &[u64],
    memory: &mut Vec<u8>,
    call: &mut CallHook<'_>,
    block_budget: usize,
) -> Vec<u64> {
    let frame_base = memory.len();
    let slots: Vec<_> = body
        .stack_slots
        .iter()
        .map(|slot| {
            let base = memory.len() as u64;
            memory.resize(memory.len() + slot.bytes as usize, 0);
            base
        })
        .collect();
    let mut values = vec![0u64; body.values.len()];
    let mut block = body.entry;
    assert_eq!(body.params(block).len() - 1, arguments.len());
    for (parameter, value) in body.params(block).iter().skip(1).zip(arguments) {
        values[parameter.value.index()] = *value;
    }
    for _ in 0..block_budget {
        for instruction in &body.instructions[range(&body.blocks[block.index()].instructions)] {
            let operands = body.args(&instruction.arguments);
            let args: Vec<_> = operands.iter().map(|value| values[value.index()]).collect();
            let value = match instruction.op {
                Op::StackCheck
                | Op::ScopedViewBegin { .. }
                | Op::ScopedViewEnd { .. }
                | Op::SafepointPoll { .. }
                | Op::RegionPublish { .. }
                | Op::RegionReset { .. }
                | Op::PromoteManaged { .. }
                | Op::RegionTransfer { .. } => continue,
                Op::Call(ref invoked) => {
                    let returned = call(Boundary::Call(&invoked.target), &args, memory);
                    assert_eq!(returned.len(), instruction.results.len());
                    values[range(&instruction.results)].copy_from_slice(&returned);
                    continue;
                }
                Op::RuntimeMachineCall(operation) => {
                    let returned = call(Boundary::Machine(operation), &args, memory);
                    assert_eq!(returned.len(), instruction.results.len());
                    values[range(&instruction.results)].copy_from_slice(&returned);
                    continue;
                }
                Op::PlatformCall(operation) => {
                    let returned = call(Boundary::Platform(operation), &args, memory);
                    assert_eq!(returned.len(), instruction.results.len());
                    values[range(&instruction.results)].copy_from_slice(&returned);
                    continue;
                }
                ref operation @ (Op::GcAlloc { .. }
                | Op::RegionAlloc { .. }
                | Op::SymbolAddr(_)
                | Op::GcWriteBarrier { .. }) => {
                    let returned = call(Boundary::Storage(operation), &args, memory);
                    assert_eq!(returned.len(), instruction.results.len());
                    values[range(&instruction.results)].copy_from_slice(&returned);
                    continue;
                }
                Op::IConst(value) | Op::FConst(value) => value,
                Op::Integer(IntOp::MulWide) => {
                    let product = u128::from(args[0]) * u128::from(args[1]);
                    values[range(&instruction.results)]
                        .copy_from_slice(&[product as u64, (product >> 64) as u64]);
                    continue;
                }
                Op::Integer(op @ (IntOp::AddCarry | IntOp::SubBorrow)) => {
                    let (value, first) = if op == IntOp::AddCarry {
                        args[0].overflowing_add(args[1])
                    } else {
                        args[0].overflowing_sub(args[1])
                    };
                    let (value, second) = if op == IntOp::AddCarry {
                        value.overflowing_add(args[2])
                    } else {
                        value.overflowing_sub(args[2])
                    };
                    values[range(&instruction.results)]
                        .copy_from_slice(&[value, u64::from(first || second)]);
                    continue;
                }
                Op::StackAddr(slot) => slots[slot.index()],
                Op::PtrOffset => args[0].wrapping_add(args[1]),
                Op::Convert(crate::lir::body::Conversion::SignExtend) => {
                    let bits = body.values[operands[0].index()].kind.ty.bytes().unwrap() * 8;
                    ((args[0] << (64 - bits)) as i64 >> (64 - bits)) as u64
                }
                Op::Convert(
                    crate::lir::body::Conversion::Truncate
                    | crate::lir::body::Conversion::ZeroExtend
                    | crate::lir::body::Conversion::PointerCast
                    | crate::lir::body::Conversion::PointerToInt
                    | crate::lir::body::Conversion::IntToPointer,
                ) => args[0],
                Op::Integer(op) => match op {
                    IntOp::Add => args[0].wrapping_add(args[1]),
                    IntOp::Sub => args[0].wrapping_sub(args[1]),
                    IntOp::Not => !args[0],
                    IntOp::Neg => args[0].wrapping_neg(),
                    IntOp::Xor => args[0] ^ args[1],
                    IntOp::RemUnsigned => args[0] % args[1],
                    IntOp::DivUnsigned => args[0] / args[1],
                    IntOp::RemSigned | IntOp::DivSigned => {
                        let left = i64::from_ne_bytes(args[0].to_ne_bytes());
                        let right = i64::from_ne_bytes(args[1].to_ne_bytes());
                        let value = if op == IntOp::RemSigned {
                            left.wrapping_rem(right)
                        } else {
                            left.wrapping_div(right)
                        };
                        u64::from_ne_bytes(value.to_ne_bytes())
                    }
                    IntOp::Mul => args[0].wrapping_mul(args[1]),
                    IntOp::And => args[0] & args[1],
                    IntOp::Or => args[0] | args[1],
                    IntOp::Shl => args[0].wrapping_shl((args[1] % 64) as u32),
                    IntOp::ShrUnsigned => args[0] >> (args[1] % 64),
                    IntOp::ShrSigned => ((args[0] as i64) >> (args[1] % 64)) as u64,
                    _ => panic!("未登记整数操作：{op:?}"),
                },
                Op::Compare { condition, signed } => {
                    let ordering = match body.values[operands[0].index()].kind.ty {
                        Type::F32 => f32::from_bits(args[0] as u32)
                            .partial_cmp(&f32::from_bits(args[1] as u32)),
                        Type::F64 => f64::from_bits(args[0]).partial_cmp(&f64::from_bits(args[1])),
                        _ if signed => Some((args[0] as i64).cmp(&(args[1] as i64))),
                        _ => Some(args[0].cmp(&args[1])),
                    };
                    u64::from(match condition {
                        Condition::Eq => ordering == Some(Ordering::Equal),
                        Condition::Ne => ordering != Some(Ordering::Equal),
                        Condition::Lt => ordering == Some(Ordering::Less),
                        Condition::Le => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
                        Condition::Gt => ordering == Some(Ordering::Greater),
                        Condition::Ge => {
                            matches!(ordering, Some(Ordering::Greater | Ordering::Equal))
                        }
                    })
                }
                Op::Store(_)
                | Op::Atomic {
                    op: crate::lir::body::AtomicOp::Store,
                    ..
                } => {
                    let bytes = body.values[operands[1].index()].kind.ty.bytes().unwrap() as usize;
                    let at = args[0] as usize;
                    memory[at..at + bytes].copy_from_slice(&args[1].to_le_bytes()[..bytes]);
                    continue;
                }
                Op::Load(_)
                | Op::Atomic {
                    op: crate::lir::body::AtomicOp::Load,
                    ..
                } => {
                    let bytes = body.values[instruction.results.start as usize]
                        .kind
                        .ty
                        .bytes()
                        .unwrap() as usize;
                    let at = args[0] as usize;
                    let mut word = [0; 8];
                    word[..bytes].copy_from_slice(&memory[at..at + bytes]);
                    u64::from_le_bytes(word)
                }
                Op::Atomic {
                    op: crate::lir::body::AtomicOp::Or,
                    ..
                } => {
                    let bytes = body.values[instruction.results.start as usize]
                        .kind
                        .ty
                        .bytes()
                        .unwrap() as usize;
                    let at = args[0] as usize;
                    let mut word = [0; 8];
                    word[..bytes].copy_from_slice(&memory[at..at + bytes]);
                    let old = u64::from_le_bytes(word);
                    memory[at..at + bytes].copy_from_slice(&(old | args[1]).to_le_bytes()[..bytes]);
                    old
                }
                Op::Memmove { bytes } | Op::Memcpy { bytes } => {
                    memory.copy_within(
                        args[1] as usize..args[1] as usize + bytes as usize,
                        args[0] as usize,
                    );
                    continue;
                }
                Op::Select => {
                    if args[0] != 0 {
                        args[1]
                    } else {
                        args[2]
                    }
                }
                Op::Memset { bytes } => {
                    memory[args[0] as usize..args[0] as usize + bytes as usize].fill(args[1] as u8);
                    continue;
                }
                ref op => panic!("纯函数解释器遇到未登记操作：{op:?}"),
            };
            let ty = body.values[instruction.results.start as usize].kind.ty;
            let bits = ty.bytes().unwrap() * 8;
            values[instruction.results.start as usize] = if bits < 64 {
                value & ((1 << bits) - 1)
            } else {
                value
            };
        }
        let edge = match &body.blocks[block.index()].terminator {
            Terminator::Invoke {
                call: invoked,
                arguments,
                results,
                normal,
                ..
            } => {
                let args: Vec<_> = body
                    .args(arguments)
                    .iter()
                    .map(|value| values[value.index()])
                    .collect();
                let returned = call(Boundary::Call(&invoked.target), &args, memory);
                assert_eq!(returned.len(), results.len());
                values[range(results)].copy_from_slice(&returned);
                *normal
            }
            Terminator::Jump(edge) => *edge,
            Terminator::Branch { condition, yes, no } => {
                if values[condition.index()] != 0 {
                    *yes
                } else {
                    *no
                }
            }
            Terminator::Switch {
                value,
                cases,
                otherwise,
            } => body.switch_cases[range(cases)]
                .iter()
                .find(|(case, _)| *case == values[value.index()])
                .map_or(*otherwise, |(_, edge)| *edge),
            Terminator::Return {
                values: returned, ..
            } => {
                let returned = body
                    .args(returned)
                    .iter()
                    .map(|value| values[value.index()])
                    .collect();
                memory.truncate(frame_base);
                return returned;
            }
            other => panic!("纯函数解释器遇到未登记终结符：{}: {other:?}", body.name),
        };
        let edge = &body.edges[edge.index()];
        let incoming: Vec<_> = body
            .args(&edge.arguments)
            .iter()
            .map(|value| values[value.index()])
            .collect();
        for (parameter, value) in body.params(edge.to).iter().zip(incoming) {
            values[parameter.value.index()] = value;
        }
        block = edge.to;
    }
    panic!(
        "固定函数 {:?} 未在 {block_budget} 个基本块预算内终止：块 {:?} 参数 {:?}",
        body.instance,
        block,
        body.params(block)
            .iter()
            .map(|p| values[p.value.index()])
            .collect::<Vec<_>>()
    )
}
