//! LIR arena：所有编号稠密；指令、参数、边和 use 链均使用连续范围。
use crate::gir::body::{
    CallKind, MemoryOrdering, NoSafepointReason, SourceInfo, SourceScope, ViewMode,
};
use crate::gir::placement::PlacementKind;
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, ops::Range};

/// body 规范字节的 revision。10：body 不再携带整世界输入指纹，同一实例的 body 只由自身内容决定。
/// 11：body 携带 poll 不动点时 caller 可见的接口，安装的 body 据此播种 poll 而不重算。
/// 13：机器操作目录含栈深度读取/复制与 cage 控制访问。
/// 14：C 按值临时副本与 sret 地址保留 Stack provenance。
/// 15：删除无执行入口的 Pin/Unpin 枚举，统一使用源码函数调用。
/// 16：共享 guard 返回载荷，end 显式携带 handle，字段屏障保留操作数。
/// 17：压缩字编码从运行期 cage 基址计算 offset。
/// 18：原生 cage 参数按值复制使用登记的 ForeignCopy 入口。
/// 20：进程与环境机器操作目录。
pub const REVISION: u32 = 20;

macro_rules! ids {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        pub struct $name(pub u32);
        impl $name {
            pub fn index(self) -> usize { usize::try_from(self.0).expect("LIR 编号适配宿主") }
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

pub fn id(value: usize) -> u32 {
    u32::try_from(value).expect("LIR arena 不超过 u32")
}
pub fn range(range: &Range<u32>) -> Range<usize> {
    usize::try_from(range.start).expect("范围适配宿主")
        ..usize::try_from(range.end).expect("范围适配宿主")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Lane {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Type {
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
    pub fn bytes(self) -> Option<u64> {
        match self {
            Self::I8 => Some(1),
            Self::I16 => Some(2),
            Self::I32 | Self::F32 => Some(4),
            Self::I64 | Self::F64 | Self::Ptr => Some(8),
            Self::V128(_) => Some(16),
            Self::Flags | Self::Mem | Self::Void => None,
        }
    }
    pub fn integer(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Provenance {
    /// `HeapDirect`：指向已知对象 payload 起点的强引用。
    GcHeap,
    /// `HeapInterior`：指向对象字段、元素或切片起点。
    GcInterior,
    /// `SharedHandle`：指向 SharedHeap stable handle slot 的受管引用。
    ///
    /// 与 `GcHeap` 严格区分：handle 只能经解析与 access guard 访问，forwarding 只切换
    /// slot；误判为直接指针会绕过 guard 与代际校验。
    SharedHandle,
    /// `CompressedRef`：指向已登记 heap cage 内 offset 的压缩受管引用。
    ///
    /// 与 `GcHeap` 严格区分：必须先经 cage id、offset 范围与 generation 的 checked
    /// 解码才能标记目标。
    CompressedRef,
    /// `StackInterior`：指向当前协程栈范围内的局部或字段。
    Stack,
    Raw,
    Code,
    Metadata,
    Foreign,
}

impl Provenance {
    pub fn managed(self) -> bool {
        matches!(
            self,
            Self::GcHeap | Self::GcInterior | Self::SharedHandle | Self::CompressedRef
        )
    }

    /// 返回栈图逻辑根种类判别值：0=Direct、1=Interior、2=Handle、3=Compressed、4=StackInterior。
    ///
    /// 判别值随契约冻结，供后续 GC 与压缩引用阶段直接复用；新增种类必须在末尾追加。
    pub fn root_kind(self) -> Option<u32> {
        match self {
            Self::GcHeap => Some(0),
            Self::GcInterior => Some(1),
            Self::SharedHandle => Some(2),
            Self::CompressedRef => Some(3),
            Self::Stack => Some(4),
            Self::Raw | Self::Code | Self::Metadata | Self::Foreign => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValueType {
    pub ty: Type,
    pub provenance: Option<Provenance>,
}
impl ValueType {
    pub const fn scalar(ty: Type) -> Self {
        Self {
            ty,
            provenance: None,
        }
    }
    pub const fn pointer(provenance: Provenance) -> Self {
        Self {
            ty: Type::Ptr,
            provenance: Some(provenance),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Origin {
    None,
    Parameter(u32),
    Stack(SlotId),
    Allocation(u32),
    Symbol,
    Derived(ValueId),
    Merge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Definition {
    Parameter { block: BlockId, index: u32 },
    Instruction { instruction: InstId, result: u32 },
    Invoke { block: BlockId, result: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub definition: Definition,
    pub kind: ValueType,
    pub origin: Origin,
    pub uses: Range<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UseSite {
    Instruction(InstId),
    Terminator(BlockId),
    Edge(EdgeId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub site: UseSite,
    pub operand: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AliasClass {
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
pub struct Access {
    pub alias: AliasClass,
    pub align: u32,
    pub volatile: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub input: ValueId,
    pub output: ValueId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Instruction {
    pub op: Op,
    pub arguments: Range<u32>,
    pub results: Range<u32>,
    pub memory: Option<Memory>,
    pub safepoint: Option<SafepointId>,
    pub source: SourceInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IntOp {
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
pub enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
    Neg,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Conversion {
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
pub enum VectorOp {
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
pub enum AtomicOp {
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
pub enum Symbol {
    Instance([u8; 32]),
    /// 擦除函数句柄专用的 checked 入口；目标 body 保留原直接调用 ABI。
    FunctionAdapter {
        target: [u8; 32],
        has_environment: bool,
    },
    External {
        key: [u8; 32],
        name: String,
    },
    Global {
        key: [u8; 32],
        thread_local: bool,
        /// 静态块里的值槽字节数；块本身是「flags 位字 8 byte + 值槽」，
        /// 符号地址是**块首**，值槽在 `+8`。
        bytes: u64,
    },
    TypeDescriptor([u8; 32]),
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
pub enum RuntimeCall {
    ValueCopy,
    ValuePublish,
    ValueDrop,
    ValueForget,
    CowSnapshot,
    ValueTransfer,
    ForeignCopy,
    ValueRepeat,
    ResourceAcquire,
    ResourceRelease,
    ResourceTransfer,
    ResourceFinalize,
    DynamicErase,
    TypeIs,
    Downcast,
    DowncastCopy,
    ChannelNew,
    ChannelClose,
    ChannelSend,
    ChannelReceive,
    ChannelTrySend,
    ChannelTryRecv,
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
    StringCompare,
    Utf8Boundary,
    /// panic 调用在编译期解析为稳定源码 descriptor，ABI 为 payload lanes、path/len、line、column。
    Panic,
    DeferPush,
    DeferAction,
    DeferEnvironment,
    DeferPop,
    WideDiv {
        signed: bool,
        remainder: bool,
    },
    /// 阻塞 I/O 原语。`op`：0 read、1 write、2 seek、3 fill。
    BlockingIo {
        op: u8,
        cancel: bool,
    },
    /// `std.mem` arena 构造：`gc_arena_new(capacity, shared)` → 句柄的 `&byte` 字段值。
    ArenaNew,
    /// `LocalArena.alloc`：`gc_arena_alloc(region, kind, bytes, granular)` → payload 指针。
    ArenaAlloc,
    /// `SyncArena.alloc`：`gc_arena_alloc_sync(region, kind, bytes, granular)` → payload 指针。
    ArenaAllocSync,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CallTarget {
    Instance([u8; 32]),
    External { key: [u8; 32], name: String },
    Runtime(RuntimeCall),
    Indirect,
    Vtable { slot: u32 },
}

/// C 参数在展开车道中的边界；SysV 寄存器耗尽必须按整个参数回退。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForeignArgument {
    pub first: u32,
    pub lanes: u32,
    pub bytes: u32,
    pub align: u32,
    pub memory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub target: CallTarget,
    pub kind: CallKind,
    pub parameters: Vec<ValueType>,
    pub results: Vec<ValueType>,
    pub may_unwind: bool,
    pub may_suspend: bool,
    pub may_allocate: bool,
    pub captures_arguments: bool,
    /// 按值聚合指针的参数编号、稳定 descriptor 与字节大小。
    pub by_value: Vec<(u32, [u8; 32], u64)>,
    /// 非 managed 调用保留原始 C 参数边界，按 first 严格升序。
    pub foreign_arguments: Vec<ForeignArgument>,
    pub sret: Option<(u32, u64, [u8; 32])>,
    /// poll pass 分类出的 managed 直接叶调用；region verifier 与预算数据流消费。
    pub poll_free_leaf: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Op {
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
        /// 分配对象的 capture 槽是否存压缩字；与对象头 `COMPRESSED_REF` 表示同源。
        compressed: bool,
    },
    /// 从当前 body 的私有 region 编号做一次 bump 分配。
    ///
    /// `region` 是 body 内按首次出现顺序稠密编号的 region；它与 `EscapeAndPlacement` 的
    /// `RegionPlan` 一一对应。
    RegionAlloc {
        region: u32,
        descriptor: [u8; 32],
        align: u32,
    },
    /// 平台范围原语；没有机器编码，由 backend 在 runtime 契约中登记。
    PlatformCall(crate::runtime::PlatformOp),
    /// runtime 机器原语；没有机器编码，由 backend 绑定登记的机器片段或 Gugu 实现。
    RuntimeMachineCall(crate::runtime::RuntimeMachineOp),
    /// 结束 region 的私有阶段并登记 export summary；`export` 为 0 表示 summary 闭合。
    RegionPublish {
        region: u32,
        export: u8,
    },
    /// 整区重置；只能跟在同一条路径上 `export == 0` 的 `RegionPublish` 之后。
    RegionReset {
        region: u32,
    },
    /// 无法证明私有性时整区保留（LocalPromote）：地址保持稳定，且不再 reset。
    PromoteManaged {
        region: u32,
    },
    /// 把一个私有 region 的所有权整体移交给 channel send 的接收 owner。
    RegionTransfer {
        region: u32,
    },
    MarkTicketBatch,
    EdgeDeltaBatch,
    ResolveSharedHandle,
    SharedAccessBegin {
        token: u32,
    },
    SharedAccessEnd {
        token: u32,
    },
    /// 共享字段写入的屏障记录；`store` 指向同一个 guard 内更早的 Store。
    SharedFieldBarrier {
        store: InstId,
        token: u32,
    },
    /// 已经过 `BarrierReserve` 的共享字段屏障；`permit` 与同 region 的 LocalHeap 屏障共用额度。
    SharedFieldBarrierReserved {
        store: InstId,
        token: u32,
        permit: PermitId,
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
    Call(Call),
    ForeignCall(Call),
    InlineAsm(u32),
    CoverageCounter(u32),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Terminator {
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
pub struct Parameter {
    pub value: ValueId,
    /// Mem 无来源；普通合流参数按 local、字段字节偏移排序。
    pub source: Option<(u32, u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub parameters: Range<u32>,
    pub instructions: Range<u32>,
    pub predecessors: Range<u32>,
    pub terminator: Terminator,
    pub source: SourceInfo,
    pub cleanup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: BlockId,
    pub to: BlockId,
    pub arguments: Range<u32>,
    pub unwind: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StackSlot {
    pub local: u32,
    pub bytes: u64,
    pub align: u32,
    pub descriptor: [u8; 32],
    pub roots: Vec<(u64, Provenance)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Lifetime {
    pub slot: SlotId,
    pub block: BlockId,
    pub position: u32,
    pub live: bool,
}

/// 逐根字段的初始化事件：`bit` 是所属栈槽 root 表的下标。
///
/// `slot.roots` 是多变体聚合的**并集超集**：某个字只在部分变体里是指针。位图把并集收窄成
/// 「该字在当前值里已经写成指针」：消费者只追踪置位的根字，因此 `Reset` 之后未被重新置位的
/// 标量字不会被当作指针。
///
/// 置位：写入完整覆盖该根字的 8 字节。复位：`StorageLive` 建立新值、`StorageDead` 结束占用
/// 时清空该槽全部位。位置顺序是语义的一部分，重写 pass 必须用 `map_anchor` 精确定位，
/// 不能丢弃或钳位事件。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RootInit {
    pub slot: SlotId,
    pub block: BlockId,
    pub position: u32,
    pub bit: u32,
    pub initialized: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SafepointKind {
    StackCheck,
    CallReturn,
    Allocation,
    Poll,
    Suspend,
    ForeignBridge,
    DirtyCpuBridge,
    Barrier,
    Select,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Safepoint {
    pub kind: SafepointKind,
    pub block: BlockId,
    pub instruction: Option<InstId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BarrierPermit {
    pub region: u32,
    /// hybrid barrier 的 shade 额度：`region` 内每条屏障写消费两个 slot。
    pub max_shades: u32,
    /// hybrid barrier 的 card-mark 额度：`region` 内每个 distinct 写入地址消费一个 slot。
    pub max_card_marks: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Data {
    pub bytes: Vec<u8>,
    pub align: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    pub parameters: Vec<ValueType>,
    pub results: Vec<ValueType>,
    pub sret: Option<(u64, u32, [u8; 32])>,
    pub by_value: Vec<(u32, [u8; 32], u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub descriptor: [u8; 32],
    pub bytes: u64,
    pub roots: Vec<(u64, Provenance)>,
}

/// 单个 body 的 poll 预算摘要；由 poll pass 写入，verifier 与预算数据流消费。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PollSummary {
    /// 入口是否仍保留 `StackCheck`。
    pub entry_stack_check: bool,
    /// 从入口 `StackCheck` 之后起、沿任意路径的饱和 poll 成本。
    pub poll_free_cost: u32,
    /// 去掉 poll/statepoint/checked-entry 调用后的 CFG 是否仍含环。
    pub has_poll_free_cycle: bool,
}

/// poll 不动点时 caller 可见的接口：caller 的预算数据流与叶标记只读这一份。
///
/// 它是插入 poll 之前的 body 在不动点上的取值，与插入之后重算的 [`PollSummary`] 可以不同；
/// 叶标记不由 body 内容决定，只能随 body 保存。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PollInterface {
    /// 不动点时的预算摘要。
    pub summary: PollSummary,
    /// 是否 poll-free 叶。
    pub leaf: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Body {
    pub revision: u32,
    pub instance: [u8; 32],
    pub owner: [u8; 32],
    pub name: String,
    pub target: String,
    pub signature: Signature,
    pub values: Vec<Value>,
    pub blocks: Vec<Block>,
    pub instructions: Vec<Instruction>,
    pub operands: Vec<ValueId>,
    pub parameters: Vec<Parameter>,
    pub edges: Vec<Edge>,
    pub predecessors: Vec<EdgeId>,
    pub switch_cases: Vec<(u64, EdgeId)>,
    pub stack_slots: Vec<StackSlot>,
    pub lifetimes: Vec<Lifetime>,
    pub root_inits: Vec<RootInit>,
    pub safepoints: Vec<Safepoint>,
    pub barrier_permits: Vec<BarrierPermit>,
    pub no_safepoint_regions: Vec<NoSafepointReason>,
    pub source_scopes: Vec<SourceScope>,
    pub uses: Vec<Use>,
    pub data: Vec<Data>,
    pub assembly: Vec<crate::hir::Assembly>,
    pub environments: Vec<Environment>,
    pub entry: BlockId,
    pub poll_summary: PollSummary,
    pub poll_interface: PollInterface,
}

impl Body {
    pub fn args(&self, values: &Range<u32>) -> &[ValueId] {
        &self.operands[range(values)]
    }
    pub fn params(&self, block: BlockId) -> &[Parameter] {
        &self.parameters[range(&self.blocks[block.index()].parameters)]
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        crate::hash::hash_domain(
            "gugu-lir-body-v1",
            &crate::canonical::to_vec(self).expect("LIR 编码"),
        )
    }
}
