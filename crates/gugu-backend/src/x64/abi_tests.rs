//! 双目标 C ABI 分类一致性矩阵：SysV 与 Microsoft x64 的黄金槽位表。
//! 表中的期望按 C ABI 规则独立推导（等同于 C 对照程序的调用布局），
//! 覆盖 0/1/2/16/17-byte aggregate 边界、shadow space、栈对齐、sret、
//! 寄存器耗尽回退与 Win64 按位置共享槽。

use super::abi::{self, AbiSlot};
use super::reg::{Gpr, Xmm};
use crate::gir::body::CallKind;
use crate::lir::body::{Call, CallTarget, ForeignArgument, Provenance, Type, ValueType};
use crate::target::TargetName;

fn foreign_call(parameters: Vec<ValueType>, results: Vec<ValueType>) -> Call {
    Call {
        target: CallTarget::External {
            key: [0; 32],
            name: "probe".into(),
        },
        kind: CallKind::ForeignBridge,
        parameters,
        results,
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: Vec::new(),
        sret: None,
        foreign_signature: None,
        foreign_arguments: Vec::new(),
        poll_free_leaf: false,
        scoped_arguments: Vec::new(),
    }
}

fn i64() -> ValueType {
    ValueType::scalar(Type::I64)
}
fn f64() -> ValueType {
    ValueType::scalar(Type::F64)
}
fn ptr() -> ValueType {
    ValueType::pointer(Provenance::Code)
}
fn group(first: u32, lanes: u32, bytes: u32, align: u32) -> ForeignArgument {
    ForeignArgument {
        first,
        lanes,
        bytes,
        align,
        memory: false,
    }
}

fn memory_group(first: u32, lanes: u32, bytes: u32, align: u32) -> ForeignArgument {
    ForeignArgument {
        memory: true,
        ..group(first, lanes, bytes, align)
    }
}

fn slot_of(layout: &abi::AbiLayout, index: u32) -> Option<AbiSlot> {
    layout
        .arguments
        .iter()
        .find(|argument| argument.index == index)
        .and_then(|argument| argument.slot)
}

/// 标量参数分配：SysV 整数/浮点分别计数；Win64 按位置共享槽。
#[test]
fn c_scalar_arguments_match_convention_registers() {
    let call = foreign_call(
        vec![
            i64(),
            f64(),
            i64(),
            f64(),
            i64(),
            f64(),
            i64(),
            f64(),
            i64(),
            f64(),
        ],
        vec![],
    );
    let sysv = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    let expected = [
        AbiSlot::Integer(Gpr::Rdi),
        AbiSlot::Float(Xmm::Xmm0),
        AbiSlot::Integer(Gpr::Rsi),
        AbiSlot::Float(Xmm::Xmm1),
        AbiSlot::Integer(Gpr::Rdx),
        AbiSlot::Float(Xmm::Xmm2),
        AbiSlot::Integer(Gpr::Rcx),
        AbiSlot::Float(Xmm::Xmm3),
        AbiSlot::Integer(Gpr::R8),
        AbiSlot::Float(Xmm::Xmm4),
    ];
    for (index, slot) in expected.iter().enumerate() {
        assert_eq!(slot_of(&sysv, index as u32), Some(*slot), "sysv #{index}");
    }
    assert_eq!(sysv.stack_slots, 0);

    let win = abi::classify_call(&call, TargetName::X86_64Windows).unwrap();
    let expected_win = [
        AbiSlot::Integer(Gpr::Rcx),
        AbiSlot::Float(Xmm::Xmm1),
        AbiSlot::Integer(Gpr::R8),
        AbiSlot::Float(Xmm::Xmm3),
        AbiSlot::Stack { offset: 32 },
        AbiSlot::Stack { offset: 40 },
        AbiSlot::Stack { offset: 48 },
        AbiSlot::Stack { offset: 56 },
        AbiSlot::Stack { offset: 64 },
        AbiSlot::Stack { offset: 72 },
    ];
    for (index, slot) in expected_win.iter().enumerate() {
        assert_eq!(slot_of(&win, index as u32), Some(*slot), "win #{index}");
    }
    assert_eq!(win.stack_slots, 6);
}

/// Win64：前四个位置整数/浮点交错也不互相挤占；第 5 个位置起全部进栈，
/// 且第一个栈 piece 在 32 字节 shadow space 之后。
#[test]
fn win64_positional_slots_and_shadow_space() {
    let mut call = foreign_call(vec![i64(), f64(), f64(), i64(), i64()], vec![]);
    let win = abi::classify_call(&call, TargetName::X86_64Windows).unwrap();
    assert_eq!(slot_of(&win, 0), Some(AbiSlot::Integer(Gpr::Rcx)));
    assert_eq!(slot_of(&win, 1), Some(AbiSlot::Float(Xmm::Xmm1)));
    assert_eq!(slot_of(&win, 2), Some(AbiSlot::Float(Xmm::Xmm2)));
    assert_eq!(slot_of(&win, 3), Some(AbiSlot::Integer(Gpr::R9)));
    assert_eq!(slot_of(&win, 4), Some(AbiSlot::Stack { offset: 32 }));
    call.parameters = vec![f64(), i64(), i64(), f64(), f64()];
    let win = abi::classify_call(&call, TargetName::X86_64Windows).unwrap();
    assert_eq!(slot_of(&win, 0), Some(AbiSlot::Float(Xmm::Xmm0)));
    assert_eq!(slot_of(&win, 1), Some(AbiSlot::Integer(Gpr::Rdx)));
    assert_eq!(slot_of(&win, 2), Some(AbiSlot::Integer(Gpr::R8)));
    assert_eq!(slot_of(&win, 3), Some(AbiSlot::Float(Xmm::Xmm3)));
    assert_eq!(slot_of(&win, 4), Some(AbiSlot::Stack { offset: 32 }));
}

/// aggregate 边界：1、2、16 字节寄存器组；17 字节与 memory 组走栈并保留对齐填充。
#[test]
fn sysv_aggregate_boundaries_classify_registers_vs_stack() {
    // {u8} 1-byte aggregate 仍是寄存器 piece。
    let mut call = foreign_call(vec![i64()], vec![]);
    call.foreign_arguments = vec![group(0, 1, 1, 1)];
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 0), Some(AbiSlot::Integer(Gpr::Rdi)));

    // {u16} 2-byte aggregate 单 lane 寄存器。
    call.foreign_arguments = vec![group(0, 1, 2, 2)];
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 0), Some(AbiSlot::Integer(Gpr::Rdi)));

    // {u64,u64} 16-byte 两 lane 占相邻整数寄存器。
    let mut call = foreign_call(vec![i64(), i64(), i64()], vec![]);
    call.foreign_arguments = vec![group(0, 2, 16, 8)];
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 0), Some(AbiSlot::Integer(Gpr::Rdi)));
    assert_eq!(slot_of(&layout, 1), Some(AbiSlot::Integer(Gpr::Rsi)));
    assert_eq!(slot_of(&layout, 2), Some(AbiSlot::Integer(Gpr::Rdx)));

    // 17-byte 三 lane（memory=true，SysV 大聚合内存传参）：全组进栈。
    let mut call = foreign_call(vec![i64(), i64(), i64()], vec![]);
    call.foreign_arguments = vec![memory_group(0, 3, 17, 8)];
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 0), Some(AbiSlot::Stack { offset: 0 }));
    assert_eq!(slot_of(&layout, 1), Some(AbiSlot::Stack { offset: 8 }));
    assert_eq!(slot_of(&layout, 2), Some(AbiSlot::Stack { offset: 16 }));
    assert_eq!(layout.stack_slots, 3);

    // 寄存器装不下的 17-byte 组同样整体回退到栈（跨 bank 不足时回滚）。
    let mut call = foreign_call(vec![i64(); 5], vec![]);
    call.parameters.extend([i64(), i64(), i64(), i64()]);
    call.foreign_arguments = vec![group(5, 3, 17, 8)];
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 5), Some(AbiSlot::Stack { offset: 0 }));
    assert_eq!(slot_of(&layout, 6), Some(AbiSlot::Stack { offset: 8 }));
    assert_eq!(slot_of(&layout, 7), Some(AbiSlot::Stack { offset: 16 }));
    assert_eq!(slot_of(&layout, 8), Some(AbiSlot::Integer(Gpr::R9)));
}

/// 0-byte aggregate：前端不产生 lane，零尺寸参数不占槽。
#[test]
fn zero_sized_values_occupy_no_slot() {
    let call = foreign_call(
        vec![
            ValueType::scalar(Type::Void),
            i64(),
            ValueType::scalar(Type::Mem),
        ],
        vec![],
    );
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 0), None);
    assert_eq!(slot_of(&layout, 1), Some(AbiSlot::Integer(Gpr::Rdi)));
    assert_eq!(slot_of(&layout, 2), None);
    let layout = abi::classify_call(&call, TargetName::X86_64Windows).unwrap();
    assert_eq!(slot_of(&layout, 1), Some(AbiSlot::Integer(Gpr::Rcx)));
}

/// 16-byte 对齐组：栈位置先按 align 填充，组后下一段从对齐结束处继续。
#[test]
fn sysv_stack_aggregate_respects_alignment_padding() {
    let mut call = foreign_call(vec![i64(); 7], vec![]);
    call.foreign_arguments = vec![group(6, 1, 8, 32)];
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    // 第 7 个整数耗尽 Rdi..R9 后第一栈槽位置为 0，32 对齐组从对齐位置 0 起。
    assert_eq!(slot_of(&layout, 6), Some(AbiSlot::Stack { offset: 0 }));
    // 16 对齐组前 7 个整数：栈从 0 开始填充到 16。
    let mut call = foreign_call(vec![i64(); 8], vec![]);
    call.foreign_arguments = vec![group(6, 1, 16, 16)];
    call.foreign_arguments.push(group(7, 1, 1, 1));
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 6), Some(AbiSlot::Stack { offset: 0 }));
}

/// sret：结果超过两个 piece 时隐藏指针占第一个整数槽，results 为空。
#[test]
fn sret_hidden_pointer_consumes_first_integer_slot() {
    let mut call = foreign_call(vec![ptr(), i64()], vec![]);
    call.sret = Some((0, 24, [7; 32]));
    let sysv = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert!(sysv.sret);
    assert!(sysv.results.is_empty());
    assert!(sysv.arguments[0].indirect);
    assert_eq!(slot_of(&sysv, 0), Some(AbiSlot::Integer(Gpr::Rdi)));
    assert_eq!(slot_of(&sysv, 1), Some(AbiSlot::Integer(Gpr::Rsi)));
    let win = abi::classify_call(&call, TargetName::X86_64Windows).unwrap();
    assert!(win.sret);
    assert!(win.arguments[0].indirect);
    assert_eq!(slot_of(&win, 0), Some(AbiSlot::Integer(Gpr::Rcx)));
    assert_eq!(slot_of(&win, 1), Some(AbiSlot::Integer(Gpr::Rdx)));
}

/// 返回值：整数 RAX/RDX、浮点 XMM0/XMM1，与 SysV/MS 一致。
#[test]
fn c_return_slots_match_reference_abis() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let layout = abi::classify_call(&foreign_call(vec![], vec![i64(), i64()]), target).unwrap();
        assert_eq!(
            layout.results.iter().map(|r| r.slot).collect::<Vec<_>>(),
            vec![
                Some(AbiSlot::Integer(Gpr::Rax)),
                Some(AbiSlot::Integer(Gpr::Rdx))
            ],
            "{target:?} integer results"
        );
        let layout = abi::classify_call(&foreign_call(vec![], vec![f64(), f64()]), target).unwrap();
        assert_eq!(
            layout.results.iter().map(|r| r.slot).collect::<Vec<_>>(),
            vec![
                Some(AbiSlot::Float(Xmm::Xmm0)),
                Some(AbiSlot::Float(Xmm::Xmm1))
            ],
            "{target:?} float results"
        );
        // 混合返回按声明序分配两 bank。
        let layout = abi::classify_call(&foreign_call(vec![], vec![i64(), f64()]), target).unwrap();
        assert_eq!(layout.results[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
        assert_eq!(layout.results[1].slot, Some(AbiSlot::Float(Xmm::Xmm0)));
    }
}

/// SysV 寄存器耗尽：第 7 个整数与第 9 个浮点都从栈偏移 0 起分配。
#[test]
fn sysv_register_exhaustion_spills_to_stack_in_order() {
    let mut call = foreign_call(vec![i64(); 7], vec![]);
    call.parameters.extend([f64(); 9]);
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 6), Some(AbiSlot::Stack { offset: 0 }));
    assert_eq!(slot_of(&layout, 15), Some(AbiSlot::Stack { offset: 8 }));
    assert_eq!(layout.stack_slots, 2);
}

/// 混合 aggregate 与普通参数：组前后标量槽位不串扰，栈组之后标量继续走寄存器。
#[test]
fn sysv_aggregate_group_does_not_consume_scalar_registers() {
    let mut call = foreign_call(vec![i64(), i64(), i64(), i64(), i64()], vec![]);
    // {double,double} 16-byte 浮点组（memory=false）占用两个浮点槽。
    call.foreign_arguments = vec![group(1, 2, 16, 8)];
    call.parameters[1] = f64();
    call.parameters[2] = f64();
    let layout = abi::classify_call(&call, TargetName::X86_64Linux).unwrap();
    assert_eq!(slot_of(&layout, 0), Some(AbiSlot::Integer(Gpr::Rdi)));
    assert_eq!(slot_of(&layout, 1), Some(AbiSlot::Float(Xmm::Xmm0)));
    assert_eq!(slot_of(&layout, 2), Some(AbiSlot::Float(Xmm::Xmm1)));
    assert_eq!(slot_of(&layout, 3), Some(AbiSlot::Integer(Gpr::Rsi)));
    assert_eq!(slot_of(&layout, 4), Some(AbiSlot::Integer(Gpr::Rdx)));
}
