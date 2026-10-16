//! x64 lowering 与编码器的公开验收支持。
//!
//! 三部分证据：
//!
//! - **确定性用例**：`cases()` 覆盖每条 lowering 规则，用物理寄存器实例化并交给宿主 VM 执行。
//!   用例字节末尾由宿主补 `ret`；`cold_edges` 里的 `rel32` 由宿主补 stub（冷边 stub 记一次进入）；
//!   `expects_cold_edge` 说明该用例是否必须从冷边离开（`TrapIf` 触发）。
//! - **解码夹具**：`decode()` 给出压缩引用解码序列、控制记录初值与压缩字；期望值来自
//!   `CompressionPlane` 的同一次参考解码，宿主要把 `cage-control` 的 `pc-rel32` 重定位补成
//!   实际地址后再执行。
//! - **上下文切换片段**：`switch_fragment()` 由编码器生成，必须与运行时固定片段逐字节相同
//!   （含 `restore_offset`）。该片段属于运行时 ABI（负责重建 `r14`/`r15`），因此不过
//!   lowering 的 register discipline 检查。

use std::mem::offset_of;

use super::contract::EncoderContract;
use super::encode::assemble;
use super::inst::{Inst, Mem, Operand, RelocKind, RelocTarget, Scale, Sequence};
use super::lower::{self, SiteValue};
use super::reg::{Gpr, Reg};
use super::table::{self, OperandKind};
use crate::lir::body::{Op, Type, ValueType};
use crate::runtime::cage::{CompressionPlane, is_canonical};
use crate::runtime::cage_control::{CAGE_CANONICAL_LIMIT, CAGE_CONTROL_FIELDS, CageControlRecord};
use crate::runtime::compression_schema::{CompressionPolicyV1, CompressionRuntimeContract};
use crate::runtime::coroutine::CoroutineContext;
use crate::runtime::gc_metadata_contract::GC_ARENA_BYTES;
use crate::runtime::platform::{FakePlatform, PlatformProfile};
use crate::runtime::provider::RuntimeSeed;
use crate::target::{CpuBaseline, PointerCompression, TargetName};

/// 验收 harness 构造失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum X64HarnessError {
    /// lowering 拒绝该 op 与类型组合。
    Lowering {
        /// 用例名。
        case: String,
        /// lowering 的诊断文本。
        detail: String,
    },
    /// lowering 产出的序列没过 instruction verifier。
    Verification {
        /// 用例名。
        case: String,
        /// verifier 的诊断文本。
        detail: String,
    },
    /// 序列字节编码失败。
    Encoding {
        /// 用例名。
        case: String,
        /// 编码器的诊断文本。
        detail: String,
    },
    /// 输入寄存器无法避开序列 scratch。
    RegisterAvoidance {
        /// 用例名。
        case: String,
    },
    /// 用例字节里出现了需要宿主链接的非冷边重定位。
    UnexpectedRelocation {
        /// 用例名。
        case: String,
    },
    /// 描述符表缺少需要的 form。
    MissingForm {
        /// 缺失的助记符。
        mnemonic: String,
    },
    /// 上下文切换片段的字节或恢复偏移与运行时固定片段不一致。
    SwitchMismatch {
        /// 期望字节数。
        expected_bytes: u32,
        /// 实际字节数。
        actual_bytes: u32,
    },
    /// 压缩契约或参照平面不可用。
    DecodeUnavailable {
        /// 诊断文本。
        detail: String,
    },
}

impl std::fmt::Display for X64HarnessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lowering { case, detail } => write!(formatter, "{case} 无法 lower：{detail}"),
            Self::Verification { case, detail } => {
                write!(formatter, "{case} 的序列非法：{detail}")
            }
            Self::Encoding { case, detail } => write!(formatter, "{case} 无法编码：{detail}"),
            Self::RegisterAvoidance { case } => {
                write!(formatter, "{case} 的输入寄存器无法避开序列 scratch")
            }
            Self::UnexpectedRelocation { case } => {
                write!(formatter, "{case} 的字节里有需要宿主链接的非冷边重定位")
            }
            Self::MissingForm { mnemonic } => write!(formatter, "描述符表缺少 {mnemonic} 的 form"),
            Self::SwitchMismatch {
                expected_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "上下文切换片段字节数不一致：运行时 {expected_bytes}，编码器 {actual_bytes}"
            ),
            Self::DecodeUnavailable { detail } => write!(formatter, "解码夹具不可用：{detail}"),
        }
    }
}

impl std::error::Error for X64HarnessError {}

/// 用例寄存器编号：GPR 用 x86 编码编号（0..15 = `rax`/`rcx`/`rdx`/`rbx`/`rsp`/`rbp`/`rsi`/`rdi`/`r8`..`r15`），
/// 16..31 = `xmm0`..`xmm15`。宿主填寄存器时跳过 `rsp`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X64Register {
    /// 通用寄存器。
    Gpr(u8),
    /// XMM 寄存器。
    Xmm(u8),
}

impl X64Register {
    /// 返回寄存器编号。
    pub const fn code(self) -> u8 {
        match self {
            Self::Gpr(code) | Self::Xmm(code) => code,
        }
    }

    /// 返回寄存器名。
    pub const fn name(self) -> &'static str {
        const GPR: [&str; 16] = [
            "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11",
            "r12", "r13", "r14", "r15",
        ];
        const XMM: [&str; 16] = [
            "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8", "xmm9",
            "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
        ];
        match self {
            Self::Gpr(code) => GPR[usize::from(code & 0x0F)],
            Self::Xmm(code) => XMM[usize::from(code & 0x0F)],
        }
    }
}

/// 需要宿主修正的重定位视图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64RelocationView {
    /// 字段在片段字节内的偏移。
    pub offset: u32,
    /// `pc-rel32`/`abs64`/`rva32`。
    pub kind: &'static str,
    /// `cage-control`/`cold`/`lir`。
    pub target: &'static str,
    /// 目标内加数；`cage-control` 时是控制记录字段偏移。
    pub addend: i64,
}

/// 内存槽位：宿主把槽位基址放进 `memory_base` 寄存器，槽位按 `u64` 下标寻址。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X64MemorySlot {
    /// 槽位下标。
    pub index: u32,
    /// 执行前的值。
    pub initial: u64,
    /// 执行后必须出现的值。
    pub expected: u64,
}

/// 可执行片段与它的宿主修正要求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64Code {
    /// 机器字节。
    pub bytes: Vec<u8>,
    /// 需要宿主修正的重定位。
    pub relocations: Vec<X64RelocationView>,
    /// 冷边 `rel32` 字段的字节偏移。
    pub cold_edges: Vec<u32>,
}

/// 一个确定性用例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64Case {
    /// 用例名。
    pub name: String,
    /// 片段字节与修正要求。
    pub code: X64Code,
    /// 执行前的寄存器值；`Xmm` 的 `u128` 是整 16 字节值。
    pub inputs: Vec<(X64Register, u128)>,
    /// 结果寄存器；`None` 表示只通过内存或冷边观察。
    pub output: Option<X64Register>,
    /// `output` 必须等于的值。
    pub expected: u128,
    /// 其余结果寄存器的期望值：`(结果下标, 值)`。
    pub extra_results: Vec<(usize, u128)>,
    /// 内存槽位。
    pub memory: Vec<X64MemorySlot>,
    /// 接收内存基址的寄存器。
    pub memory_base: Option<X64Register>,
    /// 该用例是否必须从冷边离开。
    pub expects_cold_edge: bool,
}

/// 压缩引用解码夹具。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64DecodeFixture {
    /// 控制记录初值；`base` 字段是 cage 基址。
    pub control: Vec<u8>,
    /// 控制记录字段表 `(名, offset, size)`；与机器序列读取顺序一致。
    pub control_fields: Vec<(String, u32, u32)>,
    /// 解码片段与它的 `cage-control` 重定位。
    pub code: X64Code,
    /// 压缩字与期望解码结果：`None` 表示空字，`Some` 之外的非空字都必须被拒绝。
    pub words: Vec<(u64, Option<u64>)>,
    /// `decodes` 计数字段 `(offset, size)`。
    pub decodes_field: (u32, u32),
    /// `rejections` 计数字段 `(offset, size)`。
    pub rejections_field: (u32, u32),
    /// 执行完全部压缩字后的 `decodes` 期望值。
    pub decodes: u64,
    /// 执行完全部压缩字后的 `rejections` 期望值。
    pub rejections: u64,
}

/// 确定性自检报告。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64HarnessReport {
    /// 目标名。
    pub target: String,
    /// form 目录长度。
    pub form_count: u32,
    /// 超出目标基线、只作负例的 form 数。
    pub beyond_baseline_forms: u32,
    /// 用例数。
    pub case_count: u32,
    /// 上下文切换片段字节数。
    pub switch_bytes: u32,
    /// 编码器片段与运行时固定片段是否逐字节相同。
    pub switch_matches_runtime: bool,
    /// 解码夹具的压缩字数。
    pub decode_words: u32,
    /// 真实 cage 编译产出的解码点数；编译失败时为 0。
    pub cage_decode_sites: u32,
    /// 全部自检是否成立。
    pub invariants_hold: bool,
}

/// 验收 harness。
pub struct X64Harness {
    target: TargetName,
    contract: EncoderContract,
    cases: Vec<X64Case>,
    decode: X64DecodeFixture,
    switch: X64Code,
    switch_restore_offset: u32,
    switch_matches: bool,
}

impl X64Harness {
    /// 构建 harness：契约、用例表、解码夹具与上下文切换片段。
    pub fn new(target: TargetName) -> Result<Self, X64HarnessError> {
        let contract = EncoderContract::build(target.descriptor().cpu_baseline);
        contract.verify().map_err(|error| X64HarnessError::DecodeUnavailable {
            detail: error.message().to_owned(),
        })?;
        let cases = super::harness_cases::cases(target)?;
        let decode = decode_fixture(target.descriptor().cpu_baseline)?;
        let (switch, switch_restore_offset, switch_matches) = switch_fragment()?;
        Ok(Self {
            target,
            contract,
            cases,
            decode,
            switch,
            switch_restore_offset,
            switch_matches,
        })
    }

    /// 返回全部用例。
    pub fn cases(&self) -> &[X64Case] {
        &self.cases
    }

    /// 返回解码夹具。
    pub const fn decode(&self) -> &X64DecodeFixture {
        &self.decode
    }

    /// 返回编码器生成的上下文切换片段。
    pub const fn switch_fragment(&self) -> &X64Code {
        &self.switch
    }

    /// 返回上下文切换片段的 restore 入口字节偏移。
    pub const fn switch_restore_offset(&self) -> u32 {
        self.switch_restore_offset
    }

    /// 返回编码器契约。
    pub const fn contract(&self) -> &EncoderContract {
        &self.contract
    }

    /// 汇总自检报告；会做一次真实的 cage 编译以统计解码点数。
    pub fn report(&self) -> X64HarnessReport {
        let cage_decode_sites = cage_decode_sites(self.target);
        let invariants_hold = self.switch_matches
            && !self.cases.is_empty()
            && !self.decode.words.is_empty()
            && self.contract.beyond_baseline_count() > 0
            && cage_decode_sites > 0
            && self.switch_restore_offset < u32::try_from(self.switch.bytes.len()).unwrap_or(u32::MAX);
        X64HarnessReport {
            target: self.target.name().to_owned(),
            form_count: u32::try_from(self.contract.forms.len()).unwrap_or(u32::MAX),
            beyond_baseline_forms: self.contract.beyond_baseline_count(),
            case_count: u32::try_from(self.cases.len()).unwrap_or(u32::MAX),
            switch_bytes: u32::try_from(self.switch.bytes.len()).unwrap_or(u32::MAX),
            switch_matches_runtime: self.switch_matches,
            decode_words: u32::try_from(self.decode.words.len()).unwrap_or(u32::MAX),
            cage_decode_sites,
            invariants_hold,
        }
    }
}

/// 一次真实编译里 `DecodeCompressedRef` 的站点数。
fn cage_decode_sites(target: TargetName) -> u32 {
    let request = crate::CompileRequest::single_file("main.gg", COMPRESSED_CAPTURE, target)
        .with_compression_policy(CompressionPolicyV1::cage(4 * GC_ARENA_BYTES));
    let compilation = crate::Compiler::new().compile(request);
    if !compilation.is_success() {
        return 0;
    }
    let Some(lir) = compilation.lir() else {
        return 0;
    };
    let mut sites = 0_u32;
    for body in lir.bodies() {
        for instruction in &body.instructions {
            if matches!(instruction.op, Op::DecodeCompressedRef) {
                sites += 1;
            }
        }
    }
    sites
}

/// cage 开启的捕获闭包夹具：闭包环境逃逸到 LocalHeap，因而出现解码点。
const COMPRESSED_CAPTURE: &str = "fn make() fn() int {\n let value = 1\n return fn() int { return value }\n}\nfn main() {\n let closure = make()\n _ = closure()\n}";

/// 解码夹具：参照平面给出控制记录、压缩字与期望解码结果。
fn decode_fixture(baseline: CpuBaseline) -> Result<X64DecodeFixture, X64HarnessError> {
    let contract = CompressionRuntimeContract::new(
        CompressionPolicyV1::cage(4 * GC_ARENA_BYTES),
        PointerCompression::x86_64(),
    )
    .map_err(|error| X64HarnessError::DecodeUnavailable {
        detail: error.message().to_owned(),
    })?;
    // 基址贴近 canonical 上界：合法 offset 与越界 canonical 的 offset 都存在。
    let base = CAGE_CANONICAL_LIMIT - GC_ARENA_BYTES;
    let mut platform = FakePlatform::with_capacity(
        PlatformProfile::Linux,
        base,
        contract.cage_bytes(),
        RuntimeSeed::new(0x52),
    );
    let mut plane = CompressionPlane::new(&contract);
    plane
        .reserve(&mut platform)
        .map_err(|error| X64HarnessError::DecodeUnavailable {
            detail: error.message().to_owned(),
        })?;
    let record = CageControlRecord::from_plane(&plane).map_err(|error| {
        X64HarnessError::DecodeUnavailable {
            detail: error.message().to_owned(),
        }
    })?;
    let descriptor = plane
        .cage_descriptor()
        .ok_or_else(|| X64HarnessError::DecodeUnavailable {
            detail: "预留后缺少 cage 描述".to_owned(),
        })?;
    let words = decode_words(&contract, &descriptor, &mut plane)?;
    let (code, reasons) = decode_code(baseline)?;
    if !reasons
        .iter()
        .all(|relocation| relocation.target == "cage-control")
    {
        return Err(X64HarnessError::DecodeUnavailable {
            detail: "解码片段只允许 cage-control 重定位".to_owned(),
        });
    }
    Ok(X64DecodeFixture {
        control: record.to_bytes(),
        control_fields: CAGE_CONTROL_FIELDS
            .iter()
            .map(|(name, offset, size)| ((*name).to_owned(), *offset, *size))
            .collect(),
        code,
        words,
        decodes_field: control_field("decodes"),
        rejections_field: control_field("rejections"),
        decodes: plane.stats().decodes,
        rejections: plane.stats().rejections,
    })
}

/// 控制记录字段的 `(offset, size)`。
fn control_field(name: &str) -> (u32, u32) {
    let (_, offset, size) = CAGE_CONTROL_FIELDS
        .iter()
        .find(|(field, _, _)| *field == name)
        .expect("字段表登记全部控制记录字段");
    (*offset, *size)
}

/// 构造压缩字：合法、空、错 cage id、陈旧 generation、越界 offset、非 canonical 各一。
fn decode_words(
    contract: &CompressionRuntimeContract,
    descriptor: &crate::runtime::cage::CageDescriptor,
    plane: &mut CompressionPlane,
) -> Result<Vec<(u64, Option<u64>)>, X64HarnessError> {
    let shift_id = u32::try_from(contract.cage_id_shift()).expect("偏移适配");
    let shift_generation = u32::try_from(contract.cage_generation_shift()).expect("偏移适配");
    let pack = |id: u64, generation: u64, offset: u64| -> u64 {
        (id << shift_id) | (generation << shift_generation) | offset
    };
    let generation = u64::from(descriptor.generation);
    let headroom = CAGE_CANONICAL_LIMIT - descriptor.base;
    let candidates = [
        pack(0, generation, 0x40),
        contract.null_word(),
        pack(1, generation, 0x40),
        pack(0, generation + 1, 0x40),
        pack(0, generation, descriptor.len),
        pack(0, generation, headroom),
    ];
    let mut words = Vec::with_capacity(candidates.len());
    for word in candidates {
        let decoded = plane
            .decode(word)
            .map_err(|error| X64HarnessError::DecodeUnavailable {
                detail: error.message().to_owned(),
            })?;
        if let Some(address) = decoded {
            debug_assert!(is_canonical(address, contract.canonical_bits()));
        }
        words.push((word, decoded));
    }
    Ok(words)
}

/// 解码片段：`DecodeCompressedRef` 的 lowering 结果与 `cage-control` 重定位。
fn decode_code(baseline: CpuBaseline) -> Result<(X64Code, Vec<X64RelocationView>), X64HarnessError> {
    let operands = [SiteValue {
        ty: ValueType::scalar(Type::Ptr),
        reg: Reg::Gpr(Gpr::Rax),
    }];
    let results = [SiteValue {
        ty: ValueType::scalar(Type::Ptr),
        reg: Reg::Gpr(Gpr::Rbx),
    }];
    let lowered = lower::lower(
        &Op::DecodeCompressedRef,
        &operands,
        &results,
        &crate::backend::x64::lower::probe_source(),
    )
    .map_err(|error| X64HarnessError::Lowering {
        case: "decode".to_owned(),
        detail: error.to_string(),
    })?;
    super::verify::verify_sequence(&lowered.sequence, baseline).map_err(|error| {
        X64HarnessError::Verification {
            case: "decode".to_owned(),
            detail: error.to_string(),
        }
    })?;
    let assembled = assemble(&lowered.sequence).map_err(|error| X64HarnessError::Encoding {
        case: "decode".to_owned(),
        detail: error.to_string(),
    })?;
    let code = X64Code {
        bytes: assembled.bytes,
        relocations: assembled
            .relocations
            .iter()
            .map(|relocation| relocation_view(relocation.offset, relocation.kind, &relocation.target, relocation.addend))
            .collect(),
        cold_edges: assembled
            .relocations
            .iter()
            .filter(|relocation| matches!(relocation.target, RelocTarget::Cold(_)))
            .map(|relocation| relocation.offset)
            .collect(),
    };
    let reasons = code.relocations.clone();
    Ok((code, reasons))
}

/// 重定位视图的构造入口；用例表与夹具共用。
pub(crate) fn relocation_view(
    offset: u32,
    kind: RelocKind,
    target: &RelocTarget,
    addend: i64,
) -> X64RelocationView {
    X64RelocationView {
        offset,
        kind: match kind {
            RelocKind::PcRel32 => "pc-rel32",
            RelocKind::Abs64 => "abs64",
            RelocKind::Rva32 => "rva32",
        },
        target: match target {
            RelocTarget::CageControl => "cage-control",
            RelocTarget::Cold(_) => "cold",
            RelocTarget::Lir(_) => "lir",
        },
        addend,
    }
}

/// 用描述符表编码上下文切换片段，并与运行时固定片段逐字节比较。
fn switch_fragment() -> Result<(X64Code, u32, bool), X64HarnessError> {
    let rsp = disp(offset_of!(CoroutineContext, rsp))?;
    let rip = disp(offset_of!(CoroutineContext, rip))?;
    let rbx = disp(offset_of!(CoroutineContext, rbx))?;
    let rbp = disp(offset_of!(CoroutineContext, rbp))?;
    let r12 = disp(offset_of!(CoroutineContext, r12))?;
    let r13 = disp(offset_of!(CoroutineContext, r13))?;
    let mut sequence = Sequence::new();
    // lea rax, [rsp + 8]；保存调用者 return PC。
    push(
        &mut sequence,
        "lea",
        &[OperandKind::Mem, OperandKind::R64],
        vec![memory(Reg::Gpr(Gpr::Rsp), 8), register(Reg::Gpr(Gpr::Rax))],
    )?;
    push(
        &mut sequence,
        "mov",
        &[OperandKind::Rm64, OperandKind::R64],
        vec![memory(Reg::Gpr(Gpr::Rdi), rsp), register(Reg::Gpr(Gpr::Rax))],
    )?;
    push(
        &mut sequence,
        "mov",
        &[OperandKind::Rm64, OperandKind::R64],
        vec![memory(Reg::Gpr(Gpr::Rsp), 0), register(Reg::Gpr(Gpr::Rax))],
    )?;
    push(
        &mut sequence,
        "mov",
        &[OperandKind::Rm64, OperandKind::R64],
        vec![memory(Reg::Gpr(Gpr::Rdi), rip), register(Reg::Gpr(Gpr::Rax))],
    )?;
    for (field, register) in [
        (rbx, Gpr::Rbx),
        (rbp, Gpr::Rbp),
        (r12, Gpr::R12),
        (r13, Gpr::R13),
    ] {
        push(
            &mut sequence,
            "mov",
            &[OperandKind::Rm64, OperandKind::R64],
            vec![
                memory(Reg::Gpr(Gpr::Rdi), field),
                register(Reg::Gpr(register)),
            ],
        )?;
    }
    let restore_offset = u32::try_from(
        encode::len_of(&sequence).map_err(|error| X64HarnessError::Encoding {
            case: "switch".to_owned(),
            detail: error.to_string(),
        })?,
    )
    .map_err(|_| X64HarnessError::MissingForm {
        mnemonic: "switch".to_owned(),
    })?;
    // mov r14, rdx；mov r15, rcx：内部调用约定的固定寄存器。
    push(
        &mut sequence,
        "mov",
        &[OperandKind::Rm64, OperandKind::R64],
        vec![register(Reg::Gpr(Gpr::Rdx)), register(Reg::Gpr(Gpr::R14))],
    )?;
    push(
        &mut sequence,
        "mov",
        &[OperandKind::Rm64, OperandKind::R64],
        vec![register(Reg::Gpr(Gpr::Rcx)), register(Reg::Gpr(Gpr::R15))],
    )?;
    for (field, register) in [
        (rbx, Gpr::Rbx),
        (rbp, Gpr::Rbp),
        (r12, Gpr::R12),
        (r13, Gpr::R13),
        (rsp, Gpr::Rsp),
    ] {
        push(
            &mut sequence,
            "mov",
            &[OperandKind::Rm64, OperandKind::R64],
            vec![register(Reg::Gpr(register)), memory(Reg::Gpr(Gpr::Rsi), field)],
        )?;
    }
    push(
        &mut sequence,
        "jmp",
        &[OperandKind::Rm64],
        vec![memory(Reg::Gpr(Gpr::Rsi), rip)],
    )?;
    let assembled = assemble(&sequence).map_err(|error| X64HarnessError::Encoding {
        case: "switch".to_owned(),
        detail: error.to_string(),
    })?;
    let fixed = crate::runtime::ContextSwitchCode::fixed();
    let matches = assembled.bytes == fixed.bytes && restore_offset == fixed.restore_offset;
    if !matches {
        return Err(X64HarnessError::SwitchMismatch {
            expected_bytes: u32::try_from(fixed.bytes.len()).unwrap_or(u32::MAX),
            actual_bytes: u32::try_from(assembled.bytes.len()).unwrap_or(u32::MAX),
        });
    }
    Ok((
        X64Code {
            bytes: assembled.bytes,
            relocations: Vec::new(),
            cold_edges: Vec::new(),
        },
        restore_offset,
        matches,
    ))
}

fn disp(offset: usize) -> Result<i32, X64HarnessError> {
    i32::try_from(offset).map_err(|_| X64HarnessError::MissingForm {
        mnemonic: "context disp8".to_owned(),
    })
}

fn memory(base: Reg, disp: i32) -> Operand {
    Operand::Mem(Mem {
        base: Some(base),
        index: None,
        scale: Scale::One,
        disp,
    })
}

fn register(reg: Reg) -> Operand {
    Operand::Reg(reg)
}

fn push(
    sequence: &mut Sequence,
    mnemonic: &str,
    kinds: &[OperandKind],
    operands: Vec<Operand>,
) -> Result<(), X64HarnessError> {
    let Some(form) = table::form_id(mnemonic, kinds) else {
        return Err(X64HarnessError::MissingForm {
            mnemonic: mnemonic.to_owned(),
        });
    };
    sequence.instructions.push(Inst {
        form,
        operands,
        lock: false,
    });
    Ok(())
}

/// 单条指令或多条指令序列的编码长度。
mod encode {
    use super::super::encode;
    use super::Sequence;

    /// 返回序列的字节数。
    pub(super) fn len_of(sequence: &Sequence) -> Result<u32, encode::EncodeError> {
        encode::encoded_len_of(sequence)
    }
}
