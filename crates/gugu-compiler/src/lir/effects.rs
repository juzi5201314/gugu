//! effect 分类唯一归属；构造器与 verifier 共用，不能由任意指令自报无副作用。
use super::body::{Call, Op, RuntimeCall, SafepointKind};
use super::pass::policy::POLL_BUDGET;
use crate::frontend::gir::body::CallKind;
use crate::frontend::hir::Assembly;
use crate::frontend::semantics::assembly::AssemblyContext;

impl Op {
    pub(crate) fn has_memory(&self) -> bool {
        match self {
            Self::IConst(_)
            | Self::FConst(_)
            | Self::SymbolAddr(_)
            | Self::StackAddr(_)
            | Self::PtrOffset
            | Self::Integer(_)
            | Self::Float(_)
            | Self::Compare { .. }
            | Self::Convert(_)
            | Self::Vector(_)
            | Self::Select => false,
            Self::TrapIf
            | Self::Load(_)
            | Self::Store(_)
            | Self::Memcpy { .. }
            | Self::Memmove { .. }
            | Self::Memset { .. }
            | Self::Atomic { .. }
            | Self::GcAlloc { .. }
            | Self::RegionAlloc { .. }
            | Self::RegionPublish
            | Self::RegionReset
            | Self::PromoteManaged
            | Self::MarkTicketBatch
            | Self::EdgeDeltaBatch
            | Self::ResolveSharedHandle
            | Self::SharedAccessBegin { .. }
            | Self::SharedAccessEnd { .. }
            | Self::ForwardSharedHandle
            | Self::DecodeCompressedRef
            | Self::BarrierReserve(_)
            | Self::GcWriteBarrier { .. }
            | Self::GcWriteBarrierReserved { .. }
            | Self::ScopedViewBegin { .. }
            | Self::ScopedViewEnd { .. }
            | Self::SafepointPoll { .. }
            | Self::StackCheck
            | Self::NoSafepointBegin(_)
            | Self::NoSafepointEnd(_)
            | Self::CoroutineSwitch
            | Self::Park
            | Self::Ready
            | Self::Call(_)
            | Self::ForeignCall(_)
            | Self::InlineAsm(_)
            | Self::CoverageCounter(_) => true,
        }
    }

    pub(crate) fn fence(&self) -> bool {
        match self {
            Self::Load(access) | Self::Store(access) => access.volatile,
            Self::Memcpy { .. } | Self::Memmove { .. } | Self::Memset { .. } => false,
            _ => self.has_memory(),
        }
    }

    pub(crate) fn safepoint_kind(&self) -> Option<SafepointKind> {
        match self {
            Self::StackCheck => Some(SafepointKind::StackCheck),
            Self::SafepointPoll { .. } => Some(SafepointKind::Poll),
            Self::GcAlloc { .. } | Self::RegionAlloc { .. } | Self::PromoteManaged => {
                Some(SafepointKind::Allocation)
            }
            Self::BarrierReserve(_) | Self::GcWriteBarrier { .. } => Some(SafepointKind::Barrier),
            Self::CoroutineSwitch | Self::Park => Some(SafepointKind::Suspend),
            Self::Call(call) | Self::ForeignCall(call) => call.safepoint_kind(),
            Self::ResolveSharedHandle | Self::ForwardSharedHandle => {
                Some(SafepointKind::CallReturn)
            }
            _ => None,
        }
    }

    /// 固定 poll 成本表；权重与 [`super::pass::policy::POLL_COST_REVISION`] 绑定。
    ///
    /// 成本表本体；只读取内联汇编模板与上下文。
    pub(crate) fn poll_cost_with(&self, assembly: &[Assembly]) -> u32 {
        match self {
            Self::IConst(_)
            | Self::FConst(_)
            | Self::SymbolAddr(_)
            | Self::StackAddr(_)
            | Self::PtrOffset
            | Self::Integer(_)
            | Self::Float(_)
            | Self::Compare { .. }
            | Self::Convert(_)
            | Self::Vector(_)
            | Self::Select
            | Self::TrapIf
            | Self::ScopedViewBegin { .. }
            | Self::ScopedViewEnd { .. }
            | Self::SharedAccessBegin { .. }
            | Self::SharedAccessEnd { .. }
            | Self::NoSafepointBegin(_)
            | Self::NoSafepointEnd(_)
            | Self::CoverageCounter(_)
            | Self::SafepointPoll { .. }
            | Self::StackCheck => 1,
            Self::Load(_) | Self::Store(_) => 4,
            Self::Memcpy { .. } | Self::Memmove { .. } | Self::Memset { .. } => 8,
            Self::Atomic { op, .. } => {
                if *op == super::body::AtomicOp::Fence {
                    16
                } else {
                    8
                }
            }
            Self::BarrierReserve(_)
            | Self::GcWriteBarrier { .. }
            | Self::GcWriteBarrierReserved { .. } => 8,
            Self::GcAlloc { .. }
            | Self::RegionAlloc { .. }
            | Self::PromoteManaged
            | Self::RegionPublish
            | Self::RegionReset
            | Self::MarkTicketBatch
            | Self::EdgeDeltaBatch
            | Self::ResolveSharedHandle
            | Self::ForwardSharedHandle
            | Self::DecodeCompressedRef
            | Self::CoroutineSwitch
            | Self::Park
            | Self::Ready => 16,
            Self::Call(call) | Self::ForeignCall(call) => match call.kind {
                CallKind::ForeignLeaf { .. } => POLL_BUDGET,
                _ => 5,
            },
            Self::InlineAsm(index) => {
                let Some(assembly) = usize::try_from(*index)
                    .ok()
                    .and_then(|index| assembly.get(index))
                else {
                    return POLL_BUDGET;
                };
                if assembly.context != AssemblyContext::Managed {
                    return POLL_BUDGET;
                }
                let lines = assembly
                    .template
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count();
                (u32::try_from(lines)
                    .expect("汇编行数适配 u32")
                    .saturating_mul(4))
                .clamp(1, 64)
            }
        }
    }

    /// 切断点判定；显式 poll、分配、屏障、调用、挂起与入口 `StackCheck` 由
    /// `safepoint_kind` 覆盖；`ForeignLeaf`、native 汇编与 `NoSafepointBegin` 另行切断。
    pub(crate) fn poll_cut_point_with(&self, assembly: &[Assembly]) -> bool {
        if let Self::Call(call) | Self::ForeignCall(call) = self
            && call.poll_free_leaf
        {
            return false;
        }
        if self.safepoint_kind().is_some() {
            return true;
        }
        match self {
            Self::Call(call) | Self::ForeignCall(call) => {
                matches!(call.kind, CallKind::ForeignLeaf { .. })
            }
            Self::InlineAsm(index) => usize::try_from(*index)
                .ok()
                .and_then(|index| assembly.get(index))
                .is_none_or(|assembly| assembly.context != AssemblyContext::Managed),
            Self::NoSafepointBegin(_) => true,
            _ => false,
        }
    }
}

impl Call {
    pub(crate) fn safepoint_kind(&self) -> Option<SafepointKind> {
        match self.kind {
            CallKind::ForeignBridge => Some(SafepointKind::ForeignBridge),
            CallKind::ForeignBridgeDirtyCpu => Some(SafepointKind::DirtyCpuBridge),
            CallKind::ForeignLeaf { .. } => None,
            CallKind::Managed if self.may_suspend => Some(SafepointKind::Suspend),
            CallKind::Managed if self.may_allocate => Some(SafepointKind::Allocation),
            CallKind::Managed => Some(SafepointKind::CallReturn),
        }
    }
}

impl RuntimeCall {
    /// 固定 runtime 接口属性：unwind、suspend、allocate、捕获参数。
    pub(crate) fn effects(self) -> (bool, bool, bool, bool) {
        match self {
            Self::Panic => (true, false, false, false),
            Self::ChannelSend
            | Self::ChannelReceive
            | Self::JoinWait
            | Self::Yield
            | Self::SelectCommit {
                has_default: false, ..
            } => (false, true, true, true),
            Self::Spawn => (false, true, true, true),
            Self::ChannelNew
            | Self::DynamicErase
            | Self::Format
            | Self::Concat
            | Self::DeferPush => (false, false, true, true),
            Self::ValueCopy | Self::CowSnapshot | Self::DowncastCopy | Self::ValueRepeat => {
                (false, false, true, false)
            }
            Self::ValuePublish | Self::ResourceTransfer => (false, false, false, true),
            Self::ValueTransfer
            | Self::ValueDrop
            | Self::ValueForget
            | Self::ResourceAcquire
            | Self::ResourceRelease
            | Self::ResourceFinalize
            | Self::Pin
            | Self::Unpin
            | Self::TypeIs
            | Self::Downcast
            | Self::ChannelClose
            | Self::SelectCommit {
                has_default: true, ..
            }
            | Self::Utf8Boundary
            | Self::DeferAction
            | Self::DeferEnvironment
            | Self::DeferPop
            | Self::WideDiv { .. } => (false, false, false, false),
        }
    }
}
