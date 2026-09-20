//! lowering 的序列形状、关键上界与形式化不变量测试。
//!
//! 最强的一条不变量是「域内每个 op × 类型组合都能 lower、过 verifier、能编码」：它同时
//! 覆盖 form 表缺项、clobber 登记与寄存器纪律。形状断言只钉关键分支（除法 `MIN/-1`、
//! 无符号 u64→float、窄宽度移位掩码、浮点比较 parity、进位链），不复制整段序列。

use super::*;
use crate::backend::x64::encode::assemble;
use crate::backend::x64::inst::{Operand, RelocKind, RelocTarget};
use crate::backend::x64::lower as lowering;
use crate::backend::x64::reg::Reg;
use crate::backend::x64::table::Access;
use crate::backend::x64::verify::verify_sequence;
use crate::frontend::gir::body::{MemoryOrdering, ScopeId, SourceInfo};
use crate::frontend::hir::Location;
use crate::lir::body::{
    AtomicOp, Condition, Conversion, FloatOp, IntOp, Lane, Op, Type, ValueType, VectorOp,
};
use crate::runtime::cage_control::CAGE_CONTROL_FIELDS;
use crate::target::CpuBaseline;

fn source() -> SourceInfo {
    SourceInfo {
        location: Location {
            source: 0,
            start: 0,
            end: 0,
            expansion: u32::MAX,
        },
        scope: ScopeId(0),
    }
}

fn value(ty: Type, index: u32) -> SiteValue {
    SiteValue {
        ty: ValueType::scalar(ty),
        reg: Reg::Virtual(index),
    }
}

fn operands(types: &[Type]) -> Vec<SiteValue> {
    types
        .iter()
        .enumerate()
        .map(|(index, ty)| value(*ty, u32::try_from(index).expect("操作数个数适配 u32")))
        .collect()
}

fn results(types: &[Type]) -> Vec<SiteValue> {
    types
        .iter()
        .enumerate()
        .map(|(index, ty)| {
            let offset = 16 + u32::try_from(index).expect("结果个数适配 u32");
            value(*ty, offset)
        })
        .collect()
}

/// lower 并做完整校验：verifier 通过、clobber 集合一致、结果被写、操作数不被写、可编码。
fn checked(op: &Op, operand_types: &[Type], result_types: &[Type]) -> Lowered {
    let operands = operands(operand_types);
    let results = results(result_types);
    let lowered = lowering::lower(op, &operands, &results, &source())
        .unwrap_or_else(|error| panic!("{op:?} 必须能 lower：{error}"));
    let accumulated = verify_sequence(&lowered.sequence, CpuBaseline::X86_64V1)
        .unwrap_or_else(|error| panic!("{op:?} 必须过 verifier：{error}"));
    assert_eq!(
        accumulated, lowered.clobbers,
        "{op:?} 的 clobber 集合必须与序列实际写入一致"
    );
    // 方向不变量：结果必须被写，操作数必须不被写——反向的操作数顺序在这里暴露。
    let written = written_virtual_registers(&lowered);
    for result in &results {
        let Reg::Virtual(id) = result.reg else {
            continue;
        };
        if result.ty.ty == Type::Flags {
            // `Flags` 结果由标志位承载，没有目标寄存器。
            continue;
        }
        assert!(
            written.contains(&id),
            "{op:?} 的结果 v{id} 没有被写入：操作数顺序接反了"
        );
    }
    for operand in &operands {
        let Reg::Virtual(id) = operand.reg else {
            continue;
        };
        assert!(
            !written.contains(&id),
            "{op:?} 把操作数 v{id} 当成目标写了：操作数顺序接反了"
        );
    }
    assemble(&lowered.sequence).unwrap_or_else(|error| panic!("{op:?} 必须能编码：{error}"));
    lowered
}

/// 序列里被写过的虚拟寄存器编号。
fn written_virtual_registers(lowered: &Lowered) -> Vec<u32> {
    let mut written = Vec::new();
    for inst in &lowered.sequence.instructions {
        let form = table::form(inst.form);
        for (index, operand) in inst.operands.iter().enumerate() {
            let Some(access) = form.access.get(index) else {
                continue;
            };
            if !matches!(access, Access::Write | Access::ReadWrite) {
                continue;
            }
            if let Operand::Reg(Reg::Virtual(id)) = operand {
                written.push(*id);
            }
        }
    }
    written
}

fn mnemonics(lowered: &Lowered) -> Vec<&'static str> {
    lowered
        .sequence
        .instructions
        .iter()
        .map(|inst| table::form(inst.form).mnemonic)
        .collect()
}

/// 域内每个 op 的类型矩阵都在 lower 后通过 verifier 与编码器。
#[test]
fn every_domain_op_lowers_verifies_and_encodes() {
    let ints = [Type::I8, Type::I16, Type::I32, Type::I64];
    for ty in ints {
        checked(&Op::IConst(0x1234_5678), &[], &[ty]);
        for op in [
            IntOp::Add,
            IntOp::Sub,
            IntOp::Mul,
            IntOp::DivSigned,
            IntOp::DivUnsigned,
            IntOp::RemSigned,
            IntOp::RemUnsigned,
            IntOp::And,
            IntOp::Or,
            IntOp::Xor,
            IntOp::Shl,
            IntOp::ShrSigned,
            IntOp::ShrUnsigned,
        ] {
            checked(&Op::Integer(op), &[ty, ty], &[ty]);
        }
        for op in [IntOp::Neg, IntOp::Not] {
            checked(&Op::Integer(op), &[ty], &[ty]);
        }
        for condition in [
            Condition::Eq,
            Condition::Ne,
            Condition::Lt,
            Condition::Le,
            Condition::Gt,
            Condition::Ge,
        ] {
            for signed in [true, false] {
                checked(&Op::Compare { condition, signed }, &[ty, ty], &[Type::I8]);
                checked(
                    &Op::Compare { condition, signed },
                    &[ty, ty],
                    &[Type::Flags],
                );
            }
            checked(
                &Op::Convert(Conversion::IntToFloat { signed: true }),
                &[ty],
                &[Type::F64],
            );
            checked(
                &Op::Convert(Conversion::IntToFloat { signed: false }),
                &[ty],
                &[Type::F32],
            );
        }
    }
    for ty in [Type::F32, Type::F64] {
        checked(&Op::FConst(0x3FF0_0000_0000_0000), &[], &[ty]);
        for op in [FloatOp::Add, FloatOp::Sub, FloatOp::Mul, FloatOp::Div] {
            checked(&Op::Float(op), &[ty, ty], &[ty]);
        }
        checked(&Op::Float(FloatOp::Neg), &[ty], &[ty]);
        for condition in [
            Condition::Eq,
            Condition::Ne,
            Condition::Lt,
            Condition::Le,
            Condition::Gt,
            Condition::Ge,
        ] {
            checked(
                &Op::Compare {
                    condition,
                    signed: false,
                },
                &[ty, ty],
                &[Type::I8],
            );
            checked(
                &Op::Compare {
                    condition,
                    signed: false,
                },
                &[ty, ty],
                &[Type::Flags],
            );
        }
        checked(
            &Op::Convert(Conversion::FloatToInt { signed: true }),
            &[ty],
            &[Type::I32],
        );
        checked(
            &Op::Convert(Conversion::FloatToInt { signed: false }),
            &[ty],
            &[Type::I64],
        );
        checked(&Op::Select, &[Type::I8, ty, ty], &[ty]);
    }
    // 转换族。
    for target in [Type::I16, Type::I32, Type::I64] {
        checked(&Op::Convert(Conversion::ZeroExtend), &[Type::I8], &[target]);
    }
    for (source, target) in [
        (Type::I8, Type::I32),
        (Type::I8, Type::I64),
        (Type::I16, Type::I64),
        (Type::I32, Type::I64),
    ] {
        checked(&Op::Convert(Conversion::SignExtend), &[source], &[target]);
    }
    for (source, target) in [
        (Type::I64, Type::I8),
        (Type::I64, Type::I16),
        (Type::I64, Type::I32),
    ] {
        checked(&Op::Convert(Conversion::Truncate), &[source], &[target]);
    }
    checked(
        &Op::Convert(Conversion::FloatResize),
        &[Type::F32],
        &[Type::F64],
    );
    checked(
        &Op::Convert(Conversion::FloatResize),
        &[Type::F64],
        &[Type::F32],
    );
    checked(
        &Op::Convert(Conversion::Bitcast),
        &[Type::I32],
        &[Type::F32],
    );
    checked(
        &Op::Convert(Conversion::Bitcast),
        &[Type::F64],
        &[Type::I64],
    );
    for conversion in [
        Conversion::PointerToInt,
        Conversion::IntToPointer,
        Conversion::PointerCast,
        Conversion::RawToReference,
    ] {
        checked(&Op::Convert(conversion), &[Type::Ptr], &[Type::Ptr]);
    }
    // 多结果整数：进位链与宽乘法。
    for op in [IntOp::AddCarry, IntOp::SubBorrow] {
        checked(&Op::Integer(op), &[Type::I64; 3], &[Type::I64; 2]);
    }
    checked(
        &Op::Integer(IntOp::MulWide),
        &[Type::I64; 2],
        &[Type::I64; 2],
    );
    // Select 的整型、指针与浮点值面。
    checked(&Op::Select, &[Type::I8, Type::I64, Type::I64], &[Type::I64]);
    checked(&Op::Select, &[Type::I8, Type::Ptr, Type::Ptr], &[Type::Ptr]);
    checked(&Op::TrapIf, &[Type::I8], &[]);
    // 原子族：orderings 取自 LIR verifier 允许的组合。
    for op in [
        AtomicOp::Load,
        AtomicOp::Exchange,
        AtomicOp::Add,
        AtomicOp::Sub,
        AtomicOp::CompareExchange,
    ] {
        let operand_types: &[Type] = match op {
            AtomicOp::Load => &[Type::Ptr],
            AtomicOp::CompareExchange => &[Type::Ptr, Type::I64, Type::I64],
            _ => &[Type::Ptr, Type::I64],
        };
        let result_types: &[Type] = match op {
            AtomicOp::Load | AtomicOp::Exchange | AtomicOp::Add | AtomicOp::Sub => &[Type::I64],
            _ => &[Type::I64, Type::I8],
        };
        let orderings: &[MemoryOrdering] = match op {
            AtomicOp::Load => &[
                MemoryOrdering::Relaxed,
                MemoryOrdering::Acquire,
                MemoryOrdering::SeqCst,
            ],
            _ => &[
                MemoryOrdering::Relaxed,
                MemoryOrdering::Acquire,
                MemoryOrdering::Release,
                MemoryOrdering::AcqRel,
                MemoryOrdering::SeqCst,
            ],
        };
        for ordering in orderings {
            let failure = (op == AtomicOp::CompareExchange).then_some(MemoryOrdering::Relaxed);
            checked(
                &Op::Atomic {
                    op,
                    ordering: *ordering,
                    failure,
                    align: 8,
                },
                operand_types,
                result_types,
            );
        }
    }
    for ordering in [
        MemoryOrdering::Relaxed,
        MemoryOrdering::Release,
        MemoryOrdering::SeqCst,
    ] {
        checked(
            &Op::Atomic {
                op: AtomicOp::Store,
                ordering,
                failure: None,
                align: 8,
            },
            &[Type::Ptr, Type::I64],
            &[],
        );
    }
    for ordering in [
        MemoryOrdering::Acquire,
        MemoryOrdering::Release,
        MemoryOrdering::AcqRel,
        MemoryOrdering::SeqCst,
    ] {
        checked(
            &Op::Atomic {
                op: AtomicOp::Fence,
                ordering,
                failure: None,
                align: 0,
            },
            &[],
            &[],
        );
    }
    // 宽度 1/2/4/8 全覆盖。
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64] {
        let align = ty.bytes().expect("整数有字节宽度");
        for op in [
            AtomicOp::Load,
            AtomicOp::Store,
            AtomicOp::Exchange,
            AtomicOp::Add,
        ] {
            let operand_types: &[Type] = if op == AtomicOp::Load {
                &[Type::Ptr]
            } else {
                &[Type::Ptr, ty]
            };
            let result_types: &[Type] = match op {
                AtomicOp::Load | AtomicOp::Exchange | AtomicOp::Add => &[ty],
                // 宽度 1/2/4/8 的存储没有结果。
                _ => &[],
            };
            checked(
                &Op::Atomic {
                    op,
                    ordering: MemoryOrdering::SeqCst,
                    failure: None,
                    align: u32::try_from(align).expect("宽度适配 u32"),
                },
                operand_types,
                result_types,
            );
        }
    }
    checked(&Op::DecodeCompressedRef, &[Type::I64], &[Type::Ptr]);
}

/// 整数除法：I64 有符号走 `MIN/-1` 单独环绕，其余宽度不做分支。
#[test]
fn signed_wide_division_carries_min_wrap_branch() {
    let quotient = checked(
        &Op::Integer(IntOp::DivSigned),
        &[Type::I64; 2],
        &[Type::I64],
    );
    let names = mnemonics(&quotient);
    assert_eq!(
        names,
        [
            "mov", "mov", "cmp", "je", "cqo", "idiv", "mov", "jmp", "mov", "neg"
        ],
        "I64 有符号除法必须单独处理 MIN/-1"
    );
    let labels = &quotient.sequence.labels;
    assert_eq!(labels.len(), 2, "MIN/-1 与收尾各一个标签");

    let remainder = checked(
        &Op::Integer(IntOp::RemSigned),
        &[Type::I64; 2],
        &[Type::I64],
    );
    assert_eq!(
        mnemonics(&remainder).last(),
        Some(&"xor"),
        "MIN/-1 的余数为 0"
    );

    // 32 位有符号除法把源提升到 64 位，避免 32 位 MIN/-1 陷阱。
    let narrow = checked(
        &Op::Integer(IntOp::DivSigned),
        &[Type::I32; 2],
        &[Type::I32],
    );
    assert_eq!(
        mnemonics(&narrow),
        ["movsxd", "movsxd", "cqo", "idiv", "mov"]
    );
    assert!(
        !mnemonics(&narrow).contains(&"cmp"),
        "32 位路径不需要 MIN 分支"
    );
    assert!(
        narrow.clobbers.contains_gpr(Gpr::Rax) && narrow.clobbers.contains_gpr(Gpr::Rdx),
        "除法必须登记 rax/rdx"
    );
}

/// 无符号 u64 → 浮点：最高位为 1 时走折半翻倍分支。
#[test]
fn unsigned_wide_to_float_has_halving_branch() {
    let widened = checked(
        &Op::Convert(Conversion::IntToFloat { signed: false }),
        &[Type::I64],
        &[Type::F64],
    );
    let names = mnemonics(&widened);
    assert_eq!(
        names,
        [
            "test", "js", "cvtsi2sd", "jmp", "mov", "shr", "mov", "and", "or", "cvtsi2sd", "addsd"
        ],
        "u64→F64 必须按 (v >> 1) | (v & 1) 转换后翻倍"
    );
    assert_eq!(widened.sequence.labels.len(), 2);
    // 更窄的无符号源零扩展后直接转换。
    let narrow = checked(
        &Op::Convert(Conversion::IntToFloat { signed: false }),
        &[Type::I32],
        &[Type::F32],
    );
    assert_eq!(mnemonics(&narrow), ["mov", "cvtsi2ss"]);
}

/// 窄宽度移位要按操作数位宽取模，32/64 位交给硬件。
#[test]
fn narrow_shifts_mask_count_by_operand_width() {
    for (ty, mask) in [(Type::I8, 7_u64), (Type::I16, 15_u64)] {
        let shifted = checked(&Op::Integer(IntOp::Shl), &[ty, ty], &[ty]);
        let mask_immediate = shifted
            .sequence
            .instructions
            .iter()
            .find(|inst| table::form(inst.form).mnemonic == "and")
            .expect("窄宽度移位必须补掩码");
        assert_eq!(mask_immediate.operands[1], Operand::Imm(mask));
    }
    for ty in [Type::I32, Type::I64] {
        let shifted = checked(&Op::Integer(IntOp::ShrUnsigned), &[ty, ty], &[ty]);
        assert!(
            !mnemonics(&shifted).contains(&"and"),
            "32/64 位移位与硬件语义一致，不需要掩码"
        );
    }
    // 有符号右移窄宽度：先把值符号扩展到 32 位，移位后再收回规范形。
    let arithmetic = checked(
        &Op::Integer(IntOp::ShrSigned),
        &[Type::I16, Type::I16],
        &[Type::I16],
    );
    assert_eq!(
        mnemonics(&arithmetic),
        ["mov", "and", "movsx", "sar", "movzx"]
    );
}

/// 浮点比较无法用单个条件码表达 NaN 语义，必须补 parity 分支。
#[test]
fn float_compare_uses_parity_fixup() {
    for condition in [Condition::Eq, Condition::Ne, Condition::Lt, Condition::Ge] {
        let compared = checked(
            &Op::Compare {
                condition,
                signed: false,
            },
            &[Type::F64; 2],
            &[Type::I8],
        );
        let names = mnemonics(&compared);
        assert!(names.contains(&"ucomisd"), "{condition:?} 用 ucomisd 定序");
        assert!(
            names.contains(&"jp") || names.contains(&"jnp"),
            "{condition:?} 必须修正 NaN"
        );
    }
    // 结果为 Flags 时把布尔物化进 r11b 并补 test。
    let flagged = checked(
        &Op::Compare {
            condition: Condition::Lt,
            signed: false,
        },
        &[Type::F64; 2],
        &[Type::Flags],
    );
    assert_eq!(mnemonics(&flagged).last(), Some(&"test"));
}

/// 进位链：进位出是两次进位的或，两段都用 `add`/`sub`（`adc`/`sbb` 会重复计入上一段 CF）。
#[test]
fn carry_chain_ors_both_carry_outs() {
    let sum = checked(
        &Op::Integer(IntOp::AddCarry),
        &[Type::I64; 3],
        &[Type::I64; 2],
    );
    let names = mnemonics(&sum);
    assert_eq!(
        names,
        ["mov", "add", "setb", "add", "setb", "or", "movzx"],
        "进位出必须由两次进位相或"
    );
    let borrow = checked(
        &Op::Integer(IntOp::SubBorrow),
        &[Type::I64; 3],
        &[Type::I64; 2],
    );
    assert_eq!(mnemonics(&borrow)[1], "sub");
    assert_eq!(mnemonics(&borrow)[3], "sub");
    let wide = checked(
        &Op::Integer(IntOp::MulWide),
        &[Type::I64; 2],
        &[Type::I64; 2],
    );
    assert_eq!(mnemonics(&wide), ["mov", "imul", "mov", "mov"]);
    assert!(wide.clobbers.contains_gpr(Gpr::Rdx), "宽乘法必须登记 rdx");
}

/// 解码序列的字段 offset 必须等于控制记录字段表，且计数指令落在正确的字段上。
#[test]
fn decode_sequence_reads_control_fields_by_offset() {
    let lowered = checked(&Op::DecodeCompressedRef, &[Type::I64], &[Type::Ptr]);
    let field = |name: &str| {
        let (_, offset, _) = CAGE_CONTROL_FIELDS
            .iter()
            .find(|(field, _, _)| *field == name)
            .expect("字段表必须有该字段");
        i32::try_from(*offset).expect("字段偏移适配 i32")
    };
    let referenced: Vec<(&str, i32)> = lowered
        .sequence
        .instructions
        .iter()
        .flat_map(|inst| inst.operands.iter())
        .filter_map(|operand| match operand {
            Operand::Rip(RelocTarget::CageControl, addend) => Some(*addend),
            _ => None,
        })
        .map(|addend| {
            let name = CAGE_CONTROL_FIELDS
                .iter()
                .find(|(_, offset, _)| i32::try_from(*offset).expect("偏移适配") == addend)
                .map(|(name, _, _)| *name)
                .expect("Rip 只允许指向字段表内的偏移");
            (name, addend)
        })
        .collect();
    let names: Vec<&str> = referenced.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            "cage_id",
            "generation",
            "len",
            "canonical_headroom",
            "base",
            "decodes",
            "rejections"
        ],
        "解码序列必须按字段表读取记录"
    );
    assert_eq!(referenced[0].1, field("cage_id"));
    // 计数指令：decodes 在成功路径、rejections 在拒绝路径，都是 lock inc。
    let counters: Vec<(&str, bool)> = lowered
        .sequence
        .instructions
        .iter()
        .filter(|inst| table::form(inst.form).mnemonic == "inc")
        .map(|inst| {
            let Operand::Rip(RelocTarget::CageControl, addend) = &inst.operands[0] else {
                panic!("计数必须落在控制记录字段上");
            };
            let name = CAGE_CONTROL_FIELDS
                .iter()
                .find(|(_, offset, _)| i32::try_from(*offset).expect("偏移适配") == *addend)
                .map(|(name, _, _)| *name)
                .expect("计数目标必须在字段表内");
            (name, inst.lock)
        })
        .collect();
    assert_eq!(counters, [("decodes", true), ("rejections", true)]);
    // 冷边只出现在拒绝路径。
    let cold = lowered
        .sequence
        .instructions
        .iter()
        .flat_map(|inst| inst.operands.iter())
        .filter(|operand| matches!(operand, Operand::Reloc(RelocTarget::Cold(_), _)))
        .count();
    assert_eq!(cold, 1, "拒绝路径恰好一条冷边");
    assert_eq!(lowered.sequence.labels.len(), 3, "null/reject/end 三个标签");
}

/// 向量：lane 覆盖与「没有基线序列」的拒绝面必须与 `supports_vector` 同源。
#[test]
fn vector_lanes_match_supports_vector() {
    let conditions = [
        Condition::Eq,
        Condition::Ne,
        Condition::Lt,
        Condition::Le,
        Condition::Gt,
        Condition::Ge,
    ];
    for lane in [
        Lane::I8,
        Lane::I16,
        Lane::I32,
        Lane::I64,
        Lane::F32,
        Lane::F64,
    ] {
        let vector = Type::V128(lane);
        let scalar = match lane {
            Lane::I8 => Type::I8,
            Lane::I16 => Type::I16,
            Lane::I32 => Type::I32,
            Lane::I64 => Type::I64,
            Lane::F32 => Type::F32,
            Lane::F64 => Type::F64,
        };
        let mut cases: Vec<(VectorOp, Vec<Type>, Vec<Type>)> = vec![
            (VectorOp::Splat, vec![scalar], vec![vector]),
            (VectorOp::Add, vec![vector; 2], vec![vector]),
            (VectorOp::Sub, vec![vector; 2], vec![vector]),
            (VectorOp::Mul, vec![vector; 2], vec![vector]),
            (VectorOp::And, vec![vector; 2], vec![vector]),
            (VectorOp::Or, vec![vector; 2], vec![vector]),
            (VectorOp::Xor, vec![vector; 2], vec![vector]),
        ];
        for condition in conditions {
            cases.push((VectorOp::Compare(condition), vec![vector; 2], vec![vector]));
        }
        cases.push((VectorOp::Extract(1), vec![vector], vec![scalar]));
        cases.push((VectorOp::Insert(1), vec![vector, scalar], vec![vector]));
        cases.push((VectorOp::ReduceAdd, vec![vector], vec![Type::I64]));
        for (vector_op, operand_types, result_types) in cases {
            let supported = lowering::supports_vector(vector_op, lane);
            let lowered = lowering::lower(
                &Op::Vector(vector_op),
                &operands(&operand_types),
                &results(&result_types),
                &source(),
            );
            match (supported, lowered) {
                (true, Ok(lowered)) => {
                    verify_sequence(&lowered.sequence, CpuBaseline::X86_64V1).unwrap_or_else(
                        |error| panic!("{vector_op:?}/{lane:?} 必须过 verifier：{error}"),
                    );
                    assemble(&lowered.sequence).unwrap_or_else(|error| {
                        panic!("{vector_op:?}/{lane:?} 必须能编码：{error}")
                    });
                }
                (false, Err(LoweringError::Unsupported { .. })) => {}
                (false, Err(LoweringError::InvalidOperands)) => {
                    panic!("{vector_op:?}/{lane:?} 的测试类型组合必须合法");
                }
                (true, Err(error)) => {
                    panic!("{vector_op:?}/{lane:?} 声明支持却 lower 失败：{error}");
                }
                (false, Ok(_)) => {
                    panic!("{vector_op:?}/{lane:?} 声明不支持却 lower 成功");
                }
            }
        }
    }
    // 已知缺口：I8/I64 lane 乘法、I64 排序比较、浮点归约、`V128` 的 Select。
    assert!(!lowering::supports_vector(VectorOp::Mul, Lane::I8));
    assert!(!lowering::supports_vector(VectorOp::Mul, Lane::I64));
    assert!(lowering::supports_vector(VectorOp::Mul, Lane::I16));
    assert!(!lowering::supports_vector(
        VectorOp::Compare(Condition::Lt),
        Lane::I64
    ));
    assert!(lowering::supports_vector(
        VectorOp::Compare(Condition::Eq),
        Lane::I64
    ));
    assert!(!lowering::supports_vector(VectorOp::ReduceAdd, Lane::F32));
}

/// Shuffle：32 位粒度置换合成 `pshufd`，混合源走双源模板，逐字节 mask 直接拒绝。
#[test]
fn shuffle_templates_are_deterministic() {
    // 单源 32 位置换：lane 顺序 3,2,1,0。
    let reversed = [12, 13, 14, 15, 8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3];
    let lowered = checked(
        &Op::Vector(VectorOp::Shuffle(reversed)),
        &[Type::V128(Lane::I32); 2],
        &[Type::V128(Lane::I32)],
    );
    assert_eq!(mnemonics(&lowered), ["pshufd"]);
    assert_eq!(
        lowered.sequence.instructions[0].operands[2],
        Operand::Imm(0x1B)
    );

    // 双源：低半来自 a、高半来自 b。
    let mixed = [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 28, 29, 30, 31];
    let lowered = checked(
        &Op::Vector(VectorOp::Shuffle(mixed)),
        &[Type::V128(Lane::I32); 2],
        &[Type::V128(Lane::I32)],
    );
    assert_eq!(mnemonics(&lowered), ["pshufd", "pshufd", "shufps"]);
    assert!(lowered.clobbers.contains_xmm(Xmm::Xmm15));

    // 64 位粒度置换用 `shufpd`。
    let wide = [8, 9, 10, 11, 12, 13, 14, 15, 0, 1, 2, 3, 4, 5, 6, 7];
    let lowered = checked(
        &Op::Vector(VectorOp::Shuffle(wide)),
        &[Type::V128(Lane::I64); 2],
        &[Type::V128(Lane::I64)],
    );
    assert_eq!(mnemonics(&lowered), ["shufpd"]);

    // 逐字节 mask 没有基线序列。
    let bytes = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 30];
    assert!(!lowering::supports_vector(
        VectorOp::Shuffle(bytes),
        Lane::I8
    ));
    let rejected = lowering::lower(
        &Op::Vector(VectorOp::Shuffle(bytes)),
        &operands(&[Type::V128(Lane::I8); 2]),
        &results(&[Type::V128(Lane::I8)]),
        &source(),
    );
    assert!(matches!(rejected, Err(LoweringError::Unsupported { .. })));
}

/// 原子 RMW：Load/Store/Exchange/Add/Sub 有基线序列，And/Or/Xor 没有。
#[test]
fn atomic_rmw_shape_and_missing_sequences() {
    let add = checked(
        &Op::Atomic {
            op: AtomicOp::Add,
            ordering: MemoryOrdering::SeqCst,
            failure: None,
            align: 8,
        },
        &[Type::Ptr, Type::I64],
        &[Type::I64],
    );
    assert_eq!(mnemonics(&add), ["mov", "xadd"]);
    assert!(add.sequence.instructions[1].lock, "xadd 必须带 lock");

    let exchange = checked(
        &Op::Atomic {
            op: AtomicOp::Exchange,
            ordering: MemoryOrdering::SeqCst,
            failure: None,
            align: 8,
        },
        &[Type::Ptr, Type::I64],
        &[Type::I64],
    );
    assert_eq!(mnemonics(&exchange), ["mov", "xchg"]);
    assert!(!exchange.sequence.instructions[1].lock, "xchg 隐式锁定");

    let compare_exchange = checked(
        &Op::Atomic {
            op: AtomicOp::CompareExchange,
            ordering: MemoryOrdering::SeqCst,
            failure: Some(MemoryOrdering::Relaxed),
            align: 8,
        },
        &[Type::Ptr, Type::I64, Type::I64],
        &[Type::I64, Type::I8],
    );
    assert_eq!(
        mnemonics(&compare_exchange),
        ["mov", "mov", "cmpxchg", "mov", "sete", "movzx"]
    );
    assert!(compare_exchange.clobbers.contains_gpr(Gpr::Rax));
    assert!(compare_exchange.clobbers.contains_gpr(Gpr::R11));

    let store = checked(
        &Op::Atomic {
            op: AtomicOp::Store,
            ordering: MemoryOrdering::SeqCst,
            failure: None,
            align: 8,
        },
        &[Type::Ptr, Type::I64],
        &[],
    );
    assert_eq!(
        mnemonics(&store),
        ["mov", "xchg"],
        "SeqCst 存储用 xchg 取得顺序"
    );
    assert!(
        store.sequence.instructions[1].lock == false,
        "xchg 隐式锁定"
    );
    assert!(
        store.clobbers.contains_gpr(Gpr::R11),
        "值先搬到 r11 再 xchg，r11 必须登记为 clobber"
    );

    for op in [AtomicOp::And, AtomicOp::Or, AtomicOp::Xor] {
        let atomic = Op::Atomic {
            op,
            ordering: MemoryOrdering::SeqCst,
            failure: None,
            align: 8,
        };
        let lowered = lowering::lower(
            &atomic,
            &operands(&[Type::Ptr, Type::I64]),
            &results(&[Type::I64]),
            &source(),
        );
        assert!(
            matches!(lowered, Err(LoweringError::Unsupported { .. })),
            "{op:?} 没有单指令 fetch 形式"
        );
        assert_eq!(lowering::poll_cost(&atomic), 1);
    }
}

/// 域内 op 的 poll 成本是机器 form 权重之和，且至少 1。
#[test]
fn poll_cost_is_positive_for_every_domain_op() {
    let ops = [
        Op::IConst(1),
        Op::FConst(1),
        Op::Integer(IntOp::Add),
        Op::Float(FloatOp::Add),
        Op::Compare {
            condition: Condition::Eq,
            signed: true,
        },
        Op::Convert(Conversion::ZeroExtend),
        Op::Select,
        Op::TrapIf,
        Op::DecodeCompressedRef,
        Op::Vector(VectorOp::Splat),
    ];
    for op in ops {
        assert!(lowering::domain(&op).is_some(), "{op:?} 属于 lowering 域");
        assert!(lowering::poll_cost(&op) >= 1, "{op:?} 的成本至少 1");
    }
    assert_eq!(
        lowering::domain(&Op::Load(crate::lir::body::Access {
            alias: crate::lir::body::AliasClass::Heap,
            align: 8,
            volatile: false,
        })),
        None,
        "本阶段不拥有访存 lowering"
    );
    // 最贵的探针组合：浮点比较要补 parity 分支。
    assert!(
        lowering::poll_cost(&Op::Compare {
            condition: Condition::Lt,
            signed: false,
        }) >= 4
    );
    // 解码序列是最长的一条。
    assert!(lowering::poll_cost(&Op::DecodeCompressedRef) >= 20);
}

/// 非法操作数组合与不支持的 op 必须报错而不是产出错误序列。
#[test]
fn invalid_operands_and_foreign_ops_are_rejected() {
    let lowered = lowering::lower(
        &Op::Integer(IntOp::Add),
        &operands(&[Type::F64]),
        &results(&[Type::F64]),
        &source(),
    );
    assert_eq!(lowered, Err(LoweringError::InvalidOperands));
    let foreign = lowering::lower(
        &Op::Load(crate::lir::body::Access {
            alias: crate::lir::body::AliasClass::Heap,
            align: 8,
            volatile: false,
        }),
        &operands(&[Type::Ptr]),
        &results(&[Type::I64]),
        &source(),
    );
    assert!(matches!(foreign, Err(LoweringError::Unsupported { .. })));
    // 结果数量不匹配。
    let arity = lowering::lower(
        &Op::Integer(IntOp::Add),
        &operands(&[Type::I64; 2]),
        &results(&[Type::I64; 2]),
        &source(),
    );
    assert_eq!(arity, Err(LoweringError::InvalidOperands));
    // 重定位种类：解码序列只用 CageControl 的 PcRel32 与冷边的 PcRel32。
    let decode = checked(&Op::DecodeCompressedRef, &[Type::I64], &[Type::Ptr]);
    let kinds: Vec<RelocKind> = decode
        .sequence
        .instructions
        .iter()
        .flat_map(|inst| inst.operands.iter())
        .filter_map(|operand| match operand {
            Operand::Rip(RelocTarget::CageControl, _) => Some(RelocKind::PcRel32),
            Operand::Reloc(RelocTarget::Cold(_), kind) => Some(*kind),
            _ => None,
        })
        .collect();
    assert!(
        kinds.iter().all(|kind| *kind == RelocKind::PcRel32),
        "解码序列只用 PcRel32 重定位"
    );
    assert_eq!(kinds.len(), 8, "七个字段 + 一条冷边");
}
