//! 内部 ABI、mangling、布局、并行拷贝与选指站点回归。

use super::abi::{self, AbiSlot};
use super::copies::{self, Copy, Temps};
use super::encode::assemble;
use super::inst::{Inst, LabelDefinition, LabelId, Operand, Sequence};
use super::layout;
use super::lower::{self, LowerCtx, LoweringError, SiteValue};
use super::mangle;
use super::reg::{Gpr, Reg, Xmm};
use super::table::{self, OperandKind};
use super::verify::verify_inst;
use crate::frontend::gir::body::{CallKind, MemoryOrdering};
use crate::frontend::gir::placement::PlacementKind;
use crate::frontend::late::universe::{MetadataShape, Shape, TypeRecord, TypeUniverse};
use crate::frontend::mono::keys::hash_domain;
use crate::lir::body::{AtomicOp, Call, CallTarget, Op, Provenance, Signature, Type, ValueType};
use crate::target::{CpuBaseline, TargetName};

/// 一个合法但只含指定布局的 universe，用于需要 descriptor 的 lowering。
fn universe_with_record(seed: u8, payload: u64, align: u64) -> ([u8; 32], TypeUniverse) {
    let canonical = vec![2_u8, seed];
    let key = hash_domain("gugu-mono-v1", &canonical);
    let record = TypeRecord {
        key,
        canonical,
        name: format!("T{seed}"),
        layout: Some((payload, align)),
        children: Vec::new(),
        shape: Shape::Other,
        metadata: MetadataShape::None,
        passing: 0,
    };
    let mut universe = TypeUniverse {
        records: vec![record],
        vtables: Vec::new(),
        fingerprint: [0; 32],
    };
    universe.fingerprint = universe.fingerprint();
    (key, universe)
}

fn signature(parameters: Vec<ValueType>, results: Vec<ValueType>) -> Signature {
    Signature {
        parameters,
        results,
        sret: None,
        by_value: Vec::new(),
    }
}

fn call(target: CallTarget, parameters: Vec<ValueType>, results: Vec<ValueType>) -> Call {
    Call {
        target,
        kind: CallKind::Managed,
        parameters,
        results,
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: Vec::new(),
        sret: None,
        poll_free_leaf: false,
    }
}

#[test]
fn integer_arg_uses_rax_and_f64_uses_xmm0() {
    let layout = abi::classify_signature(&signature(
        vec![ValueType::scalar(Type::I64), ValueType::scalar(Type::F64)],
        vec![ValueType::scalar(Type::I64)],
    ))
    .expect("内部 ABI 可分类");
    assert_eq!(layout.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
    assert_eq!(layout.arguments[1].slot, Some(AbiSlot::Float(Xmm::Xmm0)));
    assert_eq!(layout.results[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
    assert!(!layout.sret);
}

#[test]
fn sret_occupies_first_integer_slot() {
    // body 的 entry 参数顺序是 [sret 指针, 实参...]，因此第 2 个参数落到 rbx。
    let layout = abi::classify_signature(&Signature {
        parameters: vec![
            ValueType::pointer(Provenance::Foreign),
            ValueType::scalar(Type::I64),
        ],
        results: Vec::new(),
        sret: Some((24, 0, [1_u8; 32])),
        by_value: Vec::new(),
    })
    .expect("sret 可分类");
    assert!(layout.sret);
    // 隐藏返回指针复用参数 0 的位置，所以 caller 真的会把指针搬进 rax。
    assert_eq!(layout.arguments[0].index, 0);
    assert_eq!(layout.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
    assert!(layout.arguments[0].indirect);
    assert_eq!(layout.arguments[1].slot, Some(AbiSlot::Integer(Gpr::Rbx)));
}

#[test]
fn sret_contradicting_register_results_is_rejected() {
    // 地址返回与寄存器结果互斥：两者同时出现说明前端/后端契约被破坏。
    assert_eq!(
        abi::classify_signature(&Signature {
            parameters: vec![ValueType::scalar(Type::I64)],
            results: vec![ValueType::scalar(Type::I64)],
            sret: Some((24, 0, [1_u8; 32])),
            by_value: Vec::new(),
        })
        .expect_err("互斥必须拒绝"),
        LoweringError::InvalidOperands
    );
    assert!(
        abi::classify_signature(&Signature {
            parameters: vec![ValueType::scalar(Type::I64)],
            results: Vec::new(),
            sret: Some((0, 0, [1_u8; 32])),
            by_value: Vec::new(),
        })
        .is_err()
    );
}

#[test]
fn aggregate_over_16_bytes_is_indirect() {
    let key = [2_u8; 32];
    let layout = abi::classify_signature(&Signature {
        parameters: vec![ValueType::scalar(Type::Ptr)],
        results: vec![ValueType::scalar(Type::I64)],
        sret: None,
        by_value: vec![(0, key, 24)],
    })
    .expect("大聚合可分类");
    assert!(layout.arguments[0].indirect);
    assert_eq!(layout.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
}

#[test]
fn win64_positional_slots_share_integer_and_float_banks() {
    // Microsoft x64 按参数位置共用一组槽：`(i64, f64)` 是 rcx + xmm1，不是 rcx + xmm0。
    let mut windows = call(
        CallTarget::External {
            key: [0; 32],
            name: "c_fn".to_owned(),
        },
        vec![
            ValueType::scalar(Type::I64),
            ValueType::scalar(Type::F64),
            ValueType::scalar(Type::I64),
        ],
        Vec::new(),
    );
    windows.kind = CallKind::ForeignBridge;
    let layout = abi::classify_call(&windows, TargetName::X86_64Windows).expect("Win64 分类");
    assert_eq!(layout.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rcx)));
    assert_eq!(layout.arguments[1].slot, Some(AbiSlot::Float(Xmm::Xmm1)));
    assert_eq!(layout.arguments[2].slot, Some(AbiSlot::Integer(Gpr::R8)));

    // SysV 仍然是独立 bank：同一个签名给出 rdi + xmm0 + rsi。
    let linux = abi::classify_call(&windows, TargetName::X86_64Linux).expect("SysV 分类");
    assert_eq!(linux.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rdi)));
    assert_eq!(linux.arguments[1].slot, Some(AbiSlot::Float(Xmm::Xmm0)));
    assert_eq!(linux.arguments[2].slot, Some(AbiSlot::Integer(Gpr::Rsi)));
}

#[test]
fn stack_arguments_use_outgoing_offsets_and_win64_shadow_space() {
    // 内部 ABI 有 9 个整数槽：第 10 个参数落到 outgoing 区起点。
    let internal = abi::classify_signature(&signature(
        vec![ValueType::scalar(Type::I64); 10],
        Vec::new(),
    ))
    .expect("内部 ABI 可分类");
    assert_eq!(
        internal.arguments[9].slot,
        Some(AbiSlot::Stack { offset: 0 })
    );
    assert_eq!(internal.stack_slots, 1);

    // Win64 的栈参数从 32 字节 shadow space 之后开始。
    let mut windows = call(
        CallTarget::External {
            key: [0; 32],
            name: "c_fn".to_owned(),
        },
        vec![ValueType::scalar(Type::I64); 5],
        Vec::new(),
    );
    windows.kind = CallKind::ForeignBridge;
    let layout = abi::classify_call(&windows, TargetName::X86_64Windows).expect("Win64 分类");
    assert_eq!(
        layout.arguments[4].slot,
        Some(AbiSlot::Stack { offset: 32 })
    );
    assert_eq!(layout.stack_slots, 1);
}

#[test]
fn indirect_callee_is_excluded_from_argument_slots() {
    // 间接调用的目标参数（Provenance::Code）不是实参：它不占槽，后面的参数不下移。
    let indirect = call(
        CallTarget::Indirect,
        vec![
            ValueType::pointer(Provenance::Code),
            ValueType::scalar(Type::I64),
        ],
        Vec::new(),
    );
    let layout = abi::classify_call(&indirect, TargetName::X86_64Linux).expect("间接调用分类");
    assert_eq!(layout.callee, Some(0));
    assert_eq!(layout.arguments[0].slot, None);
    assert_eq!(layout.arguments[1].slot, Some(AbiSlot::Integer(Gpr::Rax)));
    assert_eq!(layout.integer_args, 1);
}

#[test]
fn vtable_lane_is_reported_and_kept_as_argument() {
    // 胖指针直接作为参数：vtable 就是 Metadata lane，且该 lane 仍作为实参传递。
    let dispatch = call(
        CallTarget::Vtable { slot: 3 },
        vec![
            ValueType::pointer(Provenance::GcHeap),
            ValueType::pointer(Provenance::Metadata),
            ValueType::scalar(Type::I64),
        ],
        Vec::new(),
    );
    let layout = abi::classify_call(&dispatch, TargetName::X86_64Linux).expect("动态派发分类");
    assert_eq!(layout.dispatch, Some(abi::Dispatch::Lane(1)));
    assert_eq!(layout.arguments[1].slot, Some(AbiSlot::Integer(Gpr::Rbx)));
    assert_eq!(layout.arguments[2].slot, Some(AbiSlot::Integer(Gpr::Rcx)));
}

#[test]
fn vtable_pairs_pointer_is_reported_when_no_metadata_lane() {
    // GIR 为 `&dyn` 物化出 (data, vtable) 胖对并只传指针：vtable 在该指针的 +8。
    let dispatch = call(
        CallTarget::Vtable { slot: 0 },
        vec![
            ValueType::pointer(Provenance::GcHeap),
            ValueType::scalar(Type::I64),
        ],
        Vec::new(),
    );
    let layout = abi::classify_call(&dispatch, TargetName::X86_64Linux).expect("胖对分类");
    assert_eq!(layout.dispatch, Some(abi::Dispatch::Pairs(0)));
    assert_eq!(layout.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
    assert_eq!(layout.arguments[1].slot, Some(AbiSlot::Integer(Gpr::Rbx)));
}

#[test]
fn missing_dispatch_or_callee_lane_is_rejected() {
    let no_receiver = call(
        CallTarget::Vtable { slot: 0 },
        vec![ValueType::scalar(Type::I64)],
        Vec::new(),
    );
    assert!(abi::classify_call(&no_receiver, TargetName::X86_64Linux).is_err());
    let no_code = call(
        CallTarget::Indirect,
        vec![ValueType::scalar(Type::I64)],
        Vec::new(),
    );
    assert!(abi::classify_call(&no_code, TargetName::X86_64Linux).is_err());
}

#[test]
fn allocation_bump_checks_carry_with_add() {
    // 溢出检测必须落在置标志位的 `add` 上：`lea` 不改 CF，后面的条件分支读不到真实进位。
    let (key, universe) = universe_with_record(7, 32, 8);
    let lowered = lower::lower_with(
        &Op::GcAlloc {
            descriptor: key,
            align: 8,
            placement: PlacementKind::LocalHeap,
            compressed: false,
        },
        &[],
        &[SiteValue {
            ty: ValueType::pointer(Provenance::GcHeap),
            reg: Reg::Virtual(0),
        }],
        &lower::probe_source(),
        LowerCtx {
            target: TargetName::X86_64Linux,
            body: None,
            universe: Some(&universe),
            raw: None,
            site: 0,
        },
    )
    .expect("快速路径可 lower");
    let mnemonics: Vec<&'static str> = lowered
        .sequence
        .instructions
        .iter()
        .map(|inst| table::form(inst.form).mnemonic)
        .collect();
    let first_carry = mnemonics
        .iter()
        .position(|name| *name == "jb")
        .expect("溢出检查存在");
    assert_eq!(
        mnemonics[first_carry - 1],
        "add",
        "进位必须由 add 产生：{mnemonics:?}"
    );
    assert!(
        mnemonics[..first_carry].iter().all(|name| *name != "lea"),
        "对齐/推进不能借 lea 伪装算术语义：{mnemonics:?}"
    );
    assert!(
        mnemonics.contains(&"and"),
        "payload 对齐掩码缺失：{mnemonics:?}"
    );
}

#[test]
fn ordinary_sequence_must_not_write_r14() {
    let form =
        table::form_id("mov", &[OperandKind::Rm64, OperandKind::R64]).expect("mov r/m64, r64");
    let bad = Inst {
        form,
        operands: vec![
            Operand::Reg(Reg::Gpr(Gpr::R14)),
            Operand::Reg(Reg::Gpr(Gpr::Rax)),
        ],
        lock: false,
    };
    assert!(verify_inst(&bad, CpuBaseline::X86_64V1).is_err());
}

#[test]
fn mangling_is_gugu_fn_plus_64_hex() {
    let name = mangle::mangle_runtime("gc_alloc_slow");
    assert!(name.starts_with("__gugu_runtime_"));
    assert_eq!(name.len(), "__gugu_runtime_".len() + 64);
    let glue = mangle::mangle_glue("memcpy");
    assert!(glue.starts_with("__gugu_glue_"));
    assert_eq!(glue.len(), "__gugu_glue_".len() + 64);
    // 符号 key 与 mangled 名同源：key 就是名字里那 64 hex 的输入，不再二次哈希。
    let symbol = mangle::runtime_symbol("gc_alloc_slow");
    assert_eq!(
        symbol,
        crate::lir::body::Symbol::External {
            key: hash_domain("gugu-runtime-symbol-v1", b"gc_alloc_slow"),
            name: name.clone(),
        }
    );
    let glue_symbol = mangle::glue_symbol("memcpy");
    assert_eq!(
        glue_symbol,
        crate::lir::body::Symbol::External {
            key: hash_domain("gugu-glue-symbol-v1", b"memcpy"),
            name: glue.clone(),
        }
    );
}

#[test]
fn nosafepoint_begin_encodes_zero_bytes() {
    let lowered =
        lower::lower(&Op::NoSafepointBegin(0), &[], &[], &lower::probe_source()).expect("空序列");
    assert!(lowered.sequence.instructions.is_empty());
    let assembled = assemble(&lowered.sequence).expect("空序列可编码");
    assert!(assembled.bytes.is_empty());
}

#[test]
fn atomic_relaxed_load_is_mov_without_mfence() {
    let lowered = lower::lower(
        &Op::Atomic {
            op: AtomicOp::Load,
            ordering: MemoryOrdering::Relaxed,
            failure: None,
            align: 8,
        },
        &[SiteValue {
            ty: ValueType::scalar(Type::Ptr),
            reg: Reg::Virtual(0),
        }],
        &[SiteValue {
            ty: ValueType::scalar(Type::I64),
            reg: Reg::Virtual(1),
        }],
        &lower::probe_source(),
    )
    .expect("Relaxed load");
    let mnemonics: Vec<_> = lowered
        .sequence
        .instructions
        .iter()
        .map(|inst| table::form(inst.form).mnemonic)
        .collect();
    assert!(mnemonics.iter().any(|name| *name == "mov"), "{mnemonics:?}");
    assert!(
        mnemonics.iter().all(|name| *name != "mfence"),
        "{mnemonics:?}"
    );
}

#[test]
fn acqrel_cas_has_lock_cmpxchg() {
    let cas = lower::lower(
        &Op::Atomic {
            op: AtomicOp::CompareExchange,
            ordering: MemoryOrdering::AcqRel,
            failure: Some(MemoryOrdering::Relaxed),
            align: 8,
        },
        &[
            SiteValue {
                ty: ValueType::scalar(Type::Ptr),
                reg: Reg::Virtual(0),
            },
            SiteValue {
                ty: ValueType::scalar(Type::I64),
                reg: Reg::Virtual(1),
            },
            SiteValue {
                ty: ValueType::scalar(Type::I64),
                reg: Reg::Virtual(2),
            },
        ],
        &[
            SiteValue {
                ty: ValueType::scalar(Type::I64),
                reg: Reg::Virtual(3),
            },
            SiteValue {
                ty: ValueType::scalar(Type::I8),
                reg: Reg::Virtual(4),
            },
        ],
        &lower::probe_source(),
    )
    .expect("CAS");
    assert!(cas.sequence.instructions.iter().any(|inst| inst.lock));
    let mnemonics: Vec<_> = cas
        .sequence
        .instructions
        .iter()
        .map(|inst| table::form(inst.form).mnemonic)
        .collect();
    assert!(
        mnemonics.iter().any(|name| *name == "cmpxchg"),
        "{mnemonics:?}"
    );
}

/// 组装「一条 rel32 跳转 + n 条 nop + 标签」的序列；标签在末尾。
fn forward_branch(nops: usize) -> Sequence {
    let mut instructions = vec![Inst {
        form: table::form_id("jmp", &[OperandKind::Rel32]).expect("jmp rel32"),
        operands: vec![Operand::Label(LabelId(0))],
        lock: false,
    }];
    let nop = table::form_id("nop", &[]).expect("nop");
    for _ in 0..nops {
        instructions.push(Inst {
            form: nop,
            operands: Vec::new(),
            lock: false,
        });
    }
    let at = u32::try_from(instructions.len()).expect("指令数适配 u32");
    Sequence {
        instructions,
        labels: vec![LabelDefinition {
            label: LabelId(0),
            at,
        }],
    }
}

#[test]
fn relax_uses_shrunk_endpoint_for_forward_branches() {
    // 收缩后位移 127 是 rel8 上界：必须收下。
    let mut fits = forward_branch(124);
    let count = layout::relax(&mut fits).expect("可收缩");
    assert_eq!(
        table::form(fits.instructions[0].form).operands,
        [OperandKind::Rel8]
    );
    assert_eq!(count, 1);

    // 收缩后位移 128 越界：rel32 下的 125 字节不能被误当成 rel8（旧实现按旧终点判定会收下）。
    let mut overflows = forward_branch(125);
    let count = layout::relax(&mut overflows).expect("保持长跳");
    assert_eq!(
        table::form(overflows.instructions[0].form).operands,
        [OperandKind::Rel32]
    );
    assert_eq!(count, 0);
    // 长跳仍可编码。
    assemble(&overflows).expect("rel32 可编码");
}

#[test]
fn relax_is_idempotent_and_counts_final_short_forms() {
    // 第二次松弛不能再计数，否则统计量会随迭代次数膨胀。
    let mut sequence = forward_branch(8);
    assert_eq!(layout::relax(&mut sequence).expect("可收缩"), 1);
    assert_eq!(layout::relax(&mut sequence).expect("已稳定"), 1);
    let mut jumps = Sequence {
        instructions: vec![
            Inst {
                form: table::form_id("jmp", &[OperandKind::Rel32]).expect("jmp rel32"),
                operands: vec![Operand::Label(LabelId(0))],
                lock: false,
            },
            Inst {
                form: table::form_id("jmp", &[OperandKind::Rel32]).expect("jmp rel32"),
                operands: vec![Operand::Label(LabelId(0))],
                lock: false,
            },
        ],
        labels: vec![LabelDefinition {
            label: LabelId(0),
            at: 2,
        }],
    };
    assert_eq!(layout::relax(&mut jumps).expect("两条短跳"), 2);
}

#[test]
fn backward_branch_accepts_exact_rel8_range() {
    // 反向跳的收缩只会缩短位移：-128 合法，-129 保持长跳。
    for (nops, expect_short) in [(126_usize, true), (127_usize, false)] {
        let mut instructions = Vec::new();
        let nop = table::form_id("nop", &[]).expect("nop");
        for _ in 0..nops {
            instructions.push(Inst {
                form: nop,
                operands: Vec::new(),
                lock: false,
            });
        }
        instructions.push(Inst {
            form: table::form_id("jmp", &[OperandKind::Rel32]).expect("jmp rel32"),
            operands: vec![Operand::Label(LabelId(0))],
            lock: false,
        });
        let mut sequence = Sequence {
            instructions,
            labels: vec![LabelDefinition {
                label: LabelId(0),
                at: 0,
            }],
        };
        let count = layout::relax(&mut sequence).expect("可松弛");
        let form = table::form(sequence.instructions[nops].form);
        assert_eq!(
            form.operands == [OperandKind::Rel8],
            expect_short,
            "nops={nops} 的收缩判定错误"
        );
        assert_eq!(count, u32::from(expect_short));
    }
}

/// 按发射顺序执行拷贝，返回虚拟寄存器 → 值的映射；源未定义即 panic。
fn apply(schedule: &[Copy], state: &mut std::collections::HashMap<u32, u32>) {
    for copy in schedule {
        let Reg::Virtual(source) = copy.src else {
            panic!("拷贝源必须是虚拟寄存器");
        };
        let value = *state.get(&source).expect("源在被读前已定义");
        let Reg::Virtual(dest) = copy.dest else {
            panic!("拷贝目标必须是虚拟寄存器");
        };
        state.insert(dest, value);
    }
}

#[test]
fn copies_break_cycles_without_losing_sources() {
    // 交换：1↔2。串行地按原顺序执行会丢掉一个源，调度器必须用临时寄存器打断环。
    let pairs = [
        Copy {
            src: Reg::Virtual(1),
            dest: Reg::Virtual(2),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Virtual(2),
            dest: Reg::Virtual(1),
            ty: Type::I64,
        },
    ];
    let mut state = std::collections::HashMap::from([(1_u32, 100_u32), (2, 200)]);
    let schedule = copies::schedule(&pairs, &mut Temps::new(10)).expect("可调度");
    apply(&schedule, &mut state);
    assert_eq!(state[&1], 200);
    assert_eq!(state[&2], 100);
    assert!(schedule.len() >= 3, "环必须引入临时寄存器：{schedule:?}");

    // 三元素环 1→2→3→1。
    let triple = [
        Copy {
            src: Reg::Virtual(1),
            dest: Reg::Virtual(2),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Virtual(2),
            dest: Reg::Virtual(3),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Virtual(3),
            dest: Reg::Virtual(1),
            ty: Type::I64,
        },
    ];
    let mut state = std::collections::HashMap::from([(1_u32, 10_u32), (2, 20), (3, 30)]);
    let schedule = copies::schedule(&triple, &mut Temps::new(10)).expect("可调度");
    apply(&schedule, &mut state);
    assert_eq!(state[&2], 10);
    assert_eq!(state[&3], 20);
    assert_eq!(state[&1], 30);

    // 无环链在目标不再被读之后直接发射，不需要临时寄存器。
    let chain = [
        Copy {
            src: Reg::Virtual(1),
            dest: Reg::Virtual(2),
            ty: Type::I64,
        },
        Copy {
            src: Reg::Virtual(3),
            dest: Reg::Virtual(4),
            ty: Type::I8,
        },
    ];
    let mut state = std::collections::HashMap::from([(1_u32, 7_u32), (3, 9)]);
    let schedule = copies::schedule(&chain, &mut Temps::new(10)).expect("可调度");
    assert_eq!(schedule.len(), 2);
    apply(&schedule, &mut state);
    assert_eq!(state[&2], 7);
    assert_eq!(state[&4], 9);
}

#[test]
fn vtable_dispatch_loads_vtable_from_fat_pair() {
    // 胖对指针形态：先取 `[receiver+8]` 的 vtable，再 `call [vtable + slot*8]`，接收者照常作为实参。
    const VTABLE_PAIR_OFFSET: i32 = 8;
    let dispatch = call(
        CallTarget::Vtable { slot: 2 },
        vec![ValueType::pointer(Provenance::GcHeap)],
        vec![ValueType::scalar(Type::I64)],
    );
    let lowered = lower::lower_with(
        &Op::Call(dispatch),
        &[SiteValue {
            ty: ValueType::pointer(Provenance::GcHeap),
            reg: Reg::Virtual(0),
        }],
        &[SiteValue {
            ty: ValueType::scalar(Type::I64),
            reg: Reg::Virtual(1),
        }],
        &lower::probe_source(),
        LowerCtx {
            target: TargetName::X86_64Linux,
            body: None,
            universe: None,
            raw: None,
            site: 0,
        },
    )
    .expect("动态派发可 lower");
    let mut loaded_vtable = false;
    let mut called_slot = false;
    for inst in &lowered.sequence.instructions {
        let mnemonic = table::form(inst.form).mnemonic;
        match inst.operands.as_slice() {
            // 该表用首操作数的 `Access` 区分 mov 方向：内存在前 + Read 即加载。
            [Operand::Mem(source), Operand::Reg(Reg::Gpr(Gpr::R11))] if mnemonic == "mov" => {
                loaded_vtable =
                    source.base == Some(Reg::Virtual(0)) && source.disp == VTABLE_PAIR_OFFSET;
            }
            [Operand::Mem(slot)] if mnemonic == "call" => {
                called_slot = slot.base == Some(Reg::Gpr(Gpr::R11)) && slot.disp == 16;
            }
            _ => {}
        }
    }
    assert!(
        loaded_vtable,
        "缺少从胖对读取 vtable 的 mov vtable 序列：{:?}",
        lowered.sequence
    );
    assert!(called_slot, "没有经 vtable 槽进入目标");
    assert!(
        lowered.clobbers.contains_gpr(Gpr::R11),
        "vtable 借用 r11 必须登记 clobber"
    );
    assert!(
        lowered
            .sequence
            .instructions
            .iter()
            .any(|inst| inst.operands
                == [
                    Operand::Reg(Reg::Gpr(Gpr::Rax)),
                    Operand::Reg(Reg::Virtual(0))
                ]),
        "接收者必须落进第一个整数参数槽"
    );
}

#[test]
fn classify_call_managed_uses_internal_integer_order() {
    let managed = call(
        CallTarget::Instance([0; 32]),
        vec![ValueType::scalar(Type::I64)],
        vec![ValueType::scalar(Type::I64)],
    );
    let linux = abi::classify_call(&managed, TargetName::X86_64Linux).expect("call");
    let windows = abi::classify_call(&managed, TargetName::X86_64Windows).expect("win");
    // 内部调用与目标无关，永远是内部 ABI 顺序。
    assert_eq!(linux.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
    assert_eq!(windows.arguments[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
}

#[test]
fn lower_error_display_for_invalid() {
    assert_eq!(
        LoweringError::InvalidOperands.to_string(),
        "lowering 的操作数不符合 op 语义"
    );
}
