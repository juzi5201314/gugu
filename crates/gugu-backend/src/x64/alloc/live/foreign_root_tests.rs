use super::*;
use crate::gir::body::CallKind;
use crate::lir::body::{
    Call, CallTarget, Origin, Provenance, Signature, Value, ValueId, ValueType,
};
use crate::x64::lower::{self, LowerCtx, SiteValue};

pub(crate) fn body(parameters: &[ValueType]) -> Body {
    Body {
        revision: crate::lir::body::REVISION,
        instance: [0; 32],
        owner: [0; 32],
        name: "bridge_roots".into(),
        target: "x86_64-linux".into(),
        signature: Signature {
            parameters: parameters.to_vec(),
            results: vec![],
            sret: None,
            by_value: vec![],
        },
        values: parameters
            .iter()
            .enumerate()
            .map(|(index, &kind)| Value {
                definition: Definition::Parameter {
                    block: BlockId(0),
                    index: index as u32,
                },
                kind,
                origin: Origin::Parameter(index as u32),
                uses: 0..0,
            })
            .collect(),
        blocks: vec![],
        instructions: vec![],
        operands: vec![],
        parameters: vec![],
        edges: vec![],
        predecessors: vec![],
        switch_cases: vec![],
        stack_slots: vec![],
        lifetimes: vec![],
        root_inits: vec![],
        safepoints: vec![],
        barrier_permits: vec![],
        no_safepoint_regions: vec![],
        source_scopes: vec![],
        uses: vec![],
        data: vec![],
        assembly: vec![],
        environments: vec![],
        entry: BlockId(0),
        poll_summary: Default::default(),
        poll_interface: Default::default(),
    }
}

#[test]
fn bridge_packet_roots_cover_register_and_stack_copies() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        for kind in [CallKind::ForeignBridge, CallKind::ForeignBridgeDirtyCpu] {
            let parameters = vec![ValueType::pointer(Provenance::Stack); 8];
            let body = body(&parameters);
            let call = Call {
                target: CallTarget::External {
                    key: [0; 32],
                    name: "probe".into(),
                },
                kind,
                parameters,
                results: vec![],
                may_unwind: false,
                may_suspend: true,
                may_allocate: true,
                captures_arguments: false,
                by_value: (1..8).map(|index| (index, [0; 32], 24)).collect(),
                sret: Some((0, 24, [0; 32])),
                foreign_signature: None,
                foreign_arguments: vec![],
                poll_free_leaf: false,
                scoped_arguments: Vec::new(),
            };
            let operands: Vec<_> = call
                .parameters
                .iter()
                .enumerate()
                .map(|(index, &ty)| SiteValue {
                    ty,
                    reg: Reg::Virtual(index as u32),
                })
                .collect();
            let lowered = lower::lower_with(
                &Op::ForeignCall(call.clone()),
                &operands,
                &[],
                &lower::probe_source(),
                LowerCtx {
                    target,
                    body: None,
                    universe: None,
                    raw: None,
                    site: 0,
                },
            )
            .unwrap();
            let call_index = lowered
                .sequence
                .instructions
                .iter()
                .position(|inst| table::form(inst.form).mnemonic == "call")
                .unwrap() as u32;
            let mut live = LiveInfo {
                points: vec![Point {
                    site: 0,
                    kind: PointKind::Bridge,
                    block: 0,
                    use_slot: 0,
                    def_slot: 1,
                    instructions: call_index..call_index + 1,
                    mask: Clobbers::NONE,
                    clobber: Clobbers::NONE,
                    pointer_spill: true,
                    fixed_physical: true,
                    outgoing_roots: vec![],
                }],
                sites: vec![SitePoints {
                    site: 0,
                    points: 0..1,
                }],
                values: vec![],
            };
            attach_outgoing(
                &body,
                &call,
                &(0..8).map(ValueId).collect::<Vec<_>>(),
                target,
                &lowered,
                0,
                &mut live,
            )
            .unwrap();
            let offsets: Vec<_> = live.points[0]
                .outgoing_roots
                .iter()
                .map(|root| (root.value, root.root, root.offset))
                .collect();
            let expected = if target == TargetName::X86_64Linux {
                [16, 24, 32, 40, 48, 56, 176, 184]
            } else {
                [16, 24, 32, 40, 208, 216, 224, 232]
            };
            assert_eq!(
                offsets,
                expected
                    .into_iter()
                    .enumerate()
                    .map(|(index, offset)| (index as u32, 2, offset))
                    .collect::<Vec<_>>(),
                "{target:?} {kind:?}：仅地址车道是根，fixup 栈深度不是根"
            );
        }
    }
}
