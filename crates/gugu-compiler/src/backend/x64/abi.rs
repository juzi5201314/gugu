//! 内部 ABI 与 C ABI 分类：消费 LIR 已有的 `signature.sret` / `by_value`。
//!
//! 内部整数参数序是 `rax, rbx, rcx, rdx, rdi, rsi, r8, r9, r10`；浮点是 `xmm0..xmm7`。
//! `rsp`/`r14`/`r15`/`r11` 永不作为普通值。C import/export 走 Linux SysV 或 Windows
//! Microsoft x64，不加 `__gugu_` 前缀；C 侧的完整互换由 C bridge thunk 负责，本模块只
//! 提供直接调用所需的槽位分类。
//!
//! 三处特殊参数位置由分类器显式处理，不占普通参数槽：
//!
//! - 隐藏 sret 指针（`sret`）复用该参数的位置，占第一个整数槽；
//! - 间接调用目标（`callee`，`Provenance::Code` 参数）不占槽，只作为 `call` 的操作数；
//! - 动态派发指针（`dispatch`，`Provenance::Metadata` 参数）既作为参数传递，也是 vtable
//!   槽的读取来源。
//!
//! LIR 只把不超过 16 字节的聚合拆成标量 lane，`by_value` 只登记“由 caller 传地址”的参数，
//! 因此每个 `by_value` 项都恰好占一个整数槽。C 侧超过 8 字节的聚合返回值需要 thunk 的隐藏
//! 指针，不在直接调用模型内。

use crate::frontend::gir::body::CallKind;
use crate::lir::body::{Call, CallTarget, Provenance, Signature, Type, ValueType};
use crate::target::TargetName;

use super::lower::LoweringError;
use super::reg::{Gpr, Xmm};

/// 内部 ABI 整数参数槽；顺序即规范原文。
pub(crate) const INTERNAL_INTEGER: [Gpr; 9] = [
    Gpr::Rax,
    Gpr::Rbx,
    Gpr::Rcx,
    Gpr::Rdx,
    Gpr::Rdi,
    Gpr::Rsi,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
];

/// 内部 ABI 浮点参数槽。
pub(crate) const INTERNAL_FLOAT: [Xmm; 8] = [
    Xmm::Xmm0,
    Xmm::Xmm1,
    Xmm::Xmm2,
    Xmm::Xmm3,
    Xmm::Xmm4,
    Xmm::Xmm5,
    Xmm::Xmm6,
    Xmm::Xmm7,
];

/// Linux SysV 整数参数槽。
const SYSV_INTEGER: [Gpr; 6] = [Gpr::Rdi, Gpr::Rsi, Gpr::Rdx, Gpr::Rcx, Gpr::R8, Gpr::R9];

/// Windows x64 整数参数槽。
const WIN_INTEGER: [Gpr; 4] = [Gpr::Rcx, Gpr::Rdx, Gpr::R8, Gpr::R9];

/// Windows x64 浮点参数槽；与整数槽按同一位置共用。
const WIN_FLOAT: [Xmm; 4] = [Xmm::Xmm0, Xmm::Xmm1, Xmm::Xmm2, Xmm::Xmm3];

/// 内部 ABI 整数返回槽。
const INTERNAL_RETURN_INTEGER: [Gpr; 2] = [Gpr::Rax, Gpr::Rbx];
/// C ABI 整数返回槽（SysV 与 Microsoft x64 一致）。
const C_RETURN_INTEGER: [Gpr; 2] = [Gpr::Rax, Gpr::Rdx];
/// ABI 浮点返回槽。
const RETURN_FLOAT: [Xmm; 2] = [Xmm::Xmm0, Xmm::Xmm1];

/// Windows x64 调用者必须为被调者预留的 shadow space 字节数。
const WIN_SHADOW_BYTES: u32 = 32;

/// 一个 ABI 槽：物理寄存器或 outgoing 区里的 8 字节 piece。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AbiSlot {
    /// 整数/指针槽。
    Integer(Gpr),
    /// 浮点/向量槽。
    Float(Xmm),
    /// 栈上的 8 字节 piece；`offset` 相对 caller outgoing 区起点。
    Stack { offset: u32 },
}

/// 一个按值参数或返回值的 ABI 表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AbiValue {
    /// 源参数/结果下标。
    pub index: u32,
    /// 机器类型。
    pub ty: ValueType,
    /// 占用的槽；`None` 表示被排除的参数或零尺寸值。
    pub slot: Option<AbiSlot>,
    /// 间接：caller 传地址，只占一个整数槽。
    pub indirect: bool,
}

/// 动态派发的 vtable 来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Dispatch {
    /// vtable 指针本身就是这个参数（`dyn` 胖指针的第二 lane，`Provenance::Metadata`）。
    Lane(u32),
    /// 参数是指向胖对的指针：胖对按 `dyn` 值布局存放，vtable 在 `+8`。
    Pairs(u32),
}

/// 一次调用或函数入口的 ABI 布局。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AbiLayout {
    /// 参数布局，含隐藏 sret。
    pub arguments: Vec<AbiValue>,
    /// 返回布局；sret 时为空，结果经隐藏指针写出。
    pub results: Vec<AbiValue>,
    /// 隐藏 sret 占用第一个整数槽。
    pub sret: bool,
    /// 间接调用的目标参数下标；不占参数槽。
    pub callee: Option<u32>,
    /// 动态派发的 vtable 来源；该参数同时照常作为实参传递。
    pub dispatch: Option<Dispatch>,
    /// 整数参数槽消耗数（含 sret）。
    pub integer_args: u32,
    /// 浮点参数槽消耗数。
    pub float_args: u32,
    /// 栈上 8 字节 piece 数。
    pub stack_slots: u32,
}

/// 内部 ABI：函数入口签名。
pub(crate) fn classify_signature(signature: &Signature) -> Result<AbiLayout, LoweringError> {
    // body 的 sret 指针是 entry 的第一个参数（`entry_parameter` 顺序即 ABI 顺序），
    // `signature.sret` 只携带字节数、对齐与 descriptor。
    let sret = signature.sret.map(|(bytes, _, key)| (0_u32, bytes, key));
    classify(
        &signature.parameters,
        &signature.results,
        sret,
        &signature.by_value,
        Convention::Internal,
        None,
        None,
    )
}

/// 按调用种类选择内部 ABI 或 C ABI。
pub(crate) fn classify_call(call: &Call, target: TargetName) -> Result<AbiLayout, LoweringError> {
    let convention = match call.kind {
        CallKind::Managed => Convention::Internal,
        CallKind::ForeignBridge
        | CallKind::ForeignBridgeDirtyCpu
        | CallKind::ForeignLeaf { .. } => match target {
            TargetName::X86_64Linux => Convention::SysV,
            TargetName::X86_64Windows => Convention::Win64,
        },
    };
    let callee = match call.target {
        CallTarget::Indirect => Some(parameter_with_provenance(
            &call.parameters,
            Provenance::Code,
        )?),
        _ => None,
    };
    let dispatch = match call.target {
        CallTarget::Vtable { .. } => Some(dispatch_source(
            &call.parameters,
            call.sret.map(|(index, _, _)| index),
        )?),
        _ => None,
    };
    classify(
        &call.parameters,
        &call.results,
        call.sret,
        &call.by_value,
        convention,
        callee,
        dispatch,
    )
}

/// 找到携带指定 provenance 的参数下标；间接调用必须能唯一定位目标。
fn parameter_with_provenance(
    parameters: &[ValueType],
    provenance: Provenance,
) -> Result<u32, LoweringError> {
    parameters
        .iter()
        .position(|parameter| parameter.provenance == Some(provenance))
        .and_then(|index| u32::try_from(index).ok())
        .ok_or(LoweringError::InvalidOperands)
}

/// 动态派发的 vtable 来源。
///
/// `dyn` 值布局是「data 在 +0、vtable 在 +8」：胖指针直接作为参数时（`Metadata` lane）vtable
/// 就在那个寄存器里；接收者是指向胖对的指针时（GIR 为 `&dyn` 物化出的 `(data, vtable)` 对）
/// vtable 在该指针的 `+8`。两者都由 LIR 可见事实判定，不依赖接收者的具体类型。
fn dispatch_source(parameters: &[ValueType], sret: Option<u32>) -> Result<Dispatch, LoweringError> {
    if let Some(index) = parameters
        .iter()
        .position(|parameter| parameter.provenance == Some(Provenance::Metadata))
    {
        return u32::try_from(index)
            .map(Dispatch::Lane)
            .map_err(|_| LoweringError::InvalidOperands);
    }
    let receiver = parameters
        .iter()
        .enumerate()
        .find(|(index, parameter)| {
            Some(u32::try_from(*index).unwrap_or(u32::MAX)) != sret && parameter.ty == Type::Ptr
        })
        .and_then(|(index, _)| u32::try_from(index).ok())
        .ok_or(LoweringError::InvalidOperands)?;
    Ok(Dispatch::Pairs(receiver))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Convention {
    Internal,
    SysV,
    Win64,
}

/// 参数槽游标：内部与 SysV 的整数/浮点各自前进，Win64 按参数位置共用一组槽。
#[derive(Clone, Copy, Debug)]
struct Slots {
    convention: Convention,
    integer: usize,
    float: usize,
    position: usize,
    stack: u32,
}

impl Slots {
    fn new(convention: Convention) -> Self {
        Self {
            convention,
            integer: 0,
            float: 0,
            position: 0,
            stack: convention.stack_base(),
        }
    }

    fn integer(&mut self, bytes: u32) -> AbiSlot {
        if self.convention == Convention::Win64 {
            return self.positional(true, bytes);
        }
        if let Some(register) = self.convention.integer_regs().get(self.integer) {
            self.integer += 1;
            return AbiSlot::Integer(*register);
        }
        self.push(bytes)
    }

    fn float(&mut self, bytes: u32) -> AbiSlot {
        if self.convention == Convention::Win64 {
            return self.positional(false, bytes);
        }
        if let Some(register) = self.convention.float_regs().get(self.float) {
            self.float += 1;
            return AbiSlot::Float(*register);
        }
        self.push(bytes)
    }

    /// Win64：整数与浮点按参数位置共用同一组槽，位置越界后落到栈。
    fn positional(&mut self, integer: bool, bytes: u32) -> AbiSlot {
        let position = self.position;
        self.position += 1;
        match (integer, WIN_INTEGER.get(position), WIN_FLOAT.get(position)) {
            (true, Some(register), _) => {
                self.integer += 1;
                AbiSlot::Integer(*register)
            }
            (false, _, Some(register)) => {
                self.float += 1;
                AbiSlot::Float(*register)
            }
            _ => self.push(bytes),
        }
    }

    /// 栈 piece 按值宽度推进：标量占一个 8 字节槽，`V128` 占 16 字节。
    fn push(&mut self, bytes: u32) -> AbiSlot {
        let offset = self.stack;
        self.stack = self.stack.saturating_add(bytes.max(8).next_multiple_of(8));
        AbiSlot::Stack { offset }
    }

    fn stack_pieces(&self) -> u32 {
        self.stack.saturating_sub(self.convention.stack_base()) / 8
    }
}

#[allow(clippy::too_many_arguments)]
fn classify(
    parameters: &[ValueType],
    results: &[ValueType],
    sret: Option<(u32, u64, [u8; 32])>,
    by_value: &[(u32, [u8; 32], u64)],
    convention: Convention,
    callee: Option<u32>,
    dispatch: Option<Dispatch>,
) -> Result<AbiLayout, LoweringError> {
    let return_slots = classify_results(results, sret, convention)?;
    let sret_index = sret.map(|(index, _, _)| index);
    let mut slots = Slots::new(convention);
    let mut arguments = Vec::with_capacity(parameters.len());
    for (index, ty) in parameters.iter().enumerate() {
        let index = u32::try_from(index).expect("参数下标适配 u32");
        let by_address = by_value.iter().any(|(parameter, _, _)| *parameter == index);
        let value = if Some(index) == sret_index {
            // 隐藏返回指针复用该参数的位置：caller 必须真正把指针装进第一个整数槽。
            AbiValue {
                index,
                ty: *ty,
                slot: Some(slots.integer(8)),
                indirect: true,
            }
        } else if Some(index) == callee {
            // 间接调用目标不是参数。
            AbiValue {
                index,
                ty: *ty,
                slot: None,
                indirect: false,
            }
        } else {
            classify_parameter(index, *ty, by_address, &mut slots)
        };
        arguments.push(value);
    }
    Ok(AbiLayout {
        arguments,
        results: return_slots.results,
        sret: return_slots.sret,
        callee,
        dispatch,
        integer_args: u32::try_from(slots.integer).expect("整数槽适配 u32"),
        float_args: u32::try_from(slots.float).expect("浮点槽适配 u32"),
        stack_slots: slots.stack_pieces(),
    })
}

struct ReturnSlots {
    results: Vec<AbiValue>,
    sret: bool,
}

fn classify_results(
    results: &[ValueType],
    sret: Option<(u32, u64, [u8; 32])>,
    convention: Convention,
) -> Result<ReturnSlots, LoweringError> {
    let pieces = result_pieces(results, sret)?;
    let needs_sret = pieces.len() > 2;
    if needs_sret != sret.is_some() {
        return Err(LoweringError::InvalidOperands);
    }
    if needs_sret {
        return Ok(ReturnSlots {
            results: Vec::new(),
            sret: true,
        });
    }
    let mut integer = 0_usize;
    let mut float = 0_usize;
    let mut values = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.iter().enumerate() {
        let slot = assign_return(*piece, convention, &mut integer, &mut float)?;
        values.push(AbiValue {
            index: u32::try_from(index).expect("返回下标适配 u32"),
            ty: *piece,
            slot: Some(slot),
            indirect: false,
        });
    }
    Ok(ReturnSlots {
        results: values,
        sret: false,
    })
}

/// 返回值 piece 分解。
///
/// `sret` 是前端按传递规则写下的决定（footprint 超过 16 字节、COW 或 resource）；这里只校验
/// 它自洽——地址返回不能同时给出寄存器结果，也不能是零字节——不重算前端规则的边界值。
fn result_pieces(
    results: &[ValueType],
    sret: Option<(u32, u64, [u8; 32])>,
) -> Result<Vec<ValueType>, LoweringError> {
    if let Some((_, bytes, _)) = sret {
        if bytes == 0 || !results.is_empty() {
            return Err(LoweringError::InvalidOperands);
        }
        return Ok(vec![ValueType::pointer(Provenance::Stack); 3]);
    }
    let mut pieces = Vec::new();
    for ty in results {
        match ty.ty {
            Type::Flags | Type::Mem | Type::Void => {}
            Type::I8
            | Type::I16
            | Type::I32
            | Type::I64
            | Type::Ptr
            | Type::F32
            | Type::F64
            | Type::V128(_) => pieces.push(*ty),
        }
    }
    Ok(pieces)
}

fn assign_return(
    ty: ValueType,
    convention: Convention,
    integer: &mut usize,
    float: &mut usize,
) -> Result<AbiSlot, LoweringError> {
    if is_float(ty) {
        let slot = *RETURN_FLOAT
            .get(*float)
            .ok_or(LoweringError::InvalidOperands)?;
        *float += 1;
        return Ok(AbiSlot::Float(slot));
    }
    let registers = match convention {
        Convention::Internal => &INTERNAL_RETURN_INTEGER[..],
        Convention::SysV | Convention::Win64 => &C_RETURN_INTEGER[..],
    };
    let slot = *registers
        .get(*integer)
        .ok_or(LoweringError::InvalidOperands)?;
    *integer += 1;
    Ok(AbiSlot::Integer(slot))
}

/// 单个参数分类：标量走对应 bank，由 caller 传地址的聚合只占一个整数槽。
fn classify_parameter(index: u32, ty: ValueType, by_address: bool, slots: &mut Slots) -> AbiValue {
    if matches!(ty.ty, Type::Flags | Type::Mem | Type::Void) {
        return AbiValue {
            index,
            ty,
            slot: None,
            indirect: false,
        };
    }
    if by_address {
        return AbiValue {
            index,
            ty: ValueType::pointer(Provenance::Stack),
            slot: Some(slots.integer(super::lower::stack_piece_bytes(Type::Ptr))),
            indirect: true,
        };
    }
    let bytes = super::lower::stack_piece_bytes(ty.ty);
    let slot = if is_float(ty) {
        slots.float(bytes)
    } else {
        slots.integer(bytes)
    };
    AbiValue {
        index,
        ty,
        slot: Some(slot),
        indirect: false,
    }
}

fn is_float(ty: ValueType) -> bool {
    matches!(ty.ty, Type::F32 | Type::F64 | Type::V128(_))
}

impl Convention {
    fn integer_regs(self) -> &'static [Gpr] {
        match self {
            Self::Internal => &INTERNAL_INTEGER,
            Self::SysV => &SYSV_INTEGER,
            Self::Win64 => &WIN_INTEGER,
        }
    }

    fn float_regs(self) -> &'static [Xmm] {
        match self {
            Self::Internal | Self::SysV => &INTERNAL_FLOAT,
            Self::Win64 => &WIN_FLOAT,
        }
    }

    /// outgoing 区起点到第一个栈参数的字节偏移；Win64 先留 32 字节 shadow space。
    fn stack_base(self) -> u32 {
        match self {
            Self::Internal | Self::SysV => 0,
            Self::Win64 => WIN_SHADOW_BYTES,
        }
    }
}
