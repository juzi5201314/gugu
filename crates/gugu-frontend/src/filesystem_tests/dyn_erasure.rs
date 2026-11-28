use super::{CompileRequest, Compiler, Op, TargetName, named};
use crate::lir::body::Definition;

#[test]
fn zero_sized_dyn_payload_has_nonzero_allocation() {
    let compilation = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/dyn_zst.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let body = named(&compilation, "erase");
    let allocations: Vec<_> = body
        .instructions
        .iter()
        .filter(|instruction| matches!(instruction.op, Op::GcAlloc { .. }))
        .collect();
    assert_eq!(allocations.len(), 1, "擦除只分配一个 dyn 载荷对象");
    let bytes = *body.args(&allocations[0].arguments).last().unwrap();
    let Definition::Instruction { instruction, .. } = body.values[bytes.index()].definition else {
        panic!("载荷大小必须是编译期常量");
    };
    let Op::IConst(bytes) = body.instructions[instruction.index()].op else {
        panic!("载荷大小必须是整数常量");
    };
    assert!(bytes > 0, "零字节分配返回空地址，不能作为 dyn 的受管载荷");
}
