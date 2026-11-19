use super::*;
use crate::body::ForeignArgument;

#[test]
fn native_calls_require_valid_original_c_metadata_and_matching_groups() {
    use crate::foreign::{Kind, Signature, Type as CType};
    let mut call = Call {
        target: CallTarget::External {
            key: [0; 32],
            name: "probe".into(),
        },
        kind: CallKind::ForeignBridge,
        parameters: vec![],
        results: vec![],
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: vec![],
        sret: None,
        foreign_arguments: vec![],
        foreign_signature: None,
        poll_free_leaf: false,
    };
    assert!(!call_valid(&call, &[], &[]));
    call.foreign_signature = Some(Signature {
        arguments: vec![],
        result: CType {
            bytes: 0,
            align: 1,
            kind: Kind::Unit,
        },
        variadic: None,
    });
    assert!(call_valid(&call, &[], &[]));
    call.foreign_signature.as_mut().unwrap().variadic = Some(1);
    assert!(!call_valid(&call, &[], &[]));
    call.foreign_signature.as_mut().unwrap().variadic = None;
    call.foreign_signature
        .as_mut()
        .unwrap()
        .arguments
        .push(CType {
            bytes: 4,
            align: 4,
            kind: Kind::Integer { signed: true },
        });
    assert!(!call_valid(&call, &[], &[]));
    call.parameters.push(ValueType::scalar(Type::I32));
    call.foreign_arguments.push(ForeignArgument {
        first: 0,
        lanes: 1,
        bytes: 4,
        align: 4,
        memory: false,
    });
    assert!(call_valid(&call, &call.parameters, &[]));
    call.foreign_arguments[0].bytes = 8;
    assert!(!call_valid(&call, &call.parameters, &[]));
    call.kind = CallKind::Managed;
    assert!(!call_valid(&call, &call.parameters, &[]));
}

#[test]
fn foreign_stack_addresses_require_explicit_abi_temporary() {
    for kind in [
        CallKind::ForeignBridge,
        CallKind::ForeignBridgeDirtyCpu,
        CallKind::ForeignLeaf { stack: 0 },
    ] {
        let mut call = Call {
            target: CallTarget::External {
                key: [0; 32],
                name: "probe".into(),
            },
            kind,
            parameters: vec![ValueType::pointer(Provenance::Stack)],
            results: vec![],
            may_unwind: false,
            may_suspend: false,
            may_allocate: false,
            captures_arguments: false,
            by_value: vec![],
            foreign_signature: Some(crate::foreign::Signature {
                arguments: vec![crate::foreign::Type {
                    bytes: 24,
                    align: 8,
                    kind: crate::foreign::Kind::Aggregate {
                        hfa: None,
                        natural_align: 8,
                    },
                }],
                result: crate::foreign::Type {
                    bytes: 0,
                    align: 1,
                    kind: crate::foreign::Kind::Unit,
                },
                variadic: None,
            }),
            foreign_arguments: vec![ForeignArgument {
                first: 0,
                lanes: 1,
                bytes: 24,
                align: 8,
                memory: true,
            }],
            sret: None,
            poll_free_leaf: false,
        };
        assert!(!call_valid(&call, &call.parameters, &[]));
        call.by_value.push((0, [0; 32], 24));
        assert!(call_valid(&call, &call.parameters, &[]));
        call.by_value.clear();
        call.foreign_arguments.clear();
        call.foreign_signature.as_mut().unwrap().arguments.clear();
        call.sret = Some((0, 24, [0; 32]));
        assert!(call_valid(&call, &call.parameters, &[]));
        for provenance in [
            Provenance::GcHeap,
            Provenance::GcInterior,
            Provenance::SharedHandle,
            Provenance::CompressedRef,
        ] {
            call.parameters[0] = ValueType::pointer(provenance);
            assert!(
                !call_valid(&call, &call.parameters, &[]),
                "{provenance:?} 不能伪装成 ABI 临时地址"
            );
        }
        call.parameters[0] = ValueType::pointer(Provenance::Raw);
        call.sret = None;
        call.results = vec![ValueType::pointer(Provenance::Stack)];
        assert!(!call_valid(&call, &call.parameters, &call.results));
    }
}
