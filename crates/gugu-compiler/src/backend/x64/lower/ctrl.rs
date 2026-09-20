//! 换栈片段与终结符 lowering。

use crate::backend::x64::abi::{self, AbiLayout, AbiSlot};
use crate::backend::x64::inst::{LabelId, Operand, RelocKind, RelocTarget};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::{Gpr, Reg, Xmm};
use crate::backend::x64::table::{Access, OperandKind};
use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{BlockId, Body, Call, CallTarget, Symbol, Terminator, Type, ValueId};
use crate::target::TargetName;

use super::{Builder, LoweringError, SiteValue, gpr, move_kinds, reg};

const REL32: &[OperandKind] = &[OperandKind::Rel32];
const RM64: &[OperandKind] = &[OperandKind::Rm64];
const XMMRM_XMM: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::Xmm];

/// 复用与 `ContextSwitchCode::fixed()` 同源的 62-byte 片段。
pub(super) fn coroutine_switch(builder: &mut Builder) -> Result<(), LoweringError> {
    // rdi=保存位置，rsi=恢复位置，rdx=CoroutineHot*，rcx=LogicalProcessor*。
    // 片段本身写 r14/r15，属于 runtime ABI，不走普通序列的保留寄存器检查。
    // 选指站点用 call 进入同一符号，避免在普通函数体里直接写 r14/r15。
    let symbol = Symbol::External {
        key: crate::frontend::mono::keys::hash_domain(
            "gugu-runtime-symbol-v1",
            b"coroutine_switch",
        ),
        name: mangle::mangle_runtime("coroutine_switch"),
    };
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}

/// 终结符序列。Jump 的 fallthrough 由布局阶段省略。
pub(crate) fn terminator(
    body: &Body,
    terminator: &Terminator,
    values: impl Fn(&[ValueId]) -> Vec<SiteValue>,
    target: TargetName,
    universe: &TypeUniverse,
    builder: &mut Builder,
    emit_jump: bool,
    invert_branch: bool,
    block_label: impl Fn(BlockId) -> LabelId,
    from: BlockId,
) -> Result<(), LoweringError> {
    match terminator {
        Terminator::Jump(edge) => {
            if emit_jump {
                emit_to(builder, "jmp", block_label(body.edges[edge.index()].to))?;
            }
            Ok(())
        }
        Terminator::Branch {
            condition, yes, no, ..
        } => {
            let cond = values(&[*condition]);
            let [value] = cond.as_slice() else {
                return Err(LoweringError::InvalidOperands);
            };
            builder.emit(
                "test",
                super::wide_kinds(8),
                Access::Read,
                vec![reg(value.reg), reg(value.reg)],
            );
            let (taken, fall) = if invert_branch {
                (*no, *yes)
            } else {
                (*yes, *no)
            };
            let mnemonic = if invert_branch { "je" } else { "jne" };
            emit_to(builder, mnemonic, block_label(body.edges[taken.index()].to))?;
            if emit_jump {
                emit_to(builder, "jmp", block_label(body.edges[fall.index()].to))?;
            }
            Ok(())
        }
        Terminator::Switch {
            value,
            cases,
            otherwise,
        } => {
            let scrutinee = values(&[*value]);
            let [scrutinee] = scrutinee.as_slice() else {
                return Err(LoweringError::InvalidOperands);
            };
            for (imm, edge) in &body.switch_cases[crate::lir::body::range(cases)] {
                builder.emit(
                    "cmp",
                    &[OperandKind::Rm64, OperandKind::Imm32],
                    Access::Read,
                    vec![reg(scrutinee.reg), super::imm(*imm)],
                );
                emit_to(builder, "je", block_label(body.edges[edge.index()].to))?;
            }
            emit_to(
                builder,
                "jmp",
                block_label(body.edges[otherwise.index()].to),
            )?;
            Ok(())
        }
        Terminator::Invoke {
            call,
            arguments,
            results,
            unwind,
            ..
        } => {
            let args = values(body.args(arguments));
            let dests = result_values(body, results);
            emit_managed_or_foreign(call, &args, &dests, target, universe, builder)?;
            let _ = unwind;
            Ok(())
        }
        Terminator::Return { values: range, .. } => {
            let returned = values(body.args(range));
            let layout = abi::classify_signature(&body.signature, universe)?;
            move_returns(&returned, &layout, builder)?;
            builder.emit("ret", &[], Access::Read, Vec::new());
            Ok(())
        }
        Terminator::TailCall {
            call, arguments, ..
        } => {
            let args = values(body.args(arguments));
            shuffle_call(call, &args, target, universe, builder)?;
            jump_callee(call, builder)?;
            Ok(())
        }
        Terminator::ResumePanic { .. } | Terminator::Trap { .. } => {
            builder.emit(
                "jmp",
                REL32,
                Access::Read,
                vec![super::cold_branch(
                    crate::backend::x64::inst::ColdEdgeKind::Trap,
                    &body.blocks[body.entry.index()].source,
                    1 << 31 | from.0,
                )],
            );
            Ok(())
        }
        Terminator::Unreachable { .. } => {
            builder.emit("ud2", &[], Access::Read, Vec::new());
            Ok(())
        }
    }
}

fn result_values(body: &Body, results: &std::ops::Range<u32>) -> Vec<SiteValue> {
    crate::lir::body::range(results)
        .map(|index| SiteValue {
            ty: body.values[index].kind,
            reg: Reg::Virtual(crate::lir::body::id(index)),
        })
        .collect()
}

fn emit_to(
    builder: &mut Builder,
    mnemonic: &'static str,
    label: LabelId,
) -> Result<(), LoweringError> {
    builder.emit(mnemonic, REL32, Access::Read, vec![Operand::Label(label)]);
    Ok(())
}

fn emit_managed_or_foreign(
    call: &Call,
    args: &[SiteValue],
    results: &[SiteValue],
    target: TargetName,
    universe: &TypeUniverse,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    shuffle_call(call, args, target, universe, builder)?;
    call_target(call, builder)?;
    let layout = abi::classify_call(call, target, universe)?;
    move_returns(results, &layout, builder)
}

fn shuffle_call(
    call: &Call,
    args: &[SiteValue],
    target: TargetName,
    universe: &TypeUniverse,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let layout = abi::classify_call(call, target, universe)?;
    for value in &layout.arguments {
        if value.index == u32::MAX {
            continue;
        }
        let index = usize::try_from(value.index).expect("参数下标适配 usize");
        let Some(src) = args.get(index) else {
            continue;
        };
        let Some(slot) = value.slots.first() else {
            continue;
        };
        match *slot {
            AbiSlot::Integer(gpr) => move_gpr(builder, src.reg, gpr)?,
            AbiSlot::Float(xmm) => move_xmm(builder, src.reg, xmm)?,
            AbiSlot::Stack { .. } => {}
        }
    }
    Ok(())
}

fn call_target(call: &Call, builder: &mut Builder) -> Result<(), LoweringError> {
    match &call.target {
        CallTarget::Instance(key) => rel32_call(builder, Symbol::Instance(*key)),
        CallTarget::External { key, name } => rel32_call(
            builder,
            Symbol::External {
                key: *key,
                name: name.clone(),
            },
        ),
        CallTarget::Runtime(runtime) => {
            let name = mangle::mangle_runtime_call(*runtime);
            rel32_call(
                builder,
                Symbol::External {
                    key: crate::frontend::mono::keys::hash_domain(
                        "gugu-runtime-symbol-v1",
                        name.as_bytes(),
                    ),
                    name,
                },
            )
        }
        CallTarget::Indirect | CallTarget::Vtable { .. } => {
            builder.emit("call", RM64, Access::Read, vec![reg(gpr(Gpr::Rax))]);
            Ok(())
        }
    }
}

fn jump_callee(call: &Call, builder: &mut Builder) -> Result<(), LoweringError> {
    match &call.target {
        CallTarget::Instance(key) => rel32_jump(builder, Symbol::Instance(*key)),
        CallTarget::External { key, name } => rel32_jump(
            builder,
            Symbol::External {
                key: *key,
                name: name.clone(),
            },
        ),
        CallTarget::Runtime(runtime) => {
            let name = mangle::mangle_runtime_call(*runtime);
            rel32_jump(
                builder,
                Symbol::External {
                    key: crate::frontend::mono::keys::hash_domain(
                        "gugu-runtime-symbol-v1",
                        name.as_bytes(),
                    ),
                    name,
                },
            )
        }
        CallTarget::Indirect | CallTarget::Vtable { .. } => {
            builder.emit("jmp", RM64, Access::Read, vec![reg(gpr(Gpr::Rax))]);
            Ok(())
        }
    }
}

fn rel32_call(builder: &mut Builder, symbol: Symbol) -> Result<(), LoweringError> {
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}

fn rel32_jump(builder: &mut Builder, symbol: Symbol) -> Result<(), LoweringError> {
    builder.emit(
        "jmp",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}

fn move_returns(
    returned: &[SiteValue],
    layout: &AbiLayout,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    for (index, value) in layout.results.iter().enumerate() {
        let Some(src) = returned.get(index) else {
            continue;
        };
        let Some(slot) = value.slots.first() else {
            continue;
        };
        match *slot {
            AbiSlot::Integer(gpr) => move_gpr(builder, src.reg, gpr)?,
            AbiSlot::Float(xmm) => move_xmm(builder, src.reg, xmm)?,
            AbiSlot::Stack { .. } => {}
        }
    }
    let _ = Type::I64;
    Ok(())
}

fn move_gpr(builder: &mut Builder, src: Reg, dest: Gpr) -> Result<(), LoweringError> {
    if src == Reg::Gpr(dest) {
        return Ok(());
    }
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(gpr(dest)), reg(src)],
    );
    Ok(())
}

fn move_xmm(builder: &mut Builder, src: Reg, dest: Xmm) -> Result<(), LoweringError> {
    if src == Reg::Xmm(dest) {
        return Ok(());
    }
    builder.emit(
        "movaps",
        XMMRM_XMM,
        Access::Write,
        vec![reg(Reg::Xmm(dest)), reg(src)],
    );
    Ok(())
}
