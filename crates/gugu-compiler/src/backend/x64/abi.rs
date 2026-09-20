//! 内部 ABI 与 C ABI 分类：消费 LIR 已有的 `signature.sret` / `by_value`。
//!
//! 内部整数参数序是 `rax, rbx, rcx, rdx, rdi, rsi, r8, r9, r10`；浮点是 `xmm0..xmm7`。
//! `rsp`/`r14`/`r15`/`r11` 永不作为普通值。C import/export 走 Linux SysV 或 Windows
//! Microsoft x64，不加 `__gugu_` 前缀。

use crate::frontend::gir::body::CallKind;
use crate::frontend::gir::passing::PassingClass;
use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{Call, Signature, Type, ValueType};
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

/// Windows x64 浮点参数槽。
const WIN_FLOAT: [Xmm; 4] = [Xmm::Xmm0, Xmm::Xmm1, Xmm::Xmm2, Xmm::Xmm3];

/// 一个 ABI 槽：物理寄存器或 spill 到栈。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AbiSlot {
    /// 整数/指针槽。
    Integer(Gpr),
    /// 浮点/向量槽。
    Float(Xmm),
    /// 栈上的 8 字节 piece；`offset` 相对 spill 区起点。
    Stack { offset: u32 },
}

/// 一个按值参数或返回值的 ABI 表示。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AbiValue {
    /// 源参数/结果下标。
    pub index: u32,
    /// 机器类型。
    pub ty: ValueType,
    /// 占用的槽；空表示零尺寸被跳过。
    pub slots: Vec<AbiSlot>,
    /// 间接：caller 传地址，只占一个整数槽。
    pub indirect: bool,
}

/// 一次调用或函数入口的 ABI 布局。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AbiLayout {
    /// 参数布局，含可选隐藏 sret。
    pub arguments: Vec<AbiValue>,
    /// 返回布局；sret 时为空，结果经隐藏指针写出。
    pub results: Vec<AbiValue>,
    /// 隐藏 sret 占用第一个整数槽。
    pub sret: bool,
    /// 整数参数槽消耗数（含 sret）。
    pub integer_args: u32,
    /// 浮点参数槽消耗数。
    pub float_args: u32,
    /// 栈上 8 字节 piece 数。
    pub stack_slots: u32,
}

/// 内部 ABI：函数入口签名。
pub(crate) fn classify_signature(
    signature: &Signature,
    universe: &TypeUniverse,
) -> Result<AbiLayout, LoweringError> {
    classify(
        &signature.parameters,
        &signature.results,
        signature.sret,
        &signature.by_value,
        Convention::Internal,
        universe,
    )
}

/// 按调用种类选择内部 ABI 或 C ABI。
pub(crate) fn classify_call(
    call: &Call,
    target: TargetName,
    universe: &TypeUniverse,
) -> Result<AbiLayout, LoweringError> {
    let convention = match call.kind {
        CallKind::Managed => Convention::Internal,
        CallKind::ForeignBridge
        | CallKind::ForeignBridgeDirtyCpu
        | CallKind::ForeignLeaf { .. } => match target {
            TargetName::X86_64Linux => Convention::SysV,
            TargetName::X86_64Windows => Convention::Win64,
        },
    };
    classify(
        &call.parameters,
        &call.results,
        call.sret.map(|(_, bytes, key)| (bytes, 0, key)),
        &call.by_value,
        convention,
        universe,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Convention {
    Internal,
    SysV,
    Win64,
}

fn classify(
    parameters: &[ValueType],
    results: &[ValueType],
    sret: Option<(u64, u32, [u8; 32])>,
    by_value: &[(u32, [u8; 32], u64)],
    convention: Convention,
    universe: &TypeUniverse,
) -> Result<AbiLayout, LoweringError> {
    let integer_regs = convention.integer_regs();
    let float_regs = convention.float_regs();
    let return_slots = classify_results(results, sret, convention, universe)?;
    let mut integer = usize::from(return_slots.sret);
    let mut float = 0_usize;
    let mut stack = 0_u32;
    let mut arguments = Vec::with_capacity(parameters.len() + usize::from(return_slots.sret));
    if return_slots.sret {
        arguments.push(AbiValue {
            index: u32::MAX,
            ty: ValueType::pointer(crate::lir::body::Provenance::Stack),
            slots: vec![AbiSlot::Integer(integer_regs[0])],
            indirect: true,
        });
    }
    for (index, ty) in parameters.iter().enumerate() {
        let index = u32::try_from(index).expect("参数下标适配 u32");
        let by_value_entry = by_value
            .iter()
            .find(|(parameter, _, _)| *parameter == index);
        let value = classify_parameter(
            index,
            *ty,
            by_value_entry.map(|(_, key, bytes)| (*key, *bytes)),
            convention,
            universe,
            integer_regs,
            float_regs,
            &mut integer,
            &mut float,
            &mut stack,
        )?;
        arguments.push(value);
    }
    Ok(AbiLayout {
        arguments,
        results: return_slots.results,
        sret: return_slots.sret,
        integer_args: u32::try_from(integer).expect("整数槽适配 u32"),
        float_args: u32::try_from(float).expect("浮点槽适配 u32"),
        stack_slots: stack,
    })
}

struct ReturnSlots {
    results: Vec<AbiValue>,
    sret: bool,
}

fn classify_results(
    results: &[ValueType],
    sret: Option<(u64, u32, [u8; 32])>,
    convention: Convention,
    universe: &TypeUniverse,
) -> Result<ReturnSlots, LoweringError> {
    let pieces = result_pieces(results, sret, universe)?;
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
    let integer_regs = convention.return_integer();
    let float_regs = convention.return_float();
    let mut values = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.iter().enumerate() {
        let slot = assign_return(*piece, integer_regs, float_regs, &mut integer, &mut float)?;
        values.push(AbiValue {
            index: u32::try_from(index).expect("返回下标适配 u32"),
            ty: *piece,
            slots: vec![slot],
            indirect: false,
        });
    }
    Ok(ReturnSlots {
        results: values,
        sret: false,
    })
}

fn result_pieces(
    results: &[ValueType],
    sret: Option<(u64, u32, [u8; 32])>,
    universe: &TypeUniverse,
) -> Result<Vec<ValueType>, LoweringError> {
    if let Some((bytes, _, key)) = sret {
        let passing = universe.record(&key).map_or(0, |record| record.passing);
        if bytes > 16 || passing & (PassingClass::COW.bits() | PassingClass::RESOURCE.bits()) != 0 {
            return Ok(vec![
                ValueType::pointer(crate::lir::body::Provenance::Stack);
                3
            ]);
        }
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
    integer_regs: &[Gpr],
    float_regs: &[Xmm],
    integer: &mut usize,
    float: &mut usize,
) -> Result<AbiSlot, LoweringError> {
    if is_float(ty) {
        let slot = *float_regs
            .get(*float)
            .ok_or(LoweringError::InvalidOperands)?;
        *float += 1;
        return Ok(AbiSlot::Float(slot));
    }
    let slot = *integer_regs
        .get(*integer)
        .ok_or(LoweringError::InvalidOperands)?;
    *integer += 1;
    Ok(AbiSlot::Integer(slot))
}

fn classify_parameter(
    index: u32,
    ty: ValueType,
    by_value: Option<([u8; 32], u64)>,
    convention: Convention,
    universe: &TypeUniverse,
    integer_regs: &[Gpr],
    float_regs: &[Xmm],
    integer: &mut usize,
    float: &mut usize,
    stack: &mut u32,
) -> Result<AbiValue, LoweringError> {
    if matches!(ty.ty, Type::Flags | Type::Mem | Type::Void) {
        return Ok(AbiValue {
            index,
            ty,
            slots: Vec::new(),
            indirect: false,
        });
    }
    if let Some((key, bytes)) = by_value {
        return classify_aggregate(
            index,
            ty,
            key,
            bytes,
            convention,
            universe,
            integer_regs,
            float_regs,
            integer,
            float,
            stack,
        );
    }
    let slot = assign_value(ty, integer_regs, float_regs, integer, float, stack)?;
    Ok(AbiValue {
        index,
        ty,
        slots: vec![slot],
        indirect: false,
    })
}

fn classify_aggregate(
    index: u32,
    ty: ValueType,
    key: [u8; 32],
    bytes: u64,
    convention: Convention,
    universe: &TypeUniverse,
    integer_regs: &[Gpr],
    float_regs: &[Xmm],
    integer: &mut usize,
    float: &mut usize,
    stack: &mut u32,
) -> Result<AbiValue, LoweringError> {
    if bytes == 0 {
        return Ok(AbiValue {
            index,
            ty,
            slots: Vec::new(),
            indirect: false,
        });
    }
    let passing = universe.record(&key).map_or(0, |record| record.passing);
    let indirect = bytes > 16
        || passing & (PassingClass::COW.bits() | PassingClass::RESOURCE.bits()) != 0
        || matches!(convention, Convention::Win64) && !matches!(bytes, 1 | 2 | 4 | 8);
    if indirect {
        let slot = take_integer(integer_regs, integer, stack)?;
        return Ok(AbiValue {
            index,
            ty: ValueType::pointer(crate::lir::body::Provenance::Stack),
            slots: vec![slot],
            indirect: true,
        });
    }
    let piece_count = usize::try_from(bytes.div_ceil(8)).expect("聚合 piece 数适配 usize");
    let pieces = pieces_of(key, bytes, universe);
    let saved_integer = *integer;
    let saved_float = *float;
    let saved_stack = *stack;
    let mut slots = Vec::with_capacity(piece_count);
    let mut fits = true;
    for piece in &pieces {
        match assign_value(*piece, integer_regs, float_regs, integer, float, stack) {
            Ok(slot) => {
                if matches!(slot, AbiSlot::Stack { .. }) {
                    fits = false;
                    break;
                }
                slots.push(slot);
            }
            Err(_) => {
                fits = false;
                break;
            }
        }
    }
    if !fits || *integer > integer_regs.len() || *float > float_regs.len() {
        *integer = saved_integer;
        *float = saved_float;
        *stack = saved_stack;
        let mut slots = Vec::with_capacity(pieces.len());
        for _ in &pieces {
            slots.push(take_stack(stack));
        }
        return Ok(AbiValue {
            index,
            ty,
            slots,
            indirect: false,
        });
    }
    Ok(AbiValue {
        index,
        ty,
        slots,
        indirect: false,
    })
}

fn pieces_of(key: [u8; 32], bytes: u64, universe: &TypeUniverse) -> Vec<ValueType> {
    let float_scalar = universe.record(&key).ok().and_then(|record| {
        if bytes == 4 {
            Some(ValueType::scalar(Type::F32))
        } else if bytes == 8 {
            Some(ValueType::scalar(Type::F64))
        } else {
            None
        }
        .filter(|_| matches!(record.layout, Some((size, _)) if size == bytes))
        .filter(|_| record.children.is_empty())
    });
    if let Some(scalar) = float_scalar {
        return vec![scalar];
    }
    let count = bytes.div_ceil(8);
    (0..count).map(|_| ValueType::scalar(Type::I64)).collect()
}

fn assign_value(
    ty: ValueType,
    integer_regs: &[Gpr],
    float_regs: &[Xmm],
    integer: &mut usize,
    float: &mut usize,
    stack: &mut u32,
) -> Result<AbiSlot, LoweringError> {
    if is_float(ty) {
        if let Some(reg) = float_regs.get(*float) {
            *float += 1;
            return Ok(AbiSlot::Float(*reg));
        }
        return Ok(take_stack(stack));
    }
    take_integer(integer_regs, integer, stack)
}

fn take_integer(
    integer_regs: &[Gpr],
    integer: &mut usize,
    stack: &mut u32,
) -> Result<AbiSlot, LoweringError> {
    if let Some(reg) = integer_regs.get(*integer) {
        *integer += 1;
        return Ok(AbiSlot::Integer(*reg));
    }
    Ok(take_stack(stack))
}

fn take_stack(stack: &mut u32) -> AbiSlot {
    let offset = *stack * 8;
    *stack += 1;
    AbiSlot::Stack { offset }
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

    fn return_integer(self) -> &'static [Gpr] {
        match self {
            Self::Internal => &[Gpr::Rax, Gpr::Rbx],
            Self::SysV | Self::Win64 => &[Gpr::Rax, Gpr::Rdx],
        }
    }

    fn return_float(self) -> &'static [Xmm] {
        &[Xmm::Xmm0, Xmm::Xmm1]
    }
}
