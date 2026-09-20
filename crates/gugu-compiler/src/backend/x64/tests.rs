//! 编码字节 fixture 与 instruction verifier 负例回归。
//!
//! 字节对照 Intel SDM 手工验证；重定位断言字段宽度与 addend；verifier 负例覆盖
//! 超 baseline、lock/xchg 约束、保留寄存器与寄存器纪律。

use super::contract::EncoderContract;
use super::encode::{assemble, encoded_len};
use super::inst::{
    ConstraintKind, Inst, LabelDefinition, LabelId, Mem, Operand, RelocKind, RelocTarget,
    Relocation, Scale, Sequence,
};
use super::reg::{Gpr, Reg, Xmm};
use super::table::{self, FormId, OperandKind};
use super::verify::{verify_inst, verify_sequence};
use crate::lir::body::Symbol;
use crate::target::CpuBaseline;

const KINDS_R64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const KINDS_R64_RM64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];

fn form(mnemonic: &str, kinds: &[OperandKind]) -> FormId {
    table::form_id(mnemonic, kinds).expect("表内必须有该形式")
}

/// 按访问语义精确匹配同形不同方向的 form（如 `mov` 的 89/8B）。
fn form_matching(mnemonic: &str, kinds: &[OperandKind], access: &[super::table::Access]) -> FormId {
    table::FORMS
        .iter()
        .position(|form| {
            form.mnemonic == mnemonic && form.operands == kinds && form.access == access
        })
        .and_then(|index| u16::try_from(index).ok())
        .map(FormId)
        .expect("表内必须有该形式")
}

fn reg(gpr: Gpr) -> Operand {
    Operand::Reg(Reg::Gpr(gpr))
}

fn xmm(xmm: Xmm) -> Operand {
    Operand::Reg(Reg::Xmm(xmm))
}

fn inst(mnemonic: &str, kinds: &[OperandKind], operands: Vec<Operand>) -> Inst {
    Inst {
        form: form(mnemonic, kinds),
        operands,
        lock: false,
    }
}

fn locked(inst: Inst) -> Inst {
    Inst { lock: true, ..inst }
}

fn sequence(instructions: Vec<Inst>) -> Sequence {
    Sequence {
        instructions,
        labels: Vec::new(),
    }
}

fn bytes(inst: &Inst) -> Vec<u8> {
    let assembled = assemble(&sequence(vec![inst.clone()])).expect("序列可编码");
    assert_eq!(
        u32::try_from(assembled.bytes.len()).expect("长度适配"),
        encoded_len(inst).expect("长度可计算"),
        "长度计算必须与写入一致"
    );
    assembled.bytes
}

#[test]
fn mov_forms_encode_all_widths() {
    // mov r64, r64（8B/r 反向方向由 opcode 89 表达）→ 48 89 c8（rax = rcx）。
    assert_eq!(
        bytes(&inst(
            "mov",
            KINDS_R64_R64,
            vec![reg(Gpr::Rax), reg(Gpr::Rcx)]
        )),
        [0x48, 0x89, 0xC8]
    );
    // mov r32, r32 → 89 c8。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::Rm32, OperandKind::R32],
            vec![reg(Gpr::Rax), reg(Gpr::Rcx)]
        )),
        [0x89, 0xC8]
    );
    // mov r16, r16 → 66 89 c8。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::Rm16, OperandKind::R16],
            vec![reg(Gpr::Rax), reg(Gpr::Rcx)]
        )),
        [0x66, 0x89, 0xC8]
    );
    // mov r8, r8 → 88 c8（al = cl）。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::Rm8, OperandKind::R8],
            vec![reg(Gpr::Rax), reg(Gpr::Rcx)]
        )),
        [0x88, 0xC8]
    );
    // mov r32, imm32 → b8 + 值。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::R32, OperandKind::Imm32],
            vec![reg(Gpr::Rax), Operand::Imm(0x1122_3344)]
        )),
        [0xB8, 0x44, 0x33, 0x22, 0x11]
    );
    // mov r64, imm64 → 48 b8 + 8 字节。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::R64, OperandKind::Imm64],
            vec![reg(Gpr::Rax), Operand::Imm(0x1122_3344_5566_7788)]
        )),
        [0x48, 0xB8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
    );
    // mov r64, imm32 → 48 c7 c0 + 符号扩展 imm32。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::R64, OperandKind::Imm32],
            vec![reg(Gpr::Rax), Operand::Imm(1)]
        )),
        [0x48, 0xC7, 0xC0, 1, 0, 0, 0]
    );
    // mov r/m64, imm32（内存目标）→ 48 c7 40 08 + imm32。
    assert_eq!(
        bytes(&inst(
            "mov",
            &[OperandKind::Rm64, OperandKind::Imm32],
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rax)),
                    index: None,
                    scale: Scale::One,
                    disp: 8,
                }),
                Operand::Imm(1)
            ]
        )),
        [0x48, 0xC7, 0x40, 8, 1, 0, 0, 0]
    );
}

#[test]
fn extend_forms_encode_wide_and_byte_operands() {
    // movzx eax, cl → 0f b6 c1。
    assert_eq!(
        bytes(&inst(
            "movzx",
            &[OperandKind::Rm8, OperandKind::R32],
            vec![reg(Gpr::Rcx), reg(Gpr::Rax)]
        )),
        [0x0F, 0xB6, 0xC1]
    );
    // movzx r15, byte [r12] → 4d 0f b6 3c 24（SIB 无位移 + r12 基址）。
    assert_eq!(
        bytes(&inst(
            "movzx",
            &[OperandKind::Rm8, OperandKind::R64],
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::R12)),
                    index: None,
                    scale: Scale::One,
                    disp: 0,
                }),
                reg(Gpr::R15)
            ]
        )),
        [0x4D, 0x0F, 0xB6, 0x3C, 0x24]
    );
    // movsx rax, cx → 48 0f bf c1。
    assert_eq!(
        bytes(&inst(
            "movsx",
            &[OperandKind::Rm16, OperandKind::R64],
            vec![reg(Gpr::Rcx), reg(Gpr::Rax)]
        )),
        [0x48, 0x0F, 0xBF, 0xC1]
    );
    // movsxd rax, ecx → 48 63 c1。
    assert_eq!(
        bytes(&inst(
            "movsxd",
            &[OperandKind::Rm32, OperandKind::R64],
            vec![reg(Gpr::Rcx), reg(Gpr::Rax)]
        )),
        [0x48, 0x63, 0xC1]
    );
}

#[test]
fn lea_encodes_base_index_scale_and_disp() {
    // lea rax, [rbx + rcx*4 + 16] → 48 8d 44 8b 10。
    assert_eq!(
        bytes(&inst(
            "lea",
            &[OperandKind::Mem, OperandKind::R64],
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rbx)),
                    index: Some(Reg::Gpr(Gpr::Rcx)),
                    scale: Scale::Four,
                    disp: 16,
                }),
                reg(Gpr::Rax)
            ]
        )),
        [0x48, 0x8D, 0x44, 0x8B, 0x10]
    );
    // lea rax, [rbp] → 48 8d 45 00（rbp 零位移必须写 disp8）。
    assert_eq!(
        bytes(&inst(
            "lea",
            &[OperandKind::Mem, OperandKind::R64],
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rbp)),
                    index: None,
                    scale: Scale::One,
                    disp: 0,
                }),
                reg(Gpr::Rax)
            ]
        )),
        [0x48, 0x8D, 0x45, 0x00]
    );
    // lea rax, [r13+r12*8+0x400] → 4b 8d 84 e5 00 04 00 00（扩展基址/索引 + disp32）。
    assert_eq!(
        bytes(&inst(
            "lea",
            &[OperandKind::Mem, OperandKind::R64],
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::R13)),
                    index: Some(Reg::Gpr(Gpr::R12)),
                    scale: Scale::Eight,
                    disp: 0x400,
                }),
                reg(Gpr::Rax)
            ]
        )),
        [0x4B, 0x8D, 0x84, 0xE5, 0x00, 0x04, 0x00, 0x00]
    );
}

#[test]
fn rip_relative_operand_carries_pc_rel32_relocation() {
    let target = RelocTarget::Lir(Symbol::Data(7));
    let instruction = Inst {
        form: form_matching(
            "mov",
            &[OperandKind::Rm64, OperandKind::R64],
            &[super::table::Access::Read, super::table::Access::Write],
        ),
        operands: vec![Operand::Rip(target.clone(), 4), reg(Gpr::Rax)],
        lock: false,
    };
    let assembled = assemble(&sequence(vec![instruction])).expect("序列可编码");
    assert_eq!(assembled.bytes, [0x48, 0x8B, 0x05, 4, 0, 0, 0]);
    assert_eq!(
        assembled.relocations,
        [Relocation {
            offset: 3,
            kind: RelocKind::PcRel32,
            target,
            addend: 4,
        }]
    );
}

#[test]
fn symbol_relocations_use_declared_field_widths() {
    // mov rax, imm64 承载 8 字节绝对重定位。
    let absolute = RelocTarget::Lir(Symbol::Instance([7; 32]));
    let instruction = inst(
        "mov",
        &[OperandKind::R64, OperandKind::Imm64],
        vec![
            reg(Gpr::Rax),
            Operand::Reloc(absolute.clone(), RelocKind::Abs64),
        ],
    );
    let assembled = assemble(&sequence(vec![instruction])).expect("序列可编码");
    assert_eq!(assembled.bytes, [0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        assembled.relocations,
        [Relocation {
            offset: 2,
            kind: RelocKind::Abs64,
            target: absolute,
            addend: 0,
        }]
    );
    // mov r32, imm32 承载 4 字节 RVA 重定位。
    let rva = RelocTarget::Lir(Symbol::TypeId([3; 32]));
    let instruction = inst(
        "mov",
        &[OperandKind::R32, OperandKind::Imm32],
        vec![reg(Gpr::Rax), Operand::Reloc(rva.clone(), RelocKind::Rva32)],
    );
    let assembled = assemble(&sequence(vec![instruction])).expect("序列可编码");
    assert_eq!(assembled.bytes, [0xB8, 0, 0, 0, 0]);
    assert_eq!(
        assembled.relocations,
        [Relocation {
            offset: 1,
            kind: RelocKind::Rva32,
            target: rva.clone(),
            addend: 0,
        }]
    );
    // 宽度不匹配必须拒绝：imm32 字段承载 Abs64。
    let mismatch = inst(
        "mov",
        &[OperandKind::R32, OperandKind::Imm32],
        vec![reg(Gpr::Rax), Operand::Reloc(rva, RelocKind::Abs64)],
    );
    assert!(assemble(&sequence(vec![mismatch])).is_err());
}

#[test]
fn lock_prefix_is_written_before_rex_and_requires_memory() {
    // lock add qword [rax], rcx → f0 48 01 08。
    assert_eq!(
        bytes(&locked(inst(
            "add",
            KINDS_R64_RM64,
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rax)),
                    index: None,
                    scale: Scale::One,
                    disp: 0,
                }),
                reg(Gpr::Rcx)
            ]
        ))),
        [0xF0, 0x48, 0x01, 0x08]
    );
    // lock 不允许寄存器目标。
    let register_target = inst("add", KINDS_R64_RM64, vec![reg(Gpr::Rax), reg(Gpr::Rcx)]);
    assert!(verify_inst(&locked(register_target.clone()), CpuBaseline::X86_64V1).is_err());
    assert!(assemble(&sequence(vec![locked(register_target)])).is_err());
    // xchg 带内存已隐式锁定，显式 lock 只保留一种写法。
    let exchange = inst(
        "xchg",
        KINDS_R64_RM64,
        vec![
            Operand::Mem(Mem {
                base: Some(Reg::Gpr(Gpr::Rax)),
                index: None,
                scale: Scale::One,
                disp: 0,
            }),
            reg(Gpr::Rcx),
        ],
    );
    assert_eq!(bytes(&exchange), [0x48, 0x87, 0x08]);
    assert!(verify_inst(&locked(exchange), CpuBaseline::X86_64V1).is_err());
}

#[test]
fn atomic_forms_encode_and_require_expected_rax() {
    // lock cmpxchg [rax], rcx → f0 48 0f b1 08。
    assert_eq!(
        bytes(&locked(inst(
            "cmpxchg",
            KINDS_R64_RM64,
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rax)),
                    index: None,
                    scale: Scale::One,
                    disp: 0,
                }),
                reg(Gpr::Rcx)
            ]
        ))),
        [0xF0, 0x48, 0x0F, 0xB1, 0x08]
    );
    // lock xadd [rax], rcx → f0 48 0f c1 08。
    assert_eq!(
        bytes(&locked(inst(
            "xadd",
            KINDS_R64_RM64,
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rax)),
                    index: None,
                    scale: Scale::One,
                    disp: 0,
                }),
                reg(Gpr::Rcx)
            ]
        ))),
        [0xF0, 0x48, 0x0F, 0xC1, 0x08]
    );
    // cmpxchg 之前必须先写 rax。
    let compare_exchange = locked(inst(
        "cmpxchg",
        KINDS_R64_RM64,
        vec![
            Operand::Mem(Mem {
                base: Some(Reg::Gpr(Gpr::Rax)),
                index: None,
                scale: Scale::One,
                disp: 0,
            }),
            reg(Gpr::Rcx),
        ],
    ));
    assert!(
        verify_sequence(
            &sequence(vec![compare_exchange.clone()]),
            CpuBaseline::X86_64V1
        )
        .is_err()
    );
    let expected = inst(
        "mov",
        &[OperandKind::Rm64, OperandKind::R64],
        vec![reg(Gpr::Rax), reg(Gpr::Rax)],
    );
    assert!(
        verify_sequence(
            &sequence(vec![expected, compare_exchange]),
            CpuBaseline::X86_64V1
        )
        .is_ok()
    );
}

#[test]
fn setcc_and_cmov_use_reg_field_encoding() {
    // sete al → 0f 94 c0。
    assert_eq!(
        bytes(&inst("sete", &[OperandKind::Rm8], vec![reg(Gpr::Rax)])),
        [0x0F, 0x94, 0xC0]
    );
    // setg r11b → 0f 9f c3。
    assert_eq!(
        bytes(&inst("setg", &[OperandKind::Rm8], vec![reg(Gpr::Rbx)])),
        [0x0F, 0x9F, 0xC3]
    );
    // cmove rax, rcx → 48 0f 44 c1。
    assert_eq!(
        bytes(&inst(
            "cmove",
            KINDS_R64_RM64,
            vec![reg(Gpr::Rcx), reg(Gpr::Rax)]
        )),
        [0x48, 0x0F, 0x44, 0xC1]
    );
}

#[test]
fn branches_resolve_labels_and_cold_edges() {
    // jmp 前向标签：e9 + rel32（目标在 mov 之后，距离 5）。
    let jump = inst(
        "jmp",
        &[OperandKind::Rel32],
        vec![Operand::Label(LabelId(0))],
    );
    let mov = inst(
        "mov",
        &[OperandKind::R32, OperandKind::Imm32],
        vec![reg(Gpr::Rax), Operand::Imm(1)],
    );
    let program = Sequence {
        instructions: vec![jump, mov],
        labels: vec![LabelDefinition {
            label: LabelId(0),
            at: 2,
        }],
    };
    let assembled = assemble(&program).expect("分支可解析");
    // e9 05 00 00 00：下一条指令起点 + 5 落在序列末尾。
    assert_eq!(assembled.bytes, [0xE9, 5, 0, 0, 0, 0xB8, 1, 0, 0, 0]);
    assert!(assembled.relocations.is_empty());

    // 冷边分支产生 PcRel32 重定位且只出现一次。
    let edge = super::inst::ColdEdge {
        kind: super::inst::ColdEdgeKind::Trap,
        source: crate::frontend::gir::body::SourceInfo {
            location: crate::frontend::hir::Location {
                source: 0,
                start: 0,
                end: 0,
                expansion: 0,
            },
            scope: crate::frontend::gir::body::ScopeId(0),
        },
    };
    let cold = inst(
        "jne",
        &[OperandKind::Rel32],
        vec![Operand::Reloc(
            RelocTarget::Cold(edge.clone()),
            RelocKind::PcRel32,
        )],
    );
    let assembled = assemble(&sequence(vec![cold.clone()])).expect("冷边可编码");
    assert_eq!(assembled.bytes, [0x0F, 0x85, 0, 0, 0, 0]);
    assert_eq!(assembled.relocations.len(), 1);
    assert_eq!(assembled.relocations[0].kind, RelocKind::PcRel32);
    assert!(matches!(
        assembled.relocations[0].target,
        RelocTarget::Cold(_)
    ));
    assert!(
        verify_sequence(&sequence(vec![cold.clone(), cold]), CpuBaseline::X86_64V1).is_err(),
        "同一条冷边最多被引用一次"
    );
}

#[test]
fn verifier_rejects_forms_beyond_baseline() {
    let cases = [
        (
            "pshufb",
            &[OperandKind::XmmRm, OperandKind::Xmm][..],
            vec![xmm(Xmm::Xmm1), xmm(Xmm::Xmm0)],
        ),
        (
            "pmulld",
            &[OperandKind::XmmRm, OperandKind::Xmm][..],
            vec![xmm(Xmm::Xmm1), xmm(Xmm::Xmm0)],
        ),
        (
            "vaddps",
            &[OperandKind::Xmm, OperandKind::Xmm, OperandKind::XmmRm][..],
            vec![xmm(Xmm::Xmm0), xmm(Xmm::Xmm1), xmm(Xmm::Xmm2)],
        ),
    ];
    for (mnemonic, kinds, operands) in cases {
        let instruction = inst(mnemonic, kinds, operands);
        let error = verify_inst(&instruction, CpuBaseline::X86_64V1)
            .expect_err(&format!("{mnemonic} 超出 x86-64-v1"));
        assert!(error.message().contains("x86_64-v1"), "{}", error.message());
    }
}

#[test]
fn verifier_rejects_invalid_memory_and_reserved_registers() {
    let with_memory = |base: Option<Reg>, index: Option<Reg>, scale: Scale| {
        inst(
            "mov",
            KINDS_R64_RM64,
            vec![
                Operand::Mem(Mem {
                    base,
                    index,
                    scale,
                    disp: 0,
                }),
                reg(Gpr::Rax),
            ],
        )
    };
    let gpr = Reg::Gpr;
    // index == rsp 非法。
    assert!(
        verify_inst(
            &with_memory(Some(gpr(Gpr::Rbx)), Some(gpr(Gpr::Rsp)), Scale::One),
            CpuBaseline::X86_64V1
        )
        .is_err()
    );
    // 无 index 时 scale 必须是 1。
    assert!(
        verify_inst(
            &with_memory(Some(gpr(Gpr::Rbx)), None, Scale::Two),
            CpuBaseline::X86_64V1
        )
        .is_err()
    );
    // 写保留寄存器非法；只读合法。
    let write_reserved = Inst {
        form: form_matching(
            "mov",
            &[OperandKind::Rm64, OperandKind::R64],
            &[super::table::Access::Read, super::table::Access::Write],
        ),
        operands: vec![reg(Gpr::Rax), reg(Gpr::R14)],
        lock: false,
    };
    assert!(verify_inst(&write_reserved, CpuBaseline::X86_64V1).is_err());
    let read_reserved = Inst {
        form: form_matching(
            "mov",
            &[OperandKind::Rm64, OperandKind::R64],
            &[super::table::Access::Read, super::table::Access::Write],
        ),
        operands: vec![reg(Gpr::R14), reg(Gpr::Rax)],
        lock: false,
    };
    assert!(verify_inst(&read_reserved, CpuBaseline::X86_64V1).is_ok());
    // 字节寄存器不可编码的物理寄存器。
    let byte_reserved = inst(
        "mov",
        &[OperandKind::Rm8, OperandKind::R8],
        vec![reg(Gpr::Rsp), reg(Gpr::Rax)],
    );
    assert!(verify_inst(&byte_reserved, CpuBaseline::X86_64V1).is_err());
}

#[test]
fn verifier_enforces_virtual_register_discipline() {
    // 序列写过的寄存器必须「先写后读」：mov v1, v0 之后 mov v0, rax 不能倒过来读 v0。
    let write_first = inst(
        "mov",
        KINDS_R64_RM64,
        vec![Operand::Reg(Reg::Virtual(0)), reg(Gpr::Rax)],
    );
    let read_after = inst(
        "mov",
        KINDS_R64_RM64,
        vec![Operand::Reg(Reg::Virtual(1)), Operand::Reg(Reg::Virtual(0))],
    );
    let write_before_read = inst(
        "mov",
        KINDS_R64_RM64,
        vec![Operand::Reg(Reg::Virtual(0)), reg(Gpr::Rcx)],
    );
    assert!(
        verify_sequence(
            &sequence(vec![write_first.clone(), read_after.clone()]),
            CpuBaseline::X86_64V1
        )
        .is_ok(),
        "先写后读合法"
    );
    assert!(
        verify_sequence(
            &sequence(vec![read_after, write_first.clone()]),
            CpuBaseline::X86_64V1
        )
        .is_err(),
        "同一寄存器读在写之前必须拒绝"
    );
    // 序列从未写过的虚拟寄存器是活入操作数，读它不构成违例。
    let live_in = inst(
        "mov",
        KINDS_R64_RM64,
        vec![Operand::Reg(Reg::Virtual(0)), Operand::Reg(Reg::Virtual(7))],
    );
    assert!(
        verify_sequence(
            &sequence(vec![live_in, write_before_read]),
            CpuBaseline::X86_64V1
        )
        .is_ok(),
        "活入操作数由上游站点定义"
    );
}

#[test]
fn verifier_rejects_label_structures() {
    let jump = inst(
        "jmp",
        &[OperandKind::Rel32],
        vec![Operand::Label(LabelId(0))],
    );
    // 引用未定义标签。
    assert!(verify_sequence(&sequence(vec![jump.clone()]), CpuBaseline::X86_64V1).is_err());
    // 同一标签被多条分支引用合法（合并点），且两条分支都要回填到同一目标。
    let merged = Sequence {
        instructions: vec![jump.clone(), jump],
        labels: vec![LabelDefinition {
            label: LabelId(0),
            at: 2,
        }],
    };
    assert!(verify_sequence(&merged, CpuBaseline::X86_64V1).is_ok());
    let assembled = assemble(&merged).expect("合并标签可编码");
    // 两条分支都必须回填到标签位置：第一条跨过第二条，第二条落在末尾。
    for (index, offset) in &assembled.instruction_offsets {
        let start = *offset as usize;
        let displacement = i32::from_le_bytes(
            assembled.bytes[start + 1..start + 5]
                .try_into()
                .expect("rel32 字段"),
        );
        let target = offset + 5 + u32::try_from(displacement).expect("位移非负");
        assert_eq!(
            target, assembled.labels[0],
            "第 {index} 条分支必须指向同一标签"
        );
    }
    // 定义不连续（缺 0）同样拒绝。
    let hole = Sequence {
        instructions: Vec::new(),
        labels: vec![LabelDefinition {
            label: LabelId(1),
            at: 0,
        }],
    };
    assert!(verify_sequence(&hole, CpuBaseline::X86_64V1).is_err());
    // 重复定义拒绝。
    let duplicate = Sequence {
        instructions: Vec::new(),
        labels: vec![
            LabelDefinition {
                label: LabelId(0),
                at: 0,
            },
            LabelDefinition {
                label: LabelId(0),
                at: 0,
            },
        ],
    };
    assert!(verify_sequence(&duplicate, CpuBaseline::X86_64V1).is_err());
}

#[test]
fn encoder_rejects_vex_and_unknown_shapes() {
    let vex = inst(
        "vaddps",
        &[OperandKind::Xmm, OperandKind::Xmm, OperandKind::XmmRm],
        vec![xmm(Xmm::Xmm0), xmm(Xmm::Xmm1), xmm(Xmm::Xmm2)],
    );
    assert!(assemble(&sequence(vec![vex])).is_err());
    // 操作数数量不匹配。
    let malformed = inst("ret", &[], vec![reg(Gpr::Rax)]);
    assert!(assemble(&sequence(vec![malformed])).is_err());
    // 虚拟寄存器用低 3 位占位编码，物理分配交给分配阶段的约束。
    let virtual_operand = inst(
        "mov",
        KINDS_R64_RM64,
        vec![Operand::Reg(Reg::Virtual(1)), reg(Gpr::Rax)],
    );
    let assembled = assemble(&sequence(vec![virtual_operand])).expect("虚拟寄存器可占位编码");
    assert_eq!(assembled.bytes, [0x48, 0x89, 0xC1]);
    assert!(assembled.constraints.is_empty(), "占位编码不登记约束");
    // 字节位置的虚拟寄存器登记 `ByteEncodable` 约束。
    let virtual_byte = inst(
        "mov",
        &[OperandKind::Rm8, OperandKind::R8],
        vec![Operand::Reg(Reg::Virtual(2)), reg(Gpr::Rax)],
    );
    let assembled = assemble(&sequence(vec![virtual_byte])).expect("虚拟字节寄存器可占位编码");
    assert_eq!(assembled.bytes, [0x88, 0xC2]);
    let [constraint] = assembled.constraints[..] else {
        panic!("字节位置必须登记一条约束");
    };
    assert_eq!(constraint.register, Reg::Virtual(2));
    assert_eq!(constraint.kind, ConstraintKind::ByteEncodable);
}

#[test]
fn encoder_contract_matches_descriptor_table() {
    let contract = EncoderContract::build(CpuBaseline::X86_64V1);
    contract.verify().expect("构建出的契约必须自洽");
    assert_eq!(contract.forms.len(), table::FORMS.len());
    assert_eq!(
        contract.fingerprint(),
        contract.clone().compute_fingerprint(),
        "指纹必须可由内容重算"
    );
    // 超基线登记项只用于 verifier 负例，不参与 lowering。
    assert_eq!(contract.beyond_baseline_count(), 5);
    let dump = contract.dump();
    assert!(dump.contains("baseline=x86_64-v1"), "{dump}");
    assert!(dump.contains("encoder-form 0 mov "), "{dump}");
}

#[test]
fn encoder_contract_rejects_tampered_catalog() {
    let contract = EncoderContract::build(CpuBaseline::X86_64V1);
    let mut tampered = contract.clone();
    tampered.forms[0].mnemonic.push_str("x");
    assert!(tampered.verify().is_err(), "助记符篡改必须被发现");
    let mut reordered = contract.clone();
    reordered.forms.swap(0, 1);
    assert!(reordered.verify().is_err(), "目录顺序篡改必须被发现");
    let mut unknown_feature = contract;
    unknown_feature.forms[0].feature = "avx512".to_owned();
    assert!(
        unknown_feature.verify().is_err(),
        "未登记的特性名必须被发现"
    );
}

/// SSE2 整型向量形式必须带 66 前缀：缺前缀会编码成 MMX 形式，只在执行期暴露。
#[test]
fn sse2_integer_forms_carry_operand_size_prefix() {
    /// 非 SSE2 整型的 `p` 助记符：`pause` 用 F3，字/半字洗牌各用 F2/F3。
    const EXCEPTIONS: &[&str] = &["pause", "pshuflw", "pshufhw"];
    for form in table::FORMS {
        if !form.mnemonic.starts_with('p') || EXCEPTIONS.contains(&form.mnemonic) {
            continue;
        }
        assert_eq!(
            form.prefix,
            table::Prefix::OperandSize66,
            "{} 缺少 66 前缀",
            form.mnemonic
        );
    }
}

/// 无操作数形式也要发射声明的固定 ModRM：`0F AE /6`、`/5`、`/7`。
#[test]
fn fence_forms_emit_fixed_modrm() {
    for (mnemonic, modrm) in [("mfence", 0xF0_u8), ("lfence", 0xE8), ("sfence", 0xF8)] {
        assert_eq!(
            bytes(&inst(mnemonic, &[], Vec::new())),
            vec![0x0F, 0xAE, modrm],
            "{mnemonic} 的字节"
        );
    }
}

/// harness 覆盖全部 lowering 规则，且编码器生成的上下文切换片段与运行时固定片段一致。
#[test]
fn harness_cases_and_switch_fragment_are_consistent() {
    let harness = super::harness::X64Harness::new(crate::target::TargetName::X86_64Linux)
        .expect("harness 构建");
    let fixed = crate::runtime::ContextSwitchCode::fixed();
    assert_eq!(
        harness.switch_fragment().bytes,
        fixed.bytes,
        "编码器片段必须与运行时固定片段逐字节相同"
    );
    assert_eq!(harness.switch_restore_offset(), fixed.restore_offset);
    // 用例名唯一，便于失败定位。
    let mut names: Vec<&str> = harness
        .cases()
        .iter()
        .map(|case| case.name.as_str())
        .collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "用例名必须唯一");
    assert!(count >= 150, "用例表必须覆盖全部 lowering 规则：{count}");
    // 解码夹具：合法、空、错 cage id、陈旧 generation、越界 offset、非 canonical 各一。
    assert_eq!(harness.decode().words.len(), 6);
    assert_eq!(harness.decode().decodes, 1);
    assert_eq!(harness.decode().rejections, 4);
    assert_eq!(
        harness.decode().control_fields.len(),
        crate::runtime::cage_control::CAGE_CONTROL_FIELDS.len()
    );
}
