//! effect 分类唯一归属；构造器与 verifier 共用，不能由任意指令自报无副作用。
use super::body::{Call, Op, RuntimeCall, SafepointKind};
use crate::frontend::gir::body::CallKind;

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
