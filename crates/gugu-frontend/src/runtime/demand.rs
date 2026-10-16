//! 将可达 GIR 操作的隐含 runtime 调用纳入同一个单态化闭包。
use crate::frontend::gir::body::{
    GirBody, IntrinsicOp, Rvalue, StatementKind, SuspendReason, Terminator,
};
use crate::frontend::{hir, mono::keys::MonoContext};
use std::collections::BTreeSet;

/// 启动与登记机器片段无条件引用的源入口。
pub(crate) const IMAGE_ENTRIES: &[&str] =
    &["rt0_start", "panic_dispatch", "panic_unwind", "gc_dispatch"];

/// 只入队已落地的源定义；尚未落地的 intrinsic 由最终 LIR glue 门禁拒绝。
pub(crate) fn definition(context: &MonoContext<'_>, glue: &str) -> Option<hir::DefId> {
    let Some(super::GlueDef {
        def: super::GlueDefKind::Item { source, item },
        ..
    }) = super::GLUE_DEFS.iter().find(|entry| entry.glue == glue)
    else {
        return None;
    };
    context
        .module
        .definitions
        .iter()
        .enumerate()
        .find(|(index, definition)| {
            context.owner_of[*index].is_some()
                && definition.name == *item
                && definition.location.as_ref().is_some_and(|location| {
                    context.module.sources
                        [usize::try_from(location.source).expect("源编号适配宿主")]
                    .path
                        == *source
                })
        })
        .map(|(index, _)| hir::DefId(u32::try_from(index).expect("定义编号适配 u32")))
}

pub(crate) fn referenced(body: &GirBody) -> BTreeSet<&'static str> {
    let mut demand = BTreeSet::new();
    for statement in &body.statements {
        if let StatementKind::Assign(_, value) = &statement.kind {
            match value {
                Rvalue::AllocObject { .. }
                | Rvalue::AllocArray { .. }
                | Rvalue::Ref(_)
                | Rvalue::RawAddress(_)
                | Rvalue::StackSlotAddress(_) => {
                    // placement 在实例闭合后才形成；两种分配域共享真实分配操作的需求。
                    demand.extend([
                        "gc_alloc_slow",
                        "gc_region_slow",
                        "gc_region_publish",
                        "gc_region_reset",
                        "gc_region_promote",
                        "gc_region_transfer",
                        "gc_write_barrier",
                        "gc_write_barrier_reserved",
                    ]);
                }
                Rvalue::Repeat { .. } => {
                    demand.insert("value_repeat");
                }
                Rvalue::Intrinsic { op, .. } => match op {
                    IntrinsicOp::ChanNew => {
                        demand.insert("channel_new");
                    }
                    IntrinsicOp::ChanClose => {
                        demand.insert("channel_close");
                    }
                    IntrinsicOp::ChanTrySend => {
                        demand.insert("channel_try_send");
                    }
                    IntrinsicOp::ChanTryRecv => {
                        demand.insert("channel_try_recv");
                    }
                    IntrinsicOp::Spawn(_) => {
                        demand.insert("spawn");
                    }
                    IntrinsicOp::DeferChainPush { .. } => {
                        demand.insert("defer_push");
                    }
                    IntrinsicOp::DeferChainAction => {
                        demand.insert("defer_action");
                    }
                    IntrinsicOp::DeferChainEnv { .. } => {
                        demand.insert("defer_environment");
                    }
                    IntrinsicOp::DeferChainPop => {
                        demand.insert("defer_pop");
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    for block in &body.blocks {
        match &block.terminator {
            Terminator::Suspend { reason, .. } => {
                demand.insert(match reason {
                    SuspendReason::ChanSend { .. } => "channel_send",
                    SuspendReason::ChanRecv { .. } => "channel_receive",
                    SuspendReason::JoinWait { .. } => "join_wait",
                    SuspendReason::Yield => "yield",
                });
            }
            Terminator::SelectCommit { .. } => {
                demand.insert("select_commit");
            }
            _ => {}
        }
    }
    demand
}
