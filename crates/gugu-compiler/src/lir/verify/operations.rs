use super::{compatible, invalid};
use crate::Diagnostic;
use crate::frontend::gir::body::{CallKind, MemoryOrdering};
use crate::lir::body::{
    AtomicOp, Body, Call, CallTarget, Conversion, Definition, FloatOp, IntOp, Op, Provenance,
    RuntimeCall, Symbol, Terminator, Type, ValueId, ValueType, VectorOp, range,
};
use std::collections::BTreeSet;

pub(super) fn verify(body: &Body) -> Result<(), Diagnostic> {
    for instruction in &body.instructions {
        let args: Vec<_> = body
            .args(&instruction.arguments)
            .iter()
            .map(|value| body.values[value.index()].kind)
            .collect();
        let results: Vec<_> = body.values[range(&instruction.results)]
            .iter()
            .map(|value| value.kind)
            .collect();
        if results
            .iter()
            .any(|kind| matches!(kind.ty, Type::Mem | Type::Void))
        {
            return Err(invalid("Mem/Void 不能伪装成普通指令结果"));
        }
        let a: Vec<_> = args.iter().map(|kind| kind.ty).collect();
        let r: Vec<_> = results.iter().map(|kind| kind.ty).collect();
        let valid = match &instruction.op {
            Op::IConst(value) => {
                a.is_empty()
                    && results.len() == 1
                    && (r[0].integer() && integer_fits(*value, r[0])
                        || r[0] == Type::Ptr && *value == 0)
            }
            Op::FConst(value) => {
                a.is_empty()
                    && (r == [Type::F64] || r == [Type::F32] && *value <= u64::from(u32::MAX))
            }
            Op::SymbolAddr(symbol) => symbol_valid(symbol, &args, &results, body),
            Op::StackAddr(slot) => {
                a.is_empty()
                    && r == [Type::Ptr]
                    && slot.index() < body.stack_slots.len()
                    && results[0].provenance == Some(Provenance::Stack)
            }
            Op::PtrOffset => a == [Type::Ptr, Type::I64] && r == [Type::Ptr],
            Op::Integer(op) => match op {
                IntOp::AddCarry | IntOp::SubBorrow => {
                    a == [Type::I64, Type::I64, Type::I64] && r == [Type::I64, Type::I64]
                }
                IntOp::MulWide => a == [Type::I64, Type::I64] && r == [Type::I64, Type::I64],
                IntOp::Neg | IntOp::Not => a.len() == 1 && a[0].integer() && r == a,
                _ => a.len() == 2 && a[0].integer() && a[1] == a[0] && r == [a[0]],
            },
            Op::Float(op) => {
                a.len() == if *op == FloatOp::Neg { 1 } else { 2 }
                    && a.iter().all(|ty| *ty == a[0])
                    && matches!(a[0], Type::F32 | Type::F64)
                    && r == [a[0]]
            }
            Op::Compare { .. } => {
                a.len() == 2
                    && a[0] == a[1]
                    && (a[0].integer() || matches!(a[0], Type::F32 | Type::F64 | Type::Ptr))
                    && (r == [Type::I8] || r == [Type::Flags])
            }
            Op::Convert(conversion) => convert(*conversion, &a, &r),
            Op::Vector(operation) => vector(*operation, &a, &r),
            Op::Select => {
                a.len() == 3
                    && a[0] == Type::I8
                    && a[1] == a[2]
                    && r == [a[1]]
                    && compatible(results[0], args[1])
                    && compatible(results[0], args[2])
            }
            Op::TrapIf => a == [Type::I8] && r.is_empty(),
            Op::Load(access) => {
                a == [Type::Ptr]
                    && r.len() == 1
                    && r[0].bytes().is_some()
                    && access.align.is_power_of_two()
            }
            Op::Store(access) => {
                a.len() == 2
                    && a[0] == Type::Ptr
                    && a[1].bytes().is_some()
                    && r.is_empty()
                    && access.align.is_power_of_two()
            }
            Op::Memcpy { .. } | Op::Memmove { .. } => a == [Type::Ptr, Type::Ptr] && r.is_empty(),
            Op::Memset { .. } => a == [Type::Ptr, Type::I8] && r.is_empty(),
            Op::Atomic {
                op,
                ordering,
                failure,
                align,
            } => atomic(*op, *ordering, *failure, *align, &args, &results),
            Op::GcAlloc { align, .. } => {
                a == [Type::I64]
                    && r == [Type::Ptr]
                    && results[0].provenance == Some(Provenance::GcHeap)
                    && align.is_power_of_two()
            }
            Op::RegionAlloc { align, .. } => {
                a == [Type::I64]
                    && r == [Type::Ptr]
                    && results[0].provenance == Some(Provenance::GcHeap)
                    && align.is_power_of_two()
            }
            Op::PlatformCall(op) => platform_call_valid(*op, &args, &results),
            Op::RegionPublish => {
                a == [Type::Ptr]
                    && r.is_empty()
                    && body
                        .args(&instruction.arguments)
                        .first()
                        .is_some_and(|value| region_pointer(body, *value))
            }
            Op::MarkTicketBatch | Op::EdgeDeltaBatch | Op::ForwardSharedHandle => {
                r.is_empty()
                    && !a.is_empty()
                    && args.iter().all(|kind| {
                        !matches!(
                            kind.provenance,
                            Some(Provenance::GcHeap | Provenance::GcInterior | Provenance::Stack)
                        )
                    })
            }
            Op::RegionReset => {
                a == [Type::Ptr]
                    && r.is_empty()
                    && region_pointer(body, body.args(&instruction.arguments)[0])
            }
            Op::PromoteManaged => {
                a == [Type::Ptr]
                    && r == [Type::Ptr]
                    && args[0].provenance.is_some_and(Provenance::managed)
                    && results[0].provenance == Some(Provenance::GcHeap)
            }
            Op::ResolveSharedHandle | Op::DecodeCompressedRef => {
                a.len() == 1
                    && matches!(a[0], Type::Ptr | Type::I32 | Type::I64)
                    && r == [Type::Ptr]
                    && results[0].provenance.is_some_and(Provenance::managed)
            }
            Op::SharedAccessBegin { .. } => a == [Type::Ptr] && r.is_empty(),
            Op::SharedAccessEnd { .. } | Op::ScopedViewEnd { .. } => a.is_empty() && r.is_empty(),
            Op::ScopedViewBegin { .. } => a == [Type::Ptr] && r.is_empty(),
            Op::BarrierReserve(permit) => {
                a.is_empty() && r.is_empty() && permit.index() < body.barrier_permits.len()
            }
            Op::GcWriteBarrier { store } => {
                a.len() == 3
                    && a.iter().all(|ty| *ty == Type::Ptr)
                    && r.is_empty()
                    && store.index() < body.instructions.len()
            }
            Op::GcWriteBarrierReserved { store, permit } => {
                a.len() == 3
                    && a.iter().all(|ty| *ty == Type::Ptr)
                    && r.is_empty()
                    && store.index() < body.instructions.len()
                    && permit.index() < body.barrier_permits.len()
            }
            Op::SafepointPoll { .. }
            | Op::StackCheck
            | Op::CoroutineSwitch
            | Op::Park
            | Op::Ready
            | Op::CoverageCounter(_) => a.is_empty() && r.is_empty(),
            Op::NoSafepointBegin(region) | Op::NoSafepointEnd(region) => {
                a.is_empty()
                    && r.is_empty()
                    && usize::try_from(*region)
                        .is_ok_and(|index| index < body.no_safepoint_regions.len())
            }
            Op::Call(call) => {
                !call.may_unwind
                    && call.kind == CallKind::Managed
                    && call_valid(call, &args, &results)
            }
            Op::ForeignCall(call) => {
                !call.may_unwind
                    && call.kind != CallKind::Managed
                    && !matches!(call.target, CallTarget::Runtime(_))
                    && call_valid(call, &args, &results)
            }
            Op::InlineAsm(index) => {
                usize::try_from(*index).is_ok_and(|index| index < body.assembly.len())
                    && args.iter().chain(&results).all(|kind| {
                        !matches!(
                            kind.ty,
                            Type::Flags | Type::Mem | Type::Void | Type::V128(_)
                        )
                    })
            }
        };
        if !valid {
            return Err(invalid(&format!(
                "LIR 指令的类型、arity 或 effect 不合法：{:?} args={:?} results={:?}",
                instruction.op, args, results
            )));
        }
    }
    for block in &body.blocks {
        let valid = match &block.terminator {
            Terminator::Branch { condition, .. } => matches!(
                body.values[condition.index()].kind.ty,
                Type::I8 | Type::Flags
            ),
            Terminator::Switch { value, cases, .. } => {
                let ty = body.values[value.index()].kind.ty;
                let mut seen = BTreeSet::new();
                ty.integer()
                    && body.switch_cases[range(cases)]
                        .iter()
                        .all(|(value, _)| integer_fits(*value, ty) && seen.insert(*value))
            }
            Terminator::Invoke {
                call,
                arguments,
                results,
                normal,
                unwind,
                ..
            } => {
                let args = kinds(body, body.args(arguments));
                let results: Vec<_> = body.values[range(results)]
                    .iter()
                    .map(|value| value.kind)
                    .collect();
                call.may_unwind
                    && call_valid(call, &args, &results)
                    && !body.edges[normal.index()].unwind
                    && body.edges[unwind.index()].unwind
            }
            Terminator::Return { values, .. } => {
                let values = kinds(body, body.args(values));
                values.len() == body.signature.results.len()
                    && values
                        .iter()
                        .zip(&body.signature.results)
                        .all(|(actual, expected)| compatible(*expected, *actual))
            }
            Terminator::TailCall {
                call, arguments, ..
            } => {
                let args = kinds(body, body.args(arguments));
                !call.may_unwind
                    && call.sret.is_none()
                    && body.signature.sret.is_none()
                    && call.results == body.signature.results
                    && body.stack_slots.iter().all(|slot| slot.bytes == 0)
                    && !block.cleanup
                    && !matches!(
                        call.kind,
                        CallKind::ForeignBridge | CallKind::ForeignBridgeDirtyCpu
                    )
                    && call
                        .parameters
                        .iter()
                        .filter(|kind| !matches!(kind.ty, Type::F32 | Type::F64))
                        .count()
                        <= 9
                    && call
                        .parameters
                        .iter()
                        .filter(|kind| matches!(kind.ty, Type::F32 | Type::F64))
                        .count()
                        <= 8
                    && call_valid(call, &args, &body.signature.results)
            }
            _ => true,
        };
        if !valid {
            return Err(invalid("LIR 终结符、unwind 或调用 ABI 不合法"));
        }
    }
    for value in &body.values {
        if value.kind.ty != Type::Flags {
            continue;
        }
        let Definition::Instruction { instruction, .. } = value.definition else {
            return Err(invalid("Flags 只能由当前 block 的比较产生"));
        };
        if !matches!(
            body.instructions[instruction.index()].op,
            Op::Compare { .. }
        ) {
            return Err(invalid("非比较指令生成 Flags"));
        }
        for use_site in &body.uses[range(&value.uses)] {
            let crate::lir::body::UseSite::Terminator(block) = use_site.site else {
                return Err(invalid("Flags 跨指令或跨 block 传播"));
            };
            if body.blocks[block.index()].instructions.end != instruction.0 + 1
                || !matches!(
                    body.blocks[block.index()].terminator,
                    Terminator::Branch { .. }
                )
            {
                return Err(invalid("Flags 在分支前已被其它指令破坏"));
            }
        }
    }
    Ok(())
}

fn kinds(body: &Body, values: &[ValueId]) -> Vec<ValueType> {
    values
        .iter()
        .map(|value| body.values[value.index()].kind)
        .collect()
}
fn integer_fits(value: u64, ty: Type) -> bool {
    match ty {
        Type::I8 => value <= u64::from(u8::MAX),
        Type::I16 => value <= u64::from(u16::MAX),
        Type::I32 => value <= u64::from(u32::MAX),
        Type::I64 => true,
        _ => false,
    }
}
fn symbol_valid(symbol: &Symbol, args: &[ValueType], results: &[ValueType], body: &Body) -> bool {
    if !args.is_empty() || results.len() != 1 {
        return false;
    }
    match symbol {
        Symbol::TypeId(_) => results[0] == ValueType::scalar(Type::I32),
        Symbol::Instance(_) | Symbol::External { .. } => {
            results[0] == ValueType::pointer(Provenance::Code)
        }
        Symbol::TypeDescriptor(_) | Symbol::Vtable { .. } => {
            results[0] == ValueType::pointer(Provenance::Metadata)
        }
        Symbol::TypeRecords => results[0] == ValueType::pointer(Provenance::Metadata),
        Symbol::TypeNames => results[0] == ValueType::pointer(Provenance::GcHeap),
        Symbol::Global { .. } => {
            results[0].ty == Type::Ptr
                && matches!(
                    results[0].provenance,
                    Some(Provenance::Foreign | Provenance::GcInterior)
                )
        }
        Symbol::Data(index) => {
            usize::try_from(*index).is_ok_and(|index| index < body.data.len())
                && results[0].ty == Type::Ptr
                && matches!(
                    results[0].provenance,
                    Some(Provenance::Raw | Provenance::Metadata | Provenance::GcHeap)
                )
        }
    }
}
fn convert(conversion: Conversion, args: &[Type], results: &[Type]) -> bool {
    if args.len() != 1 || results.len() != 1 {
        return false;
    }
    let (source, target) = (args[0], results[0]);
    match conversion {
        Conversion::SignExtend | Conversion::ZeroExtend => {
            source.integer() && target.integer() && source.bytes() < target.bytes()
        }
        Conversion::Truncate => {
            source.integer() && target.integer() && source.bytes() > target.bytes()
        }
        Conversion::IntToFloat { .. } => {
            source.integer() && matches!(target, Type::F32 | Type::F64)
        }
        Conversion::FloatToInt { .. } => {
            matches!(source, Type::F32 | Type::F64) && target.integer()
        }
        Conversion::FloatResize => matches!(
            (source, target),
            (Type::F32, Type::F64) | (Type::F64, Type::F32)
        ),
        Conversion::Bitcast => {
            source.bytes().is_some()
                && source.bytes() == target.bytes()
                && source != Type::Ptr
                && target != Type::Ptr
        }
        Conversion::PointerToInt => source == Type::Ptr && target.integer(),
        Conversion::IntToPointer => source.integer() && target == Type::Ptr,
        Conversion::PointerCast | Conversion::RawToReference => {
            source == Type::Ptr && target == Type::Ptr
        }
    }
}
fn vector(operation: VectorOp, args: &[Type], results: &[Type]) -> bool {
    if results.len() != 1 {
        return false;
    }
    match operation {
        VectorOp::Splat => {
            args.len() == 1
                && args[0].bytes().is_some()
                && args[0] != Type::Ptr
                && matches!(results[0], Type::V128(_))
        }
        VectorOp::Extract(index) => {
            args.len() == 1
                && matches!(args[0], Type::V128(_))
                && results[0]
                    .bytes()
                    .is_some_and(|size| u64::from(index) < 16 / size)
                && results[0] != Type::Ptr
        }
        VectorOp::Insert(index) => {
            args.len() == 2
                && matches!(args[0], Type::V128(_))
                && results == [args[0]]
                && args[1]
                    .bytes()
                    .is_some_and(|size| u64::from(index) < 16 / size)
                && args[1] != Type::Ptr
        }
        VectorOp::Shuffle(mask) => {
            args.len() == 2
                && args[0] == args[1]
                && matches!(args[0], Type::V128(_))
                && results == [args[0]]
                && mask.iter().all(|lane| *lane < 32)
        }
        VectorOp::ReduceAdd => {
            args.len() == 1
                && matches!(
                    args[0],
                    Type::V128(
                        crate::lir::body::Lane::I8
                            | crate::lir::body::Lane::I16
                            | crate::lir::body::Lane::I32
                            | crate::lir::body::Lane::I64
                    )
                )
                && results[0].integer()
        }
        _ => {
            args.len() == 2
                && args[0] == args[1]
                && matches!(args[0], Type::V128(_))
                && results == [args[0]]
        }
    }
}

fn atomic(
    op: AtomicOp,
    ordering: MemoryOrdering,
    failure: Option<MemoryOrdering>,
    align: u32,
    args: &[ValueType],
    results: &[ValueType],
) -> bool {
    use MemoryOrdering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
    let order = match op {
        AtomicOp::Load => matches!(ordering, Relaxed | Acquire | SeqCst),
        AtomicOp::Store => matches!(ordering, Relaxed | Release | SeqCst),
        AtomicOp::Fence => ordering != Relaxed,
        _ => true,
    };
    if !order {
        return false;
    }
    if op == AtomicOp::Fence {
        return args.is_empty() && results.is_empty() && failure.is_none();
    }
    if op == AtomicOp::CompareExchange {
        let valid = match failure {
            Some(Relaxed) => true,
            Some(Acquire) => matches!(ordering, Acquire | AcqRel | SeqCst),
            Some(SeqCst) => ordering == SeqCst,
            _ => false,
        };
        if !valid {
            return false;
        }
    } else if failure.is_some() {
        return false;
    }
    let Some(pointer) = args.first() else {
        return false;
    };
    if pointer.ty != Type::Ptr {
        return false;
    }
    let value = if op == AtomicOp::Load {
        results.first()
    } else {
        args.get(1)
    };
    let Some(value) = value else {
        return false;
    };
    if !(value.ty.integer() || value == &ValueType::pointer(Provenance::Raw))
        || !align.is_power_of_two()
        || value
            .ty
            .bytes()
            .is_none_or(|bytes| bytes > 8 || u64::from(align) < bytes)
    {
        return false;
    }
    match op {
        AtomicOp::Load => args.len() == 1 && results == [*value],
        AtomicOp::Store => args.len() == 2 && results.is_empty(),
        AtomicOp::CompareExchange => {
            args.len() == 3
                && args[1] == args[2]
                && results == [*value, ValueType::scalar(Type::I8)]
        }
        AtomicOp::Fence => false,
        _ => args.len() == 2 && results == [*value],
    }
}

fn region_pointer(body: &Body, mut value: ValueId) -> bool {
    for _ in 0..body.values.len() {
        let current = &body.values[value.index()];
        match current.origin {
            crate::lir::body::Origin::Derived(base) => value = base,
            crate::lir::body::Origin::Allocation(_) => {
                let Definition::Instruction { instruction, .. } = current.definition else {
                    return false;
                };
                return matches!(
                    body.instructions[instruction.index()].op,
                    Op::RegionAlloc { .. }
                );
            }
            _ => return false,
        }
    }
    false
}

/// 平台范围调用的结构规则。
///
/// 实参与结果只允许平台无关的整数、布尔或 `Raw`/`Metadata` provenance：平台调用不得携带
/// 受管引用穿过 runtime 边界，也不得把 `Mem`/`Void` 当作普通值。状态操作没有结果，查询返回
/// 一个标量，`entropy` 返回 `Raw` 指针与长度。
fn platform_call_valid(
    op: crate::runtime::PlatformOp,
    args: &[ValueType],
    results: &[ValueType],
) -> bool {
    use crate::runtime::PlatformOp as Kind;
    let arguments_ok = args.iter().all(|kind| {
        matches!(kind.ty, Type::I64 | Type::I32)
            && !matches!(
                kind.provenance,
                Some(Provenance::GcHeap | Provenance::GcInterior | Provenance::Stack)
            )
    });
    if !arguments_ok {
        return false;
    }
    match op {
        // 预留返回一个稳定 range 编号；参数是字节数与对齐，domain 由调用上下文决定。
        Kind::ReserveAligned => {
            args.len() == 2
                && results.len() == 1
                && results[0].ty == Type::I64
                && results[0].provenance.is_none()
        }
        // `wake` 返回实际释放的等待者数。
        Kind::Wake => {
            args.len() == 2
                && results.len() == 1
                && results[0].ty == Type::I64
                && results[0].provenance.is_none()
        }
        // `wait` 返回布尔；布尔在 LIR 中是 `I8` 标量。
        Kind::Wait => {
            args.len() == 2
                && results.len() == 1
                && results[0].ty == Type::I8
                && results[0].provenance.is_none()
        }
        // `low_memory_hint` 是无参数查询。
        Kind::LowMemoryHint => {
            args.is_empty()
                && results.len() == 1
                && results[0].ty == Type::I8
                && results[0].provenance.is_none()
        }
        // `entropy` 返回平台所有的 `Raw` 字节指针；长度由调用方请求时已知。
        Kind::Entropy => {
            args.len() == 1
                && results.len() == 1
                && results[0].ty == Type::Ptr
                && results[0].provenance == Some(Provenance::Raw)
        }
        Kind::Commit
        | Kind::Decommit
        | Kind::Release
        | Kind::ProtectGuard
        | Kind::Unprotect
        | Kind::Zero
        | Kind::HugePageHint => args.len() == 1 && results.is_empty(),
        Kind::SetDumpPolicy => args.len() == 2 && results.is_empty(),
    }
}

fn resource_call_valid(args: &[ValueType], results: &[ValueType]) -> bool {
    args.len() == 2
        && results.is_empty()
        && args[0].ty == Type::Ptr
        && matches!(
            args[0].provenance,
            Some(Provenance::GcHeap | Provenance::GcInterior | Provenance::Stack)
        )
        && args[1] == ValueType::pointer(Provenance::Metadata)
}

fn call_valid(call: &Call, args: &[ValueType], results: &[ValueType]) -> bool {
    if call.parameters.len() != args.len()
        || call.results.len() != results.len()
        || call
            .parameters
            .iter()
            .zip(args)
            .any(|(expected, actual)| !compatible(*expected, *actual))
        || call
            .results
            .iter()
            .zip(results)
            .any(|(expected, actual)| !compatible(*expected, *actual))
        || args.iter().chain(results).any(|kind| {
            matches!(
                kind.ty,
                Type::V128(_) | Type::Flags | Type::Mem | Type::Void
            )
        })
    {
        return false;
    }
    if matches!(call.target, CallTarget::Runtime(_)) && call.kind != CallKind::Managed {
        return false;
    }
    if call.kind != CallKind::Managed
        && args.iter().chain(results).any(|kind| {
            matches!(
                kind.provenance,
                Some(Provenance::GcHeap | Provenance::GcInterior | Provenance::Stack)
            )
        })
    {
        return false;
    }
    if matches!(call.kind, CallKind::ForeignLeaf { .. }) && (call.may_suspend || call.may_allocate)
    {
        return false;
    }
    if let CallTarget::Runtime(runtime) = call.target {
        let effects = runtime.effects();
        if (
            call.may_unwind,
            call.may_suspend,
            call.may_allocate,
            call.captures_arguments,
        ) != effects
        {
            return false;
        }
        if runtime == RuntimeCall::ValueTransfer
            && (args.len() != 3
                || args[0].ty != Type::Ptr
                || args[1].ty != Type::Ptr
                || args[2] != ValueType::pointer(Provenance::Metadata))
        {
            return false;
        }
        if matches!(
            runtime,
            RuntimeCall::ResourceAcquire
                | RuntimeCall::ResourceRelease
                | RuntimeCall::ResourceTransfer
                | RuntimeCall::ResourceFinalize
        ) && !resource_call_valid(args, results)
        {
            return false;
        }
    }
    if call.by_value.iter().any(|(index, _, _)| {
        args.get(usize::try_from(*index).expect("参数编号"))
            .is_none_or(|kind| kind.ty != Type::Ptr)
    }) {
        return false;
    }
    if call.sret.is_some_and(|(index, _, _)| {
        args.get(usize::try_from(index).expect("sret 编号"))
            .is_none_or(|kind| kind.ty != Type::Ptr)
    }) {
        return false;
    }
    !matches!(call.target, CallTarget::Indirect)
        || args
            .iter()
            .any(|kind| kind.provenance == Some(Provenance::Code))
}
