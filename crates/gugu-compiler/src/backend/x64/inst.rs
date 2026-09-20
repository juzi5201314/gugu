//! x86_64 指令表示：操作数、序列、重定位与编码结果。

use serde::{Deserialize, Serialize};

use super::reg::{Clobbers, Reg};
use super::table::FormId;
use crate::frontend::gir::body::SourceInfo;
use crate::lir::body::Symbol;

/// 内存操作数的比例因子。
///
/// 1/2/4/8 全集的 SIB 编码与字节 fixture 属于本阶段的编码器能力；本阶段的 lowering
/// （数值、向量、原子、解码）只用基址形式，倍率索引由后续内存访问阶段产生。
#[allow(
    dead_code,
    reason = "倍率索引由后续内存访问阶段的 lowering 产生，本阶段只被 SIB 字节 fixture 构造"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Scale {
    One,
    Two,
    Four,
    Eight,
}

impl Scale {
    /// 返回 SIB 编码用的移位量。
    pub(crate) const fn shift(self) -> u8 {
        match self {
            Self::One => 0,
            Self::Two => 1,
            Self::Four => 2,
            Self::Eight => 3,
        }
    }
}

/// 通过 `base + index * scale + disp` 寻址的内存操作数。
///
/// base/index 可以是虚拟寄存器：编码器用确定性的占位编码写字节，物理分配必须满足
/// `RegisterConstraint` 之外由分配阶段负责。虚拟 base 的零位移一律写 disp8 形式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Mem {
    pub(crate) base: Option<Reg>,
    pub(crate) index: Option<Reg>,
    pub(crate) scale: Scale,
    pub(crate) disp: i32,
}

/// 分支与 RIP 相对操作数的局部标签编号。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LabelId(pub(crate) u32);

/// 重定位目标。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum RelocTarget {
    /// LIR 符号（函数实例、外部、数据与类型元数据）。
    Lir(Symbol),
    /// 压缩引用控制记录字段。
    CageControl,
    /// 冷边（拒绝路径或 trap）。
    Cold(ColdEdge),
}

/// 重定位的字段宽度与计算方式。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum RelocKind {
    /// 32 位 PC 相对。
    PcRel32,
    /// 64 位绝对值。
    Abs64,
    /// 32 位 image-relative RVA。
    Rva32,
}

/// 冷边种类：与热路径共享寄存器状态、只被重定位指向的 out-of-line 位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ColdEdgeKind {
    /// 压缩引用解码拒绝：cage id、generation、offset 或 bounds 不匹配。
    CompressionDecodeRejected,
    /// 语言级 trap（检查失败或不可达）。
    Trap,
}

/// 冷边携带种类与来源位置。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ColdEdge {
    pub(crate) kind: ColdEdgeKind,
    pub(crate) source: SourceInfo,
}

/// 指令操作数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Operand {
    /// 物理或虚拟寄存器。
    Reg(Reg),
    /// 立即数；宽度由 form 的 `imm` 字段给出。
    Imm(u64),
    /// 基址、索引与位移寻址。
    Mem(Mem),
    /// RIP 相对内存操作数；disp32 字段由重定位写入，`i32` 是附加修正。
    Rip(RelocTarget, i32),
    /// 立即数字段承载重定位。
    Reloc(RelocTarget, RelocKind),
    /// 局部标签引用（分支目标）。
    Label(LabelId),
}

/// 一条待编码指令：form 与操作数一一对应。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Inst {
    pub(crate) form: FormId,
    pub(crate) operands: Vec<Operand>,
    /// 显式 `lock` 前缀；只有 `lock_allowed` 的 form 与内存操作数可以携带。
    pub(crate) lock: bool,
}

/// 标签定义：`at` 是定义点之前的指令下标；等于指令总数表示序列末尾。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LabelDefinition {
    pub(crate) label: LabelId,
    pub(crate) at: u32,
}

/// 一段机器指令序列。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Sequence {
    pub(crate) instructions: Vec<Inst>,
    /// 标签定义；编号在序列内唯一。
    pub(crate) labels: Vec<LabelDefinition>,
}

impl Sequence {
    /// 空序列。
    pub(crate) fn new() -> Self {
        Self {
            instructions: Vec::new(),
            labels: Vec::new(),
        }
    }
}

/// 物理分配必须满足的寄存器约束。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegisterConstraint {
    /// 序列内字节偏移。
    pub(crate) offset: u32,
    /// 被约束的寄存器。
    pub(crate) register: Reg,
    /// 约束种类。
    pub(crate) kind: ConstraintKind,
}

/// 约束种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ConstraintKind {
    /// 该寄存器必须能作为字节寄存器编码（低字节不指向 `ah`/`ch`/`dh`/`bh`）。
    ByteEncodable,
}

/// 一条重定位：在 `offset` 处按 `kind` 写入 `target + addend`。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Relocation {
    pub(crate) offset: u32,
    pub(crate) kind: RelocKind,
    pub(crate) target: RelocTarget,
    pub(crate) addend: i64,
}

/// 编码结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Assembled {
    /// 机器字节。
    pub(crate) bytes: Vec<u8>,
    /// 按 offset 升序的重定位。
    pub(crate) relocations: Vec<Relocation>,
    /// 标签的解析后字节偏移。
    pub(crate) labels: Vec<u32>,
    /// 每条指令的 `(指令下标, 起始字节偏移)`。
    pub(crate) instruction_offsets: Vec<(u32, u32)>,
    /// 需要物理分配满足的约束。
    pub(crate) constraints: Vec<RegisterConstraint>,
    /// 序列写到的物理寄存器集合。
    pub(crate) clobbers: Clobbers,
}
