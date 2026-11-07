use super::*;

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
            foreign_arguments: vec![],
            sret: None,
            poll_free_leaf: false,
        };
        assert!(!call_valid(&call, &call.parameters, &[]));
        call.by_value.push((0, [0; 32], 24));
        assert!(call_valid(&call, &call.parameters, &[]));
        call.by_value.clear();
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
