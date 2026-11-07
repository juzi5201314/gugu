//! LIR `Op` 的稳定 GVN key 编码。
use crate::body::{Op, ValueId};
use crate::canonical;

pub fn encode_op_key(op: &Op, arguments: &[ValueId]) -> Vec<u8> {
    canonical::to_vec(&(op, arguments)).expect("GVN op key 编码")
}

#[cfg(test)]
mod tests {
    use super::encode_op_key;
    use crate::body::{IntOp, Op, ValueId};

    #[test]
    fn key_distinguishes_operation_and_arguments() {
        let add = encode_op_key(&Op::Integer(IntOp::Add), &[ValueId(1), ValueId(2)]);
        let swapped = encode_op_key(&Op::Integer(IntOp::Add), &[ValueId(2), ValueId(1)]);
        let sub = encode_op_key(&Op::Integer(IntOp::Sub), &[ValueId(1), ValueId(2)]);
        assert_ne!(add, swapped);
        assert_ne!(add, sub);
        assert_eq!(
            add,
            encode_op_key(&Op::Integer(IntOp::Add), &[ValueId(1), ValueId(2)])
        );
    }

    #[test]
    fn key_covers_memory_vector_runtime_symbol_and_conversion_fields() {
        use crate::body::{
            Access, AliasClass, Call, CallTarget, Conversion, RuntimeCall, Symbol, VectorOp,
        };
        use crate::gir::body::CallKind;
        let access = Access {
            alias: AliasClass::Foreign,
            align: 8,
            volatile: false,
        };
        let runtime = |cases| {
            Op::Call(Call {
                target: CallTarget::Runtime(RuntimeCall::SelectCommit {
                    cases,
                    has_default: false,
                }),
                kind: CallKind::Managed,
                parameters: Vec::new(),
                results: Vec::new(),
                may_unwind: false,
                may_suspend: true,
                may_allocate: false,
                captures_arguments: false,
                by_value: Vec::new(),
                sret: None,
                foreign_arguments: Vec::new(),
                poll_free_leaf: false,
            })
        };
        for (left, right) in [
            (
                Op::Load(access.clone()),
                Op::Load(Access { align: 4, ..access }),
            ),
            (
                Op::Vector(VectorOp::Extract(1)),
                Op::Vector(VectorOp::Extract(2)),
            ),
            (runtime(1), runtime(2)),
            (
                Op::SymbolAddr(Symbol::Instance([1; 32])),
                Op::SymbolAddr(Symbol::Instance([2; 32])),
            ),
            (
                Op::Convert(Conversion::IntToFloat { signed: true }),
                Op::Convert(Conversion::IntToFloat { signed: false }),
            ),
        ] {
            let key = encode_op_key(&left, &[ValueId(3)]);
            assert_eq!(key, encode_op_key(&left, &[ValueId(3)]));
            assert_ne!(key, encode_op_key(&right, &[ValueId(3)]));
        }
    }
}
