//! Private direct calls without checked-entry or safepoint obligations.
use super::Builder;
use crate::{
    a64::{Error, abi, machine::Item, machine::VtableDispatch},
    gir::body::CallKind,
    lir::body::{Body, Call, CallTarget, Provenance, Type, ValueId},
};

pub fn requires_maps(call: &Call) -> bool {
    if matches!(
        call.kind,
        CallKind::ForeignBridge | CallKind::ForeignBridgeDirtyCpu
    ) {
        return true;
    }
    call.kind == CallKind::Managed
        && (!call.poll_free_leaf
            || call.may_unwind
            || call.may_suspend
            || call.may_allocate
            || call
                .parameters
                .iter()
                .chain(&call.results)
                .any(|kind| kind.provenance.is_some_and(|p| p.managed())))
}

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Isolated,
    Runtime(Option<(u32, u32)>),
}

pub fn lower(
    body: &Body,
    call: &Call,
    args: &[ValueId],
    results: &[ValueId],
    tail: bool,
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    lower_mode(
        body,
        call,
        args,
        results,
        CallTail {
            tail,
            mode: Mode::Isolated,
            landing: None,
        },
        b,
    )
}

/// 调用的尾态描述：是否尾调用、调用模式与 invoke 异常落点。
pub(crate) struct CallTail {
    pub tail: bool,
    pub mode: Mode,
    pub landing: Option<(u32, u32)>,
}

pub(crate) fn lower_mode(
    body: &Body,
    call: &Call,
    args: &[ValueId],
    results: &[ValueId],
    exit: CallTail,
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let CallTail {
        tail,
        mode,
        landing,
    } = exit;
    let leaf = matches!(call.kind, CallKind::ForeignLeaf { .. });
    if landing.is_some() && !matches!(mode, Mode::Runtime(Some(_))) && !leaf {
        return Err(crate::a64::error(
            "A64 invoke requires a CallReturn safepoint anchor",
        ));
    }
    let runtime = matches!(mode, Mode::Runtime(_));
    if call.kind != CallKind::Managed {
        if leaf {
            // leaf 直调：当前协程栈上按 C ABI 发调用，无安全点记录、不能
            // 挂起/分配；may_unwind 是前端的保守标记，C 目标不会真正
            // unwind 到托管帧，invoke 的落点边是死边，不参与发射。
            if tail
                || call.may_suspend
                || call.may_allocate
                || call.captures_arguments
                || matches!(
                    call.target,
                    CallTarget::Runtime(_) | CallTarget::Vtable { .. }
                )
                || args
                    .iter()
                    .map(|id| body.values[id.index()].kind)
                    .collect::<Vec<_>>()
                    != call.parameters
                || results
                    .iter()
                    .map(|id| body.values[id.index()].kind)
                    .collect::<Vec<_>>()
                    != call.results
            {
                return Err(crate::a64::error(
                    "invalid A64 foreign leaf signature/effects",
                ));
            }
            let platform = crate::a64::foreign_packet::platform(body)?;
            crate::a64::foreign_packet::plan(call, platform)?;
            b.push(Item::ForeignLeaf {
                call: Box::new(call.clone()),
                platform,
                arguments: args.iter().map(|id| id.0).collect(),
                results: results.iter().map(|id| id.0).collect(),
            });
            for result in results {
                if abi::class(body.values[result.index()].kind.ty) == abi::FPR {
                    continue;
                }
                let bits = super::bits(body.values[result.index()].kind.ty)?;
                if bits < 64 {
                    b.normalize(result.0, result.0, bits, false)?;
                }
            }
            return Ok(());
        }
        if !matches!(
            call.kind,
            CallKind::ForeignBridge | CallKind::ForeignBridgeDirtyCpu
        ) {
            return Err(crate::a64::error(
                "A64 ForeignLeaf requires bounded native-stack leaf integration",
            ));
        }
        let Mode::Runtime(Some(safepoint)) = mode else {
            return Err(crate::a64::error(
                "A64 foreign bridge requires an authoritative suspension map",
            ));
        };
        if tail
            || call.poll_free_leaf
            || call.may_unwind
            || call.captures_arguments
            || args
                .iter()
                .map(|id| body.values[id.index()].kind)
                .collect::<Vec<_>>()
                != call.parameters
            || results
                .iter()
                .map(|id| body.values[id.index()].kind)
                .collect::<Vec<_>>()
                != call.results
        {
            return Err(crate::a64::error(
                "invalid A64 foreign bridge signature/effects",
            ));
        }
        let platform = crate::a64::foreign_packet::platform(body)?;
        crate::a64::foreign_packet::plan(call, platform)?;
        b.push(Item::ForeignCall {
            call: Box::new(call.clone()),
            platform,
            arguments: args.iter().map(|id| id.0).collect(),
            results: results.iter().map(|id| id.0).collect(),
            safepoint,
        });
        return Ok(());
    }
    if requires_maps(call) && !runtime {
        return Err(crate::a64::error(
            "requires checked entry / stack maps (4c)",
        ));
    }
    let (target, callee, dispatch) = match &call.target {
        CallTarget::Instance(key) => (abi::symbol(key), None, None),
        CallTarget::Runtime(_) => (
            b.runtime_catalog
                .ok_or_else(|| {
                    crate::a64::error("A64 runtime call requires a canonical source catalog")
                })?
                .call_target(call)?,
            None,
            None,
        ),
        CallTarget::Indirect => {
            // Code-provenance 参数是 callee：不占 ABI 槽，发射时物化到 x8 后 `blr`（x16/x17 是并行拷贝组的破环/槽间暂存）。
            let codes = call
                .parameters
                .iter()
                .enumerate()
                .filter(|(_, p)| p.provenance == Some(Provenance::Code))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let &[position] = codes.as_slice() else {
                return Err(crate::a64::error(
                    "A64 indirect call requires exactly one Code callee",
                ));
            };
            (String::new(), Some((args[position].0, position)), None)
        }
        CallTarget::Vtable { slot } => (
            String::new(),
            None,
            Some(vtable_dispatch(call, args, *slot)?),
        ),
        _ => {
            return Err(crate::a64::error(
                "only direct Instance or bound runtime calls supported",
            ));
        }
    };
    if tail && (callee.is_some() || dispatch.is_some()) {
        return Err(crate::a64::error("A64 indirect tail call unsupported"));
    }
    // captures_arguments 只约束 verify 层的逃逸检查；对 codegen 而言实参按
    // 同一 ABI 落位即可（callee 在调用期间自行保存值）。
    if !call.foreign_arguments.is_empty() {
        return Err(crate::a64::error("capturing/foreign call unsupported"));
    }
    // by_value/sret 参数是 caller 侧临时副本/结果缓冲的地址：一个 Ptr 车道，聚合读写
    // 由 LIR 自身完成，与 x64 内部 ABI 同层语义。
    let pointer_lane = |index: u32| {
        call.parameters
            .get(index as usize)
            .is_none_or(|p| p.ty != Type::Ptr)
    };
    if call.by_value.iter().any(|(index, ..)| pointer_lane(*index))
        || call.sret.is_some_and(|(index, ..)| pointer_lane(index))
    {
        return Err(crate::a64::error("invalid by_value/sret pointer lane"));
    }
    // 与 LIR verify 同一兼容契约：GcHeap/Stack 可传给 GcInterior，Raw 可传给 Foreign。
    let compatible = |ids: &[ValueId], expected: &[crate::lir::body::ValueType]| {
        ids.len() == expected.len()
            && ids.iter().zip(expected).all(|(id, expected)| {
                crate::lir::verify::compatible(*expected, body.values[id.index()].kind)
            })
    };
    if !compatible(args, &call.parameters) || (!tail && !compatible(results, &call.results)) {
        return Err(crate::a64::error("call argument/result signature mismatch"));
    }
    abi::results(call.results.iter().map(|p| p.ty))?;
    for kind in call.parameters.iter().chain(&call.results) {
        if abi::class(kind.ty) == abi::GPR {
            super::bits(kind.ty)?;
        }
        if kind.provenance.is_some_and(|p| p.managed()) && !runtime {
            return Err(crate::a64::error(
                "requires checked entry / stack maps (4c)",
            ));
        }
    }
    if let Some((index, bytes, _)) = call.sret
        && (index != 0
            || bytes == 0
            || !call.results.is_empty()
            || call.parameters.first().is_none_or(|p| p.ty != Type::Ptr))
    {
        return Err(crate::a64::error("invalid hidden sret pointer/signature"));
    }
    let arguments = args
        .iter()
        .enumerate()
        .filter(|(index, _)| callee.is_none_or(|(_, position)| position != *index))
        .map(|(_, id)| id.0)
        .collect();
    if tail {
        if call.sret.is_some()
            || body.signature.sret.is_some()
            || call.results != body.signature.results
            || body.stack_slots.iter().any(|slot| slot.bytes != 0)
            || abi::arguments(call.parameters.iter().map(|p| p.ty))?.1
                > abi::arguments(body.signature.parameters.iter().map(|p| p.ty))?.1
            || call
                .parameters
                .iter()
                .any(|p| p.provenance == Some(crate::lir::body::Provenance::Stack))
        {
            return Err(crate::a64::error(
                "tail call requires reusable incoming area, matching results and no local stack escape/sret",
            ));
        }
        b.push(Item::TailCall { target, arguments });
    } else {
        b.push(Item::Call {
            suspend: call.may_suspend,
            target,
            callee: callee.map(|(value, _)| value),
            dispatch,
            arguments,
            results: results.iter().map(|id| id.0).collect(),
            safepoint: match mode {
                Mode::Isolated => None,
                Mode::Runtime(site) => site,
            },
            landing,
        });
        for result in results {
            if abi::class(body.values[result.index()].kind.ty) == abi::FPR {
                continue;
            }
            let bits = super::bits(body.values[result.index()].kind.ty)?;
            if bits < 64 {
                b.normalize(result.0, result.0, bits, false)?;
            }
        }
    }
    Ok(())
}

/// `dyn` 值布局是「`data@0`、`vtable@8`」：参数带 `Metadata` provenance 时该 lane 本身就是
/// vtable（`vtable_offset = 0`）；接收者是指向胖对的指针时 vtable 在 `+8`。两者都只依赖
/// LIR 可见事实，与 x64 `dispatch_source` 同层语义。
fn vtable_dispatch(call: &Call, args: &[ValueId], slot: u32) -> Result<VtableDispatch, Error> {
    let slot_offset = slot
        .checked_mul(8)
        .filter(|offset| *offset <= 32_760)
        .ok_or_else(|| crate::a64::error("A64 vtable slot out of immediate range"))?;
    let sret = call.sret.map(|(index, ..)| index);
    if let Some(index) = call
        .parameters
        .iter()
        .position(|p| p.provenance == Some(Provenance::Metadata))
    {
        return args
            .get(index)
            .map(|id| VtableDispatch {
                receiver: id.0,
                vtable_offset: 0,
                slot_offset,
            })
            .ok_or_else(|| crate::a64::error("A64 vtable dispatch lane missing"));
    }
    call.parameters
        .iter()
        .enumerate()
        .find(|(index, p)| {
            Some(u32::try_from(*index).unwrap_or(u32::MAX)) != sret && p.ty == Type::Ptr
        })
        .and_then(|(index, _)| args.get(index))
        .map(|id| VtableDispatch {
            receiver: id.0,
            vtable_offset: 8,
            slot_offset,
        })
        .ok_or_else(|| crate::a64::error("A64 vtable dispatch receiver missing"))
}
