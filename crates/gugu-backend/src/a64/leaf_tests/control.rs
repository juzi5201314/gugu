use super::*;

#[test]
fn entry_header_backedge_preserves_register_and_stack_parameter_updates() {
    let mut types = [Type::I64; 9];
    types[0] = Type::I8;
    let mut f = Fixture::new(&types, &[Type::I8]);
    let done = f.block(&[Type::I8]);
    let register = f.param(0, 0);
    let counter = f.param(0, 8);
    let one8 = f.constant(Type::I8, 1);
    let one64 = f.constant(Type::I64, 1);
    let next_register = f.op(Op::Integer(IntOp::Sub), &[register, one8], &[Type::I8])[0];
    let next_counter = f.op(Op::Integer(IntOp::Sub), &[counter, one64], &[Type::I64])[0];
    let zero = f.constant(Type::I64, 0);
    let condition = f.op(
        Op::Compare {
            condition: Condition::Ne,
            signed: false,
        },
        &[next_counter, zero],
        &[Type::I8],
    )[0];
    let mut parameters = (0..9).map(|index| f.param(0, index)).collect::<Vec<_>>();
    parameters[0] = next_register;
    parameters[8] = next_counter;
    let repeat = f.edge(0, &parameters);
    let exit = f.edge(done, &[next_register]);
    f.body.blocks[0].terminator = Terminator::Branch {
        condition,
        yes: repeat,
        no: exit,
    };
    f.switch_to(done);
    f.ret(&[f.param(done, 0)]);
    let fragment = f.compile();
    assert!(
        fragment.selected.blocks[0]
            .items
            .iter()
            .any(|site| matches!(site.item, machine::Item::Incoming { .. }))
    );
    assert!(
        fragment
            .selected
            .blocks
            .iter()
            .skip(1)
            .flat_map(|block| &block.items)
            .all(|site| !matches!(site.item, machine::Item::Incoming { .. }))
    );
    assert_eq!(vm::run(&fragment, &[10, 0, 0, 0, 0, 0, 0, 0, 3])[0], 7);
}

#[test]
fn return_copies_resolve_physical_cycles() {
    let mut f = Fixture::new(&[Type::I64; 2], &[Type::I64; 2]);
    f.ret(&[f.param(0, 1), f.param(0, 0)]);
    let fragment = f.compile();
    assert_eq!(vm::run(&fragment, &[3, 7]), [7, 3]);
    assert!(
        decoded(&fragment)
            .iter()
            .any(|inst| inst.operands.contains(&inst::Operand::Reg(reg::Reg::x(16))))
    );
}

#[test]
fn diamond_edge_parameters_and_switch_default() {
    let mut f = Fixture::new(&[Type::I8], &[Type::I64]);
    let join = f.block(&[Type::I64]);
    let yes_value = f.constant(Type::I64, 42);
    let no_value = f.constant(Type::I64, 7);
    let yes = f.edge(join, &[yes_value]);
    let no = f.edge(join, &[no_value]);
    f.body.blocks[0].terminator = Terminator::Branch {
        condition: f.param(0, 0),
        yes,
        no,
    };
    f.switch_to(join);
    f.ret(&[f.param(join, 0)]);
    let fragment = f.compile();
    assert_eq!(vm::run(&fragment, &[1])[0], 42);
    assert_eq!(vm::run(&fragment, &[0])[0], 7);
    let mut f = Fixture::new(&[Type::I64], &[Type::I64]);
    let join = f.block(&[Type::I64]);
    let a = f.constant(Type::I64, 11);
    let b = f.constant(Type::I64, 99);
    let case = f.edge(join, &[a]);
    let default = f.edge(join, &[b]);
    f.body.switch_cases.push((3, case));
    f.body.blocks[0].terminator = Terminator::Switch {
        value: f.param(0, 0),
        cases: 0..1,
        otherwise: default,
    };
    f.switch_to(join);
    f.ret(&[f.param(join, 0)]);
    let fragment = f.compile();
    assert_eq!(vm::run(&fragment, &[3])[0], 11);
    assert_eq!(vm::run(&fragment, &[8])[0], 99);
}

#[test]
fn bounded_loop_parameters_and_parallel_swap() {
    let mut f = Fixture::new(&[], &[Type::I64]);
    let loop_block = f.block(&[Type::I64; 3]);
    let done = f.block(&[Type::I64; 2]);
    let a = f.constant(Type::I64, 10);
    let b = f.constant(Type::I64, 20);
    let n = f.constant(Type::I64, 3);
    let start = f.edge(loop_block, &[a, b, n]);
    f.body.blocks[0].terminator = Terminator::Jump(start);
    f.switch_to(loop_block);
    let (a, b, n) = (
        f.param(loop_block, 0),
        f.param(loop_block, 1),
        f.param(loop_block, 2),
    );
    let one = f.constant(Type::I64, 1);
    let next = f.op(Op::Integer(IntOp::Sub), &[n, one], &[Type::I64])[0];
    let zero = f.constant(Type::I64, 0);
    let condition = f.op(
        Op::Compare {
            condition: Condition::Ne,
            signed: false,
        },
        &[next, zero],
        &[Type::I8],
    )[0];
    let repeat = f.edge(loop_block, &[b, a, next]);
    let exit = f.edge(done, &[b, a]);
    f.body.blocks[loop_block].terminator = Terminator::Branch {
        condition,
        yes: repeat,
        no: exit,
    };
    f.switch_to(done);
    f.ret(&[f.param(done, 0)]);
    assert_eq!(vm::run(&f.compile(), &[])[0], 20);
}

#[test]
fn empty_leaf_traps_and_bad_cfg_errors() {
    let mut f = Fixture::new(&[], &[]);
    f.ret(&[]);
    assert_eq!(f.compile().bytes, 0xd65f03c0_u32.to_le_bytes());
    f.body.blocks[0].terminator = Terminator::Trap { memory: f.memory };
    assert_eq!(decoded(&f.compile())[0].text().unwrap(), "brk #0");
    f.body.blocks[0].terminator = Terminator::Jump(EdgeId(123));
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("block 0")
    );
    f.body.blocks.clear();
    assert!(codegen::compile_leaf(&f.body).is_err());
}

#[test]
fn invoke_unwind_landing_enters_edge_copy_block() {
    let mut f = Fixture::new(&[], &[]);
    let normal_target = f.block(&[]);
    let unwind_target = f.block(&[Type::I64]);
    let value = f.constant(Type::I64, 7);
    let normal = f.edge(normal_target, &[]);
    let unwind = f.edge(unwind_target, &[value]);
    f.body.edges[unwind.index()].unwind = true;
    let output = f.value(
        Type::Mem,
        Definition::Invoke {
            block: BlockId(0),
            result: 0,
        },
    );
    let safepoint = SafepointId(f.body.safepoints.len() as u32);
    f.body.safepoints.push(Safepoint {
        kind: SafepointKind::CallReturn,
        block: BlockId(0),
        instruction: None,
    });
    f.body.blocks[0].terminator = Terminator::Invoke {
        call: Call {
            target: CallTarget::Instance([1; 32]),
            kind: crate::gir::body::CallKind::Managed,
            parameters: vec![],
            results: vec![],
            may_unwind: true,
            may_suspend: false,
            may_allocate: false,
            captures_arguments: false,
            by_value: vec![],
            foreign_arguments: vec![],
            foreign_signature: None,
            sret: None,
            poll_free_leaf: false,
        },
        arguments: f.args(&[]),
        results: 0..0,
        memory: Memory {
            input: f.memory,
            output,
        },
        normal,
        unwind,
        safepoint: Some(safepoint),
    };
    f.switch_to(normal_target);
    f.ret(&[]);
    f.switch_to(unwind_target);
    f.ret(&[]);

    let function = select::select_runtime(&f.body).unwrap();
    let unwind_edge_block = (f.body.blocks.len() + unwind.index() + 1) as u32;
    let landing = function.blocks[1]
        .items
        .iter()
        .find_map(|site| match &site.item {
            machine::Item::Call { landing, .. } => *landing,
            _ => None,
        });
    assert_eq!(landing, Some((unwind_edge_block, unwind_target as u32)));
    assert!(
        function.blocks[unwind_edge_block as usize]
            .items
            .iter()
            .any(|site| matches!(&site.item, machine::Item::Copies(pairs)
                if pairs == &[(machine::MReg::Virtual(value.0), machine::MReg::Virtual(f.param(unwind_target, 0).0))]))
    );
}
