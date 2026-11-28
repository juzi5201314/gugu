use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, word},
};
use crate::backend::x64::{
    codegen::X64World,
    inst::{RelocKind, RelocTarget},
    mangle::mangle_symbol,
};
use crate::lir::body::Symbol;
use crate::runtime::entry::{Rt0Payload, rt0_params};
use std::mem::offset_of;

#[test]
fn runtime_type_names_resolve_to_the_image_type_section() {
    let compiled = Compiler::shared().compile_full_for_test(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/type_names.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let section = compiled.image_plan().unwrap().gc_type_section();
    let symbols = compiled.x64_world().unwrap().type_symbols(section).unwrap();
    assert_elf_type_relocations(
        compiled.x64_world().unwrap(),
        compiled.image_plan().unwrap().linux_image(),
        section,
    );
    let mut harness = Harness::new(&compiled);
    let mut memory = vec![0; 262144];
    let base = 131072;
    memory[base..base + section.len()].copy_from_slice(section);
    for (symbol, offset) in [(Symbol::TypeRecords, 24), (Symbol::TypeNames, 64)] {
        let expected = u64::from_le_bytes(section[offset..offset + 8].try_into().unwrap());
        assert_eq!(symbols[&mangle_symbol(&symbol)], expected);
        harness.type_pools[usize::from(offset == 64)] = base as u64 + expected;
    }
    assert_eq!(harness.invoke("main.gg", "check", &[], &mut memory), [1]);
    assert!(compiled.x64_world().unwrap().type_symbols(&[]).is_err());
}

fn elf_file_offset(image: &[u8], address: u64) -> usize {
    let headers = word(image, 32);
    let size = u16::from_le_bytes(image[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(image[56..58].try_into().unwrap()) as u64;
    for index in 0..count {
        let at = headers + index * size;
        let kind = u32::from_le_bytes(image[at as usize..at as usize + 4].try_into().unwrap());
        let base = word(image, at + 16);
        if kind == 1 && (base..base + word(image, at + 32)).contains(&address) {
            return (word(image, at + 8) + address - base) as usize;
        }
    }
    panic!("ELF 地址 {address:#x} 不在已落盘的 LOAD 段内");
}

fn displacement(image: &[u8], at: usize) -> i64 {
    i64::from(i32::from_le_bytes(image[at..at + 4].try_into().unwrap()))
}

fn assert_elf_type_relocations(world: &X64World, image: &[u8], types: &[u8]) {
    let entry = word(image, 24);
    let entry_at = elf_file_offset(image, entry);
    assert_eq!(&image[entry_at..entry_at + 3], &[0x48, 0x8d, 0x1d]);
    let params = (entry + 7).wrapping_add_signed(displacement(image, entry_at + 3));
    let payload_slot = elf_file_offset(image, params + rt0_params::PAYLOAD_VA as u64 * 8);
    let payload = elf_file_offset(image, word(image, payload_slot as u64)) as u64;
    let fragments_base = word(
        image,
        payload + offset_of!(Rt0Payload, fragment_base) as u64,
    );
    let types_base = word(image, payload + offset_of!(Rt0Payload, gc_types) as u64);
    let types_at = elf_file_offset(image, types_base);
    assert_eq!(&image[types_at..types_at + types.len()], types);

    // unwind 记录按代码地址排序，顺序与按符号名排序的 LIR 片段一致；机器 glue 只占地址间隙。
    let mut fragments: Vec<_> = world.fragments.iter().collect();
    fragments.sort_by_key(|fragment| &fragment.symbol);
    let unwind = &world.metadata.unwind_section;
    let records = word(unwind, 20);
    let mut seen = [false; 2];
    for (index, fragment) in fragments.into_iter().enumerate() {
        let function = fragments_base + word(unwind, records + index as u64 * 32);
        for relocation in &fragment.relocations {
            let (pool, header) = match relocation.target {
                RelocTarget::Lir(Symbol::TypeRecords) => (0, 24),
                RelocTarget::Lir(Symbol::TypeNames) => (1, 64),
                _ => continue,
            };
            assert_eq!(relocation.kind, RelocKind::PcRel32);
            let slot = function + u64::from(relocation.offset);
            let target = (slot + 4).wrapping_add_signed(
                displacement(image, elf_file_offset(image, slot)) - relocation.addend,
            );
            assert_eq!(
                target,
                types_base + word(types, header),
                "类型池必须指向真实类型段"
            );
            seen[pool] = true;
        }
    }
    assert_eq!(seen, [true; 2], "两个类型池必须实际参与镜像重定位");
}
