//! 真实源码的 C 类型布局和调用分组贯穿到平台寄存器/栈槽分类。
use crate::backend::x64::{
    abi::{self, AbiSlot},
    reg::{Gpr, Xmm},
};
use crate::lir::body::{Call, CallTarget};
use crate::lir::body::{Op, Terminator, Type};
use crate::{CompileRequest, Compiler, TargetName};

fn verify(target: TargetName) {
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/foreign_abi.gg"),
        target,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let mut calls = std::collections::BTreeMap::new();
    for body in compilation.lir.as_ref().unwrap().bodies() {
        for instruction in &body.instructions {
            if let Op::ForeignCall(call) = &instruction.op {
                record(&mut calls, call);
            }
        }
        for block in &body.blocks {
            if let Terminator::Invoke { call, .. } = &block.terminator {
                record(&mut calls, call);
            }
        }
    }
    let floats = calls["floats"];
    let mixed = calls["mixed"];
    let large = calls["large"];
    let small = calls["small"];
    let transparent = calls["transparent"];
    if target == TargetName::X86_64Linux {
        assert_eq!(
            floats.parameters.iter().map(|ty| ty.ty).collect::<Vec<_>>(),
            [Type::F64, Type::F64]
        );
        assert_eq!(floats.results, floats.parameters);
        let layout = abi::classify_call(mixed, target).unwrap();
        assert_eq!(layout.arguments[6].slot, Some(AbiSlot::Stack { offset: 0 }));
        assert_eq!(layout.arguments[7].slot, Some(AbiSlot::Stack { offset: 8 }));
        assert_eq!(layout.arguments[8].slot, Some(AbiSlot::Float(Xmm::Xmm0)));
        assert_eq!(layout.results[0].slot, Some(AbiSlot::Integer(Gpr::Rax)));
        assert_eq!(layout.results[1].slot, Some(AbiSlot::Float(Xmm::Xmm0)));
        assert_eq!(large.parameters.len(), 4, "sret + 三个按值栈车道");
        let layout = abi::classify_call(large, target).unwrap();
        for index in 0..3 {
            assert_eq!(
                layout.arguments[index + 1].slot,
                Some(AbiSlot::Stack {
                    offset: index as u32 * 8
                })
            );
        }
        assert_eq!(small.results[0].ty, Type::F32);
    } else {
        assert!(floats.sret.is_some());
        assert_eq!(floats.parameters.len(), 2, "sret + 独立副本指针");
        assert_eq!(floats.by_value[0].0, 1);
        assert!(mixed.sret.is_some());
        assert_eq!(mixed.parameters.len(), 9);
        assert_eq!(large.parameters.len(), 2);
        assert_eq!(large.by_value[0].2, 24);
        assert_eq!(small.parameters[0].ty, Type::I32);
        assert_eq!(small.results[0].ty, Type::I32);
    }
    assert!(large.sret.is_some());
    assert_eq!(transparent.parameters[0].ty, Type::F32);
    assert_eq!(transparent.results[0].ty, Type::F32);
}

fn record<'a>(calls: &mut std::collections::BTreeMap<&'a str, &'a Call>, call: &'a Call) {
    if let CallTarget::External { name, .. } = &call.target {
        calls.insert(name, call);
    }
}

#[test]
fn foreign_aggregates_follow_sysv_abi() {
    verify(TargetName::X86_64Linux);
}
#[test]
fn foreign_aggregates_follow_win64_abi() {
    verify(TargetName::X86_64Windows);
}
