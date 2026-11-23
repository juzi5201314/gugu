use super::linux_vm::Vm;
use super::*;
use crate::{NativeImport, a64::relocation};

#[test]
fn resolver_imports_and_addresses_follow_linux_c_contract() {
    let names = ["getaddrinfo", "freeaddrinfo"];
    assert_eq!(
        linux_resolver_imports("libc.so"),
        names.map(|name| NativeImport::function("libc.so", name))
    );
    let fragment = linux_resolver_symbol();
    assert_eq!(fragment.relocations.len(), 4);
    let bytes = relocation::relocate(fragment, 0, |name| {
        Ok(match name {
            _ if name == linux_import_stub_symbol("getaddrinfo") => 0x12340,
            _ if name == linux_import_stub_symbol("freeaddrinfo") => 0x23450,
            _ => panic!("unexpected symbol {name}"),
        })
    })
    .unwrap();
    let code = instructions(&Fragment {
        bytes,
        relocations: vec![],
    });
    for (op, address) in [(0, 0x12340), (1, 0x23450)] {
        let mut vm = Vm::new(0, [0; 6]);
        vm.registers[0] = op;
        assert_eq!(
            vm.run_result(&code, &mut |_: &mut Vm| panic!(
                "resolver must not perform syscalls"
            )),
            address
        );
    }
}

#[test]
fn eager_import_stub_preserves_arguments_and_tail_calls_the_got_pointer() {
    let fragment = linux_import_stub("getaddrinfo");
    let code = instructions(&fragment);
    assert_eq!(
        code.iter().map(form).collect::<Vec<_>>(),
        ["adrp", "add.X.imm", "ldr.X.Unsigned", "br"]
    );
    for (index, kind) in [RelocKind::AdrpPage21, RelocKind::AddLo12]
        .into_iter()
        .enumerate()
    {
        assert_eq!(fragment.relocations[index].relocation.kind, kind);
        assert_eq!(
            fragment.relocations[index].relocation.target,
            linux_import_slot_symbol("getaddrinfo")
        );
    }
    assert_eq!(at(&code, 8).operands[0], reg(Reg::x(16)));
    assert_eq!(at(&code, 12).operands, [reg(Reg::x(16))]);
}

#[test]
#[should_panic(expected = "native protocol trap")]
fn invalid_resolver_operation_traps() {
    let code = instructions(&linux_resolver_symbol());
    let mut vm = Vm::new(0, [0; 6]);
    vm.registers[0] = u64::MAX;
    vm.run(&code, |_| panic!("unexpected syscall"));
}
