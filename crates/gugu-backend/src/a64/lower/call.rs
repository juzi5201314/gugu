//! Private direct calls without checked-entry or safepoint obligations.
use super::Builder;
use crate::{
    a64::{Error, abi, machine::Item},
    gir::body::CallKind,
    lir::body::{Body, Call, CallTarget, Type, ValueId},
};

pub fn requires_maps(call: &Call) -> bool {
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
    lower_mode(body, call, args, results, tail, Mode::Isolated, b)
}

pub(crate) fn lower_mode(
    body: &Body,
    call: &Call,
    args: &[ValueId],
    results: &[ValueId],
    tail: bool,
    mode: Mode,
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let runtime = matches!(mode, Mode::Runtime(_));
    if call.kind != CallKind::Managed {
        return Err(crate::a64::error(
            "only internal managed direct calls supported",
        ));
    }
    if requires_maps(call) && !runtime {
        return Err(crate::a64::error(
            "requires checked entry / stack maps (4c)",
        ));
    }
    let CallTarget::Instance(key) = &call.target else {
        return Err(crate::a64::error("only direct Instance calls supported"));
    };
    if call.captures_arguments || !call.by_value.is_empty() || !call.foreign_arguments.is_empty() {
        return Err(crate::a64::error(
            "capturing/aggregate/foreign call unsupported",
        ));
    }
    let kinds = |ids: &[ValueId]| {
        ids.iter()
            .map(|id| body.values[id.index()].kind)
            .collect::<Vec<_>>()
    };
    if kinds(args) != call.parameters
        || (!tail && kinds(results) != call.results)
        || call.results.len() > 2
    {
        return Err(crate::a64::error("call argument/result signature mismatch"));
    }
    for kind in call.parameters.iter().chain(&call.results) {
        super::bits(kind.ty)?;
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
    let arguments = args.iter().map(|id| id.0).collect();
    if tail {
        if call.sret.is_some()
            || body.signature.sret.is_some()
            || call.results != body.signature.results
            || body.stack_slots.iter().any(|slot| slot.bytes != 0)
            || args.len().saturating_sub(8) > body.signature.parameters.len().saturating_sub(8)
            || call
                .parameters
                .iter()
                .any(|p| p.provenance == Some(crate::lir::body::Provenance::Stack))
        {
            return Err(crate::a64::error(
                "tail call requires reusable incoming area, matching results and no local stack escape/sret",
            ));
        }
        b.push(Item::TailCall {
            target: abi::symbol(key),
            arguments,
        });
    } else {
        b.push(Item::Call {
            suspend: call.may_suspend,
            target: abi::symbol(key),
            arguments,
            results: results.iter().map(|id| id.0).collect(),
            safepoint: match mode {
                Mode::Isolated => None,
                Mode::Runtime(site) => site,
            },
        });
        for result in results {
            let bits = super::bits(body.values[result.index()].kind.ty)?;
            if bits < 64 {
                b.normalize(result.0, result.0, bits, false)?;
            }
        }
    }
    Ok(())
}
