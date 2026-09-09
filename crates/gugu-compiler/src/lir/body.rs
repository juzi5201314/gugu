//! LIR arena：所有编号稠密；指令、参数、边和 use 链均使用连续范围。
use crate::frontend::gir::body::{
    CallKind, MemoryOrdering, NoSafepointReason, SourceInfo, SourceScope, ViewMode,
};
use crate::frontend::gir::placement::PlacementKind;
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, ops::Range};

pub(crate) const REVISION: u32 = 1;

macro_rules! ids {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        pub(crate) struct $name(pub(crate) u32);
        impl $name {
            pub(crate) fn index(self) -> usize { usize::try_from(self.0).expect("LIR 编号适配宿主") }
        }
    )* };
}
ids!(
    ValueId,
    BlockId,
    InstId,
    EdgeId,
    SlotId,
    SafepointId,
    PermitId
);

pub(crate) fn id(value: usize) -> u32 {
    u32::try_from(value).expect("LIR arena 不超过 u32")
}
pub(crate) fn range(range: &Range<u32>) -> Range<usize> {
    usize::try_from(range.start).expect("范围适配宿主")
        ..usize::try_from(range.end).expect("范围适配宿主")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum Lane {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum Type {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    V128(Lane),
    Ptr,
    Flags,
    Mem,
    Void,
}

impl Type {
    pub(crate) fn bytes(self) -> Option<u64> {
        match self {
            Self::I8 => Some(1),
            Self::I16 => Some(2),
            Self::I32 | Self::F32 => Some(4),
            Self::I64 | Self::F64 | Self::Ptr => Some(8),
            Self::V128(_) => Some(16),
            Self::Flags | Self::Mem | Self::Void => None,
        }
    }
    pub(crate) fn integer(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum Provenance {
    GcHeap,
    GcInterior,
    Stack,
    Raw,
    Code,
    Metadata,
    Foreign,
}

impl Provenance {
    pub(crate) fn managed(self) -> bool {
        matches!(self, Self::GcHeap | Self::GcInterior)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ValueType {
    pub(crate) ty: Type,
    pub(crate) provenance: Option<Provenance>,
}
impl ValueType {
    pub(crate) const fn scalar(ty: Type) -> Self {
        Self {
            ty,
            provenance: None,
        }
    }
    pub(crate) const fn pointer(provenance: Provenance) -> Self {
        Self {
            ty: Type::Ptr,
            provenance: Some(provenance),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Origin {
    None,
    Parameter(u32),
    Stack(SlotId),
    Allocation(u32),
    Symbol,
    Derived(ValueId),
    Merge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Definition {
    Parameter { block: BlockId, index: u32 },
    Instruction { instruction: InstId, result: u32 },
    Invoke { block: BlockId, result: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Value {
    pub(crate) definition: Definition,
    pub(crate) kind: ValueType,
    pub(crate) origin: Origin,
    pub(crate) uses: Range<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum UseSite {
    Instruction(InstId),
    Terminator(BlockId),
    Edge(EdgeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Use {
    pub(crate) site: UseSite,
    pub(crate) operand: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum AliasClass {
    Stack(SlotId),
    FreshHeap(u32),
    Global([u8; 32]),
    ThreadLocal([u8; 32]),
    Heap,
    Foreign,
    Atomic,
    Volatile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Access {
    pub(crate) alias: AliasClass,
    pub(crate) align: u32,
    pub(crate) volatile: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Memory {
    pub(crate) input: ValueId,
    pub(crate) output: ValueId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Instruction {
    pub(crate) op: Op,
    pub(crate) arguments: Range<u32>,
    pub(crate) results: Range<u32>,
    pub(crate) memory: Option<Memory>,
    pub(crate) safepoint: Option<SafepointId>,
    pub(crate) source: SourceInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum IntOp {
    Add,
    Sub,
    Mul,
    AddCarry,
    SubBorrow,
    MulWide,
    DivSigned,
    DivUnsigned,
    RemSigned,
    RemUnsigned,
    And,
    Or,
    Xor,
    Shl,
    ShrSigned,
    ShrUnsigned,
    Neg,
    Not,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
    Neg,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Condition {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Conversion {
    SignExtend,
    ZeroExtend,
    Truncate,
    IntToFloat {
        signed: bool,
    },
    FloatToInt {
        signed: bool,
    },
    FloatResize,
    Bitcast,
    PointerToInt,
    IntToPointer,
    PointerCast,
    /// 显式由裸指针构造引用：只允许出现在 `unsafe` 源码的 `(&T)(p)` 转换里。
    RawToReference,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum VectorOp {
    Splat,
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Compare(Condition),
    Shuffle([u8; 16]),
    Extract(u8),
    Insert(u8),
    ReduceAdd,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum AtomicOp {
    Load,
    Store,
    Exchange,
    Add,
    Sub,
    And,
    Or,
    Xor,
    CompareExchange,
    Fence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Symbol {
    Instance([u8; 32]),
    External {
        key: [u8; 32],
        name: String,
    },
    Global {
        key: [u8; 32],
        thread_local: bool,
    },
    TypeDescriptor([u8; 32]),
    TypeId([u8; 32]),
    /// type section 的 `TypeRecord` 数组基址；记录按稠密 `TypeId` 顺序、固定 80 字节。
    TypeRecords,
    /// type section 的 name pool 基址；`TypeRecord.name_offset/len` 相对它。
    TypeNames,
    Vtable {
        interface: [u8; 32],
        concrete: [u8; 32],
    },
    Data(u32),
}

/// 只能调用登记 runtime 入口；调用参数与 descriptor 已完成字节布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum RuntimeCall {
    ValueCopy,
    ValuePublish,
    ValueDrop,
    ValueForget,
    CowSnapshot,
    ValueTransfer,
    ValueRepeat,
    ResourceAcquire,
    ResourceRelease,
    ResourceTransfer,
    ResourceFinalize,
    Pin,
    Unpin,
    DynamicErase,
    TypeIs,
    Downcast,
    DowncastCopy,
    ChannelNew,
    ChannelClose,
    ChannelSend,
    ChannelReceive,
    JoinWait,
    Yield,
    Spawn,
    SelectCommit {
        cases: u32,
        has_default: bool,
    },
    Format,
    /// 字符串拼接：参数为两组 `(data, len)`，返回新的 `(data, len)`。
    Concat,
    Utf8Boundary,
    Panic,
    DeferPush,
    DeferAction,
    DeferEnvironment,
    DeferPop,
    WideDiv {
        signed: bool,
        remainder: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CallTarget {
    Instance([u8; 32]),
    External { key: [u8; 32], name: String },
    Runtime(RuntimeCall),
    Indirect,
    Vtable { slot: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Call {
    pub(crate) target: CallTarget,
    pub(crate) kind: CallKind,
    pub(crate) parameters: Vec<ValueType>,
    pub(crate) results: Vec<ValueType>,
    pub(crate) may_unwind: bool,
    pub(crate) may_suspend: bool,
    pub(crate) may_allocate: bool,
    pub(crate) captures_arguments: bool,
    /// 按值聚合指针的参数编号、稳定 descriptor 与字节大小。
    pub(crate) by_value: Vec<(u32, [u8; 32], u64)>,
    pub(crate) sret: Option<(u32, u64, [u8; 32])>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Op {
    IConst(u64),
    FConst(u64),
    SymbolAddr(Symbol),
    StackAddr(SlotId),
    PtrOffset,
    Integer(IntOp),
    Float(FloatOp),
    Compare {
        condition: Condition,
        signed: bool,
    },
    Convert(Conversion),
    Vector(VectorOp),
    Select,
    TrapIf,
    Load(Access),
    Store(Access),
    Memcpy {
        bytes: u64,
    },
    Memmove {
        bytes: u64,
    },
    Memset {
        bytes: u64,
    },
    Atomic {
        op: AtomicOp,
        ordering: MemoryOrdering,
        failure: Option<MemoryOrdering>,
        align: u32,
    },
    GcAlloc {
        descriptor: [u8; 32],
        align: u32,
        placement: PlacementKind,
    },
    RegionAlloc {
        descriptor: [u8; 32],
        align: u32,
    },
    RegionPublish,
    RegionReset,
    PromoteManaged,
    MarkTicketBatch,
    EdgeDeltaBatch,
    ResolveSharedHandle,
    SharedAccessBegin {
        token: u32,
    },
    SharedAccessEnd {
        token: u32,
    },
    ForwardSharedHandle,
    DecodeCompressedRef,
    BarrierReserve(PermitId),
    GcWriteBarrier {
        store: InstId,
    },
    GcWriteBarrierReserved {
        store: InstId,
        permit: PermitId,
    },
    ScopedViewBegin {
        mode: ViewMode,
        token: u32,
    },
    ScopedViewEnd {
        token: u32,
    },
    SafepointPoll {
        interval: NonZeroU32,
    },
    StackCheck,
    NoSafepointBegin(u32),
    NoSafepointEnd(u32),
    CoroutineSwitch,
    Park,
    Ready,
    Call(Call),
    ForeignCall(Call),
    InlineAsm(u32),
    CoverageCounter(u32),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Terminator {
    Jump(EdgeId),
    Branch {
        condition: ValueId,
        yes: EdgeId,
        no: EdgeId,
    },
    Switch {
        value: ValueId,
        cases: Range<u32>,
        otherwise: EdgeId,
    },
    Invoke {
        call: Call,
        arguments: Range<u32>,
        results: Range<u32>,
        memory: Memory,
        normal: EdgeId,
        unwind: EdgeId,
        safepoint: Option<SafepointId>,
    },
    Return {
        values: Range<u32>,
        memory: ValueId,
    },
    ResumePanic {
        memory: ValueId,
    },
    TailCall {
        call: Call,
        arguments: Range<u32>,
        memory: ValueId,
    },
    Trap {
        memory: ValueId,
    },
    Unreachable {
        memory: ValueId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Parameter {
    pub(crate) value: ValueId,
    /// Mem 无来源；普通合流参数按 local、字段字节偏移排序。
    pub(crate) source: Option<(u32, u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Block {
    pub(crate) parameters: Range<u32>,
    pub(crate) instructions: Range<u32>,
    pub(crate) predecessors: Range<u32>,
    pub(crate) terminator: Terminator,
    pub(crate) source: SourceInfo,
    pub(crate) cleanup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Edge {
    pub(crate) from: BlockId,
    pub(crate) to: BlockId,
    pub(crate) arguments: Range<u32>,
    pub(crate) unwind: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StackSlot {
    pub(crate) local: u32,
    pub(crate) bytes: u64,
    pub(crate) align: u32,
    pub(crate) descriptor: [u8; 32],
    pub(crate) roots: Vec<(u64, Provenance)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Lifetime {
    pub(crate) slot: SlotId,
    pub(crate) block: BlockId,
    pub(crate) position: u32,
    pub(crate) live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum SafepointKind {
    StackCheck,
    CallReturn,
    Allocation,
    Poll,
    Suspend,
    ForeignBridge,
    DirtyCpuBridge,
    Barrier,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Safepoint {
    pub(crate) kind: SafepointKind,
    pub(crate) block: BlockId,
    pub(crate) instruction: Option<InstId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BarrierPermit {
    pub(crate) region: u32,
    pub(crate) max_shades: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Data {
    pub(crate) bytes: Vec<u8>,
    pub(crate) align: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Signature {
    pub(crate) parameters: Vec<ValueType>,
    pub(crate) results: Vec<ValueType>,
    pub(crate) sret: Option<(u64, u32, [u8; 32])>,
    pub(crate) by_value: Vec<(u32, [u8; 32], u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Environment {
    pub(crate) descriptor: [u8; 32],
    pub(crate) bytes: u64,
    pub(crate) roots: Vec<(u64, Provenance)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Body {
    pub(crate) revision: u32,
    pub(crate) instance: [u8; 32],
    pub(crate) owner: [u8; 32],
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) signature: Signature,
    pub(crate) values: Vec<Value>,
    pub(crate) blocks: Vec<Block>,
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) operands: Vec<ValueId>,
    pub(crate) parameters: Vec<Parameter>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) predecessors: Vec<EdgeId>,
    pub(crate) switch_cases: Vec<(u64, EdgeId)>,
    pub(crate) stack_slots: Vec<StackSlot>,
    pub(crate) lifetimes: Vec<Lifetime>,
    pub(crate) safepoints: Vec<Safepoint>,
    pub(crate) barrier_permits: Vec<BarrierPermit>,
    pub(crate) no_safepoint_regions: Vec<NoSafepointReason>,
    pub(crate) source_scopes: Vec<SourceScope>,
    pub(crate) uses: Vec<Use>,
    pub(crate) data: Vec<Data>,
    pub(crate) assembly: Vec<crate::frontend::hir::Assembly>,
    pub(crate) environments: Vec<Environment>,
    pub(crate) entry: BlockId,
    pub(crate) input_fingerprint: [u8; 32],
}

impl Body {
    pub(crate) fn args(&self, values: &Range<u32>) -> &[ValueId] {
        &self.operands[range(values)]
    }
    pub(crate) fn params(&self, block: BlockId) -> &[Parameter] {
        &self.parameters[range(&self.blocks[block.index()].parameters)]
    }
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        crate::frontend::mono::keys::hash_domain(
            "gugu-lir-body-v1",
            &serde_json::to_vec(self).expect("LIR 可序列化"),
        )
    }
}
