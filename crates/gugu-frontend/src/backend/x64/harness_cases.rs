//! 验收 harness 的确定性用例表：每条 lowering 规则至少一个可在宿主上执行的用例。
//!
//! 用例在 lowering 后用**物理寄存器**实例化：输入寄存器按固定候选顺序挑选，避开该序列
//! 声明的 scratch（`Lowered::clobbers`）与保留寄存器（`rsp`/`r14`/`r15`）；避让空间不足时
//! 构造失败而不是生成语义不成立的用例。期望值由本文件的独立参照实现给出，遵循
//! `docs/src/internals/backend.md` 的寄存器表示不变量：窄宽度结果零扩展到 64 位，布尔取 0/1，
//! 浮点按位模式比较，`V128` 结果比较整 128 位。

use super::harness::{
    X64Case, X64Code, X64HarnessError, X64MemorySlot, X64Register, relocation_view,
};
use super::inst::RelocTarget;
use super::lower::{self, SiteValue};
use super::reg::{Gpr, Reg, Xmm};
use super::{encode, verify};
use crate::frontend::gir::body::MemoryOrdering;
use crate::lir::body::{
    AtomicOp, Condition, Conversion, FloatOp, IntOp, Lane, Op, Type, ValueType, VectorOp,
};
use crate::target::{CpuBaseline, TargetName};

/// 物理 GPR 候选顺序（跳过 `rsp`/`r14`/`r15`）。
const GPR_ORDER: [Gpr; 13] = [
    Gpr::Rax,
    Gpr::Rbx,
    Gpr::Rcx,
    Gpr::Rdx,
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::Rbp,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
    Gpr::R11,
    Gpr::R12,
    Gpr::R13,
];

/// 物理 XMM 候选顺序（跳过 lowering 的 scratch `xmm13`..`xmm15`）。
const XMM_ORDER: [Xmm; 13] = [
    Xmm::Xmm0,
    Xmm::Xmm1,
    Xmm::Xmm2,
    Xmm::Xmm3,
    Xmm::Xmm4,
    Xmm::Xmm5,
    Xmm::Xmm6,
    Xmm::Xmm7,
    Xmm::Xmm8,
    Xmm::Xmm9,
    Xmm::Xmm10,
    Xmm::Xmm11,
    Xmm::Xmm12,
];

/// 用例构造器。
struct CaseBuilder {
    name: String,
    op: Op,
    operands: Vec<(Type, u128)>,
    results: Vec<Type>,
    output: Option<usize>,
    expected: u128,
    extra: Vec<(usize, u128)>,
    memory: Vec<X64MemorySlot>,
    /// 接收内存基址的操作数下标；该操作数不参与值输入。
    memory_base: Option<usize>,
    expects_cold_edge: bool,
}

impl CaseBuilder {
    fn new(name: impl Into<String>, op: Op) -> Self {
        Self {
            name: name.into(),
            op,
            operands: Vec::new(),
            results: Vec::new(),
            output: None,
            expected: 0,
            extra: Vec::new(),
            memory: Vec::new(),
            memory_base: None,
            expects_cold_edge: false,
        }
    }

    fn operand(mut self, ty: Type, value: u128) -> Self {
        self.operands.push((ty, value));
        self
    }

    /// 追加一个由宿主填入内存基址的指针操作数。
    fn memory_operand(mut self, ty: Type) -> Self {
        self.memory_base = Some(self.operands.len());
        self.operands.push((ty, 0));
        self
    }

    fn result(mut self, ty: Type) -> Self {
        self.results.push(ty);
        self.output = Some(self.results.len() - 1);
        self
    }

    /// 指定比较的结果下标；多结果 op 用它把 `expected` 对准主结果。
    const fn output(mut self, index: usize) -> Self {
        self.output = Some(index);
        self
    }

    const fn expected(mut self, value: u128) -> Self {
        self.expected = value;
        self
    }

    fn extra_result(mut self, index: usize, expected: u128) -> Self {
        self.extra.push((index, expected));
        self
    }

    const fn expects_cold_edge(mut self, expects: bool) -> Self {
        self.expects_cold_edge = expects;
        self
    }

    fn memory(mut self, index: u32, initial: u64, expected: u64) -> Self {
        self.memory.push(X64MemorySlot {
            index,
            initial,
            expected,
        });
        self
    }

    fn build(self, baseline: CpuBaseline) -> Result<X64Case, X64HarnessError> {
        let mut last = None;
        for shift in 0..GPR_ORDER.len() {
            match self.instantiate(shift, baseline) {
                Ok(case) => return Ok(case),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or(X64HarnessError::RegisterAvoidance { case: self.name }))
    }

    fn instantiate(&self, shift: usize, baseline: CpuBaseline) -> Result<X64Case, X64HarnessError> {
        let (operand_registers, result_registers) = assign(self, shift);
        let operands: Vec<SiteValue> = self
            .operands
            .iter()
            .zip(operand_registers.iter())
            .map(|((ty, _), register)| SiteValue {
                ty: ValueType::scalar(*ty),
                reg: *register,
            })
            .collect();
        let results: Vec<SiteValue> = self
            .results
            .iter()
            .zip(result_registers.iter())
            .map(|(ty, register)| SiteValue {
                ty: ValueType::scalar(*ty),
                reg: *register,
            })
            .collect();
        let lowered = lower::lower(&self.op, &operands, &results, &lower::probe_source()).map_err(
            |error| X64HarnessError::Lowering {
                case: self.name.clone(),
                detail: error.to_string(),
            },
        )?;
        verify::verify_sequence(&lowered.sequence, baseline).map_err(|error| {
            X64HarnessError::Verification {
                case: self.name.clone(),
                detail: error.to_string(),
            }
        })?;
        // 输入与结果寄存器都必须避开序列 scratch；冲突时换一组寄存器重试。
        if operands
            .iter()
            .chain(results.iter())
            .any(|value| clobbers_register(lowered.clobbers, value.reg))
        {
            return Err(X64HarnessError::RegisterAvoidance {
                case: self.name.clone(),
            });
        }
        let assembled =
            encode::assemble(&lowered.sequence).map_err(|error| X64HarnessError::Encoding {
                case: self.name.clone(),
                detail: error.to_string(),
            })?;
        if assembled
            .relocations
            .iter()
            .any(|relocation| !matches!(relocation.target, RelocTarget::Cold(_)))
        {
            return Err(X64HarnessError::UnexpectedRelocation {
                case: self.name.clone(),
            });
        }
        let inputs = self
            .operands
            .iter()
            .zip(operand_registers.iter())
            .enumerate()
            .filter(|(index, _)| Some(*index) != self.memory_base)
            .map(|(_, ((_, value), register))| (register_view(*register), *value))
            .collect();
        Ok(X64Case {
            name: self.name.clone(),
            code: X64Code {
                bytes: assembled.bytes,
                relocations: assembled
                    .relocations
                    .iter()
                    .map(|relocation| {
                        relocation_view(
                            relocation.offset,
                            relocation.kind,
                            &relocation.target,
                            relocation.addend,
                        )
                    })
                    .collect(),
                cold_edges: assembled
                    .relocations
                    .iter()
                    .filter(|relocation| matches!(relocation.target, RelocTarget::Cold(_)))
                    .map(|relocation| relocation.offset)
                    .collect(),
            },
            inputs,
            output: self
                .output
                .map(|index| register_view(result_registers[index])),
            expected: self.expected,
            extra_results: self
                .extra
                .iter()
                .map(|(index, expected)| (register_view(result_registers[*index]), *expected))
                .collect(),
            memory: self.memory.clone(),
            memory_base: self
                .memory_base
                .map(|index| register_view(operand_registers[index])),
            expects_cold_edge: self.expects_cold_edge,
        })
    }
}

/// 按候选顺序分配操作数与结果寄存器。
fn assign(builder: &CaseBuilder, shift: usize) -> (Vec<Reg>, Vec<Reg>) {
    let mut gpr = shift;
    let mut xmm = shift;
    let operands = builder
        .operands
        .iter()
        .map(|(ty, _)| pick(*ty, &mut gpr, &mut xmm))
        .collect();
    let results = builder
        .results
        .iter()
        .map(|ty| pick(*ty, &mut gpr, &mut xmm))
        .collect();
    (operands, results)
}

fn pick(ty: Type, gpr: &mut usize, xmm: &mut usize) -> Reg {
    if is_gpr_type(ty) {
        let register = GPR_ORDER[*gpr % GPR_ORDER.len()];
        *gpr += 1;
        Reg::Gpr(register)
    } else {
        let register = XMM_ORDER[*xmm % XMM_ORDER.len()];
        *xmm += 1;
        Reg::Xmm(register)
    }
}

fn clobbers_register(clobbers: super::reg::Clobbers, register: Reg) -> bool {
    match register {
        Reg::Gpr(gpr) => clobbers.gpr & gpr.bit() != 0,
        Reg::Xmm(xmm) => clobbers.xmm & xmm.bit() != 0,
        Reg::Virtual(_) => false,
    }
}

fn register_view(register: Reg) -> X64Register {
    match register {
        Reg::Gpr(gpr) => X64Register::Gpr(gpr.code()),
        Reg::Xmm(xmm) => X64Register::Xmm(xmm.code()),
        Reg::Virtual(_) => X64Register::Gpr(0),
    }
}

fn is_gpr_type(ty: Type) -> bool {
    matches!(ty, Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Ptr)
}

/// 构造全部确定性用例。
pub(super) fn cases(target: TargetName) -> Result<Vec<X64Case>, X64HarnessError> {
    let baseline = target.descriptor().cpu_baseline;
    let mut cases = Vec::new();
    integer_cases(&mut cases, baseline)?;
    float_cases(&mut cases, baseline)?;
    compare_cases(&mut cases, baseline)?;
    convert_cases(&mut cases, baseline)?;
    select_and_trap_cases(&mut cases, baseline)?;
    atomic_cases(&mut cases, baseline)?;
    vector_cases(&mut cases, baseline)?;
    Ok(cases)
}

/// `bits` 位的掩码。
fn mask(bits: u32) -> u128 {
    if bits >= 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    }
}

/// 窄宽度的零扩展规范形。
fn narrow(bits: u32, value: u128) -> u128 {
    value & mask(bits)
}

/// 窄宽度的符号扩展（按位模式转换，避免有符号算术与 `as`）。
fn sign_extend(bits: u32, value: u128) -> i128 {
    let shift = 128 - bits;
    let shifted = narrow(bits, value) << shift;
    i128::from_le_bytes(shifted.to_le_bytes()) >> shift
}

/// 有符号值的无符号位模式。
const fn bits_of_int(value: i128) -> u128 {
    u128::from_le_bytes(value.to_le_bytes())
}

/// 布尔结果的位模式。
const fn bits_of_bool(value: bool) -> u128 {
    if value { 1 } else { 0 }
}

fn bits_of_f64(value: f64) -> u128 {
    u128::from(value.to_bits())
}

fn f32_of(value: u128) -> f32 {
    f32::from_bits(u32::try_from(value & u128::from(u32::MAX)).expect("低 32 位适配 u32"))
}

fn bits_of_f32(value: f32) -> u128 {
    u128::from(value.to_bits())
}

/// 低 64 位的按位解释。
fn low_u64(value: u128) -> u64 {
    u64::try_from(value & u128::from(u64::MAX)).expect("低 64 位适配 u64")
}

const fn integer_type(bits: u32) -> Type {
    match bits {
        8 => Type::I8,
        16 => Type::I16,
        32 => Type::I32,
        _ => Type::I64,
    }
}

const fn bits_of_type(ty: Type) -> u32 {
    match ty {
        Type::I8 => 8,
        Type::I16 => 16,
        Type::I32 | Type::F32 => 32,
        _ => 64,
    }
}

const fn compare_holds(condition: Condition, order: std::cmp::Ordering, equal: bool) -> bool {
    match condition {
        Condition::Eq => equal,
        Condition::Ne => !equal,
        Condition::Lt => matches!(order, std::cmp::Ordering::Less),
        Condition::Le => !matches!(order, std::cmp::Ordering::Greater),
        Condition::Gt => matches!(order, std::cmp::Ordering::Greater),
        Condition::Ge => !matches!(order, std::cmp::Ordering::Less),
    }
}

fn integer_cases(cases: &mut Vec<X64Case>, baseline: CpuBaseline) -> Result<(), X64HarnessError> {
    for bits in [8_u32, 16, 32, 64] {
        let ty = integer_type(bits);
        let (a, b) = (0x3F_u128, 0x25_u128);
        for (op, mnemonic) in [
            (IntOp::Add, "add"),
            (IntOp::Sub, "sub"),
            (IntOp::And, "and"),
            (IntOp::Or, "or"),
            (IntOp::Xor, "xor"),
            (IntOp::Mul, "mul"),
        ] {
            let represented = match op {
                IntOp::Add => a.wrapping_add(b),
                IntOp::Sub => a.wrapping_sub(b),
                IntOp::And => a & b,
                IntOp::Or => a | b,
                IntOp::Xor => a ^ b,
                _ => a.wrapping_mul(b),
            };
            cases.push(
                CaseBuilder::new(format!("int_{mnemonic}_i{bits}"), Op::Integer(op))
                    .operand(ty, a)
                    .operand(ty, b)
                    .result(ty)
                    .expected(narrow(bits, represented))
                    .build(baseline)?,
            );
        }
        cases.push(
            CaseBuilder::new(format!("int_neg_i{bits}"), Op::Integer(IntOp::Neg))
                .operand(ty, 3)
                .result(ty)
                .expected(narrow(bits, 0_u128.wrapping_sub(3)))
                .build(baseline)?,
        );
        cases.push(
            CaseBuilder::new(format!("int_not_i{bits}"), Op::Integer(IntOp::Not))
                .operand(ty, 0x5A)
                .result(ty)
                .expected(narrow(bits, !0x5A_u128))
                .build(baseline)?,
        );
        // 移位量大于位宽：语义按位宽取模，lowering 必须把移位量掩码。
        let amount = u128::from(bits) + 3;
        let value = 0xF0_u128;
        for (op, mnemonic, signed) in [
            (IntOp::Shl, "shl", false),
            (IntOp::ShrUnsigned, "shr_unsigned", false),
            (IntOp::ShrSigned, "shr_signed", true),
        ] {
            let shift = u32::try_from(amount & u128::from(bits - 1)).expect("移位量适配 u32");
            let expected = match op {
                IntOp::Shl => narrow(bits, value << shift),
                IntOp::ShrUnsigned => narrow(bits, value >> shift),
                _ => narrow(
                    bits,
                    bits_of_int(sign_extend(bits, value).wrapping_shr(shift)),
                ),
            };
            let _ = signed;
            cases.push(
                CaseBuilder::new(format!("int_{mnemonic}_i{bits}"), Op::Integer(op))
                    .operand(ty, value)
                    .operand(ty, amount)
                    .result(ty)
                    .expected(expected)
                    .build(baseline)?,
            );
        }
        for (op, mnemonic, signed, quotient) in [
            (IntOp::DivSigned, "div_signed", true, true),
            (IntOp::RemSigned, "rem_signed", true, false),
            (IntOp::DivUnsigned, "div_unsigned", false, true),
            (IntOp::RemUnsigned, "rem_unsigned", false, false),
        ] {
            // 有符号用例取 `MIN / -1`：`idiv` 的溢出陷阱，lowering 必须绕开。
            let (a, b) = if signed {
                (1_u128 << (bits - 1), narrow(bits, u128::MAX))
            } else {
                (0x7F, 5)
            };
            let expected = if signed {
                let (a, b) = (sign_extend(bits, a), sign_extend(bits, b));
                if quotient {
                    bits_of_int(a.wrapping_div(b))
                } else {
                    bits_of_int(a.wrapping_rem(b))
                }
            } else if quotient {
                a / b
            } else {
                a % b
            };
            cases.push(
                CaseBuilder::new(format!("int_{mnemonic}_i{bits}"), Op::Integer(op))
                    .operand(ty, a)
                    .operand(ty, b)
                    .result(ty)
                    .expected(narrow(bits, expected))
                    .build(baseline)?,
            );
        }
    }
    let (a, b) = (u128::from(u64::MAX), 1_u128);
    cases.push(
        CaseBuilder::new("int_add_carry", Op::Integer(IntOp::AddCarry))
            .operand(Type::I64, a)
            .operand(Type::I64, b)
            .operand(Type::I64, 0)
            .result(Type::I64)
            .result(Type::I64)
            .output(0)
            .extra_result(1, 1)
            .expected(narrow(64, a.wrapping_add(b)))
            .build(baseline)?,
    );
    let (wide_a, wide_b) = (0x1234_5678_9ABC_DEF0_u128, 0x0FED_CBA9_8765_4321_u128);
    let product = bits_of_int(sign_extend(64, wide_a).wrapping_mul(sign_extend(64, wide_b)));
    cases.push(
        CaseBuilder::new("int_mul_wide", Op::Integer(IntOp::MulWide))
            .operand(Type::I64, wide_a)
            .operand(Type::I64, wide_b)
            .result(Type::I64)
            .result(Type::I64)
            .output(0)
            .extra_result(1, product >> 64)
            .expected(narrow(64, product))
            .build(baseline)?,
    );
    Ok(())
}

fn float_cases(cases: &mut Vec<X64Case>, baseline: CpuBaseline) -> Result<(), X64HarnessError> {
    let f64_values = [(6.5_f64, 1.25_f64)];
    let (a, b) = f64_values[0];
    for (op, mnemonic, expected) in [
        (FloatOp::Add, "fadd", bits_of_f64(a + b)),
        (FloatOp::Sub, "fsub", bits_of_f64(a - b)),
        (FloatOp::Mul, "fmul", bits_of_f64(a * b)),
        (FloatOp::Div, "fdiv", bits_of_f64(a / b)),
    ] {
        cases.push(
            CaseBuilder::new(format!("float_{mnemonic}_f64"), Op::Float(op))
                .operand(Type::F64, bits_of_f64(a))
                .operand(Type::F64, bits_of_f64(b))
                .result(Type::F64)
                .expected(expected)
                .build(baseline)?,
        );
    }
    let (small_a, small_b) = (6.5_f32, 1.25_f32);
    for (op, mnemonic, expected) in [
        (FloatOp::Add, "fadd", bits_of_f32(small_a + small_b)),
        (FloatOp::Sub, "fsub", bits_of_f32(small_a - small_b)),
        (FloatOp::Mul, "fmul", bits_of_f32(small_a * small_b)),
        (FloatOp::Div, "fdiv", bits_of_f32(small_a / small_b)),
    ] {
        cases.push(
            CaseBuilder::new(format!("float_{mnemonic}_f32"), Op::Float(op))
                .operand(Type::F32, bits_of_f32(small_a))
                .operand(Type::F32, bits_of_f32(small_b))
                .result(Type::F32)
                .expected(expected)
                .build(baseline)?,
        );
    }
    // 取负只翻转符号位：NaN 的位模式也必须保持。
    for (ty, value, sign) in [
        (Type::F64, bits_of_f64(-6.5_f64), 63_u32),
        (Type::F32, bits_of_f32(-6.5_f32), 31),
    ] {
        cases.push(
            CaseBuilder::new(format!("float_neg_{ty:?}"), Op::Float(FloatOp::Neg))
                .operand(ty, value)
                .result(ty)
                .expected(value ^ (1_u128 << sign))
                .build(baseline)?,
        );
    }
    Ok(())
}

fn compare_cases(cases: &mut Vec<X64Case>, baseline: CpuBaseline) -> Result<(), X64HarnessError> {
    for bits in [8_u32, 32, 64] {
        let ty = integer_type(bits);
        for condition in [
            Condition::Eq,
            Condition::Ne,
            Condition::Lt,
            Condition::Le,
            Condition::Gt,
            Condition::Ge,
        ] {
            for signed in [true, false] {
                let (a, b) = (5_u128, 9_u128);
                let order = if signed {
                    sign_extend(bits, a).cmp(&sign_extend(bits, b))
                } else {
                    a.cmp(&b)
                };
                let expected = compare_holds(condition, order, a == b);
                cases.push(
                    CaseBuilder::new(
                        format!("compare_{condition:?}_{bits}_{signed}"),
                        Op::Compare { condition, signed },
                    )
                    .operand(ty, a)
                    .operand(ty, b)
                    .result(Type::I8)
                    .expected(bits_of_bool(expected))
                    .build(baseline)?,
                );
            }
        }
    }
    let f64_pairs = [
        (6.0_f64, 3.0_f64, "ordered"),
        (3.0, 6.0, "reversed"),
        (f64::NAN, 3.0, "nan_left"),
        (3.0, f64::NAN, "nan_right"),
    ];
    for condition in [
        Condition::Eq,
        Condition::Ne,
        Condition::Lt,
        Condition::Le,
        Condition::Gt,
        Condition::Ge,
    ] {
        for (left, right, label) in f64_pairs {
            cases.push(
                CaseBuilder::new(
                    format!("compare_f64_{condition:?}_{label}"),
                    Op::Compare {
                        condition,
                        signed: false,
                    },
                )
                .operand(Type::F64, bits_of_f64(left))
                .operand(Type::F64, bits_of_f64(right))
                .result(Type::I8)
                .expected(bits_of_bool(compare_floats(condition, left, right)))
                .build(baseline)?,
            );
        }
    }
    let f32_pairs = [
        (6.0_f32, 3.0_f32, "ordered"),
        (f32::NAN, 3.0_f32, "nan_left"),
    ];
    for condition in [
        Condition::Eq,
        Condition::Ne,
        Condition::Lt,
        Condition::Le,
        Condition::Gt,
        Condition::Ge,
    ] {
        for (left, right, label) in f32_pairs {
            cases.push(
                CaseBuilder::new(
                    format!("compare_f32_{condition:?}_{label}"),
                    Op::Compare {
                        condition,
                        signed: false,
                    },
                )
                .operand(Type::F32, bits_of_f32(left))
                .operand(Type::F32, bits_of_f32(right))
                .result(Type::I8)
                .expected(bits_of_bool(compare_floats(
                    condition,
                    f64::from(left),
                    f64::from(right),
                )))
                .build(baseline)?,
            );
        }
    }
    Ok(())
}

/// 浮点比较：无序条件（`Ne`）在 NaN 上成立，有序条件不成立。
fn compare_floats(condition: Condition, left: f64, right: f64) -> bool {
    match condition {
        Condition::Eq => left == right,
        Condition::Ne => left != right,
        Condition::Lt => left < right,
        Condition::Le => left <= right,
        Condition::Gt => left > right,
        Condition::Ge => left >= right,
    }
}

fn convert_cases(cases: &mut Vec<X64Case>, baseline: CpuBaseline) -> Result<(), X64HarnessError> {
    for (source, target, value) in [
        (Type::I8, Type::I64, 0xA5_u128),
        (Type::I32, Type::I64, 0x89AB_CDEF),
        (Type::I16, Type::I32, 0x8123),
    ] {
        cases.push(
            CaseBuilder::new(
                format!("zero_extend_{source:?}_{target:?}"),
                Op::Convert(Conversion::ZeroExtend),
            )
            .operand(source, value)
            .result(target)
            .expected(narrow(bits_of_type(source), value))
            .build(baseline)?,
        );
        cases.push(
            CaseBuilder::new(
                format!("sign_extend_{source:?}_{target:?}"),
                Op::Convert(Conversion::SignExtend),
            )
            .operand(source, value)
            .result(target)
            .expected(narrow(
                bits_of_type(target),
                bits_of_int(sign_extend(bits_of_type(source), value)),
            ))
            .build(baseline)?,
        );
    }
    for (target, value) in [
        (Type::I8, 0x1234_5678_9ABC_DE5A_u128),
        (Type::I16, 0x1234_5678_9ABC_DE5A),
        (Type::I32, 0x1234_5678_9ABC_DE5A),
    ] {
        cases.push(
            CaseBuilder::new(
                format!("truncate_{target:?}"),
                Op::Convert(Conversion::Truncate),
            )
            .operand(Type::I64, value)
            .result(target)
            .expected(narrow(bits_of_type(target), value))
            .build(baseline)?,
        );
    }
    // 整数转浮点：期望值取数学上精确的结果；u64 路径超过 2^53 也必须正确。
    let two_pow_32 = f64::from(u32::MAX) + 1.0;
    for (signed, source, value, expected, label) in [
        (
            true,
            Type::I8,
            0xF0_u128,
            bits_of_f64(f64::from(-16_i8)),
            "signed_i8",
        ),
        (
            true,
            Type::I32,
            0xFFFF_FFFF,
            bits_of_f64(f64::from(-1_i32)),
            "signed_i32",
        ),
        (
            false,
            Type::I32,
            0xFFFF_FFFF,
            bits_of_f64(f64::from(u32::MAX)),
            "unsigned_i32",
        ),
        (
            false,
            Type::I64,
            0xFFFF_FFFF_FFFF_FFFF,
            bits_of_f64(two_pow_32 * two_pow_32),
            "unsigned_i64_rounding",
        ),
    ] {
        cases.push(
            CaseBuilder::new(
                format!("int_to_f64_{label}"),
                Op::Convert(Conversion::IntToFloat { signed }),
            )
            .operand(source, value)
            .result(Type::F64)
            .expected(expected)
            .build(baseline)?,
        );
    }
    for (signed, value, expected, label) in [
        (
            true,
            bits_of_f64(-7.75_f64),
            narrow(64, bits_of_int(-7)),
            "negative",
        ),
        (true, bits_of_f64(4096.5_f64), 4096_u128, "positive"),
        (false, bits_of_f64(4096.5_f64), 4096, "unsigned"),
    ] {
        cases.push(
            CaseBuilder::new(
                format!("float_to_i64_{label}"),
                Op::Convert(Conversion::FloatToInt { signed }),
            )
            .operand(Type::F64, value)
            .result(Type::I64)
            .expected(expected)
            .build(baseline)?,
        );
    }
    cases.push(
        CaseBuilder::new("float_resize_down", Op::Convert(Conversion::FloatResize))
            .operand(Type::F64, bits_of_f64(1.5))
            .result(Type::F32)
            .expected(bits_of_f32(1.5))
            .build(baseline)?,
    );
    cases.push(
        CaseBuilder::new("float_resize_up", Op::Convert(Conversion::FloatResize))
            .operand(Type::F32, bits_of_f32(1.5))
            .result(Type::F64)
            .expected(bits_of_f64(1.5))
            .build(baseline)?,
    );
    for (source, target, value) in [
        (Type::I32, Type::F32, bits_of_f32(1.5)),
        (Type::I64, Type::F64, bits_of_f64(1.5)),
    ] {
        cases.push(
            CaseBuilder::new(
                format!("bitcast_{source:?}_{target:?}"),
                Op::Convert(Conversion::Bitcast),
            )
            .operand(source, value)
            .result(target)
            .expected(value)
            .build(baseline)?,
        );
    }
    for (conversion, label) in [
        (Conversion::PointerToInt, "pointer_to_int"),
        (Conversion::IntToPointer, "int_to_pointer"),
        (Conversion::PointerCast, "pointer_cast"),
        (Conversion::RawToReference, "raw_to_reference"),
    ] {
        cases.push(
            CaseBuilder::new(label, Op::Convert(conversion))
                .operand(Type::Ptr, 0x1234_5678)
                .result(Type::Ptr)
                .expected(0x1234_5678)
                .build(baseline)?,
        );
    }
    Ok(())
}

fn select_and_trap_cases(
    cases: &mut Vec<X64Case>,
    baseline: CpuBaseline,
) -> Result<(), X64HarnessError> {
    for (ty, yes, no) in [
        (Type::I32, 7_u128, 9),
        (Type::I64, 7, 9),
        (Type::Ptr, 0x1000, 0x2000),
        (Type::F64, bits_of_f64(1.5), bits_of_f64(2.5)),
        (Type::F32, bits_of_f32(1.5), bits_of_f32(2.5)),
    ] {
        for condition in [1_u128, 0] {
            cases.push(
                CaseBuilder::new(format!("select_{ty:?}_{condition}"), Op::Select)
                    .operand(Type::I8, condition)
                    .operand(ty, yes)
                    .operand(ty, no)
                    .result(ty)
                    .expected(if condition == 1 { yes } else { no })
                    .build(baseline)?,
            );
        }
    }
    for condition in [1_u128, 0] {
        cases.push(
            CaseBuilder::new(format!("trap_if_{condition}"), Op::TrapIf)
                .operand(Type::I8, condition)
                .expects_cold_edge(condition == 1)
                .build(baseline)?,
        );
    }
    Ok(())
}

fn atomic_cases(cases: &mut Vec<X64Case>, baseline: CpuBaseline) -> Result<(), X64HarnessError> {
    for bits in [32_u32, 64] {
        let ty = integer_type(bits);
        cases.push(
            CaseBuilder::new(
                format!("atomic_load_{bits}"),
                Op::Atomic {
                    op: AtomicOp::Load,
                    ordering: MemoryOrdering::Acquire,
                    failure: None,
                    align: bits,
                },
            )
            .memory_operand(Type::Ptr)
            .result(ty)
            .memory(0, 0x1122_3344_5566_7788, 0x1122_3344_5566_7788)
            .expected(narrow(bits, 0x1122_3344_5566_7788))
            .build(baseline)?,
        );
        let stored = narrow(bits, 0x00AB_CDEF);
        cases.push(
            CaseBuilder::new(
                format!("atomic_store_seqcst_{bits}"),
                Op::Atomic {
                    op: AtomicOp::Store,
                    ordering: MemoryOrdering::SeqCst,
                    failure: None,
                    align: bits,
                },
            )
            .memory_operand(Type::Ptr)
            .operand(ty, stored)
            .memory(0, 0, low_u64(stored))
            .build(baseline)?,
        );
        for (op, mnemonic, updates) in [
            (AtomicOp::Exchange, "exchange", 0_u8),
            (AtomicOp::Add, "add", 1),
            (AtomicOp::Sub, "sub", 2),
        ] {
            let initial = narrow(bits, 100);
            let operand = narrow(bits, 23);
            let stored = match updates {
                0 => operand,
                1 => narrow(bits, initial.wrapping_add(operand)),
                _ => narrow(bits, initial.wrapping_sub(operand)),
            };
            cases.push(
                CaseBuilder::new(
                    format!("atomic_{mnemonic}_{bits}"),
                    Op::Atomic {
                        op,
                        ordering: MemoryOrdering::AcqRel,
                        failure: None,
                        align: bits,
                    },
                )
                .memory_operand(Type::Ptr)
                .operand(ty, operand)
                .result(ty)
                .memory(0, low_u64(initial), low_u64(stored))
                .expected(initial)
                .build(baseline)?,
            );
        }
        for success in [true, false] {
            let initial = narrow(bits, 0x55);
            let desired = narrow(bits, 0xAA);
            cases.push(
                CaseBuilder::new(
                    format!("atomic_cmpxchg_{bits}_{success}"),
                    Op::Atomic {
                        op: AtomicOp::CompareExchange,
                        ordering: MemoryOrdering::SeqCst,
                        failure: Some(MemoryOrdering::Acquire),
                        align: bits,
                    },
                )
                .memory_operand(Type::Ptr)
                .operand(ty, if success { initial } else { initial + 1 })
                .operand(ty, desired)
                .result(ty)
                .result(Type::I8)
                .output(0)
                .extra_result(1, bits_of_bool(success))
                .memory(
                    0,
                    low_u64(initial),
                    low_u64(if success { desired } else { initial }),
                )
                .expected(initial)
                .build(baseline)?,
            );
        }
    }
    cases.push(
        CaseBuilder::new(
            "atomic_fence_seqcst",
            Op::Atomic {
                op: AtomicOp::Fence,
                ordering: MemoryOrdering::SeqCst,
                failure: None,
                align: 8,
            },
        )
        .build(baseline)?,
    );
    Ok(())
}

const fn scalar_of_lane(lane: Lane) -> Type {
    match lane {
        Lane::I8 => Type::I8,
        Lane::I16 => Type::I16,
        Lane::I32 => Type::I32,
        Lane::I64 => Type::I64,
        Lane::F32 => Type::F32,
        Lane::F64 => Type::F64,
    }
}

/// 每个 lane 的字节数。
const fn lane_bytes(lane: Lane) -> u32 {
    match lane {
        Lane::I8 => 1,
        Lane::I16 => 2,
        Lane::I32 | Lane::F32 => 4,
        Lane::I64 | Lane::F64 => 8,
    }
}

/// lane 位宽。
const fn lane_bits(lane: Lane) -> u32 {
    lane_bytes(lane) * 8
}

/// 每个 lane 重复同一个值的 128 位向量。
fn splat_lanes(lane: Lane, value: u128) -> u128 {
    let bytes = lane_bytes(lane);
    let pattern = narrow(lane_bits(lane), value);
    let mut out = 0_u128;
    let mut offset = 0_u32;
    while offset < 16 {
        out |= pattern << (offset * 8);
        offset += bytes;
    }
    out
}

/// 每 lane 取不同样本的向量。
fn lane_sample(lane: Lane, seed: u128) -> u128 {
    let bits = lane_bits(lane);
    let mut out = 0_u128;
    for index in 0..(16 / lane_bytes(lane)) {
        let value = narrow(bits, seed.wrapping_add(u128::from(index)));
        out |= value << (index * bits);
    }
    out
}

/// 逐 lane 应用标量运算。
fn lane_combine(lane: Lane, operation: impl Fn(u128, u128) -> u128, a: u128, b: u128) -> u128 {
    let bits = lane_bits(lane);
    let mut out = 0_u128;
    for index in 0..(16 / lane_bytes(lane)) {
        let shift = index * bits;
        let left = narrow(bits, a >> shift);
        let right = narrow(bits, b >> shift);
        out |= narrow(bits, operation(left, right)) << shift;
    }
    out
}

/// 逐 lane 有符号比较；成立时该 lane 为全 1。
fn lane_compare(lane: Lane, condition: Condition, a: u128, b: u128) -> u128 {
    let bits = lane_bits(lane);
    let mut out = 0_u128;
    for index in 0..(16 / lane_bytes(lane)) {
        let shift = index * bits;
        let left = sign_extend(bits, a >> shift);
        let right = sign_extend(bits, b >> shift);
        if compare_holds(condition, left.cmp(&right), left == right) {
            out |= mask(bits) << shift;
        }
    }
    out
}

/// 逐 lane 浮点比较；成立时该 lane 为全 1。
fn lane_compare_float(
    lane: Lane,
    condition: Condition,
    a: u128,
    b: u128,
    to_f64: impl Fn(u128) -> f64,
) -> u128 {
    let bits = lane_bits(lane);
    let mut out = 0_u128;
    for index in 0..(16 / lane_bytes(lane)) {
        let shift = index * bits;
        if compare_floats(condition, to_f64(a >> shift), to_f64(b >> shift)) {
            out |= mask(bits) << shift;
        }
    }
    out
}

fn extract_lane(lane: Lane, index: u8, vector: u128) -> u128 {
    narrow(
        lane_bits(lane),
        vector >> (u32::from(index) * lane_bits(lane)),
    )
}

fn insert_lane(lane: Lane, index: u8, vector: u128, value: u128) -> u128 {
    let bits = lane_bits(lane);
    let shift = u32::from(index) * bits;
    (vector & !(mask(bits) << shift)) | (narrow(bits, value) << shift)
}

/// 按 lane 有符号累加，结果按 64 位环绕。
fn reduce_sum(lane: Lane, vector: u128) -> u128 {
    let bits = lane_bits(lane);
    let mut total = 0_i128;
    for index in 0..(16 / lane_bytes(lane)) {
        total = total.wrapping_add(sign_extend(bits, vector >> (index * bits)));
    }
    narrow(64, bits_of_int(total))
}

/// `lane` 宽度的浮点向量：`values` 是已按 lane 宽度截断的位模式，其余 lane 为 0。
fn float_vector(lane: Lane, values: [u128; 2]) -> u128 {
    let bits = lane_bits(lane);
    let mut out = 0_u128;
    for (index, value) in values.iter().enumerate() {
        out |= narrow(bits, *value) << (u32::try_from(index).expect("lane 下标适配 u32") * bits);
    }
    out
}

/// 单源 32 位置换：lane 顺序倒置。
const fn shuffle_single_mask() -> [u8; 16] {
    [12, 13, 14, 15, 8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3]
}

/// 双源合并：低半来自第一个操作数、高半来自第二个，各自倒置。
const fn shuffle_pair_mask() -> [u8; 16] {
    [12, 13, 14, 15, 8, 9, 10, 11, 20, 21, 22, 23, 16, 17, 18, 19]
}

/// 64 位宽合并：lane0 取第一个操作数的高半，lane1 取第二个操作数的低半。
const fn shuffle_wide_mask() -> [u8; 16] {
    [8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23]
}

/// mask 是源字节下标：`< 16` 取第一个操作数，`>= 16` 取第二个。
fn shuffle_expected(mask: [u8; 16], a: u128, b: u128) -> u128 {
    let mut out = 0_u128;
    for (index, source) in mask.iter().enumerate() {
        let source = u32::from(*source);
        let byte = if source < 16 {
            (a >> (source * 8)) & 0xFF
        } else {
            (b >> ((source - 16) * 8)) & 0xFF
        };
        out |= byte << (u32::try_from(index).expect("下标适配 u32") * 8);
    }
    out
}

fn vector_cases(cases: &mut Vec<X64Case>, baseline: CpuBaseline) -> Result<(), X64HarnessError> {
    for lane in [
        Lane::I8,
        Lane::I16,
        Lane::I32,
        Lane::I64,
        Lane::F32,
        Lane::F64,
    ] {
        let value = if matches!(lane, Lane::F32 | Lane::F64) {
            bits_of_f32(3.5)
        } else {
            0x7F
        };
        cases.push(
            CaseBuilder::new(
                format!("vector_splat_{lane:?}"),
                Op::Vector(VectorOp::Splat),
            )
            .operand(scalar_of_lane(lane), value)
            .result(Type::V128(lane))
            .expected(splat_lanes(lane, value))
            .build(baseline)?,
        );
    }
    for lane in [Lane::I8, Lane::I16, Lane::I32, Lane::I64] {
        for (op, mnemonic, add) in [(VectorOp::Add, "add", true), (VectorOp::Sub, "sub", false)] {
            let a = lane_sample(lane, 0x11);
            let b = lane_sample(lane, 0x05);
            let expected = lane_combine(
                lane,
                |left, right| {
                    if add {
                        left.wrapping_add(right)
                    } else {
                        left.wrapping_sub(right)
                    }
                },
                a,
                b,
            );
            cases.push(
                CaseBuilder::new(format!("vector_{mnemonic}_{lane:?}"), Op::Vector(op))
                    .operand(Type::V128(lane), a)
                    .operand(Type::V128(lane), b)
                    .result(Type::V128(lane))
                    .expected(expected)
                    .build(baseline)?,
            );
        }
    }
    for lane in [Lane::I16, Lane::I32] {
        let a = lane_sample(lane, 0x03);
        let b = lane_sample(lane, 0x05);
        cases.push(
            CaseBuilder::new(format!("vector_mul_{lane:?}"), Op::Vector(VectorOp::Mul))
                .operand(Type::V128(lane), a)
                .operand(Type::V128(lane), b)
                .result(Type::V128(lane))
                .expected(lane_combine(
                    lane,
                    |left, right| left.wrapping_mul(right),
                    a,
                    b,
                ))
                .build(baseline)?,
        );
    }
    let float_a = float_vector(Lane::F32, [bits_of_f32(6.5), bits_of_f32(1.5)]);
    let float_b = float_vector(Lane::F32, [bits_of_f32(1.25), bits_of_f32(2.0)]);
    for (op, mnemonic, operation) in [
        (VectorOp::Add, "vector_add_f32", 0_u8),
        (VectorOp::Sub, "vector_sub_f32", 1),
        (VectorOp::Mul, "vector_mul_f32", 2),
    ] {
        let expected = lane_combine(
            Lane::F32,
            |left, right| match operation {
                0 => bits_of_f32(f32_of(left) + f32_of(right)),
                1 => bits_of_f32(f32_of(left) - f32_of(right)),
                _ => bits_of_f32(f32_of(left) * f32_of(right)),
            },
            float_a,
            float_b,
        );
        cases.push(
            CaseBuilder::new(mnemonic, Op::Vector(op))
                .operand(Type::V128(Lane::F32), float_a)
                .operand(Type::V128(Lane::F32), float_b)
                .result(Type::V128(Lane::F32))
                .expected(expected)
                .build(baseline)?,
        );
    }
    let double_a = float_vector(Lane::F64, [bits_of_f64(6.5), bits_of_f64(1.5)]);
    let double_b = float_vector(Lane::F64, [bits_of_f64(1.25), bits_of_f64(2.0)]);
    cases.push(
        CaseBuilder::new("vector_add_f64", Op::Vector(VectorOp::Add))
            .operand(Type::V128(Lane::F64), double_a)
            .operand(Type::V128(Lane::F64), double_b)
            .result(Type::V128(Lane::F64))
            .expected(lane_combine(
                Lane::F64,
                |left, right| {
                    bits_of_f64(f64::from_bits(low_u64(left)) + f64::from_bits(low_u64(right)))
                },
                double_a,
                double_b,
            ))
            .build(baseline)?,
    );
    for (op, mnemonic, operation) in [
        (VectorOp::And, "vector_and", 0_u8),
        (VectorOp::Or, "vector_or", 1),
        (VectorOp::Xor, "vector_xor", 2),
    ] {
        let a = lane_sample(Lane::I32, 0x33);
        let b = lane_sample(Lane::I32, 0x0F);
        let expected = lane_combine(
            Lane::I32,
            |left, right| match operation {
                0 => left & right,
                1 => left | right,
                _ => left ^ right,
            },
            a,
            b,
        );
        cases.push(
            CaseBuilder::new(mnemonic, Op::Vector(op))
                .operand(Type::V128(Lane::I32), a)
                .operand(Type::V128(Lane::I32), b)
                .result(Type::V128(Lane::I32))
                .expected(expected)
                .build(baseline)?,
        );
    }
    for lane in [Lane::I8, Lane::I16, Lane::I32] {
        for condition in [
            Condition::Eq,
            Condition::Lt,
            Condition::Ge,
            Condition::Ne,
            Condition::Gt,
        ] {
            let a = lane_sample(lane, 3);
            let b = lane_sample(lane, 9);
            cases.push(
                CaseBuilder::new(
                    format!("vector_compare_{condition:?}_{lane:?}"),
                    Op::Vector(VectorOp::Compare(condition)),
                )
                .operand(Type::V128(lane), a)
                .operand(Type::V128(lane), b)
                .result(Type::V128(lane))
                .expected(lane_compare(lane, condition, a, b))
                .build(baseline)?,
            );
        }
    }
    for lane in [Lane::F32, Lane::F64] {
        for condition in [
            Condition::Eq,
            Condition::Lt,
            Condition::Ge,
            Condition::Ne,
            Condition::Le,
        ] {
            // NaN 也在其中：无序条件必须为真、有序条件必须为假。
            let (one_and_a_half, two_and_a_half, nan) = if lane == Lane::F32 {
                (
                    bits_of_f32(1.5),
                    bits_of_f32(2.5),
                    u128::from(f32::NAN.to_bits()),
                )
            } else {
                (
                    bits_of_f64(1.5),
                    bits_of_f64(2.5),
                    u128::from(f64::NAN.to_bits()),
                )
            };
            let a = float_vector(lane, [one_and_a_half, nan]);
            let b = float_vector(lane, [two_and_a_half, two_and_a_half]);
            let to_f64: fn(u128) -> f64 = if lane == Lane::F32 {
                |raw| f64::from(f32_of(raw))
            } else {
                |raw| f64::from_bits(low_u64(raw))
            };
            cases.push(
                CaseBuilder::new(
                    format!("vector_compare_{condition:?}_{lane:?}"),
                    Op::Vector(VectorOp::Compare(condition)),
                )
                .operand(Type::V128(lane), a)
                .operand(Type::V128(lane), b)
                .result(Type::V128(lane))
                .expected(lane_compare_float(lane, condition, a, b, to_f64))
                .build(baseline)?,
            );
        }
    }
    for (lane, index) in [
        (Lane::I16, 3_u8),
        (Lane::I32, 2),
        (Lane::I64, 1),
        (Lane::F32, 1),
        (Lane::F64, 1),
    ] {
        let vector = lane_sample(lane, 0x40);
        cases.push(
            CaseBuilder::new(
                format!("vector_extract_{lane:?}_{index}"),
                Op::Vector(VectorOp::Extract(index)),
            )
            .operand(Type::V128(lane), vector)
            .result(scalar_of_lane(lane))
            .expected(extract_lane(lane, index, vector))
            .build(baseline)?,
        );
    }
    for (lane, index, value) in [
        (Lane::I8, 3_u8, 0x5A_u128),
        (Lane::I16, 2, 0x1234),
        (Lane::I32, 1, 0x89AB_CDEF),
        (Lane::I64, 1, 0x1122_3344_5566_7788),
        (Lane::F64, 1, bits_of_f64(9.5)),
    ] {
        let base = lane_sample(lane, 0x11);
        cases.push(
            CaseBuilder::new(
                format!("vector_insert_{lane:?}_{index}"),
                Op::Vector(VectorOp::Insert(index)),
            )
            .operand(Type::V128(lane), base)
            .operand(scalar_of_lane(lane), value)
            .result(Type::V128(lane))
            .expected(insert_lane(lane, index, base, value))
            .build(baseline)?,
        );
    }
    for lane in [Lane::I8, Lane::I16, Lane::I32, Lane::I64] {
        let value = lane_sample(lane, 0x02);
        cases.push(
            CaseBuilder::new(
                format!("vector_reduce_{lane:?}"),
                Op::Vector(VectorOp::ReduceAdd),
            )
            .operand(Type::V128(lane), value)
            .result(Type::I64)
            .expected(reduce_sum(lane, value))
            .build(baseline)?,
        );
    }
    // I32 lane 的 4 项和最多 34 位：必须走 64 位累加，32 位累加会丢高位。
    let wide_i32 = (u128::from(u32::MAX) << 96)
        | (u128::from(0x7FFF_FFFF_u32) << 64)
        | (u128::from(0x8000_0000_u32) << 32)
        | u128::from(0x7FFF_FFFF_u32);
    cases.push(
        CaseBuilder::new("vector_reduce_I32_wide", Op::Vector(VectorOp::ReduceAdd))
            .operand(Type::V128(Lane::I32), wide_i32)
            .result(Type::I64)
            .expected(reduce_sum(Lane::I32, wide_i32))
            .build(baseline)?,
    );
    let a = lane_sample(Lane::I32, 0x10);
    let b = lane_sample(Lane::I32, 0x20);
    for (name, mask) in [
        ("vector_shuffle_single", shuffle_single_mask()),
        ("vector_shuffle_pair", shuffle_pair_mask()),
        ("vector_shuffle_wide", shuffle_wide_mask()),
    ] {
        cases.push(
            CaseBuilder::new(name, Op::Vector(VectorOp::Shuffle(mask)))
                .operand(Type::V128(Lane::I32), a)
                .operand(Type::V128(Lane::I32), b)
                .result(Type::V128(Lane::I32))
                .expected(shuffle_expected(mask, a, b))
                .build(baseline)?,
        );
    }
    Ok(())
}
