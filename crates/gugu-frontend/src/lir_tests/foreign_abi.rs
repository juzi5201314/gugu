//! 真实源码的 C 类型布局和调用分组贯穿到平台寄存器/栈槽分类。
use crate::backend::x64::{
    abi::{self, AbiSlot},
    reg::{Gpr, Xmm},
};
use crate::lir::body::{Call, CallTarget};
use crate::lir::body::{Op, Terminator, Type};
use crate::{CompileRequest, Compiler, TargetName};

fn verify(target: TargetName) {
    let mut request =
        CompileRequest::single_file("main.gg", include_str!("fixtures/foreign_abi.gg"), target);
    if target == TargetName::X86_64Macos {
        request = request.with_native_link(crate::NativeLink {
            imports: [
                "floats",
                "mixed",
                "large",
                "small",
                "transparent",
                "nested",
                "too_many",
                "narrow",
                "empty",
                "unions",
                "enums",
                "array",
            ]
            .map(|name| crate::NativeImport::function("/fixture/libabi.dylib", name))
            .to_vec(),
            interpreter: None,
        });
    }
    let compilation = Compiler::shared().compile(request);
    if target == TargetName::X86_64Macos {
        let errors: Vec<_> = compilation
            .diagnostics()
            .items()
            .iter()
            .filter(|diagnostic| diagnostic.severity() == crate::Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code(), crate::DiagnosticCode::BackendInvariant);
        assert_eq!(
            errors[0].message(),
            "Mach-O executable does not support C exports"
        );
    } else {
        assert!(
            compilation.is_success(),
            "{:?}",
            compilation.diagnostics().items()
        );
    }
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
    if target != TargetName::X86_64Windows {
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
    verify_metadata(&calls);
    let export = compilation
        .lir
        .as_ref()
        .unwrap()
        .exports()
        .iter()
        .find(|export| export.name == "abi_metadata_export")
        .unwrap();
    let signature = export.call.foreign_signature.as_ref().unwrap();
    assert!(signature.matches(&export.call));
    assert_eq!(
        signature.arguments[0].kind,
        crate::lir::foreign::Kind::Integer { signed: true }
    );
    assert_eq!(signature.result.kind, signature.arguments[0].kind);
}

#[test]
fn overaligned_composites_keep_object_alignment_but_use_linux_natural_pcs_alignment() {
    use crate::backend::a64::foreign::{Platform, Slot, classify};
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/foreign_overaligned.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let mut signatures = std::collections::BTreeMap::new();
    for body in compilation.lir.as_ref().unwrap().bodies() {
        let calls = body
            .instructions
            .iter()
            .filter_map(|inst| match &inst.op {
                Op::ForeignCall(call) => Some(call),
                _ => None,
            })
            .chain(
                body.blocks
                    .iter()
                    .filter_map(|block| match &block.terminator {
                        Terminator::Invoke { call, .. } => Some(call),
                        _ => None,
                    }),
            );
        for call in calls {
            if let CallTarget::External { name, .. } = &call.target
                && (name == "reg" || name == "stack")
            {
                signatures.insert(name.as_str(), call.foreign_signature.as_ref().unwrap());
            }
        }
    }
    let reg = signatures["reg"];
    assert_eq!((reg.arguments[1].bytes, reg.arguments[1].align), (16, 16));
    for platform in [Platform::Linux, Platform::Darwin] {
        let layout = classify(reg, platform).unwrap();
        assert_eq!(layout.arguments[1].pieces[0].slot, Slot::Integer(1));
        assert_eq!(layout.arguments[1].pieces[1].slot, Slot::Integer(2));
        assert_eq!(layout.arguments[2].pieces[0].slot, Slot::Integer(3));
        let layout = classify(signatures["stack"], platform).unwrap();
        let start = if platform == Platform::Linux { 8 } else { 16 };
        assert_eq!(layout.arguments[9].pieces[0].slot, Slot::Stack(start));
        assert_eq!(layout.arguments[9].pieces[1].slot, Slot::Stack(start + 8));
        assert_eq!(layout.arguments[10].pieces[0].slot, Slot::Stack(start + 16));
    }
}

#[test]
fn packed_composites_honor_packing_before_outer_alignment() {
    use crate::backend::a64::foreign::{Platform, Slot, classify};
    use crate::lir::foreign::Kind;
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/foreign_packed.gg"),
        TargetName::X86_64Linux,
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
    for (name, object_align) in [("packed", 1), ("aligned_packed", 16)] {
        let signature = calls[name].foreign_signature.as_ref().unwrap();
        assert_eq!(signature.arguments[1].align, object_align);
        for platform in [Platform::Linux, Platform::Darwin] {
            let layout = classify(signature, platform).unwrap();
            assert_eq!(layout.arguments[1].pieces[0].slot, Slot::Integer(1));
            assert_eq!(layout.arguments[1].pieces[1].slot, Slot::Integer(2));
            assert_eq!(layout.arguments[2].pieces[0].slot, Slot::Integer(3));
        }
        assert_eq!(
            signature.arguments[1].kind,
            Kind::Aggregate {
                hfa: None,
                natural_align: 1
            }
        );
    }
}

fn verify_metadata(calls: &std::collections::BTreeMap<&str, &Call>) {
    use crate::lir::foreign::{Hfa, Kind};
    for call in calls.values() {
        let signature = call
            .foreign_signature
            .as_ref()
            .expect("original C signature");
        assert!(signature.valid());
        assert_eq!(signature.variadic, None);
    }
    let signature = |name| calls[name].foreign_signature.as_ref().unwrap();
    assert_eq!(
        signature("array").result.kind,
        Kind::Aggregate {
            natural_align: 4,
            hfa: Some(Hfa {
                bits: 32,
                offsets: vec![0, 4, 8, 12]
            })
        }
    );
    assert_eq!(
        signature("floats").arguments,
        [signature("floats").result.clone()]
    );
    assert_eq!(
        signature("floats").result.kind,
        Kind::Aggregate {
            natural_align: 8,
            hfa: Some(Hfa {
                bits: 64,
                offsets: vec![0, 8]
            })
        }
    );
    assert_eq!(
        signature("small").result.kind,
        Kind::Aggregate {
            natural_align: 4,
            hfa: Some(Hfa {
                bits: 32,
                offsets: vec![0]
            })
        }
    );
    assert_eq!(
        signature("nested").result.kind,
        Kind::Aggregate {
            natural_align: 4,
            hfa: Some(Hfa {
                bits: 32,
                offsets: vec![0, 4, 8]
            })
        }
    );
    for name in ["mixed", "large", "too_many"] {
        assert_eq!(
            signature(name).result.kind,
            Kind::Aggregate {
                hfa: None,
                natural_align: if name == "too_many" { 4 } else { 8 }
            }
        );
    }
    assert_eq!(
        signature("transparent").result.kind,
        Kind::Float { bits: 32 }
    );
    assert_eq!(
        signature("narrow").arguments[0].kind,
        Kind::Integer { signed: true }
    );
    assert_eq!(
        signature("narrow").arguments[1].kind,
        Kind::Integer { signed: false }
    );
    assert_eq!(
        signature("narrow").result.kind,
        Kind::Integer { signed: true }
    );
    assert_eq!(
        signature("unions").arguments[0].kind,
        Kind::Aggregate {
            natural_align: 4,
            hfa: Some(Hfa {
                bits: 32,
                offsets: vec![0, 4, 8]
            })
        }
    );
    assert_eq!(
        signature("unions").arguments[1].kind,
        Kind::Aggregate {
            hfa: None,
            natural_align: 8
        }
    );
    assert_eq!(
        signature("enums").arguments[0].kind,
        Kind::Integer { signed: true }
    );
    assert_eq!(
        signature("enums").arguments[1].kind,
        Kind::Integer { signed: false }
    );
    assert_eq!(
        signature("enums").result.kind,
        Kind::Integer { signed: true }
    );
    assert!(signature("empty").arguments.is_empty());
    assert_eq!(signature("empty").result.kind, Kind::Unit);
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

#[test]
fn foreign_aggregates_preserve_c_metadata_on_darwin() {
    verify(TargetName::X86_64Macos);
}
