//! `OwnershipPublish`/`RootPublish` 区域的 raw 平面契约。
//!
//! `NoSafepointRegion` 内只允许完成已经取得的 slot/link 的状态写入和一次 batch publish：
//! 不分配 message node、不阻塞、不 park、不等待 owner、不执行 range refill、GC assist、
//! drop glue 或平台调用，也不遍历 queue 或做 radix forwarding。原子访问必须遵守
//! Release/Acquire 配对：chain link 使用 Release、batch tail exchange 使用 AcqRel、consumer
//! link load 使用 Acquire，禁止新增全局 `SeqCst`。可移动对象的内部地址不得穿过 publish
//! 边界。

use super::invalid_raw;
use super::regions::Layout;
use crate::Diagnostic;
use crate::frontend::gir::body::{MemoryOrdering, NoSafepointReason};
use crate::lir::body::{AtomicOp, Body, Op, Provenance, Type, ValueId};

pub(super) fn verify(body: &Body, layout: &Layout) -> Result<(), Diagnostic> {
    for (region, members) in layout.memberships.iter().enumerate() {
        if !matches!(
            body.no_safepoint_regions.get(region),
            Some(NoSafepointReason::OwnershipPublish) | Some(NoSafepointReason::RootPublish)
        ) {
            continue;
        }
        for (block_index, member) in members.iter().enumerate() {
            if !member {
                continue;
            }
            for index in window(body, block_index, region) {
                let instruction = &body.instructions[index];
                if let Some(problem) = forbidden(&instruction.op) {
                    return Err(invalid_raw(&format!(
                        "publish 区域包含不允许的操作：{problem}"
                    )));
                }
                publish(body, &instruction.op, &instruction.arguments)?;
            }
        }
    }
    Ok(())
}

/// 返回 block 内落在指定 region 中的指令下标；region 允许跨 block。
fn window(body: &Body, block_index: usize, region: usize) -> std::ops::Range<usize> {
    let block = &body.blocks[block_index];
    let start = usize::try_from(block.instructions.start).expect("指令起点");
    let end = usize::try_from(block.instructions.end).expect("指令终点");
    let mut begin = start;
    let mut finish = end;
    for index in start..end {
        match body.instructions[index].op {
            Op::NoSafepointBegin(open) if usize::try_from(open).expect("region 编号") == region =>
            {
                begin = index + 1;
            }
            Op::NoSafepointEnd(close) if usize::try_from(close).expect("region 编号") == region =>
            {
                finish = index;
            }
            _ => {}
        }
    }
    begin..finish
}

/// 校验一条 region 内的内存写入；非写入操作直接通过。
///
/// 空 publish 区域是合法的：当前 lowering 的写入可能被合法地判定为死存储而删除；真实
/// runtime lowering 写入共享状态后该写入不可消除，并且仍受这里的内存序与 provenance 检查。
fn publish(body: &Body, op: &Op, arguments: &std::ops::Range<u32>) -> Result<(), Diagnostic> {
    match op {
        Op::Store(_) => {
            let args = body.args(arguments);
            interior(body, args[0], "写入目标指针")?;
            interior(body, args[1], "被写入的值")?;
            Ok(())
        }
        Op::Atomic {
            op,
            ordering,
            failure,
            ..
        } => {
            let args = body.args(arguments);
            atomic_ordering(*op, *ordering, *failure)?;
            match op {
                AtomicOp::Load
                | AtomicOp::Add
                | AtomicOp::Sub
                | AtomicOp::And
                | AtomicOp::Or
                | AtomicOp::Xor
                | AtomicOp::Fence => {
                    interior(body, args[0], "原子访问目标指针")?;
                    Ok(())
                }
                AtomicOp::Store | AtomicOp::Exchange => {
                    interior(body, args[0], "原子访问目标指针")?;
                    interior(body, args[1], "被发布的原子值")?;
                    Ok(())
                }
                AtomicOp::CompareExchange => {
                    interior(body, args[0], "原子访问目标指针")?;
                    interior(body, args[2], "被发布的原子值")?;
                    Ok(())
                }
            }
        }
        _ => Ok(()),
    }
}

/// 校验 region 内原子访问的内存序与操作形态。
fn atomic_ordering(
    op: AtomicOp,
    ordering: MemoryOrdering,
    failure: Option<MemoryOrdering>,
) -> Result<(), Diagnostic> {
    if ordering == MemoryOrdering::SeqCst || failure == Some(MemoryOrdering::SeqCst) {
        return Err(invalid_raw(
            "publish 区域禁止新增全局 SeqCst 原子序或 fence",
        ));
    }
    match op {
        AtomicOp::Store
            if !matches!(ordering, MemoryOrdering::Release | MemoryOrdering::AcqRel) =>
        {
            Err(invalid_raw(
                "publish 区域的原子 store 必须使用 Release 或 AcqRel",
            ))
        }
        AtomicOp::Exchange if ordering != MemoryOrdering::AcqRel => {
            Err(invalid_raw("batch tail exchange 必须使用 AcqRel"))
        }
        AtomicOp::Load if ordering == MemoryOrdering::Release => {
            Err(invalid_raw("publish 区域的原子 load 不能使用 Release"))
        }
        AtomicOp::CompareExchange
            if ordering == MemoryOrdering::Relaxed || failure == Some(MemoryOrdering::Release) =>
        {
            Err(invalid_raw(
                "publish 区域的 CAS 必须形成 Release/Acquire 配对",
            ))
        }
        _ => Ok(()),
    }
}

/// 拒绝可移动对象的内部地址穿过 publish 边界。
fn interior(body: &Body, value: ValueId, what: &str) -> Result<(), Diagnostic> {
    let kind = body.values[value.index()].kind;
    if kind.provenance == Some(Provenance::GcInterior) {
        return Err(invalid_raw(&format!("{what} 不能是可移动对象的内部地址")));
    }
    if kind.ty == Type::Ptr
        && !matches!(
            kind.provenance,
            Some(
                Provenance::GcHeap
                    | Provenance::Stack
                    | Provenance::Raw
                    | Provenance::Code
                    | Provenance::Metadata
                    | Provenance::Foreign
            )
        )
    {
        return Err(invalid_raw(&format!("{what} 的 pointer provenance 未登记")));
    }
    Ok(())
}

/// 返回 region 内允许的操作之外的第一个违规原因。
fn forbidden(op: &Op) -> Option<&'static str> {
    match op {
        Op::NoSafepointBegin(_) | Op::NoSafepointEnd(_) => None,
        Op::IConst(_) | Op::FConst(_) => None,
        Op::Integer(_) | Op::Float(_) | Op::Compare { .. } | Op::Select => None,
        Op::Convert(_) | Op::PtrOffset | Op::SymbolAddr(_) => None,
        Op::Load(_) | Op::Store(_) => None,
        Op::Atomic { .. } => None,
        Op::GcWriteBarrier { .. } | Op::GcWriteBarrierReserved { .. } => None,
        Op::ScopedViewBegin { .. } | Op::ScopedViewEnd { .. } => None,
        Op::SharedAccessBegin { .. } | Op::SharedAccessEnd { .. } => None,
        Op::CoverageCounter(_) => None,
        Op::GcAlloc { .. } | Op::RegionAlloc { .. } | Op::PromoteManaged => Some("堆分配或提升"),
        Op::MarkTicketBatch | Op::EdgeDeltaBatch => Some("GC 工作消息"),
        Op::ResolveSharedHandle | Op::ForwardSharedHandle | Op::DecodeCompressedRef => {
            Some("stable handle 或压缩引用解析")
        }
        Op::RegionPublish | Op::RegionReset => Some("region 发布或重置"),
        Op::BarrierReserve(_) => Some("屏障预留"),
        Op::Memcpy { .. } | Op::Memmove { .. } | Op::Memset { .. } => Some("批量内存操作"),
        Op::Call(_) | Op::ForeignCall(_) => Some("调用或外部桥接"),
        Op::PlatformCall(_) => Some("平台范围调用"),
        Op::InlineAsm(_) => Some("内联汇编"),
        Op::TrapIf => Some("陷阱分支"),
        Op::Park | Op::CoroutineSwitch | Op::Ready => Some("调度操作"),
        Op::StackCheck | Op::SafepointPoll { .. } => Some("safepoint 轮询"),
        Op::Vector(_) => Some("向量操作"),
        _ => Some("未登记的 raw 平面操作"),
    }
}
