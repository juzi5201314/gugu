//! 将可达 GIR 操作的隐含 runtime 调用纳入同一个单态化闭包。
use crate::gir::body::{GirBody, IntrinsicOp, Rvalue, StatementKind, SuspendReason, Terminator};
use crate::runtime::{GLUE_DEFS, GlueDef, GlueDefKind};
use crate::{hir, mono::keys::MonoContext};
use std::collections::{BTreeMap, BTreeSet};

/// 启动与登记机器片段无条件引用的源入口。
pub const IMAGE_ENTRIES: &[&str] = &["rt0_start", "panic_dispatch", "panic_unwind", "gc_dispatch"];

/// 只入队已落地的源定义；尚未落地的 intrinsic 由最终 LIR glue 门禁拒绝。
pub fn definition(context: &MonoContext<'_>, glue: &str) -> Option<hir::DefId> {
    let Some(GlueDef {
        def: GlueDefKind::Item { source, item },
        ..
    }) = GLUE_DEFS.iter().find(|entry| entry.glue == glue)
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

pub fn referenced(
    context: &MonoContext<'_>,
    body: &GirBody,
    bindings: &BTreeMap<String, crate::semantics::Ty>,
) -> BTreeSet<&'static str> {
    let mut demand = BTreeSet::new();
    for statement in &body.statements {
        use crate::gir::body::{ResourceActionKind as Resource, ValueActionKind as Value};
        match &statement.kind {
            StatementKind::ResourceAction { action, .. } => {
                demand.insert(match action {
                    Resource::AcquireLease => "resource_acquire",
                    Resource::ReleaseLease => "resource_release",
                    Resource::Transfer => "resource_transfer",
                    Resource::Finalize => "resource_finalize",
                });
            }
            StatementKind::ValueAction { action, .. } => {
                demand.insert(match action {
                    Value::Copy => "value_copy",
                    Value::Publish => "value_publish",
                    Value::Drop => "value_drop",
                    Value::Forget => "value_forget",
                });
            }
            _ => {}
        }
        if let StatementKind::Assign(place, value) = &statement.kind {
            // 聚合写入托管槽时由 LIR 使用同一 descriptor 搬运与发布。
            demand.insert("value_transfer");
            demand.insert("value_forget");
            demand.insert("resource_release");
            match value {
                Rvalue::Compare { .. } => {
                    demand.insert("string_compare");
                }
                // `+` 只在 string 结果上发 `concat` runtime 调用：按实例绑定代入后
                // 赋值目标 local 的具体类型判定（如 `T::Output` 在 T=string 实例里是
                // String），不给普通整数加法强行实例化 std.text 依赖。
                Rvalue::BinaryOp {
                    op: crate::gir::body::BinaryOp::Add,
                    ..
                } if place.is_local()
                    && matches!(
                        context
                            .type_at(body.locals[place.local.index()].ty, bindings)
                            .as_ref(),
                        Ok(crate::semantics::Ty::String)
                    ) =>
                {
                    demand.insert("concat");
                }
                Rvalue::CheckedOp {
                    kind: crate::gir::body::CheckOpKind::Utf8Boundary,
                    ..
                } => {
                    demand.insert("utf8_boundary");
                }
                Rvalue::CowSnapshot(_) => {
                    demand.insert("cow_snapshot");
                }
                Rvalue::AllocObject { .. }
                | Rvalue::AllocArray { .. }
                | Rvalue::Ref(_)
                | Rvalue::RawAddress(_)
                | Rvalue::StackSlotAddress(_) => {
                    // placement 在实例闭合后才形成；两种分配域共享真实分配操作的需求。
                    demand.extend([
                        "gc_alloc_slow",
                        "gc_shared_alloc",
                        "gc_resolve_shared_handle",
                        "gc_shared_access_begin",
                        "gc_shared_access_end",
                        "gc_shared_field_barrier",
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
        if matches!(&block.terminator, Terminator::Call { call_kind, .. } if *call_kind != crate::gir::body::CallKind::Managed)
        {
            demand.insert("foreign_copy");
        }
        match &block.terminator {
            Terminator::Call {
                call_kind: crate::gir::body::CallKind::ForeignBridge,
                ..
            } => {
                demand.insert("foreign_dispatch");
            }
            Terminator::Call {
                call_kind: crate::gir::body::CallKind::ForeignBridgeDirtyCpu,
                ..
            } => {
                demand.insert("foreign_dirty_dispatch");
            }
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
